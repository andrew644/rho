use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    DisableBracketedPaste, EnableBracketedPaste, MouseEventKind,
};
use futures_util::StreamExt;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use ratatui::Frame;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{broadcast, mpsc};

use crate::agent::{AgentCommand, AgentEvent};
use crate::config::ThemeName;
use crate::loops::parse_loop_spec;
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
    Name(String),
    Model(String),
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
    if let Some(rest) = input.strip_prefix('/') {
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
            "name" => Command::Name(args.to_string()),
            "model" => Command::Model(args.to_string()),
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

#[derive(Clone)]
struct Entry {
    text: String,
    kind: Kind,
    output: Option<String>,
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
    let src_lines: Vec<&str> = if text.is_empty() { vec![""] } else { text.lines().collect() };
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

const BUILTIN_COMMANDS: [&str; 11] = [
    "compact", "exit", "help", "loop", "model", "name", "new", "quit", "resume", "session",
    "stop",
];

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

fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = vec![String::new()];
    for word in text.split_whitespace() {
        let cur = lines.last().unwrap().chars().count();
        let sep = usize::from(cur > 0);
        if cur + sep + word.chars().count() <= width {
            if sep > 0 {
                lines.last_mut().unwrap().push(' ');
            }
            lines.last_mut().unwrap().push_str(word);
        } else {
            lines.push(String::new());
            let chars: Vec<char> = word.chars().collect();
            let mut start = 0;
            while start + width < chars.len() {
                lines.last_mut().unwrap().extend(&chars[start..start + width]);
                lines.push(String::new());
                start += width;
            }
            lines.last_mut().unwrap().extend(&chars[start..]);
        }
    }
    if lines.len() > 1 && lines[0].is_empty() {
        lines.remove(0);
    } else if lines.len() == 1 && lines[0].is_empty() {
        lines.clear();
    }
    lines
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
    cwd: PathBuf,
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
}

impl App {
    pub fn new(
        cwd: PathBuf,
        model: String,
        known_models: Vec<String>,
        templates: Vec<Template>,
        context_window: u32,
        theme: Theme,
    ) -> Self {
        Self {
            entries: vec![Entry {
                text: "rho — type a prompt, /help for commands".to_string(),
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
            cwd,
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
            if self.input == format!("/{}", m[i]) {
                let idx = if back {
                    (i + m.len() - 1) % m.len()
                } else {
                    (i + 1) % m.len()
                };
                self.input = format!("/{}", m[idx]);
                self.cursor = self.input.chars().count();
                self.completion = Some((m, idx));
                return;
            }
        }
        let nchars = self.input.chars().count();
        if !self.input.starts_with('/') || self.cursor != nchars {
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
            self.input = format!("/{} ", matches[0]);
            self.completion = None;
        } else {
            self.input = format!("/{}", matches[0]);
            self.completion = Some((matches, 0));
        }
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
            AgentEvent::AssistantDone { text } => {
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
            }
            AgentEvent::LoopFinished { iterations, reason } => {
                self.loop_status = None;
                self.push(
                    format!("loop finished: {reason} after {iterations} iteration(s)"),
                    Kind::Info,
                );
            }
            AgentEvent::Idle => {
                self.busy = false;
                self.status = "idle".to_string();
                self.current_tool = None;
            }
            AgentEvent::Info(message) => {
                self.push(message, Kind::Info);
            }
            AgentEvent::ContextUsage {
                est_tokens,
                message_count,
                session_path,
                session_name,
            } => {
                self.est_tokens = est_tokens;
                self.message_count = message_count;
                self.session_path = session_path;
                self.session_name = session_name;
            }
            AgentEvent::Error(message) => {
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
                cancel.store(true, Ordering::SeqCst);
                self.push("interrupt requested", Kind::Info);
                false
            }
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.expand_tools = !self.expand_tools;
                false
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.show_reasoning = !self.show_reasoning;
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

    fn input_layout(&self, width: usize, rows: usize) -> (Vec<Line<'static>>, usize, usize) {
        let rows = rows.max(1);
        let before: String = self.input.chars().take(self.cursor).collect();
        let cur_line = before.matches('\n').count();
        let cur_col = before
            .chars()
            .rev()
            .take_while(|c| *c != '\n')
            .count();
        let lines: Vec<&str> = self.input.split('\n').collect();
        let top = cur_line.saturating_sub(rows - 1);
        let bottom = (top + rows).min(lines.len());
        let hscroll = cur_col.saturating_sub(width.saturating_sub(4));
        let view = (top..bottom)
            .map(|i| {
                let prompt = if i == 0 { "❯ " } else { "  " };
                let text: String = lines[i].chars().skip(hscroll).collect();
                Line::from(Span::raw(format!("{prompt}{text}")))
            })
            .collect();
        (view, cur_line - top, cur_col.saturating_sub(hscroll))
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
        let elapsed = self
            .turn_start
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
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
        let cwd_name = self
            .cwd
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.cwd.display().to_string());
        let ident = match &self.session_name {
            Some(name) if !name.is_empty() => format!("{name} · {cwd_name}"),
            _ => cwd_name,
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
        let _ = agent.send(AgentCommand::Prompt { text });
    }

    fn show_help(&mut self) {
        self.push(
            "commands: /loop [forever|N|10m|until \"cond\"] <task>, /compact [instructions], /new, /resume [n], /session, /name <text>, /model [id], /stop, /help, /quit",
            Kind::Info,
        );
        self.push("!cmd runs a shell command and shares its output with the model; !!cmd runs it without sharing", Kind::Info);
        let help = templates::render_help(&self.templates);
        if !help.is_empty() {
            self.push("prompt templates:", Kind::Info);
            for line in help.lines() {
                self.push(line, Kind::Info);
            }
        }
    }

    fn show_sessions(&mut self) {
        self.session_list = session::list_sessions(&self.cwd, 10);
        if self.session_list.is_empty() {
            self.push("no saved sessions for this directory", Kind::Info);
            return;
        }
        self.push("recent sessions (use /resume <n> to open):", Kind::Info);
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
                let _ = agent.send(AgentCommand::Interrupt);
                self.push("interrupt requested", Kind::Info);
            }
            Some(InputAction::Command(Command::Loop(args))) => match parse_loop_spec(&args) {
                Ok(spec) => {
                    self.history.push(format!("/loop {args}"));
                    self.busy = true;
                    self.status = "looping".to_string();
                    let _ = agent.send(AgentCommand::Loop { spec });
                }
                Err(e) => self.push(e, Kind::Error),
            },
            Some(InputAction::Command(Command::Help)) => self.show_help(),
            Some(InputAction::Command(Command::Compact(instructions))) => {
                self.history.push(match &instructions {
                    Some(i) => format!("/compact {i}"),
                    None => "/compact".to_string(),
                });
                self.busy = true;
                self.status = "compacting".to_string();
                let _ = agent.send(AgentCommand::Compact { instructions });
            }
            Some(InputAction::Command(Command::New)) => {
                let _ = agent.send(AgentCommand::NewSession);
                self.entries.clear();
                self.pending = None;
                self.push("new session started", Kind::Info);
            }
            Some(InputAction::Command(Command::Resume(args))) => {
                if args.is_empty() {
                    self.show_sessions();
                } else {
                    match args.parse::<usize>() {
                        Ok(n) if n >= 1 && n <= self.session_list.len() => {
                            let path = self.session_list[n - 1].path.clone();
                            let _ = agent.send(AgentCommand::Resume { path });
                            self.entries.clear();
                            self.push(format!("resumed session {}", n), Kind::Info);
                        }
                        _ => self.push("usage: /resume to list, /resume <n> to open", Kind::Error),
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
            Some(InputAction::Command(Command::Name(name))) => {
                let _ = agent.send(AgentCommand::SetName { name: name.clone() });
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
                    let _ = agent.send(AgentCommand::SetModel { id: arg });
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
                    self.push(format!("unknown template: /{name}"), Kind::Error);
                    return false;
                };
                let parsed = templates::tokenize_args(&args);
                let expanded = templates::expand_template(&template.body, &parsed);
                let display = if args.is_empty() {
                    format!("/{name}")
                } else {
                    format!("/{name} {args}")
                };
                self.submit_prompt(&display, expanded, agent);
            }
            Some(InputAction::Command(Command::Shell { command, to_model })) => {
                self.history.push(command.clone());
                let prefix = if to_model { "!" } else { "!!" };
                self.push(format!("{prefix} {command}"), Kind::User);
                let _ = agent.send(AgentCommand::Shell { command, to_model });
            }
            Some(InputAction::Command(Command::Unknown(name))) => {
                self.push(format!("unknown command: /{name}"), Kind::Error);
            }
            None => {}
        }
        false
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.update_status();
        let input_height = if self.input.contains('\n') { 5 } else { 3 };
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
        if !self.follow && new_lines > 0 {
            self.scroll_from_bottom += new_lines;
        }
        self.last_rendered_lines = lines.len();
        let visible = text_area.height as usize;
        let scroll = lines
            .len()
            .saturating_sub(visible + self.scroll_from_bottom);
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
        let prompt_lines = self.input_layout(inner.width as usize, inner.height as usize);
        frame.render_widget(Paragraph::new(prompt_lines.0), inner);
        frame.set_cursor_position((
            inner.x.saturating_add(2 + prompt_lines.2 as u16),
            inner.y.saturating_add(prompt_lines.1 as u16),
        ));
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
    pub cwd: PathBuf,
    pub model: String,
    pub known_models: Vec<String>,
    pub templates: Vec<Template>,
    pub context_window: u32,
    pub theme: ThemeName,
}

async fn run_inner(
    terminal: &mut ratatui::DefaultTerminal,
    agent: mpsc::UnboundedSender<AgentCommand>,
    cancel: Arc<AtomicBool>,
    events: &mut broadcast::Receiver<AgentEvent>,
    opts: TuiOptions,
) -> anyhow::Result<()> {
    let mut app = App::new(
        opts.cwd,
        opts.model,
        opts.known_models,
        opts.templates,
        opts.context_window,
        Theme::new(opts.theme),
    );
    let mut keys = crossterm::event::EventStream::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(80));
    loop {
        terminal.draw(|frame| app.draw(frame, frame.area()))?;
        tokio::select! {
            maybe_event = events.recv() => {
                match maybe_event {
                    Ok(event) => app.on_event(event),
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            maybe_key = keys.next() => {
                match maybe_key {
                    Some(Ok(Event::Key(key))) => {
                        if app.handle_key(key, &agent, &cancel) {
                            break;
                        }
                    }
                    Some(Ok(Event::Mouse(mouse))) => match mouse.kind {
                        MouseEventKind::ScrollUp => app.scroll_by(3),
                        MouseEventKind::ScrollDown => app.scroll_by(-3),
                        _ => {}
                    },
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
            classify_input("/quit", &tpl),
            Some(InputAction::Command(Command::Quit))
        );
        assert_eq!(
            classify_input("/exit", &tpl),
            Some(InputAction::Command(Command::Quit))
        );
        assert_eq!(
            classify_input("/loop 5 build", &tpl),
            Some(InputAction::Command(Command::Loop("5 build".into())))
        );
        assert_eq!(
            classify_input("/wat", &tpl),
            Some(InputAction::Command(Command::Unknown("wat".into())))
        );
    }

    #[test]
    fn classify_templates_and_builtins() {
        let tpl = vec!["review".to_string()];
        assert_eq!(
            classify_input("/review concurrency", &tpl),
            Some(InputAction::Command(Command::Template {
                name: "review".into(),
                args: "concurrency".into()
            }))
        );
        assert_eq!(
            classify_input("/quit", &tpl),
            Some(InputAction::Command(Command::Quit))
        );
        assert_eq!(
            classify_input("/compact focus on tests", &tpl),
            Some(InputAction::Command(Command::Compact(Some(
                "focus on tests".into()
            ))))
        );
        assert_eq!(
            classify_input("/compact", &tpl),
            Some(InputAction::Command(Command::Compact(None)))
        );
        assert_eq!(
            classify_input("/resume 2", &tpl),
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
        );
        app.on_submit("/greet world", &tx);
        match rx.try_recv().unwrap() {
            AgentCommand::Prompt { text } => assert_eq!(text, "hello world"),
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(app.entries.iter().any(|e| e.text == "/greet world"));
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
        assert_eq!(app.entries.iter().filter(|e| e.kind == Kind::Tool).count(), 1);
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
            output: (1..=50).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n"),
        });
        let entry = app.entries.iter().find(|e| e.kind == Kind::Tool).unwrap();
        let lines = entry_lines(entry, 80, false, &Theme::new(ThemeName::Dark));
        assert_eq!(lines.len(), TOOL_OUTPUT_LINES + 2, "header + 20 lines + note");
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(texts[1], "  line 1");
        assert!(texts[21].contains("+30 lines (Ctrl+O to expand)"), "{texts:#?}");
        let expanded = entry_lines(entry, 80, true, &Theme::new(ThemeName::Dark));
        assert_eq!(expanded.len(), 51);
    }

    #[test]
    fn quit_command_exits() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut app = test_app();
        let key = |code| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
        for c in "/quit".chars() {
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
            AgentCommand::Prompt { text } => assert_eq!(text, "line one\nline two"),
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
    fn tab_completes_unique_prefix_with_space() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut app = test_app();
        let key = |code| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
        for c in "/res".chars() {
            app.handle_key(key(KeyCode::Char(c)), &tx, &cancel);
        }
        app.handle_key(key(KeyCode::Tab), &tx, &cancel);
        assert_eq!(app.input, "/resume ");
        assert_eq!(app.cursor, 8);
    }

    #[test]
    fn tab_cycles_when_ambiguous_and_backtab_reverses() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut app = test_app();
        let key = |code| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
        app.handle_key(key(KeyCode::Char('/')), &tx, &cancel);
        app.handle_key(key(KeyCode::Tab), &tx, &cancel);
        let first = app.input.clone();
        assert!(first.starts_with('/') && !first.contains(' '), "{first}");
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
        for c in "/gr".chars() {
            app.handle_key(key(KeyCode::Char(c)), &tx, &cancel);
        }
        app.handle_key(key(KeyCode::Tab), &tx, &cancel);
        assert_eq!(app.input, "/greet ");
    }

    #[test]
    fn editing_resets_completion_cycle() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut app = test_app();
        let key = |code| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
        app.handle_key(key(KeyCode::Char('/')), &tx, &cancel);
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
        let (view, row, col) = app.input_layout(40, 2);
        let texts: Vec<String> = view
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(texts, vec!["  l3", "  l4"]);
        assert_eq!((row, col), (1, 2));
        app.cursor = 3;
        let (view, row, _) = app.input_layout(40, 2);
        assert_eq!(row, 1);
        assert_eq!(view[0].spans[0].content, "❯ l1");
    }
}

#[cfg(test)]
mod wrap_tests {
    use super::{entry_lines, wrap_words, Entry, Kind, Theme};
    use crate::config::ThemeName;
    use std::path::PathBuf;

    #[test]
    fn wraps_on_word_boundaries() {
        assert_eq!(wrap_words("acknowledge this", 12), vec!["acknowledge", "this"]);
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
        let mut app = super::App::new(PathBuf::from("/tmp"), "m".into(), vec![], vec![], 128_000, Theme::new(ThemeName::Dark));
        app.on_event(super::AgentEvent::ReasoningDelta("line one\n".into()));
        app.on_event(super::AgentEvent::ReasoningDelta("line two".into()));
        let pending = app.pending.as_ref().unwrap();
        assert_eq!(pending.text, "line one\nline two");
        assert_eq!(entry_lines(pending, 40, false, &Theme::new(ThemeName::Dark)).len(), 2);
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
        assert_eq!(
            lines,
            vec!["before", "    fn main() {}", "after"]
        );
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
        assert_eq!(
            lines,
            vec!["    code", "word word", "word word", "word"]
        );
    }
}

#[cfg(test)]
mod draw_tests {
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
        )
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
        for c in "/quit".chars() {
            app.handle_key(key(KeyCode::Char(c)), &tx, &cancel);
        }
        assert!(app.handle_key(key(KeyCode::Enter), &tx, &cancel));
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
        let mut app = App::new(PathBuf::from("/tmp"), "m".into(), vec!["m".into()], vec![], 128_000, Theme::new(ThemeName::Dark));
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
        let mut app = App::new(PathBuf::from("/tmp"), "m".into(), vec!["m".into()], vec![], 128_000, Theme::new(ThemeName::Dark));
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
        let mut app = App::new(PathBuf::from("/tmp"), "m".into(), vec!["m".into()], vec![], 128_000, Theme::new(ThemeName::Dark));
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
        let mut app = App::new(PathBuf::from("/tmp"), "m".into(), vec!["m".into()], vec![], 128_000, Theme::new(ThemeName::Dark));
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
        let mut app = App::new(PathBuf::from("/tmp"), "m".into(), vec!["m".into()], vec![], 128_000, Theme::new(ThemeName::Dark));
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
    fn footer_shows_session_name_and_cwd() {
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
        let mut app = App::new(PathBuf::from("/tmp"), "m".into(), vec!["m".into()], vec![], 128_000, Theme::new(ThemeName::Dark));
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
        let mut app = App::new(PathBuf::from("/tmp"), "m".into(), vec!["m".into()], vec![], 128_000, Theme::new(ThemeName::Dark));
        app.push("hello there".to_string(), Kind::User);
        app.push("a response".to_string(), Kind::Assistant);
        let screen = render(&mut app, 80, 24);
        let user_row = screen.iter().position(|l| l.contains("hello there")).unwrap();
        let resp_row = screen.iter().position(|l| l.contains("a response")).unwrap();
        assert_eq!(resp_row, user_row + 2, "blank line between turns: {screen:#?}");
        assert_eq!(screen[user_row + 1].trim(), "");
    }

    #[test]
    fn ctrl_r_toggles_reasoning_visibility() {
        let mut app = App::new(PathBuf::from("/tmp"), "m".into(), vec!["m".into()], vec![], 128_000, Theme::new(ThemeName::Dark));
        app.push("because reasons".to_string(), Kind::Reasoning);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let key = |code| {
            crossterm::event::KeyEvent::new(code, KeyModifiers::CONTROL)
        };
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
