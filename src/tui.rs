use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use futures_util::StreamExt;
use tokio::sync::{broadcast, mpsc};

use crate::agent::{AgentCommand, AgentEvent};
use crate::loops::parse_loop_spec;

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
    Unknown(String),
}

pub fn classify_input(input: &str) -> Option<InputAction> {
    let input = input.trim();
    if input.is_empty() {
        return None;
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
        Kind::User => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
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
}

impl App {
    pub fn new() -> Self {
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
        }
    }

    fn push(&mut self, text: impl Into<String>, kind: Kind) {
        self.flush_pending();
        self.entries.push(Entry { text: text.into(), kind });
        self.scroll_from_bottom = 0;
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
                self.pending = Some(Entry { text: text.to_string(), kind });
            }
        }
        self.scroll_from_bottom = 0;
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
            AgentEvent::ToolFinished { name, is_error, output } => {
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
                    *status = format!("{} iter {iterations}", status.split(" iter ").next().unwrap_or("loop"));
                }
            }
            AgentEvent::LoopFinished { iterations, reason } => {
                self.loop_status = None;
                self.push(format!("loop finished: {reason} after {iterations} iteration(s)"), Kind::Info);
            }
            AgentEvent::Idle => {
                self.busy = false;
                self.status = "idle".to_string();
                self.current_tool = None;
            }
            AgentEvent::Error(message) => {
                self.push(message, Kind::Error);
            }
        }
    }

    pub fn handle_key(&mut self, key: event::KeyEvent, agent: &mpsc::UnboundedSender<AgentCommand>) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        match key.code {
            KeyCode::Esc => {
                let _ = agent.send(AgentCommand::Interrupt);
                self.push("interrupt requested", Kind::Info);
                false
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => true,
            KeyCode::PageUp => {
                self.scroll_from_bottom += 10;
                false
            }
            KeyCode::PageDown => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(10);
                false
            }
            KeyCode::End => {
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
                match classify_input(&input) {
                    Some(InputAction::Submit(text)) => {
                        self.history.push(text.clone());
                        self.push(text.clone(), Kind::User);
                        self.busy = true;
                        self.status = "thinking…".to_string();
                        let _ = agent.send(AgentCommand::Prompt { text });
                    }
                    Some(InputAction::Command(Command::Quit)) => return true,
                    Some(InputAction::Command(Command::Stop)) => {
                        let _ = agent.send(AgentCommand::Interrupt);
                        self.push("interrupt requested", Kind::Info);
                    }
                    Some(InputAction::Command(Command::Loop(args))) => {
                        match parse_loop_spec(&args) {
                            Ok(spec) => {
                                self.history.push(format!("/loop {args}"));
                                self.busy = true;
                                self.status = "looping…".to_string();
                                let _ = agent.send(AgentCommand::Loop { spec });
                            }
                            Err(e) => self.push(e, Kind::Error),
                        }
                    }
                    Some(InputAction::Command(Command::Help)) => {
                        self.push(
                            "commands: /loop [forever|N|10m|until \"cond\"] <task>, /stop, /help, /quit",
                            Kind::Info,
                        );
                    }
                    Some(InputAction::Command(Command::Unknown(name))) => {
                        self.push(format!("unknown command: /{name}"), Kind::Error);
                    }
                    None => {}
                }
                false
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

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.update_status();
        let chunks = if self.loop_status.is_some() {
            let split = Layout::vertical([
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(2),
            ])
            .split(area);
            (Some(split[1]), split[0], split[2])
        } else {
            let split =
                Layout::vertical([Constraint::Min(2), Constraint::Length(2)]).split(area);
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
        let visible = chunks.1.height as usize;
        let scroll = lines.len().saturating_sub(visible + self.scroll_from_bottom);
        let paragraph = Paragraph::new(lines).scroll((0, scroll as u16));
        frame.render_widget(paragraph, chunks.1);

        if let Some(bar) = chunks.0 {
            let status = self.loop_status.clone().unwrap_or_default();
            frame.render_widget(
                Paragraph::new(Span::styled(status, Style::default().fg(Color::Green))),
                bar,
            );
        }

        let input_block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", self.status));
        let inner = input_block.inner(chunks.2);
        frame.render_widget(input_block, chunks.2);
        let prompt = Paragraph::new(Span::styled(
            format!("❯ {}", self.input),
            Style::default(),
        ));
        frame.render_widget(prompt, inner);
        frame.set_cursor_position((
            inner.x.saturating_add(2 + self.cursor.min(inner.width.saturating_sub(3) as usize) as u16),
            inner.y,
        ));
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }
}

pub async fn run(
    agent: mpsc::UnboundedSender<AgentCommand>,
    mut events: broadcast::Receiver<AgentEvent>,
) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let result = run_inner(&mut terminal, agent, &mut events).await;
    ratatui::restore();
    result
}

async fn run_inner(
    terminal: &mut ratatui::DefaultTerminal,
    agent: mpsc::UnboundedSender<AgentCommand>,
    events: &mut broadcast::Receiver<AgentEvent>,
) -> anyhow::Result<()> {
    let mut app = App::new();
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
                    if app.handle_key(key, &agent) {
                        break;
                    }
                }
            }
            _ = ticker.tick() => {
                ticks += 1;
                if app.busy && ticks % 5 == 0 {
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

    #[test]
    fn classify_inputs() {
        assert_eq!(classify_input("hello"), Some(InputAction::Submit("hello".into())));
        assert_eq!(classify_input("  "), None);
        assert_eq!(classify_input("/quit"), Some(InputAction::Command(Command::Quit)));
        assert_eq!(classify_input("/exit"), Some(InputAction::Command(Command::Quit)));
        assert_eq!(
            classify_input("/loop 5 build"),
            Some(InputAction::Command(Command::Loop("5 build".into())))
        );
        assert_eq!(
            classify_input("/wat"),
            Some(InputAction::Command(Command::Unknown("wat".into())))
        );
    }

    #[test]
    fn app_tracks_streaming_and_done() {
        let mut app = App::new();
        app.on_event(AgentEvent::TextDelta("Hel".into()));
        app.on_event(AgentEvent::TextDelta("lo".into()));
        assert_eq!(app.pending.as_ref().unwrap().text, "Hello");
        app.on_event(AgentEvent::AssistantDone { text: "Hello".into() });
        assert!(app.pending.is_none());
        assert_eq!(app.entries.iter().filter(|e| e.text == "Hello").count(), 1);
        assert!(!app.busy);
    }

    #[test]
    fn assistant_done_without_streaming_pushes_once() {
        let mut app = App::new();
        app.on_event(AgentEvent::AssistantDone { text: "Direct".into() });
        assert_eq!(app.entries.iter().filter(|e| e.text == "Direct").count(), 1);
    }

    #[test]
    fn tool_events_render() {
        let mut app = App::new();
        app.on_event(AgentEvent::ToolStarted { name: "bash".into(), args: "{cmd}".into() });
        app.on_event(AgentEvent::ToolFinished { name: "bash".into(), is_error: false, output: "ok".into() });
        assert!(app.entries.iter().any(|e| e.text.contains("→ bash")));
        assert!(app.entries.iter().any(|e| e.text.contains("✓ ok")));
    }

    #[test]
    fn editing_moves_cursor_and_inserts() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new();
        let key = |code| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
        app.handle_key(key(KeyCode::Char('b')), &tx);
        app.handle_key(key(KeyCode::Char('c')), &tx);
        app.handle_key(key(KeyCode::Left), &tx);
        app.handle_key(key(KeyCode::Char('a')), &tx);
        assert_eq!(app.input, "bac");
        assert_eq!(app.cursor, 2);
        app.handle_key(key(KeyCode::Backspace), &tx);
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

    #[test]
    fn draws_entries_and_pending() {
        let mut app = App::new();
        app.push("first user message", Kind::User);
        app.on_event(AgentEvent::TextDelta("streaming answer".into()));
        let mut backend = TestBackend::new(80, 12);
        let mut term = ratatui::Terminal::new(backend).unwrap();
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
}
