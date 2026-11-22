use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

use crate::agent::{AgentCommand, AgentEvent};
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

fn kind_style(kind: Kind) -> Style {
    match kind {
        Kind::User => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        Kind::Assistant => Style::default(),
        Kind::Reasoning => Style::default().fg(Color::DarkGray),
        Kind::Tool => Style::default().fg(Color::Yellow),
        Kind::Error => Style::default().fg(Color::Red),
        Kind::Info => Style::default().fg(Color::Green),
    }
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            out.push(String::new());
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        for chunk in chars.chunks(width) {
            out.push(chunk.iter().collect());
        }
    }
    out
}

pub struct App {
    entries: Vec<Entry>,
    pending: Option<Entry>,
    input: String,
    cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    scroll_from_bottom: usize,
    status: String,
    loop_status: Option<String>,
    busy: bool,
    current_tool: Option<String>,
    quit: bool,
    follow: bool,
    last_rendered_lines: usize,
    cwd: PathBuf,
    model: String,
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
    ) -> Self {
        Self {
            entries: vec![Entry {
                text: "rho — type a prompt, /help for commands".to_string(),
                kind: Kind::Info,
            }],
            pending: None,
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_index: None,
            scroll_from_bottom: 0,
            status: "idle".to_string(),
            loop_status: None,
            busy: false,
            current_tool: None,
            quit: false,
            follow: true,
            last_rendered_lines: 0,
            cwd,
            model,
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

    fn push(&mut self, text: impl Into<String>, kind: Kind) {
        self.flush_pending();
        self.entries.push(Entry {
            text: text.into(),
            kind,
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
                if self.pending.as_ref().map(|p| p.text.ends_with('\n')) == Some(true) {
                    self.flush_pending();
                }
            }
            AgentEvent::TextDelta(t) => {
                self.busy = true;
                self.append_pending(&t, Kind::Assistant);
            }
            AgentEvent::ToolStarted { name, args } => {
                self.current_tool = Some(name.clone());
                self.push(format!("→ {name} {args}"), Kind::Tool);
            }
            AgentEvent::ToolFinished {
                name,
                is_error,
                output,
            } => {
                self.current_tool = None;
                let short: String = output.trim().chars().take(400).collect();
                let prefix = if is_error { "✗" } else { "✓" };
                let _ = name;
                self.push(format!("{prefix} {short}"), Kind::Tool);
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
        match key.code {
            KeyCode::Esc => {
                cancel.store(true, Ordering::SeqCst);
                self.push("interrupt requested", Kind::Info);
                false
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => true,
            KeyCode::PageUp => {
                self.follow = false;
                self.scroll_from_bottom += 10;
                false
            }
            KeyCode::PageDown => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(10);
                self.follow = self.scroll_from_bottom == 0;
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
                self.input.insert(self.cursor, c);
                self.cursor += 1;
                false
            }
            _ => false,
        }
    }

    fn update_status(&mut self) {
        if let Some(tool) = &self.current_tool {
            self.status = format!("running {tool}…");
        } else if self.busy && self.status == "idle" {
            self.status = "thinking…".to_string();
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
        self.status = "thinking…".to_string();
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
                    self.status = "looping…".to_string();
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
                self.status = "compacting…".to_string();
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
                        format!("unknown model: {arg} (available: {})", self.known_models.join(", ")),
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
        let chunks = if self.loop_status.is_some() {
            let split = Layout::vertical([
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(area);
            (Some(split[1]), split[0], split[2])
        } else {
            let split = Layout::vertical([Constraint::Min(2), Constraint::Length(3)]).split(area);
            (None, split[0], split[1])
        };

        let width = chunks.1.width as usize;
        let mut lines: Vec<Line> = Vec::new();
        let append_entry = |entry: &Entry, lines: &mut Vec<Line>| {
            for line in wrap_text(&entry.text, width.saturating_sub(2)) {
                lines.push(Line::from(Span::styled(
                    format!("{}{line}", kind_prefix(entry.kind)),
                    kind_style(entry.kind),
                )));
            }
        };
        for entry in &self.entries {
            append_entry(entry, &mut lines);
        }
        if let Some(pending) = &self.pending {
            append_entry(pending, &mut lines);
        }
        let new_lines = lines.len().saturating_sub(self.last_rendered_lines);
        if !self.follow && new_lines > 0 {
            self.scroll_from_bottom += new_lines;
        }
        self.last_rendered_lines = lines.len();
        let visible = chunks.1.height as usize;
        let scroll = lines
            .len()
            .saturating_sub(visible + self.scroll_from_bottom);
        let paragraph = Paragraph::new(lines).scroll((scroll as u16, 0));
        frame.render_widget(paragraph, chunks.1);

        if let Some(bar) = chunks.0 {
            let status = self.loop_status.clone().unwrap_or_default();
            frame.render_widget(
                Paragraph::new(Span::styled(status, Style::default().fg(Color::Green))),
                bar,
            );
        }

        let input_block = Block::default().borders(Borders::ALL).title(format!(
            " {} · ~{} tok · {} ",
            self.model, self.est_tokens, self.status
        ));
        let inner = input_block.inner(chunks.2);
        frame.render_widget(input_block, chunks.2);
        let prompt = Paragraph::new(Span::styled(format!("❯ {}", self.input), Style::default()));
        frame.render_widget(prompt, inner);
        frame.set_cursor_position((
            inner
                .x
                .saturating_add(2 + self.cursor.min(inner.width.saturating_sub(3) as usize) as u16),
            inner.y,
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
    let result = run_inner(&mut terminal, agent, cancel, &mut events, opts).await;
    ratatui::restore();
    result
}

pub struct TuiOptions {
    pub cwd: PathBuf,
    pub model: String,
    pub known_models: Vec<String>,
    pub templates: Vec<Template>,
}

async fn run_inner(
    terminal: &mut ratatui::DefaultTerminal,
    agent: mpsc::UnboundedSender<AgentCommand>,
    cancel: Arc<AtomicBool>,
    events: &mut broadcast::Receiver<AgentEvent>,
    opts: TuiOptions,
) -> anyhow::Result<()> {
    let mut app = App::new(opts.cwd, opts.model, opts.known_models, opts.templates);
    let mut keys = crossterm::event::EventStream::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(80));
    let mut ticks = 0u64;
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
                if let Some(Ok(Event::Key(key))) = maybe_key {
                    if app.handle_key(key, &agent, &cancel) {
                        break;
                    }
                }
            }
            _ = ticker.tick() => {
                ticks += 1;
                if app.busy && ticks.is_multiple_of(5) {
                    app.status.push('.');
                    if app.status.len() > app.status.rfind('…').map(|i| i + 4).unwrap_or(0) + 3 {
                        let base: String = app.status.chars().take_while(|c| *c != '.').collect();
                        app.status = base;
                    }
                }
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
            args: "{cmd}".into(),
        });
        app.on_event(AgentEvent::ToolFinished {
            name: "bash".into(),
            is_error: false,
            output: "ok".into(),
        });
        assert!(app.entries.iter().any(|e| e.text.contains("→ bash")));
        assert!(app.entries.iter().any(|e| e.text.contains("✓ ok")));
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
}

#[cfg(test)]
mod wrap_tests {
    use super::wrap_text;

    #[test]
    fn wraps_long_lines() {
        let w = wrap_text("abcdefgh", 3);
        assert_eq!(w, vec!["abc", "def", "gh"]);
        assert_eq!(wrap_text("a\nb", 10), vec!["a", "b"]);
        assert_eq!(wrap_text("", 10), Vec::<String>::new());
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
        assert!(screen.iter().any(|l| l.contains("line 45")), "after PgUp: {screen:#?}");
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
        assert!(screen.iter().any(|l| l.contains("line 59")), "after PageDown");
    }


    #[test]
    fn scrolled_view_survives_streaming_output() {
        let mut app = App::new(PathBuf::from("/tmp"), "m".into(), vec!["m".into()], vec![]);
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
        assert!(screen.iter().any(|l| l.contains("more streaming text")), "{screen:#?}");
        assert!(screen.iter().any(|l| l.contains("bash")));
    }

    #[test]
    fn long_lines_wrap_within_width() {
        let mut app = App::new(PathBuf::from("/tmp"), "m".into(), vec!["m".into()], vec![]);
        app.push("x".repeat(500), Kind::Assistant);
        let screen = render(&mut app, 80, 24);
        for row in &screen {
            assert!(row.trim_end().chars().count() <= 80, "row too wide: {row:?}");
        }
        assert!(screen.iter().any(|l| l.starts_with('x')));
    }
}
