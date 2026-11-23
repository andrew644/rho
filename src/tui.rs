use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
};
use futures_util::StreamExt;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use ratatui::Frame;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

use crate::agent::{AgentCommand, AgentEvent, Origin};
use crate::clipboard::{Clipboard, ClipboardMode};
use crate::config::ThemeName;
use crate::discord::{Bridge, RemoteCommand};
use crate::loops::parse_loop_spec;
use crate::selection::{self, Anchor, Selection};
use crate::session::{self, SessionInfo};
use crate::templates::{self, Template};

#[derive(Debug, PartialEq)]
pub enum InputAction {
    Submit(String),
    Command(Command),
}

#[derive(Debug, PartialEq)]
pub enum Command {
    Quit,
    Stop,
    Loop(String),
    Help,
    Compact(Option<String>),
    New,
    Resume(String),
    Session,
    Discord,
    Name(String),
    Model(String),
    Pwd(String),
    Template { name: String, args: String },
    Shell { command: String, to_model: bool },
    Unknown(String),
}

pub fn classify_input(input: &str, template_names: &[String]) -> Option<InputAction> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    if let Some(rest) = input.strip_prefix("!!") {
        let command = rest.trim();
        if !command.is_empty() {
            return Some(InputAction::Command(Command::Shell {
                command: command.to_string(),
                to_model: false,
            }));
        }
        return Some(InputAction::Command(Command::Unknown("!!".to_string())));
    }
    if let Some(rest) = input.strip_prefix('!') {
        let command = rest.trim();
        if !command.is_empty() {
            return Some(InputAction::Command(Command::Shell {
                command: command.to_string(),
                to_model: true,
            }));
        }
        return Some(InputAction::Command(Command::Unknown("!".to_string())));
    }
    if let Some(rest) = input.strip_prefix(':') {
        let (name, args) = match rest.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim()),
            None => (rest, ""),
        };
        let cmd = match name {
            "quit" | "exit" => Command::Quit,
            "stop" | "interrupt" => Command::Stop,
            "loop" => Command::Loop(args.to_string()),
            "help" => Command::Help,
            "compact" => Command::Compact(if args.is_empty() {
                None
            } else {
                Some(args.to_string())
            }),
            "new" => Command::New,
            "resume" => Command::Resume(args.to_string()),
            "session" => Command::Session,
            "discord" => Command::Discord,
            "name" => Command::Name(args.to_string()),
            "model" => Command::Model(args.to_string()),
            "pwd" => Command::Pwd(args.to_string()),
            other if template_names.iter().any(|t| t == other) => Command::Template {
                name: other.to_string(),
                args: args.to_string(),
            },
            other => Command::Unknown(other.to_string()),
        };
        Some(InputAction::Command(cmd))
    } else {
        Some(InputAction::Submit(input.to_string()))
    }
}

/// True when text typed into the input would be read as a command rather than
/// as a prompt: a `:` command or a `!`/`!!` shell escape. The Discord bridge
/// asks this before queueing a message, because commands belong to the window
/// that draws the transcript, not to the model.
pub fn is_command(input: &str) -> bool {
    matches!(classify_input(input, &[]), Some(InputAction::Command(_)))
}

#[derive(Clone)]
struct Entry {
    text: String,
    kind: Kind,
    output: Option<String>,
}

/// A command that came from Discord: every transcript line it adds is posted
/// back to the channel that asked for it. `base` is where those lines start in
/// `entries`, so the reply says exactly what the window showed.
struct Capture {
    origin: Origin,
    request: String,
    base: usize,
}

/// Commands whose output the agent writes rather than the window: their lines
/// land after the command was handed over, so the reply waits for the agent to
/// go idle instead of posting an empty answer.
fn owes_agent_output(action: &Option<InputAction>) -> bool {
    matches!(
        action,
        Some(InputAction::Command(
            Command::Shell { .. }
                | Command::Loop(_)
                | Command::Compact(_)
                | Command::New
                | Command::Resume(_)
                | Command::Name(_)
                | Command::Model(_)
                | Command::Pwd(_)
                | Command::Template { .. }
        ))
    )
}

const TOOL_OUTPUT_LINES: usize = 20;

fn summarize_args(args: &str) -> String {
    let parsed = serde_json::from_str::<serde_json::Value>(args).ok();
    let values = parsed.as_ref().and_then(|v| v.as_object());
    let summary = match values {
        Some(map) => ["path", "command"]
            .iter()
            .find_map(|k| map.get(*k).and_then(|v| v.as_str()))
            .or_else(|| map.values().find_map(|v| v.as_str()))
            .unwrap_or(args)
            .to_string(),
        None => args.to_string(),
    };
    let first_line = summary.lines().next().unwrap_or("");
    let mut short: String = first_line.chars().take(60).collect();
    if first_line.chars().count() > 60 {
        short.push('…');
    }
    short
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    User,
    Assistant,
    Reasoning,
    Tool,
    Error,
    Info,
}

fn kind_prefix(kind: Kind) -> &'static str {
    match kind {
        Kind::User => "❯ ",
        Kind::Assistant => "",
        Kind::Reasoning => "∴ ",
        Kind::Tool => "# ",
        Kind::Error => "! ",
        Kind::Info => "· ",
    }
}

fn kind_style(theme: &Theme, kind: Kind) -> Style {
    match theme.name {
        ThemeName::Dark => match kind {
            Kind::User => Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            Kind::Assistant => Style::default(),
            Kind::Reasoning => Style::default().fg(Color::DarkGray),
            Kind::Tool => Style::default().fg(Color::Yellow),
            Kind::Error => Style::default().fg(Color::Red),
            Kind::Info => Style::default().fg(Color::Green),
        },
        ThemeName::Light => match kind {
            Kind::User => Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
            Kind::Assistant => Style::default().fg(Color::Black),
            Kind::Reasoning => Style::default().fg(Color::Blue),
            Kind::Tool => Style::default().fg(Color::Magenta),
            Kind::Error => Style::default().fg(Color::Red),
            Kind::Info => Style::default().fg(Color::Green),
        },
        ThemeName::Ansi => match kind {
            Kind::User => Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
            Kind::Assistant => Style::default(),
            Kind::Reasoning => Style::default().fg(Color::Magenta),
            Kind::Tool => Style::default().fg(Color::Yellow),
            Kind::Error => Style::default().fg(Color::Red),
            Kind::Info => Style::default().fg(Color::Green),
        },
    }
}

#[derive(Clone)]
pub struct Theme {
    name: ThemeName,
}

impl Theme {
    pub fn new(name: ThemeName) -> Self {
        Self { name }
    }

    fn output_style(&self) -> Style {
        match self.name {
            ThemeName::Light => Style::default().fg(Color::Black),
            _ => Style::default().fg(Color::DarkGray),
        }
    }

    fn code_style(&self) -> Style {
        match self.name {
            ThemeName::Dark => Style::default().fg(Color::White).bg(Color::DarkGray),
            ThemeName::Light => Style::default().fg(Color::Black).bg(Color::DarkGray),
            ThemeName::Ansi => Style::default().add_modifier(Modifier::UNDERLINED),
        }
    }

    fn busy_border_color(&self) -> Color {
        match self.name {
            ThemeName::Light => Color::Black,
            _ => Color::DarkGray,
        }
    }
}

fn assistant_lines(text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let style = kind_style(theme, Kind::Assistant);
    let code_style = theme.code_style();
    let mut lines = Vec::new();
    let mut in_fence = false;
    let src_lines: Vec<&str> = if text.is_empty() {
        vec![""]
    } else {
        text.lines().collect()
    };
    for src in src_lines {
        if src.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            lines.push(Line::from(Span::styled(format!("    {src}"), code_style)));
        } else {
            let mut wrapped = wrap_words(src, width.max(1));
            if wrapped.is_empty() {
                wrapped.push(String::new());
            }
            for w in wrapped {
                lines.push(Line::from(Span::styled(w, style)));
            }
        }
    }
    lines
}

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

const INPUT_INDENT: usize = 2;

/// Clicks this close together at the same cell are counted as a double/triple click.
const CLICK_CHAIN: Duration = Duration::from_millis(400);

/// How long the footer keeps showing the result of the last copy.
const NOTICE_TTL: Duration = Duration::from_secs(4);

const SELECTION_STYLE: Style = Style::new().add_modifier(Modifier::REVERSED);

#[derive(Debug)]
struct InputRow {
    line: usize,
    sub: usize,
    start: usize,
    text: String,
}

#[derive(Debug)]
struct InputLayout {
    lines: Vec<Line<'static>>,
    cursor_row: usize,
    cursor_col: usize,
}

const BUILTIN_COMMANDS: [&str; 13] = [
    "compact", "discord", "exit", "help", "loop", "model", "name", "new", "pwd", "quit", "resume",
    "session", "stop",
];

fn expand_home(token: &str) -> String {
    let Some(home) = dirs::home_dir() else {
        return token.to_string();
    };
    if token == "~" {
        home.display().to_string()
    } else if let Some(rest) = token.strip_prefix("~/") {
        format!("{}/{}", home.display(), rest)
    } else {
        token.to_string()
    }
}

fn fmt_tokens(n: usize) -> String {
    let (v, unit) = if n >= 1_000_000 {
        (n as f64 / 1_000_000.0, 'M')
    } else if n >= 1000 {
        (n as f64 / 1000.0, 'k')
    } else {
        return n.to_string();
    };
    let mut num = format!("{v:.1}");
    if let Some(stripped) = num.strip_suffix(".0") {
        num = stripped.to_string();
    }
    format!("{num}{unit}")
}

struct Run {
    col: usize,
    start: usize,
    end: usize,
    space: bool,
}

fn text_runs(text: &str) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for (col, (byte, c)) in text.char_indices().enumerate() {
        let space = c.is_whitespace();
        match runs.last_mut() {
            Some(run) if run.space == space => run.end = byte + c.len_utf8(),
            _ => runs.push(Run {
                col,
                start: byte,
                end: byte + c.len_utf8(),
                space,
            }),
        }
    }
    runs
}

struct Wrapper {
    width: usize,
    rows: Vec<(usize, String)>,
    row: String,
    row_start: usize,
}

impl Wrapper {
    fn new(width: usize) -> Self {
        Self {
            width: width.max(1),
            rows: Vec::new(),
            row: String::new(),
            row_start: 0,
        }
    }

    fn len(&self) -> usize {
        self.row.chars().count()
    }

    fn flush(&mut self) {
        if !self.row.is_empty() {
            let text = std::mem::take(&mut self.row);
            self.rows.push((self.row_start, text));
        }
    }

    fn push(&mut self, col: usize, text: &str) {
        for (col, c) in (col..).zip(text.chars()) {
            if self.row.is_empty() {
                self.row_start = col;
            } else if self.len() == self.width {
                self.flush();
                self.row_start = col;
            }
            self.row.push(c);
        }
    }

    fn finish(mut self) -> Vec<(usize, String)> {
        self.flush();
        self.rows
    }
}

fn wrap_line(text: &str, width: usize) -> Vec<(usize, String)> {
    let mut w = Wrapper::new(width);
    let mut gap = String::new();
    let mut gap_col = 0usize;
    for run in text_runs(text) {
        let piece = &text[run.start..run.end];
        if run.space {
            if gap.is_empty() {
                gap_col = run.col;
            }
            gap.push_str(piece);
            continue;
        }
        let pending = std::mem::take(&mut gap);
        let fits = w.row.is_empty()
            || w.len() + pending.chars().count() + piece.chars().count() <= w.width;
        if fits {
            w.push(gap_col, &pending);
        } else {
            w.flush();
        }
        w.push(run.col, piece);
    }
    w.push(gap_col, &gap);
    w.finish()
}

fn wrap_words(text: &str, width: usize) -> Vec<String> {
    wrap_line(text.trim_start(), width)
        .into_iter()
        .map(|(_, line)| line.trim_end().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

fn entry_lines(
    entry: &Entry,
    width: usize,
    expand_tools: bool,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if entry.kind == Kind::Assistant {
        return assistant_lines(&entry.text, width, theme);
    }
    let style = kind_style(theme, entry.kind);
    let prefix = kind_prefix(entry.kind);
    let indent = prefix.chars().count();
    let avail = width.saturating_sub(indent).max(1);
    let mut lines = Vec::new();
    let src_lines: Vec<&str> = if entry.text.is_empty() {
        vec![""]
    } else {
        entry.text.lines().collect()
    };
    for (i, src) in src_lines.iter().enumerate() {
        let mut wrapped = wrap_words(src, avail);
        if wrapped.is_empty() {
            wrapped.push(String::new());
        }
        for (j, w) in wrapped.iter().enumerate() {
            let text = if i == 0 && j == 0 {
                format!("{prefix}{w}")
            } else {
                format!("{}{w}", " ".repeat(indent))
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
    }
    if let Some(output) = &entry.output {
        let out_indent = "  ";
        let out_avail = width.saturating_sub(out_indent.chars().count()).max(1);
        let out_lines: Vec<&str> = output.lines().collect();
        let limit = if expand_tools {
            out_lines.len()
        } else {
            out_lines.len().min(TOOL_OUTPUT_LINES)
        };
        for src in out_lines.iter().take(limit) {
            let mut wrapped = wrap_words(src, out_avail);
            if wrapped.is_empty() {
                wrapped.push(String::new());
            }
            for w in wrapped {
                lines.push(Line::from(Span::styled(
                    format!("{out_indent}{w}"),
                    theme.output_style(),
                )));
            }
        }
        if limit < out_lines.len() {
            lines.push(Line::from(Span::styled(
                format!("  … +{} lines (Ctrl+O to expand)", out_lines.len() - limit),
                theme.output_style(),
            )));
        }
    }
    lines
}

pub struct App {
    entries: Vec<Entry>,
    pending: Option<Entry>,
    input: String,
    cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    scroll_from_bottom: usize,
    show_reasoning: bool,
    expand_tools: bool,
    pending_tool: Option<usize>,
    status: String,
    spinner_frame: usize,
    turn_start: Option<Instant>,
    completion: Option<(Vec<String>, usize)>,
    loop_status: Option<String>,
    busy: bool,
    current_tool: Option<String>,
    quit: bool,
    follow: bool,
    last_rendered_lines: usize,
    last_scroll: usize,
    pwd: PathBuf,
    model: String,
    theme: Theme,
    context_window: u32,
    known_models: Vec<String>,
    templates: Vec<Template>,
    session_list: Vec<SessionInfo>,
    est_tokens: usize,
    message_count: usize,
    session_path: Option<String>,
    session_name: Option<String>,
    selection: Option<Selection>,
    selecting: bool,
    clicks: u32,
    last_click: Option<(Instant, u16, u16)>,
    screen: Vec<Vec<char>>,
    text_area: Rect,
    clipboard: Clipboard,
    notice: Option<(String, Instant)>,
    discord: Option<Arc<Bridge>>,
    capture: Option<Capture>,
    remote_queue: VecDeque<(Origin, String)>,
    agent_touched: bool,
}

impl App {
    pub fn new(
        pwd: PathBuf,
        model: String,
        known_models: Vec<String>,
        templates: Vec<Template>,
        context_window: u32,
        theme: Theme,
        session_path: Option<String>,
    ) -> Self {
        let welcome = format!(
            "rho v{} · model {model} · context {} tokens\npwd {}\nsession {}\n:help for commands · Esc interrupts · Ctrl+R toggles reasoning · Ctrl+O expands tool output · drag to select and copy",
            env!("CARGO_PKG_VERSION"),
            fmt_tokens(context_window as usize),
            pwd.display(),
            session_path.as_deref().unwrap_or("(new session)"),
        );
        Self {
            entries: vec![Entry {
                text: welcome,
                kind: Kind::Info,
                output: None,
            }],
            pending: None,
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_index: None,
            scroll_from_bottom: 0,
            show_reasoning: true,
            expand_tools: false,
            pending_tool: None,
            status: "idle".to_string(),
            spinner_frame: 0,
            turn_start: None,
            completion: None,
            loop_status: None,
            busy: false,
            current_tool: None,
            quit: false,
            follow: true,
            last_rendered_lines: 0,
            last_scroll: 0,
            pwd,
            model,
            theme,
            context_window,
            known_models,
            templates,
            session_list: Vec::new(),
            est_tokens: 0,
            message_count: 0,
            session_path: None,
            session_name: None,
            selection: None,
            selecting: false,
            clicks: 0,
            last_click: None,
            screen: Vec::new(),
            text_area: Rect::ZERO,
            clipboard: Clipboard::new(ClipboardMode::default()),
            notice: None,
            discord: None,
            capture: None,
            remote_queue: VecDeque::new(),
            agent_touched: false,
        }
    }

    fn template_names(&self) -> Vec<String> {
        self.templates.iter().map(|t| t.name.clone()).collect()
    }

    fn command_names(&self) -> Vec<String> {
        let mut names: Vec<String> = BUILTIN_COMMANDS.iter().map(|n| n.to_string()).collect();
        names.extend(self.template_names());
        names.sort();
        names.dedup();
        names
    }

    fn complete_input(&mut self, back: bool) {
        if let Some((m, i)) = self.completion.clone() {
            if m.len() > 1 && self.input == m[i] {
                let idx = if back {
                    (i + m.len() - 1) % m.len()
                } else {
                    (i + 1) % m.len()
                };
                self.input = m[idx].clone();
                self.cursor = self.input.chars().count();
                self.completion = Some((m, idx));
                return;
            }
        }
        let nchars = self.input.chars().count();
        if !self.input.starts_with(':') || self.cursor != nchars {
            return;
        }
        if let Some(rest) = self.input.strip_prefix(":pwd ").map(|s| s.to_string()) {
            let token = rest.trim_start().to_string();
            if !token.contains(char::is_whitespace) {
                self.complete_path(&token);
            }
            return;
        }
        let prefix: String = self.input.chars().skip(1).collect();
        if prefix.contains(char::is_whitespace) {
            return;
        }
        let matches: Vec<String> = self
            .command_names()
            .into_iter()
            .filter(|n| n.starts_with(&prefix))
            .collect();
        if matches.is_empty() {
            self.completion = None;
            return;
        }
        if matches.len() == 1 {
            self.input = format!(":{} ", matches[0]);
            self.completion = None;
        } else {
            let cands: Vec<String> = matches.iter().map(|n| format!(":{n}")).collect();
            self.input = cands[0].clone();
            self.completion = Some((cands, 0));
        }
        self.cursor = self.input.chars().count();
    }

    fn complete_path(&mut self, token: &str) {
        let token = expand_home(token);
        let (parent, base) = match token.rfind('/') {
            Some(i) => (&token[..=i], &token[i + 1..]),
            None => ("", token.as_str()),
        };
        let dir = self.pwd.join(parent);
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        name.starts_with(base)
                            && (base.starts_with('.') || !name.starts_with('.'))
                            && e.path().is_dir()
                    })
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        if names.is_empty() {
            self.completion = None;
            return;
        }
        names.sort();
        let cands: Vec<String> = names.iter().map(|n| format!(":pwd {parent}{n}/")).collect();
        self.input = cands[0].clone();
        self.completion = if cands.len() == 1 {
            None
        } else {
            Some((cands, 0))
        };
        self.cursor = self.input.chars().count();
    }

    fn push(&mut self, text: impl Into<String>, kind: Kind) {
        self.flush_pending();
        self.entries.push(Entry {
            text: text.into(),
            kind,
            output: None,
        });
        if self.follow {
            self.scroll_from_bottom = 0;
        }
    }

    fn flush_pending(&mut self) {
        if let Some(entry) = self.pending.take() {
            if !entry.text.trim().is_empty() {
                self.entries.push(entry);
            }
        }
    }

    fn append_pending(&mut self, text: &str, kind: Kind) {
        match &mut self.pending {
            Some(entry) if entry.kind == kind => entry.text.push_str(text),
            _ => {
                self.flush_pending();
                self.pending = Some(Entry {
                    text: text.to_string(),
                    kind,
                    output: None,
                });
            }
        }
        if self.follow {
            self.scroll_from_bottom = 0;
        }
    }

    pub fn on_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::ReasoningDelta(t) => {
                self.busy = true;
                self.append_pending(&t, Kind::Reasoning);
            }
            AgentEvent::TextDelta(t) => {
                self.busy = true;
                self.append_pending(&t, Kind::Assistant);
            }
            AgentEvent::ToolStarted { name, args } => {
                self.current_tool = Some(name.clone());
                self.flush_pending();
                self.entries.push(Entry {
                    text: format!("{name} {} …", summarize_args(&args)),
                    kind: Kind::Tool,
                    output: None,
                });
                self.pending_tool = Some(self.entries.len() - 1);
                if self.follow {
                    self.scroll_from_bottom = 0;
                }
            }
            AgentEvent::ToolFinished {
                name,
                is_error,
                output,
            } => {
                self.current_tool = None;
                let mark = if is_error { "✗" } else { "✓" };
                let entry = self
                    .pending_tool
                    .and_then(|i| self.entries.get_mut(i))
                    .filter(|e| e.kind == Kind::Tool && e.output.is_none());
                match entry {
                    Some(entry) => {
                        entry.text = format!("{} {mark}", entry.text.trim_end_matches(" …"));
                        entry.output = Some(output.trim_end().to_string());
                    }
                    None => self.push(format!("{name} {mark}"), Kind::Tool),
                }
                self.pending_tool = None;
            }
            AgentEvent::AssistantDone { text, .. } => {
                let streamed = self.pending.take();
                match streamed {
                    Some(p) if !p.text.trim().is_empty() => {
                        self.entries.push(p);
                        self.scroll_from_bottom = 0;
                    }
                    _ => {
                        if !text.trim().is_empty() {
                            self.push(text, Kind::Assistant);
                        }
                    }
                }
                self.busy = false;
                self.status = "idle".to_string();
            }
            AgentEvent::LoopStarted { description } => {
                self.loop_status = Some(format!("loop [{description}] iter 0"));
                self.push(format!("loop started [{description}]"), Kind::Info);
            }
            AgentEvent::LoopIterationDone { iterations } => {
                if let Some(status) = &mut self.loop_status {
                    *status = format!(
                        "{} iter {iterations}",
                        status.split(" iter ").next().unwrap_or("loop")
                    );
                }
                self.post_capture(true);
            }
            AgentEvent::LoopFinished { iterations, reason } => {
                self.loop_status = None;
                self.push(
                    format!("loop finished: {reason} after {iterations} iteration(s)"),
                    Kind::Info,
                );
            }
            AgentEvent::Idle { .. } => {
                self.busy = false;
                self.status = "idle".to_string();
                self.current_tool = None;
            }
            AgentEvent::Info(message) => {
                self.push(message, Kind::Info);
            }
            AgentEvent::Fault(message) => {
                self.push(message, Kind::Error);
            }
            AgentEvent::ContextUsage {
                est_tokens,
                message_count,
                session_path,
                session_name,
                pwd,
            } => {
                self.est_tokens = est_tokens;
                self.message_count = message_count;
                self.session_path = session_path;
                self.session_name = session_name;
                self.pwd = PathBuf::from(pwd);
            }
            AgentEvent::Error { message, .. } => {
                self.push(message, Kind::Error);
            }
        }
    }

    pub fn handle_key(
        &mut self,
        key: event::KeyEvent,
        agent: &mpsc::UnboundedSender<AgentCommand>,
        cancel: &Arc<AtomicBool>,
    ) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        if !matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.completion = None;
        }
        match key.code {
            KeyCode::Tab => {
                self.complete_input(false);
                false
            }
            KeyCode::BackTab => {
                self.complete_input(true);
                false
            }
            KeyCode::Esc => {
                self.clear_selection();
                cancel.store(true, Ordering::SeqCst);
                self.push("interrupt requested", Kind::Info);
                false
            }
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.expand_tools = !self.expand_tools;
                self.clear_selection();
                false
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.show_reasoning = !self.show_reasoning;
                self.clear_selection();
                self.push(
                    if self.show_reasoning {
                        "reasoning shown"
                    } else {
                        "reasoning hidden"
                    },
                    Kind::Info,
                );
                false
            }
            KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.copy_selection();
                false
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => true,
            KeyCode::PageUp => {
                self.scroll_by(10);
                false
            }
            KeyCode::PageDown => {
                self.scroll_by(-10);
                false
            }
            KeyCode::End => {
                self.follow = true;
                self.scroll_from_bottom = 0;
                false
            }
            KeyCode::Up if self.input.is_empty() => {
                if !self.history.is_empty() {
                    let idx = match self.history_index {
                        Some(i) => i.saturating_sub(1),
                        None => self.history.len() - 1,
                    };
                    self.history_index = Some(idx);
                    self.input = self.history[idx].clone();
                    self.cursor = self.input.len();
                }
                false
            }
            KeyCode::Down if self.history_index.is_some() => {
                match self.history_index {
                    Some(i) if i + 1 < self.history.len() => {
                        self.history_index = Some(i + 1);
                        self.input = self.history[i + 1].clone();
                    }
                    _ => {
                        self.history_index = None;
                        self.input.clear();
                    }
                }
                self.cursor = self.input.len();
                false
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert_text("\n");
                false
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_text("\n");
                false
            }
            KeyCode::Enter => {
                let input = std::mem::take(&mut self.input);
                self.cursor = 0;
                self.history_index = None;
                self.on_submit(&input, agent)
            }
            KeyCode::Backspace => {
                let mut chars: Vec<char> = self.input.chars().collect();
                if self.cursor > 0 {
                    chars.remove(self.cursor - 1);
                    self.cursor -= 1;
                    self.input = chars.into_iter().collect();
                }
                false
            }
            KeyCode::Delete => {
                let mut chars: Vec<char> = self.input.chars().collect();
                if self.cursor < chars.len() {
                    chars.remove(self.cursor);
                    self.input = chars.into_iter().collect();
                }
                false
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                false
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.input.len());
                false
            }
            KeyCode::Home => {
                self.cursor = 0;
                false
            }
            KeyCode::Char(c) => {
                if c == '\n' {
                    self.insert_text("\n");
                } else {
                    self.input.insert(self.cursor, c);
                    self.cursor += 1;
                }
                false
            }
            _ => false,
        }
    }

    pub fn insert_text(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.input.insert_str(self.cursor, &text);
        self.cursor += text.chars().count();
    }

    fn wrapped_input(&self, width: usize) -> Vec<InputRow> {
        let avail = width.saturating_sub(INPUT_INDENT).max(1);
        let mut flat: Vec<InputRow> = Vec::new();
        for (line, text) in self.input.split('\n').enumerate() {
            let mut wrapped = wrap_line(text, avail);
            if wrapped.is_empty() {
                wrapped.push((0, String::new()));
            }
            flat.extend(
                wrapped
                    .into_iter()
                    .enumerate()
                    .map(|(sub, (start, text))| InputRow {
                        line,
                        sub,
                        start,
                        text,
                    }),
            );
        }
        flat
    }

    fn input_height(&self, width: usize) -> u16 {
        (self.wrapped_input(width).len() as u16 + 2).clamp(3, 8)
    }

    fn input_layout(&self, width: usize, rows: usize) -> InputLayout {
        let rows = rows.max(1);
        let before: String = self.input.chars().take(self.cursor).collect();
        let cur_line = before.matches('\n').count();
        let cur_col = before.chars().rev().take_while(|c| *c != '\n').count();

        let flat = self.wrapped_input(width);
        let cursor_row = flat
            .iter()
            .rposition(|row| row.line == cur_line && row.start <= cur_col)
            .or_else(|| flat.iter().rposition(|row| row.line < cur_line))
            .unwrap_or(0);
        let top = cursor_row.saturating_sub(rows - 1);
        let bottom = (top + rows).min(flat.len());
        let lines = (top..bottom)
            .map(|i| {
                let row = &flat[i];
                let prompt = if row.line == 0 && row.sub == 0 {
                    "❯ "
                } else {
                    "  "
                };
                Line::from(Span::raw(format!("{prompt}{}", row.text)))
            })
            .collect();
        let cursor = &flat[cursor_row];
        InputLayout {
            lines,
            cursor_row: cursor_row - top,
            cursor_col: (cur_col - cursor.start).min(cursor.text.chars().count()),
        }
    }

    pub fn scroll_by(&mut self, lines: i32) {
        if lines > 0 {
            self.follow = false;
            self.scroll_from_bottom += lines as usize;
        } else if lines < 0 {
            self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub((-lines) as usize);
            self.follow = self.scroll_from_bottom == 0;
        }
    }

    /// Keep a selection on the text it was made over while the view moves.
    fn follow_selection(&mut self, delta: i32) {
        if delta == 0 || self.selection.is_none() {
            return;
        }
        let (_, _, top, bottom) = self.select_bounds();
        if let Some(selection) = &mut self.selection {
            selection.shift_rows(delta, top, bottom);
        }
    }

    fn clear_selection(&mut self) {
        self.selection = None;
        self.selecting = false;
    }

    /// First/last selectable column and top/bottom row of the transcript.
    fn select_bounds(&self) -> (u16, u16, u16, u16) {
        let area = self.text_area;
        (
            area.left(),
            area.right().saturating_sub(1),
            area.top(),
            area.bottom().saturating_sub(1),
        )
    }

    pub fn on_mouse(&mut self, mouse: event::MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_by(3),
            MouseEventKind::ScrollDown => self.scroll_by(-3),
            MouseEventKind::Down(_) => self.press(mouse.column, mouse.row),
            MouseEventKind::Drag(_) => self.drag(mouse.column, mouse.row),
            MouseEventKind::Up(_) => self.release(mouse.column, mouse.row),
            _ => {}
        }
    }

    fn inside_text(&self, col: u16, row: u16) -> bool {
        let (first_col, last_col, top, bottom) = self.select_bounds();
        (first_col..=last_col).contains(&col) && (top..=bottom).contains(&row)
    }

    fn press(&mut self, col: u16, row: u16) {
        if self.screen.is_empty() || !self.inside_text(col, row) {
            return;
        }
        let (first_col, last_col, _, _) = self.select_bounds();
        let clicks = self.count_clicks(col, row);
        let anchored = Selection::point(Anchor::new(col, row), first_col, last_col);
        let range = match clicks {
            1 => None,
            2 => selection::word_at(&self.screen, row as usize, col as usize),
            _ => selection::line_at(
                &self.screen,
                row as usize,
                first_col as usize,
                last_col as usize,
            ),
        };
        self.selection = Some(match range {
            Some((lo, hi)) => Selection::new(
                Anchor::new(lo as u16, row),
                Anchor::new(hi as u16, row),
                first_col,
                last_col,
            ),
            None => anchored,
        });
        self.selecting = true;
    }

    fn drag(&mut self, col: u16, row: u16) {
        if !self.selecting || self.selection.is_none() {
            return;
        }
        let (first_col, last_col, top, bottom) = self.select_bounds();
        if last_col < first_col || bottom < top {
            return;
        }
        let end = Anchor::new(col.clamp(first_col, last_col), row.clamp(top, bottom));
        if let Some(selection) = &mut self.selection {
            selection.end = end;
        }
    }

    fn release(&mut self, col: u16, row: u16) {
        if !self.selecting {
            return;
        }
        self.selecting = false;
        self.drag(col, row);
        let Some(selection) = self.selection else {
            return;
        };
        if self.clicks == 1 && selection.is_point() {
            self.selection = None;
            return;
        }
        self.copy_selection();
    }

    fn count_clicks(&mut self, col: u16, row: u16) -> u32 {
        let repeated = self.last_click.is_some_and(|(at, c, r)| {
            at.elapsed() < CLICK_CHAIN && r == row && (c as i32 - col as i32).abs() <= 1
        });
        self.clicks = if repeated { self.clicks + 1 } else { 1 };
        self.last_click = Some((Instant::now(), col, row));
        self.clicks
    }

    pub fn copy_selection(&mut self) {
        let Some(selection) = self.selection else {
            self.set_notice("nothing selected — drag to select");
            return;
        };
        let text = selection::extract(&self.screen, &selection);
        if text.is_empty() {
            self.selection = None;
            self.set_notice("nothing selected — drag to select");
            return;
        };
        match self.clipboard.put(&text) {
            Ok(placement) => self.set_notice(format!(
                "copied {} characters to {}",
                text.chars().count(),
                placement.label()
            )),
            Err(e) => self.set_notice(format!("copy failed: {e}")),
        }
    }

    fn set_notice(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now()));
    }

    fn notice_text(&self) -> Option<&str> {
        let (text, at) = self.notice.as_ref()?;
        (at.elapsed() < NOTICE_TTL).then_some(text.as_str())
    }

    fn paint_selection(&self, frame: &mut Frame) {
        let Some(selection) = &self.selection else {
            return;
        };
        let buffer = frame.buffer_mut();
        let area = buffer.area;
        for row in area.top()..area.bottom() {
            let Some((lo, hi)) = selection.row_range(row) else {
                continue;
            };
            let lo = lo.max(area.left());
            let hi = hi.min(area.right().saturating_sub(1));
            for col in lo..=hi {
                buffer[(col, row)].set_style(SELECTION_STYLE);
            }
        }
    }

    fn update_status(&mut self) {
        if !self.busy {
            self.turn_start = None;
            self.status = "idle".to_string();
            return;
        }
        if self.turn_start.is_none() {
            self.turn_start = Some(Instant::now());
        }
        if let Some(tool) = &self.current_tool {
            self.status = format!("running {tool}");
        } else if self.status == "idle" {
            self.status = "thinking".to_string();
        }
    }

    fn status_line(&self) -> String {
        if !self.busy {
            return "idle".to_string();
        }
        let frame = SPINNER[self.spinner_frame % SPINNER.len()];
        let elapsed = self.turn_start.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        format!("{frame} {} {elapsed}s", self.status)
    }

    fn context_usage(&self) -> (String, Color) {
        let pct = if self.context_window > 0 {
            self.est_tokens as f64 / self.context_window as f64 * 100.0
        } else {
            0.0
        };
        let usage = format!(
            "~{}/{} ({:.1}%)",
            fmt_tokens(self.est_tokens),
            fmt_tokens(self.context_window as usize),
            pct
        );
        let color = if pct >= 90.0 {
            Color::Red
        } else if pct >= 70.0 {
            Color::Yellow
        } else {
            Color::Green
        };
        (usage, color)
    }

    fn footer_title(&self) -> Line<'static> {
        let pwd_name = self
            .pwd
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.pwd.display().to_string());
        let ident = match &self.session_name {
            Some(name) if !name.is_empty() => format!("{name} · {pwd_name}"),
            _ => pwd_name,
        };
        let (usage, usage_color) = self.context_usage();
        Line::from(vec![
            Span::styled(format!(" {ident} · "), Style::default().fg(Color::DarkGray)),
            Span::raw(self.model.clone()),
            Span::raw(format!(" · {} · ", self.status_line())),
            Span::styled(usage, Style::default().fg(usage_color)),
            Span::raw(" "),
        ])
    }

    /// Hands a command to the agent, saying so in the transcript if the agent
    /// is gone: a dropped prompt would otherwise look like a request that was
    /// simply never answered.
    fn send_to_agent(
        &mut self,
        agent: &mpsc::UnboundedSender<AgentCommand>,
        command: AgentCommand,
    ) {
        if let Err(e) = agent.send(command) {
            self.push(
                format!("the agent is not running, so the request was dropped ({e})"),
                Kind::Error,
            );
            return;
        }
        self.agent_touched = true;
    }

    /// Empties the transcript for a session change, keeping a mirrored Discord
    /// command pointed at whatever lines come next.
    fn clear_transcript(&mut self) {
        self.entries.clear();
        self.pending = None;
        self.clear_selection();
        if let Some(capture) = &mut self.capture {
            capture.base = 0;
        }
    }

    fn submit_prompt(
        &mut self,
        display: &str,
        text: String,
        agent: &mpsc::UnboundedSender<AgentCommand>,
    ) {
        self.history.push(display.to_string());
        self.follow = true;
        self.scroll_from_bottom = 0;
        self.push(display.to_string(), Kind::User);
        self.busy = true;
        self.status = "thinking".to_string();
        self.send_to_agent(
            agent,
            AgentCommand::Prompt {
                text,
                origin: Origin::Local,
            },
        );
    }

    fn show_help(&mut self) {
        self.push(
            "commands: :loop [forever|N|10m|until \"cond\"] <task>, :compact [instructions], :new, :resume [n], :session, :discord, :name <text>, :model [id], :pwd [path], :stop, :help, :quit",
            Kind::Info,
        );
        self.push("!cmd runs a shell command and shares its output with the model; !!cmd runs it without sharing", Kind::Info);
        self.push("drag selects transcript text and copies it to the clipboard; double-click selects a word, triple-click a line, Ctrl+Y copies the selection again, Esc clears it", Kind::Info);
        let help = templates::render_help(&self.templates);
        if !help.is_empty() {
            self.push("prompt templates:", Kind::Info);
            for line in help.lines() {
                self.push(line, Kind::Info);
            }
        }
    }

    fn show_sessions(&mut self) {
        self.session_list = session::list_sessions(&self.pwd, 10);
        if self.session_list.is_empty() {
            self.push("no saved sessions for this directory", Kind::Info);
            return;
        }
        self.push("recent sessions (use :resume <n> to open):", Kind::Info);
        let list = self.session_list.clone();
        for (i, s) in list.iter().enumerate() {
            let name = s.header.name.as_deref().unwrap_or("(unnamed)");
            self.push(
                format!(
                    "{}. [{name}] {} — {} msgs — {}",
                    i + 1,
                    s.header.created,
                    s.message_count,
                    s.first_prompt
                ),
                Kind::Info,
            );
        }
    }

    fn on_submit(&mut self, input: &str, agent: &mpsc::UnboundedSender<AgentCommand>) -> bool {
        match classify_input(input, &self.template_names()) {
            Some(InputAction::Submit(text)) => {
                self.submit_prompt(&text.clone(), text, agent);
            }
            Some(InputAction::Command(Command::Quit)) => return true,
            Some(InputAction::Command(Command::Stop)) => {
                self.send_to_agent(agent, AgentCommand::Interrupt);
                self.push("interrupt requested", Kind::Info);
            }
            Some(InputAction::Command(Command::Loop(args))) => match parse_loop_spec(&args) {
                Ok(spec) => {
                    self.history.push(format!(":loop {args}"));
                    self.busy = true;
                    self.status = "looping".to_string();
                    self.send_to_agent(agent, AgentCommand::Loop { spec });
                }
                Err(e) => self.push(e, Kind::Error),
            },
            Some(InputAction::Command(Command::Help)) => self.show_help(),
            Some(InputAction::Command(Command::Compact(instructions))) => {
                self.history.push(match &instructions {
                    Some(i) => format!(":compact {i}"),
                    None => ":compact".to_string(),
                });
                self.busy = true;
                self.status = "compacting".to_string();
                self.send_to_agent(agent, AgentCommand::Compact { instructions });
            }
            Some(InputAction::Command(Command::New)) => {
                self.send_to_agent(agent, AgentCommand::NewSession);
                self.clear_transcript();
                self.push("new session started", Kind::Info);
            }
            Some(InputAction::Command(Command::Pwd(path))) => {
                if path.is_empty() {
                    self.push(format!("pwd: {}", self.pwd.display()), Kind::Info);
                } else {
                    self.history.push(format!(":pwd {path}"));
                    self.send_to_agent(agent, AgentCommand::SetPwd { path: PathBuf::from(path) });
                }
            }
            Some(InputAction::Command(Command::Resume(args))) => {
                if args.is_empty() {
                    self.show_sessions();
                } else {
                    match args.parse::<usize>() {
                        Ok(n) if n >= 1 && n <= self.session_list.len() => {
                            let path = self.session_list[n - 1].path.clone();
                            self.send_to_agent(agent, AgentCommand::Resume { path });
                            self.clear_transcript();
                            self.push(format!("resumed session {}", n), Kind::Info);
                        }
                        _ => self.push("usage: :resume to list, :resume <n> to open", Kind::Error),
                    }
                }
            }
            Some(InputAction::Command(Command::Session)) => {
                let name = self.session_name.as_deref().unwrap_or("(unnamed)");
                let path = self.session_path.as_deref().unwrap_or("(not saved)");
                self.push(
                    format!(
                        "session {name} — {} messages, ~{} estimated tokens\nfile: {path}\nmodel: {}",
                        self.message_count, self.est_tokens, self.model
                    ),
                    Kind::Info,
                );
            }
            Some(InputAction::Command(Command::Discord)) => match &self.discord {
                Some(bridge) => {
                    for line in bridge.report() {
                        let kind = if line.fault { Kind::Error } else { Kind::Info };
                        self.push(line.text, kind);
                    }
                }
                None => self.push(
                    "discord bridge is off; set discord.enabled and discord.bot_token in config.yaml",
                    Kind::Info,
                ),
            },
            Some(InputAction::Command(Command::Name(name))) => {
                self.send_to_agent(agent, AgentCommand::SetName { name: name.clone() });
                self.session_name = if name.is_empty() { None } else { Some(name) };
            }
            Some(InputAction::Command(Command::Model(arg))) => {
                if arg.is_empty() {
                    let current = self.model.clone();
                    self.push(
                        format!(
                            "models: {} (current: {current})",
                            self.known_models.join(", ")
                        ),
                        Kind::Info,
                    );
                } else if self.known_models.iter().any(|m| m == &arg) {
                    self.model = arg.clone();
                    self.send_to_agent(agent, AgentCommand::SetModel { id: arg });
                } else {
                    self.push(
                        format!(
                            "unknown model: {arg} (available: {})",
                            self.known_models.join(", ")
                        ),
                        Kind::Error,
                    );
                }
            }
            Some(InputAction::Command(Command::Template { name, args })) => {
                let Some(template) = self.templates.iter().find(|t| t.name == name).cloned() else {
                    self.push(format!("unknown template: :{name}"), Kind::Error);
                    return false;
                };
                let parsed = templates::tokenize_args(&args);
                let expanded = templates::expand_template(&template.body, &parsed);
                let display = if args.is_empty() {
                    format!(":{name}")
                } else {
                    format!(":{name} {args}")
                };
                self.submit_prompt(&display, expanded, agent);
            }
            Some(InputAction::Command(Command::Shell { command, to_model })) => {
                self.history.push(command.clone());
                let prefix = if to_model { "!" } else { "!!" };
                self.push(format!("{prefix} {command}"), Kind::User);
                self.send_to_agent(agent, AgentCommand::Shell { command, to_model });
            }
            Some(InputAction::Command(Command::Unknown(name))) => {
                self.push(format!("unknown command: :{name}"), Kind::Error);
            }
            None => {}
        }
        false
    }

    /// Runs a command that arrived from Discord. Commands are the window's own
    /// business, so this is the same path a keystroke takes; what it adds to the
    /// transcript is posted back to the channel that asked.
    pub fn on_remote(
        &mut self,
        command: RemoteCommand,
        agent: &mpsc::UnboundedSender<AgentCommand>,
    ) {
        if self.busy || self.capture.is_some() || !self.remote_queue.is_empty() {
            self.remote_queue.push_back((command.origin, command.text));
            return;
        }
        self.run_remote(&command.origin, &command.text, agent);
    }

    /// The agent went idle: whatever the mirrored command was still waiting for
    /// has landed, and the commands that queued behind it can run now that their
    /// output cannot be mixed up with someone else's turn.
    pub fn remote_idle(&mut self, agent: &mpsc::UnboundedSender<AgentCommand>) {
        if self.capture.is_some() {
            self.post_capture(false);
        }
        while let Some((origin, text)) = self.remote_queue.pop_front() {
            self.run_remote(&origin, &text, agent);
            if self.capture.is_some() {
                break;
            }
        }
    }

    fn run_remote(
        &mut self,
        origin: &Origin,
        text: &str,
        agent: &mpsc::UnboundedSender<AgentCommand>,
    ) {
        let action = classify_input(text, &self.template_names());
        self.agent_touched = false;
        self.capture = Some(Capture {
            origin: origin.clone(),
            request: text.to_string(),
            base: self.entries.len(),
        });
        if matches!(action, Some(InputAction::Command(Command::Quit))) {
            self.push(
                "the keyboard is the only place :quit works, so the window stays open",
                Kind::Error,
            );
        } else {
            self.on_submit(text, agent);
        }
        if !(owes_agent_output(&action) && self.agent_touched) {
            self.post_capture(false);
        }
    }

    /// Posts the transcript lines a mirrored command has produced. `keep_open`
    /// leaves the mirror running, which is how a loop reports each iteration as
    /// it lands instead of saving everything up for an end that may never come.
    fn post_capture(&mut self, keep_open: bool) {
        self.flush_pending();
        let Some(capture) = self.capture.as_mut() else {
            return;
        };
        let mut lines: Vec<String> = Vec::new();
        for entry in self
            .entries
            .iter()
            .skip(capture.base.min(self.entries.len()))
        {
            if entry.kind == Kind::Reasoning {
                continue;
            }
            let body = match &entry.output {
                Some(output) => format!("{}\n{}", entry.text.trim_end(), output.trim_end()),
                None => entry.text.trim_end().to_string(),
            };
            if !body.trim().is_empty() {
                lines.push(body);
            }
        }
        capture.base = self.entries.len();
        let body = if lines.is_empty() {
            if keep_open {
                return;
            }
            format!("{} produced no output", capture.request)
        } else {
            lines.join("\n\n")
        };
        if let Some(bridge) = self.discord.clone() {
            bridge.post_reply(&capture.origin, &body);
        }
        if !keep_open {
            self.capture = None;
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.update_status();
        let inner_width = area.width.saturating_sub(2) as usize;
        let input_height = self
            .input_height(inner_width)
            .min(area.height.saturating_sub(4).max(3));
        let chunks = if self.loop_status.is_some() {
            let split = Layout::vertical([
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(input_height),
            ])
            .split(area);
            (Some(split[1]), split[0], split[2])
        } else {
            let split = Layout::vertical([Constraint::Min(2), Constraint::Length(input_height)])
                .split(area);
            (None, split[0], split[1])
        };

        let text_chunks =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).split(chunks.1);
        let text_area = text_chunks[0];
        let scrollbar_area = text_chunks[1];
        self.text_area = text_area;
        let width = text_area.width as usize;
        let mut lines: Vec<Line> = Vec::new();
        let mut prev_kind: Option<Kind> = None;
        let mut append = |entry: &Entry, lines: &mut Vec<Line>| {
            if !self.show_reasoning && entry.kind == Kind::Reasoning {
                return;
            }
            if prev_kind.is_some_and(|k| k != entry.kind || entry.kind == Kind::Tool) {
                lines.push(Line::from(""));
            }
            prev_kind = Some(entry.kind);
            lines.extend(entry_lines(entry, width, self.expand_tools, &self.theme));
        };
        for entry in &self.entries {
            append(entry, &mut lines);
        }
        if let Some(pending) = &self.pending {
            append(pending, &mut lines);
        }
        let new_lines = lines.len().saturating_sub(self.last_rendered_lines);
        let visible = text_area.height as usize;
        if !self.follow && new_lines > 0 {
            self.scroll_from_bottom += new_lines;
        }
        self.last_rendered_lines = lines.len();
        let scroll = lines
            .len()
            .saturating_sub(visible + self.scroll_from_bottom);
        // Rows the text moved down by, so an open selection stays on its text.
        self.follow_selection(self.last_scroll as i32 - scroll as i32);
        self.last_scroll = scroll;
        let paragraph = Paragraph::new(lines).scroll((scroll as u16, 0));
        frame.render_widget(paragraph, text_area);
        if visible > 0 && self.last_rendered_lines > visible {
            let mut scrollbar_state = ScrollbarState::new(self.last_rendered_lines)
                .position(scroll)
                .viewport_content_length(visible);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None),
                scrollbar_area,
                &mut scrollbar_state,
            );
        }

        if let Some(bar) = chunks.0 {
            let status = self.loop_status.clone().unwrap_or_default();
            frame.render_widget(
                Paragraph::new(Span::styled(status, Style::default().fg(Color::Green))),
                bar,
            );
        }

        let queued = self.busy && !self.input.is_empty();
        let mut title = self.footer_title();
        if queued {
            title.push_span(Span::styled(
                "· ⏎ queues",
                Style::default().fg(Color::Yellow),
            ));
        }
        if let Some(notice) = self.notice_text() {
            title.push_span(Span::styled(
                format!("· {notice} "),
                Style::default().fg(Color::Yellow),
            ));
        }
        let border_style = if self.busy {
            Style::default().fg(self.theme.busy_border_color())
        } else {
            Style::default()
        };
        let input_block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(border_style);
        let inner = input_block.inner(chunks.2);
        frame.render_widget(input_block, chunks.2);
        let prompt = self.input_layout(inner.width as usize, inner.height as usize);
        frame.render_widget(Paragraph::new(prompt.lines), inner);
        frame.set_cursor_position((
            inner
                .x
                .saturating_add(INPUT_INDENT as u16 + prompt.cursor_col as u16),
            inner.y.saturating_add(prompt.cursor_row as u16),
        ));
        self.paint_selection(frame);
        self.screen = selection::screen_from_buffer(frame.buffer_mut());
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }
}

pub async fn run(
    agent: mpsc::UnboundedSender<AgentCommand>,
    cancel: Arc<AtomicBool>,
    mut events: broadcast::Receiver<AgentEvent>,
    opts: TuiOptions,
) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    let result = run_inner(&mut terminal, agent, cancel, &mut events, opts).await;
    let _ = crossterm::execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste
    );
    ratatui::restore();
    result
}

pub struct TuiOptions {
    pub pwd: PathBuf,
    pub model: String,
    pub known_models: Vec<String>,
    pub templates: Vec<Template>,
    pub context_window: u32,
    pub theme: ThemeName,
    pub session_path: Option<String>,
    pub clipboard: ClipboardMode,
    pub discord: Option<Arc<Bridge>>,
    pub remote: Option<mpsc::UnboundedReceiver<RemoteCommand>>,
}

async fn run_inner(
    terminal: &mut ratatui::DefaultTerminal,
    agent: mpsc::UnboundedSender<AgentCommand>,
    cancel: Arc<AtomicBool>,
    events: &mut broadcast::Receiver<AgentEvent>,
    opts: TuiOptions,
) -> anyhow::Result<()> {
    let mut app = App::new(
        opts.pwd,
        opts.model,
        opts.known_models,
        opts.templates,
        opts.context_window,
        Theme::new(opts.theme),
        opts.session_path,
    );
    app.clipboard = Clipboard::new(opts.clipboard);
    app.discord = opts.discord;
    let mut remote = opts.remote;
    let mut keys = crossterm::event::EventStream::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(80));
    loop {
        terminal.draw(|frame| app.draw(frame, frame.area()))?;
        tokio::select! {
            maybe_event = events.recv() => {
                match maybe_event {
                    Ok(event) => {
                        let idle = matches!(event, AgentEvent::Idle { .. });
                        app.on_event(event);
                        if idle {
                            app.remote_idle(&agent);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            maybe_command = async {
                match remote.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending::<Option<RemoteCommand>>().await,
                }
            } => {
                if let Some(command) = maybe_command {
                    app.on_remote(command, &agent);
                }
            }
            maybe_key = keys.next() => {
                match maybe_key {
                    Some(Ok(Event::Key(key))) => {
                        if app.handle_key(key, &agent, &cancel) {
                            break;
                        }
                    }
                    Some(Ok(Event::Mouse(mouse))) => app.on_mouse(mouse),
                    Some(Ok(Event::Paste(text))) => app.insert_text(&text),
                    _ => {}
                }
            }
            _ = ticker.tick() => {
                app.spinner_frame += 1;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        App::new(
            PathBuf::from("/tmp"),
            "test-model".into(),
            vec!["test-model".into()],
            vec![],
            128_000,
            Theme::new(ThemeName::Dark),
            None,
        )
    }

    #[test]
    fn classify_inputs() {
        let tpl = vec!["review".to_string()];
        assert_eq!(
            classify_input("hello", &tpl),
            Some(InputAction::Submit("hello".into()))
        );
        assert_eq!(classify_input("  ", &tpl), None);
        assert_eq!(
            classify_input(":quit", &tpl),
            Some(InputAction::Command(Command::Quit))
        );
        assert_eq!(
            classify_input(":exit", &tpl),
            Some(InputAction::Command(Command::Quit))
        );
        assert_eq!(
            classify_input(":loop 5 build", &tpl),
            Some(InputAction::Command(Command::Loop("5 build".into())))
        );
        assert_eq!(
            classify_input(":wat", &tpl),
            Some(InputAction::Command(Command::Unknown("wat".into())))
        );
        assert_eq!(
            classify_input("/quit", &tpl),
            Some(InputAction::Submit("/quit".into()))
        );
    }

    #[test]
    fn classify_templates_and_builtins() {
        let tpl = vec!["review".to_string()];
        assert_eq!(
            classify_input(":review concurrency", &tpl),
            Some(InputAction::Command(Command::Template {
                name: "review".into(),
                args: "concurrency".into()
            }))
        );
        assert_eq!(
            classify_input(":quit", &tpl),
            Some(InputAction::Command(Command::Quit))
        );
        assert_eq!(
            classify_input(":compact focus on tests", &tpl),
            Some(InputAction::Command(Command::Compact(Some(
                "focus on tests".into()
            ))))
        );
        assert_eq!(
            classify_input(":compact", &tpl),
            Some(InputAction::Command(Command::Compact(None)))
        );
        assert_eq!(
            classify_input(":resume 2", &tpl),
            Some(InputAction::Command(Command::Resume("2".into())))
        );
    }

    #[test]
    fn classify_shell_escapes() {
        let tpl: Vec<String> = vec![];
        assert_eq!(
            classify_input("!git status", &tpl),
            Some(InputAction::Command(Command::Shell {
                command: "git status".into(),
                to_model: true
            }))
        );
        assert_eq!(
            classify_input("!!git status", &tpl),
            Some(InputAction::Command(Command::Shell {
                command: "git status".into(),
                to_model: false
            }))
        );
        assert_eq!(
            classify_input("!!", &tpl),
            Some(InputAction::Command(Command::Unknown("!!".into())))
        );
    }

    #[test]
    fn template_command_expands_before_submit() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "m".into(),
            vec!["m".into()],
            vec![Template {
                name: "greet".into(),
                description: "greet someone".into(),
                argument_hint: None,
                body: "hello $1".into(),
            }],
            128_000,
            Theme::new(ThemeName::Dark),
            None,
        );
        app.on_submit(":greet world", &tx);
        match rx.try_recv().unwrap() {
            AgentCommand::Prompt { text, .. } => assert_eq!(text, "hello world"),
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(app.entries.iter().any(|e| e.text == ":greet world"));
    }

    #[test]
    fn shell_escape_sends_command_without_model_turn() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = test_app();
        app.on_submit("!ls -la", &tx);
        match rx.try_recv().unwrap() {
            AgentCommand::Shell { command, to_model } => {
                assert_eq!(command, "ls -la");
                assert!(to_model);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(!app.busy);
    }

    #[test]
    fn app_tracks_streaming_and_done() {
        let mut app = test_app();
        app.on_event(AgentEvent::TextDelta("Hel".into()));
        app.on_event(AgentEvent::TextDelta("lo".into()));
        assert_eq!(app.pending.as_ref().unwrap().text, "Hello");
        app.on_event(AgentEvent::AssistantDone {
            text: "Hello".into(),
            origin: Origin::Local,
        });
        assert!(app.pending.is_none());
        assert_eq!(app.entries.iter().filter(|e| e.text == "Hello").count(), 1);
        assert!(!app.busy);
    }

    #[test]
    fn assistant_done_without_streaming_pushes_once() {
        let mut app = test_app();
        app.on_event(AgentEvent::AssistantDone {
            text: "Direct".into(),
            origin: Origin::Local,
        });
        assert_eq!(app.entries.iter().filter(|e| e.text == "Direct").count(), 1);
    }

    #[test]
    fn tool_events_render() {
        let mut app = test_app();
        app.on_event(AgentEvent::ToolStarted {
            name: "bash".into(),
            args: "{\"command\":\"ls -la\"}".into(),
        });
        app.on_event(AgentEvent::ToolFinished {
            name: "bash".into(),
            is_error: false,
            output: "ok".into(),
        });
        let entry = app.entries.iter().find(|e| e.kind == Kind::Tool).unwrap();
        assert_eq!(entry.text, "bash ls -la ✓");
        assert_eq!(entry.output.as_deref(), Some("ok"));
        assert_eq!(
            app.entries.iter().filter(|e| e.kind == Kind::Tool).count(),
            1
        );
    }

    #[test]
    fn unfinished_tool_shows_running_header() {
        let mut app = test_app();
        app.on_event(AgentEvent::ToolStarted {
            name: "read".into(),
            args: "{\"path\":\"Cargo.toml\"}".into(),
        });
        let entry = app.entries.iter().find(|e| e.kind == Kind::Tool).unwrap();
        assert_eq!(entry.text, "read Cargo.toml …");
    }

    #[test]
    fn tool_output_is_capped_and_expands() {
        let mut app = test_app();
        app.on_event(AgentEvent::ToolStarted {
            name: "bash".into(),
            args: "{\"command\":\"seq\"}".into(),
        });
        app.on_event(AgentEvent::ToolFinished {
            name: "bash".into(),
            is_error: false,
            output: (1..=50)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        });
        let entry = app.entries.iter().find(|e| e.kind == Kind::Tool).unwrap();
        let lines = entry_lines(entry, 80, false, &Theme::new(ThemeName::Dark));
        assert_eq!(
            lines.len(),
            TOOL_OUTPUT_LINES + 2,
            "header + 20 lines + note"
        );
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(texts[1], "  line 1");
        assert!(
            texts[21].contains("+30 lines (Ctrl+O to expand)"),
            "{texts:#?}"
        );
        let expanded = entry_lines(entry, 80, true, &Theme::new(ThemeName::Dark));
        assert_eq!(expanded.len(), 51);
    }

    #[test]
    fn quit_command_exits() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut app = test_app();
        let key = |code| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
        for c in ":quit".chars() {
            app.handle_key(key(KeyCode::Char(c)), &tx, &cancel);
        }
        assert!(app.handle_key(key(KeyCode::Enter), &tx, &cancel));
    }

    #[test]
    fn editing_moves_cursor_and_inserts() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut app = test_app();
        let key = |code| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
        app.handle_key(key(KeyCode::Char('b')), &tx, &cancel);
        app.handle_key(key(KeyCode::Char('c')), &tx, &cancel);
        app.handle_key(key(KeyCode::Left), &tx, &cancel);
        app.handle_key(key(KeyCode::Char('a')), &tx, &cancel);
        assert_eq!(app.input, "bac");
        assert_eq!(app.cursor, 2);
        app.handle_key(key(KeyCode::Backspace), &tx, &cancel);
        assert_eq!(app.input, "bc");
    }

    #[test]
    fn shift_enter_and_ctrl_j_insert_newlines_without_submitting() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut app = test_app();
        app.handle_key(
            crossterm::event::KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            &tx,
            &cancel,
        );
        app.handle_key(
            crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            &tx,
            &cancel,
        );
        app.handle_key(
            crossterm::event::KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
            &tx,
            &cancel,
        );
        app.handle_key(
            crossterm::event::KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            &tx,
            &cancel,
        );
        assert_eq!(app.input, "a\nb\n");
        assert_eq!(app.cursor, 4);
    }

    #[test]
    fn enter_submits_multiline_input_as_one_prompt() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut app = test_app();
        app.insert_text("line one\nline two");
        app.handle_key(
            crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &tx,
            &cancel,
        );
        match rx.try_recv().unwrap() {
            AgentCommand::Prompt { text, .. } => assert_eq!(text, "line one\nline two"),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn insert_text_normalizes_paste_line_endings() {
        let mut app = test_app();
        app.insert_text("a\r\nb\rc");
        assert_eq!(app.input, "a\nb\nc");
        assert_eq!(app.cursor, 5);
    }

    #[test]
    fn tab_completes_directories_for_pwd_argument() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("scripts")).unwrap();
        std::fs::create_dir(d.path().join("src")).unwrap();
        std::fs::create_dir(d.path().join("src").join("main")).unwrap();
        std::fs::write(d.path().join("scripts.md"), "not a dir").unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut app = test_app();
        app.pwd = d.path().to_path_buf();
        let key = |code| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
        for c in ":pwd s".chars() {
            app.handle_key(key(KeyCode::Char(c)), &tx, &cancel);
        }
        app.handle_key(key(KeyCode::Tab), &tx, &cancel);
        assert_eq!(app.input, ":pwd scripts/");
        app.handle_key(key(KeyCode::Tab), &tx, &cancel);
        assert_eq!(app.input, ":pwd src/");
        app.handle_key(key(KeyCode::Tab), &tx, &cancel);
        assert_eq!(app.input, ":pwd scripts/");
        app.completion = None;
        app.input = ":pwd src/".to_string();
        app.cursor = 9;
        app.handle_key(key(KeyCode::Tab), &tx, &cancel);
        assert_eq!(app.input, ":pwd src/main/");
        assert_eq!(app.cursor, app.input.chars().count());
    }

    #[test]
    fn pwd_submission_sends_completed_path() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = test_app();
        app.on_submit(":pwd src/main/", &tx);
        match rx.try_recv().unwrap() {
            AgentCommand::SetPwd { path } => assert_eq!(path, PathBuf::from("src/main/")),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn tab_completes_unique_prefix_with_space() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut app = test_app();
        let key = |code| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
        for c in ":res".chars() {
            app.handle_key(key(KeyCode::Char(c)), &tx, &cancel);
        }
        app.handle_key(key(KeyCode::Tab), &tx, &cancel);
        assert_eq!(app.input, ":resume ");
        assert_eq!(app.cursor, 8);
    }

    #[test]
    fn tab_cycles_when_ambiguous_and_backtab_reverses() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut app = test_app();
        let key = |code| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
        app.handle_key(key(KeyCode::Char(':')), &tx, &cancel);
        app.handle_key(key(KeyCode::Tab), &tx, &cancel);
        let first = app.input.clone();
        assert!(first.starts_with(':') && !first.contains(' '), "{first}");
        app.handle_key(key(KeyCode::Tab), &tx, &cancel);
        assert_ne!(app.input, first);
        app.handle_key(key(KeyCode::BackTab), &tx, &cancel);
        assert_eq!(app.input, first);
    }

    #[test]
    fn tab_completes_template_names() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut app = test_app();
        app.templates.push(Template {
            name: "greet".into(),
            description: String::new(),
            argument_hint: None,
            body: "hello".into(),
        });
        let key = |code| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
        for c in ":gr".chars() {
            app.handle_key(key(KeyCode::Char(c)), &tx, &cancel);
        }
        app.handle_key(key(KeyCode::Tab), &tx, &cancel);
        assert_eq!(app.input, ":greet ");
    }

    #[test]
    fn editing_resets_completion_cycle() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut app = test_app();
        let key = |code| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
        app.handle_key(key(KeyCode::Char(':')), &tx, &cancel);
        app.handle_key(key(KeyCode::Tab), &tx, &cancel);
        let first = app.input.clone();
        app.handle_key(key(KeyCode::Char('c')), &tx, &cancel);
        assert!(app.input.starts_with(&first));
        assert!(app.completion.is_none() || app.input.contains(' '));
    }

    #[test]
    fn input_layout_keeps_cursor_line_visible() {
        let mut app = test_app();
        app.insert_text("l1\nl2\nl3\nl4");
        app.cursor = app.input.len();
        let layout = app.input_layout(40, 2);
        let texts: Vec<String> = layout
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(texts, vec!["  l3", "  l4"]);
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 2));
        app.cursor = 3;
        let layout = app.input_layout(40, 2);
        assert_eq!(layout.cursor_row, 1);
        assert_eq!(layout.lines[0].spans[0].content, "❯ l1");
    }

    fn layout_texts(layout: &super::InputLayout) -> Vec<String> {
        layout
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn input_layout_wraps_on_last_whitespace_before_width() {
        let mut app = test_app();
        app.insert_text("the quick brown fox jumps");
        app.cursor = app.input.len();
        let layout = app.input_layout(20, 5);
        assert_eq!(
            layout_texts(&layout),
            vec!["❯ the quick brown", "  fox jumps"]
        );
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 9));
    }

    #[test]
    fn input_layout_cursor_sits_on_the_row_it_wraps_to() {
        let mut app = test_app();
        app.insert_text("one two three");
        app.cursor = "one t".len();
        let layout = app.input_layout(9, 5);
        assert_eq!(layout_texts(&layout), vec!["❯ one two", "  three"]);
        assert_eq!((layout.cursor_row, layout.cursor_col), (0, 5));
        app.cursor = "one two ".len();
        let layout = app.input_layout(9, 5);
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 0));
    }

    #[test]
    fn input_layout_hard_splits_tokens_longer_than_the_width() {
        let mut app = test_app();
        app.insert_text("abcdefghij");
        app.cursor = app.input.len();
        let layout = app.input_layout(8, 5);
        assert_eq!(layout_texts(&layout), vec!["❯ abcdef", "  ghij"]);
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 4));
    }

    #[test]
    fn input_layout_scrolls_to_keep_a_wrapped_cursor_row_visible() {
        let mut app = test_app();
        app.insert_text("aaaa bbbb cccc dddd");
        app.cursor = app.input.len();
        let layout = app.input_layout(10, 2);
        assert_eq!(layout_texts(&layout), vec!["  cccc", "  dddd"]);
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 4));
    }

    #[test]
    fn input_layout_shows_spaces_beyond_the_row_width_on_the_next_row() {
        let mut app = test_app();
        app.insert_text("one two   ");
        app.cursor = app.input.len();
        let layout = app.input_layout(9, 5);
        assert_eq!(layout_texts(&layout), vec!["❯ one two", "     "]);
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 3));
    }

    #[test]
    fn input_layout_keeps_trailing_spaces_on_their_own_row() {
        let mut app = test_app();
        app.insert_text("one two   ");
        app.cursor = "one two  ".len();
        let layout = app.input_layout(20, 5);
        assert_eq!(layout_texts(&layout), vec!["❯ one two   "]);
        assert_eq!((layout.cursor_row, layout.cursor_col), (0, 9));
        let layout = app.input_layout(9, 5);
        assert_eq!(layout_texts(&layout), vec!["❯ one two", "     "]);
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 2));
    }

    #[test]
    fn input_layout_renders_a_line_of_only_spaces() {
        let mut app = test_app();
        app.insert_text("a\n   \nb");
        app.cursor = "a\n   ".len();
        let layout = app.input_layout(10, 5);
        assert_eq!(layout_texts(&layout), vec!["❯ a", "     ", "  b"]);
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 3));
    }

    #[test]
    fn wrapped_input_keeps_the_prompt_on_the_first_row_only() {
        let mut app = test_app();
        app.insert_text("wrap me please\nnext");
        let layout = app.input_layout(10, 6);
        assert_eq!(
            layout_texts(&layout),
            vec!["❯ wrap me", "  please", "  next"]
        );
    }

    #[test]
    fn cursor_at_the_start_of_a_line_lands_on_that_line_not_the_previous_wrap() {
        let mut app = test_app();
        app.insert_text("wrap me please\nnext");
        app.cursor = "wrap me please\n".len();
        let layout = app.input_layout(10, 6);
        assert_eq!((layout.cursor_row, layout.cursor_col), (2, 0));
    }
}

#[cfg(test)]
mod wrap_tests {
    use super::{entry_lines, wrap_words, Entry, Kind, Theme};
    use crate::config::ThemeName;
    use std::path::PathBuf;

    #[test]
    fn wraps_on_word_boundaries() {
        assert_eq!(
            wrap_words("acknowledge this", 12),
            vec!["acknowledge", "this"]
        );
        assert_eq!(wrap_words("one two three", 20), vec!["one two three"]);
        assert_eq!(wrap_words("a b", 10), vec!["a b"]);
        assert_eq!(wrap_words("", 10), Vec::<String>::new());
    }

    #[test]
    fn hard_splits_words_longer_than_width() {
        assert_eq!(wrap_words("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(
            wrap_words("supercalifragilistic expi", 10),
            vec!["supercalif", "ragilistic", "expi"]
        );
    }

    #[test]
    fn entry_lines_use_prefix_and_hanging_indent() {
        let entry = Entry {
            text: "acknowledge this now".to_string(),
            kind: Kind::User,
            output: None,
        };
        let lines = entry_lines(&entry, 13, false, &Theme::new(ThemeName::Dark));
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(texts, vec!["❯ acknowledge", "  this now"]);
    }

    #[test]
    fn reasoning_entry_renders_as_one_block() {
        let mut app = super::App::new(
            PathBuf::from("/tmp"),
            "m".into(),
            vec![],
            vec![],
            128_000,
            Theme::new(ThemeName::Dark),
            None,
        );
        app.on_event(super::AgentEvent::ReasoningDelta("line one\n".into()));
        app.on_event(super::AgentEvent::ReasoningDelta("line two".into()));
        let pending = app.pending.as_ref().unwrap();
        assert_eq!(pending.text, "line one\nline two");
        assert_eq!(
            entry_lines(pending, 40, false, &Theme::new(ThemeName::Dark)).len(),
            2
        );
    }

    fn assistant(text: &str) -> Entry {
        Entry {
            text: text.to_string(),
            kind: Kind::Assistant,
            output: None,
        }
    }

    fn line_text(line: &super::Line) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn code_fences_render_indented_without_backticks() {
        let entry = assistant("before\n```rust\nfn main() {}\n```\nafter");
        let lines: Vec<String> = entry_lines(&entry, 60, false, &Theme::new(ThemeName::Dark))
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(lines, vec!["before", "    fn main() {}", "after"]);
    }

    #[test]
    fn code_fences_are_not_reflowed() {
        let long = "x".repeat(120);
        let entry = assistant(&format!("```\n{long}\n```"));
        let lines: Vec<String> = entry_lines(&entry, 40, false, &Theme::new(ThemeName::Dark))
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(lines, vec![format!("    {long}")]);
    }

    #[test]
    fn prose_outside_fences_still_wraps() {
        let entry = assistant("```\ncode\n```\nword word word word word");
        let lines: Vec<String> = entry_lines(&entry, 10, false, &Theme::new(ThemeName::Dark))
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(lines, vec!["    code", "word word", "word word", "word"]);
    }
}

#[cfg(test)]
mod draw_tests {
    use super::*;
    use crate::config::DiscordConfig;
    use ratatui::backend::TestBackend;

    fn test_app() -> App {
        App::new(
            PathBuf::from("/tmp"),
            "test-model".into(),
            vec!["test-model".into()],
            vec![],
            128_000,
            Theme::new(ThemeName::Dark),
            None,
        )
    }

    #[test]
    fn classify_pwd_command() {
        let tpl: Vec<String> = vec![];
        assert_eq!(
            classify_input(":pwd /tmp/other", &tpl),
            Some(InputAction::Command(Command::Pwd("/tmp/other".into())))
        );
        assert_eq!(
            classify_input(":pwd", &tpl),
            Some(InputAction::Command(Command::Pwd(String::new())))
        );
    }

    #[test]
    fn pwd_command_sends_setpwd_or_shows_current() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = test_app();
        app.on_submit(":pwd", &tx);
        assert!(app
            .entries
            .iter()
            .any(|e| e.kind == Kind::Info && e.text.contains("pwd:")));
        assert!(rx.try_recv().is_err());
        app.on_submit(":pwd ../elsewhere", &tx);
        match rx.try_recv().unwrap() {
            AgentCommand::SetPwd { path } => assert_eq!(path, PathBuf::from("../elsewhere")),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn discord_command_reports_the_bridge_or_says_it_is_off() {
        let tpl: Vec<String> = vec![];
        assert_eq!(
            classify_input(":discord", &tpl),
            Some(InputAction::Command(Command::Discord))
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = test_app();
        app.on_submit(":discord", &tx);
        assert!(app
            .entries
            .iter()
            .any(|e| e.text.contains("discord bridge is off")));

        let (events, _keep) = broadcast::channel(8);
        let config = DiscordConfig {
            enabled: true,
            bot_token: "tok".to_string(),
            allowed_user_id: "42".to_string(),
        };
        app.discord = Some(Arc::new(Bridge::new(&config, events, false)));
        app.on_submit(":discord", &tx);
        let text: String = app
            .entries
            .iter()
            .map(|e| e.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("connecting to the gateway"), "{text}");
        assert!(text.contains("allowed_user_id: 42"), "{text}");
        assert!(
            text.contains("answers go back to the DM or channel"),
            "{text}"
        );
    }

    #[test]
    fn a_request_the_agent_cannot_take_says_so_in_the_transcript() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let mut app = test_app();

        app.submit_prompt("build it", "build it".to_string(), &tx);

        assert!(app.entries.iter().any(|e| e.kind == Kind::User));
        assert!(
            app.entries
                .iter()
                .any(|e| e.kind == Kind::Error && e.text.contains("not running")),
            "{:?}",
            app.entries
                .iter()
                .map(|e| e.text.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn notes_and_faults_keep_their_kind_in_the_transcript() {
        let mut app = test_app();
        app.on_event(AgentEvent::Info(
            "discord: answering DM #9 with 1 chunk".to_string(),
        ));
        app.on_event(AgentEvent::Fault(
            "discord: could not post a reply: #9: discord REST 403: Missing Permissions"
                .to_string(),
        ));
        assert!(app
            .entries
            .iter()
            .any(|e| e.kind == Kind::Info && e.text.contains("answering DM #9")));
        assert!(app
            .entries
            .iter()
            .any(|e| e.kind == Kind::Error && e.text.contains("403")));
    }

    #[test]
    fn discord_report_draws_the_lines_that_report_failures_as_errors() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (events, _keep) = broadcast::channel(8);
        let config = DiscordConfig {
            enabled: true,
            bot_token: "tok".to_string(),
            allowed_user_id: String::new(),
        };
        let bridge = Arc::new(Bridge::new(&config, events, false));
        bridge.fail("discord: could not post a reply: #9: discord REST 401: Unauthorized");

        let mut app = test_app();
        app.discord = Some(bridge);
        app.on_submit(":discord", &tx);

        let errors: Vec<&str> = app
            .entries
            .iter()
            .filter(|e| e.kind == Kind::Error)
            .map(|e| e.text.as_str())
            .collect();
        assert!(errors.iter().any(|t| t.contains("401")), "{errors:?}");
    }

    #[test]
    fn welcome_shows_version_model_context_pwd_session() {
        let app = App::new(
            PathBuf::from("/home/user/repo"),
            "qwen4exp".into(),
            vec!["qwen4exp".into()],
            vec![],
            128_000,
            Theme::new(ThemeName::Dark),
            Some("/state/sess.jsonl".to_string()),
        );
        let text: String = app
            .entries
            .iter()
            .map(|e| e.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(&format!("rho v{}", env!("CARGO_PKG_VERSION"))));
        assert!(text.contains("qwen4exp"), "{text}");
        assert!(text.contains("128k tokens"), "{text}");
        assert!(text.contains("/home/user/repo"), "{text}");
        assert!(text.contains("/state/sess.jsonl"), "{text}");
    }

    #[test]
    fn draws_entries_and_pending() {
        let mut app = test_app();
        app.push("first user message", Kind::User);
        app.on_event(AgentEvent::TextDelta("streaming answer".into()));
        let mut term = ratatui::Terminal::new(TestBackend::new(80, 12)).unwrap();
        term.draw(|f| app.draw(f, f.area())).unwrap();
        let buf = term.backend().buffer().clone();
        let content: String = (0..buf.area().height)
            .map(|y| {
                (0..buf.area().width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(content.contains("first user message"), "{content}");
        assert!(content.contains("streaming answer"), "{content}");
    }

    #[test]
    fn typed_input_is_visible_and_quit_exits_via_keys() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut app = test_app();
        let key = |code| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
        for c in "hello world".chars() {
            app.handle_key(key(KeyCode::Char(c)), &tx, &cancel);
        }
        let mut term = ratatui::Terminal::new(TestBackend::new(80, 12)).unwrap();
        term.draw(|f| app.draw(f, f.area())).unwrap();
        let buf = term.backend().buffer().clone();
        let content: String = (0..buf.area().height)
            .map(|y| {
                (0..buf.area().width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(content.contains("❯ hello world"), "{content}");
        assert!(!app.handle_key(key(KeyCode::Enter), &tx, &cancel));
        for c in ":quit".chars() {
            app.handle_key(key(KeyCode::Char(c)), &tx, &cancel);
        }
        assert!(app.handle_key(key(KeyCode::Enter), &tx, &cancel));
    }
}

#[cfg(test)]
mod remote_tests {
    use super::*;
    use crate::config::DiscordConfig;
    use crate::discord::{Posted, Signal, Signals};

    /// An app wired to a bridge whose outbox and courtesy queue the test reads.
    fn remote_app() -> (App, mpsc::Receiver<Posted>, mpsc::Receiver<Signal>) {
        let (events, _notes) = broadcast::channel(8);
        let config = DiscordConfig {
            enabled: true,
            bot_token: "tok".to_string(),
            allowed_user_id: "42".to_string(),
        };
        let bridge = Arc::new(Bridge::new(&config, events, false));
        let (outbox, answers) = mpsc::channel(8);
        let (signals, courtesies) = mpsc::channel(32);
        bridge.attach_delivery(outbox, Signals::new(signals, bridge.clone()));
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "test-model".into(),
            vec!["test-model".into()],
            vec![],
            128_000,
            Theme::new(ThemeName::Dark),
            None,
        );
        app.discord = Some(bridge);
        (app, answers, courtesies)
    }

    fn command(text: &str) -> RemoteCommand {
        RemoteCommand {
            origin: Origin::Channel("9".to_string()),
            text: text.to_string(),
        }
    }

    #[test]
    fn command_text_is_recognised_without_a_template_list() {
        assert!(is_command(":compact"));
        assert!(is_command(":review"));
        assert!(is_command("!ls -la"));
        assert!(is_command("!!git status"));
        assert!(!is_command("build it"));
        assert!(!is_command("fix !this please"));
    }

    #[test]
    fn a_remote_command_posts_what_the_transcript_shows() {
        let (mut app, mut answers, _courtesies) = remote_app();
        let (tx, _rx) = mpsc::unbounded_channel();

        app.on_remote(command(":help"), &tx);

        let said = answers.try_recv().unwrap();
        assert_eq!(said.channel_id, "9");
        assert!(said.text.contains(":compact"), "{}", said.text);
        assert!(
            said.text.contains("!!cmd runs it without sharing"),
            "{}",
            said.text
        );
    }

    #[test]
    fn a_remote_shell_command_waits_for_its_output() {
        let (mut app, mut answers, _courtesies) = remote_app();
        let (tx, _rx) = mpsc::unbounded_channel();

        app.on_remote(command("!ls"), &tx);
        assert!(answers.try_recv().is_err());

        app.on_event(AgentEvent::ToolStarted {
            name: "bash".into(),
            args: "ls".into(),
        });
        app.on_event(AgentEvent::ToolFinished {
            name: "bash".into(),
            is_error: false,
            output: "file.txt\n".into(),
        });
        assert!(answers.try_recv().is_err(), "the agent is still working");

        app.remote_idle(&tx);

        let said = answers.try_recv().unwrap();
        assert!(said.text.contains("! ls"), "{}", said.text);
        assert!(said.text.contains("bash ls ✓"), "{}", said.text);
        assert!(said.text.contains("file.txt"), "{}", said.text);
    }

    #[test]
    fn a_command_sent_while_rho_is_busy_waits_its_turn() {
        let (mut app, mut answers, _courtesies) = remote_app();
        let (tx, _rx) = mpsc::unbounded_channel();
        app.busy = true;

        app.on_remote(command(":session"), &tx);
        assert!(answers.try_recv().is_err());

        app.remote_idle(&tx);

        let said = answers.try_recv().unwrap();
        assert!(said.text.contains("session (unnamed)"), "{}", said.text);
    }

    #[test]
    fn a_loop_reports_each_iteration_as_it_lands() {
        let (mut app, mut answers, _courtesies) = remote_app();
        let (tx, _rx) = mpsc::unbounded_channel();

        app.on_remote(command(":loop 2 build it"), &tx);
        assert!(answers.try_recv().is_err());

        app.on_event(AgentEvent::LoopStarted {
            description: "2 times".into(),
        });
        app.on_event(AgentEvent::LoopIterationDone { iterations: 1 });
        let said = answers.try_recv().unwrap();
        assert!(
            said.text.contains("loop started [2 times]"),
            "{}",
            said.text
        );
        assert!(
            answers.try_recv().is_err(),
            "nothing new since the last post"
        );

        app.on_event(AgentEvent::LoopFinished {
            iterations: 2,
            reason: "count reached".into(),
        });
        app.remote_idle(&tx);
        let said = answers.try_recv().unwrap();
        assert!(
            said.text
                .contains("loop finished: count reached after 2 iteration(s)"),
            "{}",
            said.text
        );
    }

    #[test]
    fn reasoning_is_left_out_of_the_reply() {
        let (mut app, mut answers, _courtesies) = remote_app();
        let (tx, _rx) = mpsc::unbounded_channel();

        app.on_remote(command(":compact"), &tx);
        app.on_event(AgentEvent::ReasoningDelta("thinking hard".into()));
        app.on_event(AgentEvent::Info("compacted 4 messages".into()));
        app.remote_idle(&tx);

        let said = answers.try_recv().unwrap();
        assert!(said.text.contains("compacted 4 messages"), "{}", said.text);
        assert!(!said.text.contains("thinking hard"), "{}", said.text);
    }

    #[test]
    fn a_command_that_produced_nothing_says_so() {
        let (mut app, mut answers, _courtesies) = remote_app();
        let (tx, _rx) = mpsc::unbounded_channel();

        app.on_remote(command(":name rho"), &tx);
        app.remote_idle(&tx);

        let said = answers.try_recv().unwrap();
        assert!(said.text.contains(":name rho"), "{}", said.text);
        assert!(said.text.contains("produced no output"), "{}", said.text);
    }

    /// A command a DM asked for is answered in that DM, not in the channel the
    /// bot happens to have been used in before.
    #[test]
    fn a_command_from_a_dm_is_answered_in_that_dm() {
        let (mut app, mut answers, _courtesies) = remote_app();
        let (tx, _rx) = mpsc::unbounded_channel();

        app.on_remote(
            RemoteCommand {
                origin: Origin::Dm("7".to_string()),
                text: ":help".to_string(),
            },
            &tx,
        );

        assert_eq!(answers.try_recv().unwrap().channel_id, "7");
    }

    /// The window mirrors what a remote command added to the transcript, and
    /// nothing a prompt typed at the keyboard added.
    #[test]
    fn a_prompt_typed_at_the_keyboard_stays_out_of_discord() {
        let (mut app, mut answers, _courtesies) = remote_app();
        let (tx, _rx) = mpsc::unbounded_channel();

        app.submit_prompt("build it", "build it".to_string(), &tx);
        app.on_event(AgentEvent::AssistantDone {
            text: "done".into(),
            origin: Origin::Local,
        });
        app.remote_idle(&tx);

        assert!(answers.try_recv().is_err());
    }

    #[test]
    fn quit_from_discord_leaves_the_window_open() {
        let (mut app, mut answers, _courtesies) = remote_app();
        let (tx, _rx) = mpsc::unbounded_channel();

        app.on_remote(command(":quit"), &tx);

        assert!(!app.should_quit());
        let said = answers.try_recv().unwrap();
        assert!(said.text.contains("keyboard"), "{}", said.text);
    }
}

#[cfg(test)]
mod scroll_tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn test_app() -> App {
        App::new(
            PathBuf::from("/tmp"),
            "test-model".into(),
            vec!["test-model".into()],
            vec![],
            128_000,
            Theme::new(ThemeName::Dark),
            None,
        )
    }

    fn render(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut term = ratatui::Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f, f.area())).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area().height)
            .map(|y| {
                (0..buf.area().width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn pages_vertically_not_horizontally() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "m".into(),
            vec!["m".into()],
            vec![],
            128_000,
            Theme::new(ThemeName::Dark),
            None,
        );
        for i in 0..60 {
            app.push(format!("line {i}"), Kind::Tool);
        }
        let screen = render(&mut app, 80, 24);
        assert!(screen.iter().any(|l| l.contains("line 59")), "{screen:#?}");
        assert!(
            !screen.iter().any(|l| l.contains("line 10")),
            "older lines should be scrolled past"
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        app.handle_key(
            crossterm::event::KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            &tx,
            &cancel,
        );
        let screen = render(&mut app, 80, 24);
        assert!(
            screen.iter().any(|l| l.contains("line 45")),
            "after PgUp: {screen:#?}"
        );
        assert!(
            !screen.iter().any(|l| l.contains("line 59")),
            "tail should scroll off after PgUp"
        );
        app.handle_key(
            crossterm::event::KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            &tx,
            &cancel,
        );
        let screen = render(&mut app, 80, 24);
        assert!(
            screen.iter().any(|l| l.contains("line 59")),
            "after PageDown"
        );
    }

    #[test]
    fn scrolled_view_survives_streaming_output() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "m".into(),
            vec!["m".into()],
            vec![],
            128_000,
            Theme::new(ThemeName::Dark),
            None,
        );
        for i in 0..60 {
            app.push(format!("line {i}"), Kind::Tool);
        }
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        app.handle_key(
            crossterm::event::KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            &tx,
            &cancel,
        );
        let before = render(&mut app, 80, 24);
        app.on_event(AgentEvent::TextDelta("more streaming text".into()));
        app.on_event(AgentEvent::ToolStarted {
            name: "bash".into(),
            args: "long job".into(),
        });
        let after = render(&mut app, 80, 24);
        assert_eq!(before[5], after[5], "view must not jump while scrolled up");
        app.handle_key(
            crossterm::event::KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
            &tx,
            &cancel,
        );
        let screen = render(&mut app, 80, 24);
        assert!(
            screen.iter().any(|l| l.contains("more streaming text")),
            "{screen:#?}"
        );
        assert!(screen.iter().any(|l| l.contains("bash")));
    }

    #[test]
    fn wheel_scroll_detaches_and_reattaches_follow() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "m".into(),
            vec!["m".into()],
            vec![],
            128_000,
            Theme::new(ThemeName::Dark),
            None,
        );
        assert!(app.follow);
        app.scroll_by(3);
        assert!(!app.follow);
        assert_eq!(app.scroll_from_bottom, 3);
        app.scroll_by(-3);
        assert_eq!(app.scroll_from_bottom, 0);
        assert!(app.follow);
        app.scroll_by(-5);
        assert_eq!(app.scroll_from_bottom, 0);
        assert!(app.follow);
    }

    fn right_column(screen: &[String]) -> String {
        screen
            .iter()
            .skip(1)
            .take(19)
            .map(|line| line.chars().nth(79).unwrap_or(' '))
            .collect()
    }

    #[test]
    fn scrollbar_appears_on_overflow_and_follows_scroll() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "m".into(),
            vec!["m".into()],
            vec![],
            128_000,
            Theme::new(ThemeName::Dark),
            None,
        );
        for i in 0..60 {
            app.push(format!("line {i}"), Kind::Tool);
        }
        let screen = render(&mut app, 80, 24);
        let col = right_column(&screen);
        assert!(col.contains('█'), "expected scrollbar thumb, got: {col:?}");

        app.scroll_by(30);
        let screen = render(&mut app, 80, 24);
        let scrolled_col = right_column(&screen);
        assert_ne!(col, scrolled_col, "thumb should move when scrolled");
    }

    #[test]
    fn no_scrollbar_when_content_fits() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "m".into(),
            vec!["m".into()],
            vec![],
            128_000,
            Theme::new(ThemeName::Dark),
            None,
        );
        app.push("short message".to_string(), Kind::User);
        let screen = render(&mut app, 80, 24);
        let col = right_column(&screen);
        assert!(
            col.chars().all(|c| c == ' '),
            "no scrollbar expected, got: {col:?}"
        );
    }

    #[test]
    fn themes_produce_distinct_readable_palettes() {
        let dark = Theme::new(ThemeName::Dark);
        let light = Theme::new(ThemeName::Light);
        let ansi = Theme::new(ThemeName::Ansi);
        assert_eq!(kind_style(&dark, Kind::Reasoning).fg, Some(Color::DarkGray));
        assert_eq!(kind_style(&light, Kind::Reasoning).fg, Some(Color::Blue));
        assert_eq!(kind_style(&ansi, Kind::Tool).fg, Some(Color::Yellow));
        assert_eq!(kind_style(&ansi, Kind::User).fg, Some(Color::Blue));
        assert_eq!(dark.code_style().bg, Some(Color::DarkGray));
        assert_eq!(ansi.code_style().bg, None);
        assert!(ansi
            .code_style()
            .add_modifier
            .contains(Modifier::UNDERLINED));
        assert_eq!(light.output_style().fg, Some(Color::Black));
    }

    #[test]
    fn status_line_uses_spinner_and_elapsed() {
        let mut app = test_app();
        assert_eq!(app.status_line(), "idle");
        app.busy = true;
        app.status = "thinking".to_string();
        app.turn_start = Some(std::time::Instant::now());
        app.spinner_frame = 2;
        let line = app.status_line();
        assert!(line.starts_with('⠹'), "{line}");
        assert!(line.contains("thinking"), "{line}");
        assert!(line.ends_with("0s"), "{line}");
    }

    #[test]
    fn context_usage_shows_tokens_and_percent() {
        let mut app = test_app();
        app.est_tokens = 478;
        app.context_window = 128_000;
        let (usage, color) = app.context_usage();
        assert_eq!(usage, "~478/128k (0.4%)");
        assert_eq!(color, Color::Green);
        app.est_tokens = 100_000;
        let (usage, color) = app.context_usage();
        assert_eq!(usage, "~100k/128k (78.1%)");
        assert_eq!(color, Color::Yellow);
        app.est_tokens = 120_000;
        let (_, color) = app.context_usage();
        assert_eq!(color, Color::Red);
    }

    #[test]
    fn fmt_tokens_compacts_large_values() {
        assert_eq!(fmt_tokens(478), "478");
        assert_eq!(fmt_tokens(128_000), "128k");
        assert_eq!(fmt_tokens(4096), "4.1k");
        assert_eq!(fmt_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn footer_shows_session_name_and_pwd() {
        let mut app = test_app();
        let title = app.footer_title();
        let text: String = title.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("tmp"), "{text}");
        app.session_name = Some("my-work".to_string());
        let title = app.footer_title();
        let text: String = title.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("my-work · tmp"), "{text}");
        assert!(text.contains("test-model"), "{text}");
    }

    #[test]
    fn long_lines_wrap_within_width() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "m".into(),
            vec!["m".into()],
            vec![],
            128_000,
            Theme::new(ThemeName::Dark),
            None,
        );
        app.push("x".repeat(500), Kind::Assistant);
        let screen = render(&mut app, 80, 24);
        for row in &screen {
            assert!(
                row.trim_end().chars().count() <= 80,
                "row too wide: {row:?}"
            );
        }
        assert!(screen.iter().any(|l| l.starts_with('x')));
    }

    #[test]
    fn different_kinds_are_separated_by_blank_line() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "m".into(),
            vec!["m".into()],
            vec![],
            128_000,
            Theme::new(ThemeName::Dark),
            None,
        );
        app.push("hello there".to_string(), Kind::User);
        app.push("a response".to_string(), Kind::Assistant);
        let screen = render(&mut app, 80, 24);
        let user_row = screen
            .iter()
            .position(|l| l.contains("hello there"))
            .unwrap();
        let resp_row = screen
            .iter()
            .position(|l| l.contains("a response"))
            .unwrap();
        assert_eq!(
            resp_row,
            user_row + 2,
            "blank line between turns: {screen:#?}"
        );
        assert_eq!(screen[user_row + 1].trim(), "");
    }

    #[test]
    fn typed_input_wraps_instead_of_scrolling_horizontally() {
        let mut app = test_app();
        app.insert_text("the quick brown fox jumps over the lazy dog");
        let screen = render(&mut app, 40, 12);
        let box_rows: Vec<&String> = screen
            .iter()
            .filter(|l| {
                l.starts_with('│') && (l.contains('❯') || l.contains("brown") || l.contains("lazy"))
            })
            .collect();
        assert_eq!(
            box_rows,
            vec![
                &"│❯ the quick brown fox jumps over the  │".to_string(),
                &"│  lazy dog                            │".to_string()
            ]
        );
        assert!(!screen.iter().any(|l| l.contains("dog") && l.contains('❯')));
    }

    #[test]
    fn ctrl_r_toggles_reasoning_visibility() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "m".into(),
            vec!["m".into()],
            vec![],
            128_000,
            Theme::new(ThemeName::Dark),
            None,
        );
        app.push("because reasons".to_string(), Kind::Reasoning);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let key = |code| crossterm::event::KeyEvent::new(code, KeyModifiers::CONTROL);
        assert!(render(&mut app, 80, 24)
            .iter()
            .any(|l| l.contains("because reasons")));
        app.handle_key(key(KeyCode::Char('r')), &tx, &cancel);
        assert!(!render(&mut app, 80, 24)
            .iter()
            .any(|l| l.contains("because reasons")));
        app.handle_key(key(KeyCode::Char('r')), &tx, &cancel);
        assert!(render(&mut app, 80, 24)
            .iter()
            .any(|l| l.contains("because reasons")));
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use crossterm::event::MouseButton;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn test_app() -> App {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "m".into(),
            vec!["m".into()],
            vec![],
            128_000,
            Theme::new(ThemeName::Dark),
            None,
        );
        app.clipboard = Clipboard::memory();
        app
    }

    fn draw(app: &mut App, w: u16, h: u16) -> Buffer {
        let mut term = ratatui::Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f, f.area())).unwrap();
        term.backend().buffer().clone()
    }

    fn rows(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area().height)
            .map(|y| {
                (0..buffer.area().width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    fn pointer(kind: MouseEventKind, column: u16, row: u16) -> event::MouseEvent {
        event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn down(column: u16, row: u16) -> event::MouseEvent {
        pointer(MouseEventKind::Down(MouseButton::Left), column, row)
    }

    fn drag_to(column: u16, row: u16) -> event::MouseEvent {
        pointer(MouseEventKind::Drag(MouseButton::Left), column, row)
    }

    fn release(column: u16, row: u16) -> event::MouseEvent {
        pointer(MouseEventKind::Up(MouseButton::Left), column, row)
    }

    fn click(app: &mut App, column: u16, row: u16) {
        app.on_mouse(down(column, row));
        app.on_mouse(release(column, row));
    }

    fn cells(row: &str, from: usize, to: usize) -> String {
        row.chars()
            .skip(from)
            .take(to - from + 1)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    fn inverted(buffer: &Buffer, x: u16, y: u16) -> bool {
        buffer[(x, y)]
            .style()
            .add_modifier
            .contains(Modifier::REVERSED)
    }

    fn filled_app() -> App {
        let mut app = test_app();
        for i in 0..40 {
            // Assistant entries render as consecutive rows with no blank separators.
            app.push(format!("line {i}"), Kind::Assistant);
        }
        app
    }

    #[test]
    fn dragging_selects_and_copies_the_visible_block() {
        let mut app = filled_app();
        let buffer = draw(&mut app, 80, 24);
        let text = rows(&buffer);
        let area = app.text_area;
        let (top, mid, bottom) = (area.top() + 1, area.top() + 2, area.top() + 3);

        app.on_mouse(down(area.left() + 2, top));
        app.on_mouse(drag_to(area.left() + 6, bottom));
        app.on_mouse(release(area.left() + 6, bottom));

        let copied: Vec<&str> = app.clipboard.last().unwrap().lines().collect();
        assert_eq!(copied.len(), 3);
        assert_eq!(copied[0], cells(&text[top as usize], 2, 6));
        assert_eq!(copied[1], cells(&text[mid as usize], 0, 78));
        assert_eq!(copied[2], cells(&text[bottom as usize], 0, 6));
        assert!(!copied[1].is_empty(), "middle row should carry text");
    }

    #[test]
    fn selection_is_highlighted_and_reported_in_the_footer() {
        let mut app = filled_app();
        draw(&mut app, 80, 24);
        let area = app.text_area;
        app.on_mouse(down(area.left() + 2, area.top() + 1));
        app.on_mouse(drag_to(area.left() + 6, area.top() + 3));
        app.on_mouse(release(area.left() + 6, area.top() + 3));

        let buffer = draw(&mut app, 80, 24);
        assert!(inverted(&buffer, area.left() + 3, area.top() + 1));
        assert!(inverted(&buffer, area.left() + 30, area.top() + 2));
        assert!(inverted(&buffer, area.left() + 3, area.top() + 3));
        assert!(!inverted(&buffer, area.left(), area.top() + 1));
        assert!(!inverted(&buffer, area.left() + 30, area.top() + 3));
        assert!(!inverted(&buffer, area.left() + 3, area.top()));
        assert!(!inverted(&buffer, area.left() + 3, area.top() + 4));
        let footer = rows(&buffer).join("\n");
        let expected = format!(
            "copied {} characters",
            app.clipboard.last().unwrap().chars().count()
        );
        assert!(footer.contains(&expected), "{footer}");
    }

    #[test]
    fn plain_click_copies_nothing_and_clears_the_selection() {
        let mut app = filled_app();
        draw(&mut app, 80, 24);
        let area = app.text_area;
        app.on_mouse(down(area.left() + 2, area.top() + 1));
        app.on_mouse(drag_to(area.left() + 9, area.top() + 2));
        app.on_mouse(release(area.left() + 9, area.top() + 2));
        assert_eq!(app.clipboard.history().len(), 1);
        assert!(app.selection.is_some());

        click(&mut app, area.left() + 40, area.top() + 5);
        assert_eq!(app.clipboard.history().len(), 1);
        assert!(app.selection.is_none());
    }

    #[test]
    fn double_click_selects_the_word_under_the_cursor() {
        let mut app = test_app();
        app.push("alpha beta", Kind::Tool);
        let buffer = draw(&mut app, 80, 24);
        let row = rows(&buffer)
            .iter()
            .position(|line| line.contains("# alpha beta"))
            .unwrap() as u16;

        click(&mut app, 4, row);
        assert!(app.clipboard.history().is_empty(), "single click copies");
        click(&mut app, 4, row);
        assert_eq!(app.clipboard.last(), Some("alpha"));
    }

    #[test]
    fn triple_click_selects_the_whole_row() {
        let mut app = test_app();
        app.push("alpha beta", Kind::Tool);
        let buffer = draw(&mut app, 80, 24);
        let row = rows(&buffer)
            .iter()
            .position(|line| line.contains("# alpha beta"))
            .unwrap() as u16;

        click(&mut app, 4, row);
        click(&mut app, 4, row);
        click(&mut app, 4, row);
        assert_eq!(app.clipboard.last(), Some("# alpha beta"));
    }

    #[test]
    fn ctrl_y_copies_the_selection_again() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut app = filled_app();
        draw(&mut app, 80, 24);
        let area = app.text_area;
        app.on_mouse(down(area.left(), area.top() + 1));
        app.on_mouse(drag_to(area.left() + 4, area.top() + 1));
        app.on_mouse(release(area.left() + 4, area.top() + 1));
        let once = app.clipboard.last().unwrap().to_string();
        assert!(!once.is_empty());

        app.handle_key(
            crossterm::event::KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
            &tx,
            &cancel,
        );
        assert_eq!(app.clipboard.history().len(), 2);
        assert_eq!(app.clipboard.history()[1], once);
    }

    #[test]
    fn ctrl_y_without_a_selection_says_so() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut app = filled_app();
        draw(&mut app, 80, 24);
        app.handle_key(
            crossterm::event::KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
            &tx,
            &cancel,
        );
        assert!(app.clipboard.history().is_empty());
        let buffer = draw(&mut app, 80, 24);
        assert!(rows(&buffer).join("\n").contains("nothing selected"));
    }

    #[test]
    fn scrolling_keeps_the_selection_on_the_same_text() {
        let mut app = filled_app();
        draw(&mut app, 80, 24);
        let area = app.text_area;
        app.on_mouse(down(area.left(), area.top() + 1));
        app.on_mouse(drag_to(area.left() + 20, area.top() + 3));
        app.on_mouse(release(area.left() + 20, area.top() + 3));
        let before = app.clipboard.last().unwrap().to_string();
        let rows_before = app.selection.unwrap().start.row;

        app.scroll_by(3);
        draw(&mut app, 80, 24);
        assert_eq!(app.selection.unwrap().start.row, rows_before + 3);
        app.copy_selection();
        assert_eq!(app.clipboard.history().last(), Some(&before));
    }

    #[test]
    fn streaming_keeps_the_selection_on_the_same_text() {
        let mut app = filled_app();
        draw(&mut app, 80, 24);
        let area = app.text_area;
        app.on_mouse(down(area.left(), area.top() + 2));
        app.on_mouse(drag_to(area.left() + 12, area.top() + 4));
        app.on_mouse(release(area.left() + 12, area.top() + 4));
        let before = app.clipboard.last().unwrap().to_string();
        assert!(!before.is_empty());

        app.scroll_by(4);
        app.on_event(AgentEvent::TextDelta(
            "streamed answer that spans several wrapped lines of text".into(),
        ));
        draw(&mut app, 80, 24);
        app.copy_selection();
        assert_eq!(app.clipboard.history().last(), Some(&before));
    }

    #[test]
    fn esc_clears_the_selection() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut app = filled_app();
        draw(&mut app, 80, 24);
        let area = app.text_area;
        app.on_mouse(down(area.left(), area.top() + 1));
        app.on_mouse(drag_to(area.left() + 8, area.top() + 2));
        app.on_mouse(release(area.left() + 8, area.top() + 2));
        assert!(app.selection.is_some());

        app.handle_key(
            crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &tx,
            &cancel,
        );
        assert!(app.selection.is_none());
    }

    #[test]
    fn clicks_outside_the_transcript_do_not_select() {
        let mut app = filled_app();
        draw(&mut app, 80, 24);
        let below = app.text_area.bottom();
        app.on_mouse(down(4, below));
        app.on_mouse(drag_to(30, below));
        app.on_mouse(release(30, below));
        assert!(app.selection.is_none());
        assert!(app.clipboard.history().is_empty());
    }
}
