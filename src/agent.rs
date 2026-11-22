use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::broadcast;
use tokio::sync::mpsc::UnboundedSender;

use crate::client::{Client, StreamEvent};
use crate::compaction::{
    build_summary_request, compacted_history, estimate_tokens, find_cut_point, should_compact,
};
use crate::config::Config;
use crate::loops::{LoopMode, LoopSpec};
use crate::prompt::build_system_prompt;
use crate::session::{self, SessionHeader};
use crate::tools::{all_tools, Tool, ToolResult};
use crate::types::Message;

#[derive(Debug)]
pub enum AgentCommand {
    Prompt { text: String },
    Loop { spec: LoopSpec },
    Interrupt,
    Compact { instructions: Option<String> },
    Shell { command: String, to_model: bool },
    NewSession,
    Resume { path: PathBuf },
    SetName { name: String },
    SetModel { id: String },
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    ReasoningDelta(String),
    TextDelta(String),
    ToolStarted {
        name: String,
        args: String,
    },
    ToolFinished {
        name: String,
        is_error: bool,
        output: String,
    },
    AssistantDone {
        text: String,
    },
    LoopStarted {
        description: String,
    },
    LoopIterationDone {
        iterations: usize,
    },
    LoopFinished {
        iterations: usize,
        reason: String,
    },
    Info(String),
    ContextUsage {
        est_tokens: usize,
        message_count: usize,
        session_path: Option<String>,
        session_name: Option<String>,
    },
    Idle,
    Error(String),
}

pub struct Agent {
    client: Client,
    config: Config,
    tools: Vec<Arc<dyn Tool>>,
    history: Vec<Message>,
    cancel: Arc<AtomicBool>,
    cwd: PathBuf,
    model: String,
    session_path: Option<PathBuf>,
    session_header: Option<SessionHeader>,
    truncation_recovery_used: bool,
}

impl Agent {
    pub fn spawn(
        config: Config,
        cwd: PathBuf,
        events: broadcast::Sender<AgentEvent>,
    ) -> (UnboundedSender<AgentCommand>, Arc<AtomicBool>) {
        Self::spawn_resumed(config, cwd, events, None)
    }

    pub fn spawn_resumed(
        config: Config,
        cwd: PathBuf,
        events: broadcast::Sender<AgentEvent>,
        resumed: Option<PathBuf>,
    ) -> (UnboundedSender<AgentCommand>, Arc<AtomicBool>) {
        let cancel = Arc::new(AtomicBool::new(false));
        let system = Message::system(build_system_prompt(&cwd));
        let (history, session_path, session_header) = match resumed {
            Some(path) => match session::load_session(&path) {
                Ok((header, messages)) => {
                    let mut h = vec![system.clone()];
                    h.extend(messages);
                    (h, Some(path), Some(header))
                }
                Err(e) => {
                    let _ =
                        events.send(AgentEvent::Error(format!("failed to resume session: {e}")));
                    (vec![system], None, None)
                }
            },
            None => (vec![system], None, None),
        };
        let mut agent = Agent {
            client: Client::new(&config.provider.base_url, &config.provider.api_key),
            model: config.model.id.clone(),
            config,
            tools: all_tools(cwd.clone()).into_iter().map(Arc::from).collect(),
            history,
            cancel: cancel.clone(),
            cwd,
            truncation_recovery_used: false,
            session_path,
            session_header,
        };
        if agent.session_path.is_none() {
            agent.new_session_file();
        }
        agent.persist(&events);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentCommand>();
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    AgentCommand::Prompt { text } => {
                        agent.cancel.store(false, Ordering::SeqCst);
                        agent.run_turn(&text, &events).await;
                    }
                    AgentCommand::Loop { spec } => {
                        agent.cancel.store(false, Ordering::SeqCst);
                        agent.run_loop(&spec, &events).await;
                    }
                    AgentCommand::Interrupt => {
                        agent.cancel.store(true, Ordering::SeqCst);
                    }
                    AgentCommand::Compact { instructions } => {
                        agent.cancel.store(false, Ordering::SeqCst);
                        agent
                            .compact_now(instructions.as_deref(), &events, true)
                            .await;
                    }
                    AgentCommand::Shell { command, to_model } => {
                        agent.run_shell(&command, to_model, &events).await;
                    }
                    AgentCommand::NewSession => {
                        agent.history = vec![Message::system(build_system_prompt(&agent.cwd))];
                        agent.new_session_file();
                    }
                    AgentCommand::Resume { path } => match session::load_session(&path) {
                        Ok((header, messages)) => {
                            let mut h = vec![Message::system(build_system_prompt(&agent.cwd))];
                            h.extend(messages);
                            agent.history = h;
                            agent.session_header = Some(header);
                            agent.session_path = Some(path);
                            let _ = events.send(AgentEvent::Info("session resumed".to_string()));
                        }
                        Err(e) => {
                            let _ = events
                                .send(AgentEvent::Error(format!("failed to resume session: {e}")));
                        }
                    },
                    AgentCommand::SetName { name } => {
                        if let Some(header) = &mut agent.session_header {
                            header.name = if name.is_empty() { None } else { Some(name) };
                            let _ = events.send(AgentEvent::Info(format!(
                                "session name: {}",
                                agent.session_name()
                            )));
                        }
                    }
                    AgentCommand::SetModel { id } => {
                        let known = agent.known_models();
                        if !known.iter().any(|m| m == &id) {
                            let _ = events.send(AgentEvent::Error(format!(
                                "unknown model: {id} (available: {})",
                                known.join(", ")
                            )));
                        } else {
                            agent.model = id.clone();
                            let _ = events.send(AgentEvent::Info(format!("model: {id}")));
                        }
                    }
                }
                agent.persist(&events);
                let _ = events.send(AgentEvent::Idle);
            }
        });
        (tx, cancel)
    }

    fn tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    fn known_models(&self) -> Vec<String> {
        let mut models = vec![self.config.model.id.clone()];
        models.extend(self.config.model.models.iter().cloned());
        models
    }

    fn session_name(&self) -> String {
        self.session_header
            .as_ref()
            .and_then(|h| h.name.clone())
            .unwrap_or_else(|| "(unnamed)".to_string())
    }

    fn new_session_file(&mut self) {
        let header = SessionHeader::new(&self.cwd, &self.model);
        match session::create_session_file(&self.cwd, &header) {
            Ok(path) => {
                self.session_header = Some(header);
                self.session_path = Some(path);
            }
            Err(e) => {
                self.session_header = None;
                self.session_path = None;
                eprintln!("session persistence disabled: {e}");
            }
        }
    }

    fn persist(&self, events: &broadcast::Sender<AgentEvent>) {
        if let (Some(path), Some(header)) = (&self.session_path, &self.session_header) {
            let mut header = header.clone();
            header.model = self.model.clone();
            let messages: Vec<Message> = self
                .history
                .iter()
                .filter(|m| m.role != "system")
                .cloned()
                .collect();
            if let Err(e) = session::save_session(path, &header, &messages) {
                let _ = events.send(AgentEvent::Error(format!("failed to save session: {e}")));
            }
        }
        let _ = events.send(AgentEvent::ContextUsage {
            est_tokens: estimate_tokens(&self.history),
            message_count: self.history.len(),
            session_path: self.session_path.as_ref().map(|p| p.display().to_string()),
            session_name: self.session_header.as_ref().and_then(|h| h.name.clone()),
        });
    }

    async fn run_shell(
        &mut self,
        command: &str,
        to_model: bool,
        events: &broadcast::Sender<AgentEvent>,
    ) {
        let _ = events.send(AgentEvent::ToolStarted {
            name: "bash".to_string(),
            args: command.to_string(),
        });
        let cwd = self.cwd.clone();
        let command = command.to_string();
        let cmd_for_task = command.clone();
        let result =
            tokio::task::spawn_blocking(move || crate::tools::run_bash(&cwd, &cmd_for_task, None))
                .await
                .unwrap_or_else(|e| ToolResult::err(format!("shell task failed: {e}")));
        let _ = events.send(AgentEvent::ToolFinished {
            name: "bash".to_string(),
            is_error: result.is_error,
            output: result.output.clone(),
        });
        if to_model {
            self.history.push(Message::user(format!(
                "I ran a shell command for context. Command: {command}\nOutput:\n{}",
                result.output
            )));
        }
    }

    async fn compact_now(
        &mut self,
        instructions: Option<&str>,
        events: &broadcast::Sender<AgentEvent>,
        manual: bool,
    ) {
        let Some(cut) = find_cut_point(&self.history, self.config.compaction.keep_recent_tokens)
        else {
            if manual {
                let _ = events.send(AgentEvent::Info("nothing to compact yet".to_string()));
            }
            return;
        };
        let to_summarize = &self.history[1..cut];
        let request = build_summary_request(to_summarize, instructions);
        let before = estimate_tokens(&self.history);
        let result = self
            .client
            .stream_chat(
                &self.model,
                &request,
                &[],
                self.config.model.max_tokens,
                Some(0.0),
                None,
                Some(self.cancel.clone()),
            )
            .await;
        let summary = match result {
            Ok(answer) if !answer.content.trim().is_empty() => answer.content.trim().to_string(),
            Ok(_) => {
                let _ = events.send(AgentEvent::Error(
                    "compaction produced an empty summary; history unchanged".to_string(),
                ));
                return;
            }
            Err(e) => {
                let _ = events.send(AgentEvent::Error(format!("compaction failed: {e}")));
                return;
            }
        };
        let dropped = cut - 1;
        self.history = compacted_history(&self.history[0], &summary, &self.history[cut..]);
        let after = estimate_tokens(&self.history);
        let _ = events.send(AgentEvent::Info(format!(
            "compacted {dropped} messages (~{before} → ~{after} tokens)"
        )));
    }

    async fn run_turn(&mut self, user_text: &str, events: &broadcast::Sender<AgentEvent>) {
        self.history.push(Message::user(user_text));
        if let Err(e) = self.drive(events).await {
            let _ = events.send(AgentEvent::Error(e.to_string()));
        }
    }

    async fn maybe_autocompact(&mut self, events: &broadcast::Sender<AgentEvent>) {
        if !self.config.compaction.enabled {
            return;
        }
        let est = estimate_tokens(&self.history);
        if should_compact(
            est,
            self.config.context_window as usize,
            self.config.compaction.reserve_tokens,
        ) {
            let _ = events.send(AgentEvent::Info(format!(
                "auto-compacting (est {est} tokens, window {})",
                self.config.context_window
            )));
            self.compact_now(None, events, false).await;
        }
    }

    async fn drive(&mut self, events: &broadcast::Sender<AgentEvent>) -> anyhow::Result<()> {
        let specs = self.tools.iter().map(|t| t.spec()).collect::<Vec<_>>();
        self.truncation_recovery_used = false;
        for _ in 0..self.config.loop_config.max_turns_per_run {
            if self.cancelled() {
                return Ok(());
            }
            self.maybe_autocompact(events).await;
            let (stx, mut srx) = tokio::sync::mpsc::unbounded_channel::<StreamEvent>();
            let fwd = events.clone();
            let forwarder = tokio::spawn(async move {
                while let Some(ev) = srx.recv().await {
                    let ae = match ev {
                        StreamEvent::TextDelta(t) => AgentEvent::TextDelta(t),
                        StreamEvent::ReasoningDelta(r) => AgentEvent::ReasoningDelta(r),
                        _ => continue,
                    };
                    let _ = fwd.send(ae);
                }
            });
            let assistant = match self
                .client
                .stream_chat(
                    &self.model,
                    &self.history,
                    &specs,
                    self.config.model.max_tokens,
                    None,
                    Some(stx),
                    Some(self.cancel.clone()),
                )
                .await
            {
                Ok(a) => a,
                Err(e) => {
                    forwarder.abort();
                    if self.cancelled() {
                        return Ok(());
                    }
                    return Err(e);
                }
            };
            forwarder.abort();

            if assistant.finish_reason.as_deref() == Some("length") {
                if !self.truncation_recovery_used {
                    self.truncation_recovery_used = true;
                    let _ = events.send(AgentEvent::Info(
                        "response truncated at max_tokens; compacting and retrying".to_string(),
                    ));
                    self.compact_now(None, events, false).await;
                    continue;
                }
                let _ = events.send(AgentEvent::Error(
                    "truncated response recovery failed after one compact-and-retry attempt"
                        .to_string(),
                ));
                let text = assistant.content.clone();
                self.history.push(Message {
                    role: "assistant".to_string(),
                    content: if text.is_empty() {
                        None
                    } else {
                        Some(text.clone())
                    },
                    tool_calls: None,
                    tool_call_id: None,
                });
                let _ = events.send(AgentEvent::AssistantDone { text });
                return Ok(());
            }

            let mut message = Message {
                role: "assistant".to_string(),
                content: if assistant.content.is_empty() {
                    None
                } else {
                    Some(assistant.content.clone())
                },
                tool_calls: if assistant.tool_calls.is_empty() {
                    None
                } else {
                    Some(assistant.tool_calls.clone())
                },
                tool_call_id: None,
            };
            if message.content.is_none() && message.tool_calls.is_none() {
                message.content = Some(String::new());
            }
            self.history.push(message);

            if assistant.tool_calls.is_empty() {
                let _ = events.send(AgentEvent::AssistantDone {
                    text: assistant.content.clone(),
                });
                return Ok(());
            }

            for call in &assistant.tool_calls {
                if self.cancelled() {
                    self.history.push(Message::tool_result(
                        &call.id,
                        "Interrupted by user".to_string(),
                    ));
                    continue;
                }
                let args_preview: String = call.function.arguments.chars().take(200).collect();
                let _ = events.send(AgentEvent::ToolStarted {
                    name: call.function.name.clone(),
                    args: args_preview,
                });
                let result = self
                    .execute_tool(&call.function.name, &call.function.arguments)
                    .await;
                let (output, is_error) = match result {
                    Ok(r) => (r.output, r.is_error),
                    Err(e) => (e, true),
                };
                let _ = events.send(AgentEvent::ToolFinished {
                    name: call.function.name.clone(),
                    is_error,
                    output: output.clone(),
                });
                self.history.push(Message::tool_result(&call.id, output));
            }
        }
        let _ = events.send(AgentEvent::Error(
            "max turns reached without a final answer".to_string(),
        ));
        Ok(())
    }

    async fn execute_tool(&self, name: &str, raw_args: &str) -> Result<ToolResult, String> {
        let tool = self
            .tool(name)
            .ok_or_else(|| format!("unknown tool: {name}"))?;
        let args: serde_json::Value = serde_json::from_str(raw_args)
            .map_err(|e| format!("invalid tool arguments (possibly truncated): {e}"))?;
        tokio::task::spawn_blocking(move || tool.execute(&args))
            .await
            .map_err(|e| format!("tool task failed: {e}"))
    }

    async fn run_loop(&mut self, spec: &LoopSpec, events: &broadcast::Sender<AgentEvent>) {
        let _ = events.send(AgentEvent::LoopStarted {
            description: spec.describe_mode(),
        });
        let started = Instant::now();
        let mut iterations = 0usize;
        self.history.push(Message::user(spec.prompt.clone()));
        let reason = loop {
            if self.cancelled() {
                break "interrupted".to_string();
            }
            match &spec.mode {
                LoopMode::Count(n) if iterations >= *n => break "count reached".to_string(),
                LoopMode::Duration(d) if started.elapsed() >= *d => {
                    break "duration elapsed".to_string();
                }
                _ => {}
            }
            if let Err(e) = self.drive(events).await {
                let _ = events.send(AgentEvent::Error(e.to_string()));
                break "error".to_string();
            }
            if self.cancelled() {
                break "interrupted".to_string();
            }
            iterations += 1;
            let _ = events.send(AgentEvent::LoopIterationDone { iterations });
            match &spec.mode {
                LoopMode::Until { condition } => match self.check_condition(condition).await {
                    Ok(true) => break "condition met".to_string(),
                    Ok(false) => self.history.push(Message::user(spec.prompt.clone())),
                    Err(e) => break format!("condition check failed: {e}"),
                },
                _ => self.history.push(Message::user(spec.prompt.clone())),
            }
        };
        self.history.push(Message::user(format!(
            "[loop {} — {} after {iterations} iteration(s)]",
            spec.describe_mode(),
            reason
        )));
        let _ = events.send(AgentEvent::LoopFinished { iterations, reason });
    }

    async fn check_condition(&self, condition: &str) -> Result<bool, String> {
        let prompt = format!(
            "You are evaluating a loop exit condition. Answer with exactly YES or NO.\n\
             Conversation context is above.\nCondition: {condition}"
        );
        let mut messages = self.history.clone();
        messages.push(Message::user(prompt));
        let answer = self
            .client
            .stream_chat(&self.model, &messages, &[], 128, Some(0.0), None, None)
            .await
            .map_err(|e| e.to_string())?;
        let text = if answer.content.trim().is_empty() {
            answer.reasoning.as_str()
        } else {
            answer.content.as_str()
        };
        Ok(text.trim().to_uppercase().contains("YES"))
    }
}
