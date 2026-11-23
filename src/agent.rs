use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::broadcast;
use tokio::sync::mpsc::UnboundedSender;

use crate::client::{ApiError, Client, StreamEvent};
use crate::compaction::{
    build_summary_request, compacted_history, estimate_tokens, find_cut_point, should_compact,
};
use crate::config::Config;
use crate::history::sanitize_history;
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
    SetPwd { path: PathBuf },
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
    /// Something is broken but the agent keeps running: a session that is no
    /// longer being saved, a Discord reply that was refused. Distinct from
    /// `Error`, which means this prompt produced no answer — the print runner
    /// gives up on `Error`, the Discord reply relay answers with its text, and
    /// neither is right for a failure the agent has already worked around.
    Fault(String),
    ContextUsage {
        est_tokens: usize,
        message_count: usize,
        session_path: Option<String>,
        session_name: Option<String>,
        pwd: String,
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
    pwd: PathBuf,
    model: String,
    session_path: Option<PathBuf>,
    session_header: Option<SessionHeader>,
    truncation_recovery_used: bool,
    overflow_recovery_used: bool,
}

pub const TRUNCATED_TOOL_RESULT: &str =
    "Tool call was not executed: the response hit the output token limit, so its arguments may \
     be truncated. Re-issue the tool call with complete arguments.";

fn is_context_overflow(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ApiError>()
        .is_some_and(|error| error.is_context_overflow())
}

impl Agent {
    pub fn resolve_pwd(current: &Path, input: &Path) -> Result<PathBuf, String> {
        let joined = if input.is_absolute() {
            input.to_path_buf()
        } else {
            current.join(input)
        };
        let canonical = joined
            .canonicalize()
            .map_err(|e| format!("cannot use {}: {e}", joined.display()))?;
        if !canonical.is_dir() {
            return Err(format!("not a directory: {}", canonical.display()));
        }
        Ok(canonical)
    }

    pub fn spawn(
        config: Config,
        pwd: PathBuf,
        events: broadcast::Sender<AgentEvent>,
    ) -> (UnboundedSender<AgentCommand>, Arc<AtomicBool>) {
        Self::spawn_resumed(config, pwd, events, None)
    }

    pub fn spawn_resumed(
        config: Config,
        pwd: PathBuf,
        events: broadcast::Sender<AgentEvent>,
        resumed: Option<PathBuf>,
    ) -> (UnboundedSender<AgentCommand>, Arc<AtomicBool>) {
        let cancel = Arc::new(AtomicBool::new(false));
        let system = Message::system(build_system_prompt(&pwd));
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
            tools: all_tools(pwd.clone()).into_iter().map(Arc::from).collect(),
            history,
            cancel: cancel.clone(),
            pwd,
            truncation_recovery_used: false,
            overflow_recovery_used: false,
            session_path,
            session_header,
        };
        if agent.session_path.is_none() {
            if let Err(e) = agent.new_session_file() {
                let _ = events.send(AgentEvent::Fault(e));
            }
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
                        agent.history = vec![Message::system(build_system_prompt(&agent.pwd))];
                        if let Err(e) = agent.new_session_file() {
                            let _ = events.send(AgentEvent::Fault(e));
                        }
                    }
                    AgentCommand::Resume { path } => match session::load_session(&path) {
                        Ok((header, messages)) => {
                            let mut h = vec![Message::system(build_system_prompt(&agent.pwd))];
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
                    AgentCommand::SetPwd { path } => match Agent::resolve_pwd(&agent.pwd, &path) {
                        Ok(pwd) => {
                            agent.pwd = pwd.clone();
                            agent.tools =
                                all_tools(pwd.clone()).into_iter().map(Arc::from).collect();
                            if let Some(first) = agent.history.first_mut() {
                                *first = Message::system(build_system_prompt(&pwd));
                            }
                            agent.history.push(Message::user(format!(
                                "The working directory changed to {}.",
                                pwd.display()
                            )));
                            let _ =
                                events.send(AgentEvent::Info(format!("pwd: {}", pwd.display())));
                        }
                        Err(e) => {
                            let _ = events.send(AgentEvent::Error(e));
                        }
                    },
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

    /// Opens the file this session is written to. A failure comes back as a
    /// message for the transcript instead of stderr, which the TUI paints over:
    /// a session that silently stops being saved is otherwise lost unnoticed.
    fn new_session_file(&mut self) -> Result<(), String> {
        let header = SessionHeader::new(&self.pwd, &self.model);
        match session::create_session_file(&self.pwd, &header) {
            Ok(path) => {
                self.session_header = Some(header);
                self.session_path = Some(path);
                Ok(())
            }
            Err(e) => {
                self.session_header = None;
                self.session_path = None;
                Err(format!("session persistence disabled: {e}"))
            }
        }
    }

    fn send_usage(&self, events: &broadcast::Sender<AgentEvent>) {
        let _ = events.send(AgentEvent::ContextUsage {
            est_tokens: estimate_tokens(&self.history),
            message_count: self.history.len(),
            session_path: self.session_path.as_ref().map(|p| p.display().to_string()),
            session_name: self.session_header.as_ref().and_then(|h| h.name.clone()),
            pwd: self.pwd.display().to_string(),
        });
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
        self.send_usage(events);
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
        let pwd = self.pwd.clone();
        let command = command.to_string();
        let cmd_for_task = command.clone();
        let result =
            tokio::task::spawn_blocking(move || crate::tools::run_bash(&pwd, &cmd_for_task, None))
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
    ) -> bool {
        let Some(cut) = find_cut_point(&self.history, self.config.compaction.keep_recent_tokens)
        else {
            if manual {
                let _ = events.send(AgentEvent::Info("nothing to compact yet".to_string()));
            }
            return false;
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
                return false;
            }
            Err(e) => {
                let _ = events.send(AgentEvent::Error(format!("compaction failed: {e}")));
                return false;
            }
        };
        let dropped = cut - 1;
        self.history = compacted_history(&self.history[0], &summary, &self.history[cut..]);
        let after = estimate_tokens(&self.history);
        let _ = events.send(AgentEvent::Info(format!(
            "compacted {dropped} messages (~{before} → ~{after} tokens)"
        )));
        true
    }

    async fn run_turn(&mut self, user_text: &str, events: &broadcast::Sender<AgentEvent>) {
        self.history.push(Message::user(user_text));
        self.send_usage(events);
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
        self.overflow_recovery_used = false;
        self.history = sanitize_history(&self.history);
        for _ in 0..self.config.loop_config.max_turns_per_run {
            if self.cancelled() {
                return Ok(());
            }
            self.maybe_autocompact(events).await;
            let request = sanitize_history(&self.history);
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
                    &request,
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
                    if !self.overflow_recovery_used && is_context_overflow(&e) {
                        self.overflow_recovery_used = true;
                        let _ = events.send(AgentEvent::Info(
                            "context overflow; compacting and retrying".to_string(),
                        ));
                        if self.compact_now(None, events, false).await {
                            continue;
                        }
                        let _ = events.send(AgentEvent::Error(
                            "context overflow recovery failed: nothing left to compact".to_string(),
                        ));
                    }
                    return Err(e);
                }
            };
            forwarder.abort();

            if assistant.finish_reason.as_deref() == Some("length") {
                let text = assistant.content.clone();
                if !assistant.tool_calls.is_empty() {
                    self.history.push(Message {
                        role: "assistant".to_string(),
                        content: if text.is_empty() {
                            None
                        } else {
                            Some(text.clone())
                        },
                        tool_calls: Some(assistant.tool_calls.clone()),
                        tool_call_id: None,
                        reasoning: (!assistant.reasoning.is_empty())
                            .then(|| assistant.reasoning.clone()),
                    });
                    self.send_usage(events);
                    for call in &assistant.tool_calls {
                        let _ = events.send(AgentEvent::ToolStarted {
                            name: call.function.name.clone(),
                            args: call.function.arguments.chars().take(200).collect(),
                        });
                        let _ = events.send(AgentEvent::ToolFinished {
                            name: call.function.name.clone(),
                            is_error: true,
                            output: TRUNCATED_TOOL_RESULT.to_string(),
                        });
                        self.history.push(Message::tool_result(
                            &call.id,
                            TRUNCATED_TOOL_RESULT.to_string(),
                        ));
                        self.send_usage(events);
                    }
                }
                if !self.truncation_recovery_used {
                    self.truncation_recovery_used = true;
                    let _ = events.send(AgentEvent::Info(
                        "response truncated at max_tokens; compacting and retrying".to_string(),
                    ));
                    self.compact_now(None, events, false).await;
                    continue;
                }
                let _ = events.send(AgentEvent::Error(
                    "Response was truncated before completion.".to_string(),
                ));
                let _ = events.send(AgentEvent::Error(
                    "truncated response recovery failed after one compact-and-retry attempt"
                        .to_string(),
                ));
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
                reasoning: (!assistant.reasoning.is_empty()).then(|| assistant.reasoning.clone()),
            };
            if message.content.is_none() && message.tool_calls.is_none() {
                message.content = Some(String::new());
            }
            self.history.push(message);
            self.send_usage(events);

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
                    self.send_usage(events);
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
                self.send_usage(events);
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
                    Ok(false) => {
                        self.history.push(Message::user(spec.prompt.clone()));
                        self.send_usage(events);
                    }
                    Err(e) => break format!("condition check failed: {e}"),
                },
                _ => {
                    self.history.push(Message::user(spec.prompt.clone()));
                    self.send_usage(events);
                }
            }
        };
        self.history.push(Message::user(format!(
            "[loop {} — {} after {iterations} iteration(s)]",
            spec.describe_mode(),
            reason
        )));
        self.send_usage(events);
        let _ = events.send(AgentEvent::LoopFinished { iterations, reason });
    }

    async fn check_condition(&self, condition: &str) -> Result<bool, String> {
        let prompt = format!(
            "You are evaluating a loop exit condition. Answer with exactly YES or NO.\n\
             Conversation context is above.\nCondition: {condition}"
        );
        let mut messages = sanitize_history(&self.history);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        serde_yaml::from_str(
            "provider:\n  base_url: http://127.0.0.1:1/v1\n  api_key: test\nmodel:\n  id: test-model\n",
        )
        .unwrap()
    }

    fn test_agent() -> (Agent, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config();
        let agent = Agent {
            client: Client::new(&config.provider.base_url, &config.provider.api_key),
            model: config.model.id.clone(),
            tools: all_tools(dir.path().to_path_buf())
                .into_iter()
                .map(Arc::from)
                .collect(),
            history: vec![Message::system("sys")],
            cancel: Arc::new(AtomicBool::new(false)),
            pwd: dir.path().to_path_buf(),
            config,
            session_path: None,
            session_header: None,
            truncation_recovery_used: false,
            overflow_recovery_used: false,
        };
        (agent, dir)
    }

    fn with_state_home<T>(home: &Path, run: impl FnOnce() -> T) -> T {
        static STATE_HOME: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _serial = STATE_HOME
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("XDG_STATE_HOME");
        std::env::set_var("XDG_STATE_HOME", home);
        let value = run();
        match previous {
            Some(value) => std::env::set_var("XDG_STATE_HOME", value),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
        value
    }

    fn recv_usage(rx: &mut broadcast::Receiver<AgentEvent>) -> (usize, usize) {
        match rx.try_recv().unwrap() {
            AgentEvent::ContextUsage {
                est_tokens,
                message_count,
                ..
            } => (est_tokens, message_count),
            other => panic!("expected ContextUsage, got {other:?}"),
        }
    }

    #[test]
    fn send_usage_tracks_history_growth() {
        let (tx, mut rx) = broadcast::channel(16);
        let (mut agent, _dir) = test_agent();
        agent.send_usage(&tx);
        let (before, count) = recv_usage(&mut rx);
        assert_eq!(count, 1);
        agent
            .history
            .push(Message::user("this message adds tokens to the context"));
        agent.send_usage(&tx);
        let (after, count) = recv_usage(&mut rx);
        assert_eq!(count, 2);
        assert!(after > before);
    }

    #[tokio::test]
    async fn run_turn_reports_usage_as_soon_as_the_user_message_lands() {
        let (tx, mut rx) = broadcast::channel(16);
        let (mut agent, _dir) = test_agent();
        let before = estimate_tokens(&agent.history);
        agent
            .run_turn("a prompt that should show up in the context", &tx)
            .await;
        let mut usage = None;
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::ContextUsage { est_tokens, .. } = event {
                usage = Some(est_tokens);
            }
        }
        assert!(usage.is_some_and(|est| est > before));
    }

    #[test]
    fn a_session_file_that_cannot_be_written_says_persistence_is_off() {
        let blocker = tempfile::tempdir().unwrap();
        let file = blocker.path().join("blocked");
        std::fs::write(&file, "").unwrap();
        let (mut agent, _dir) = test_agent();

        let error = with_state_home(&file, || agent.new_session_file()).unwrap_err();

        assert!(error.contains("session persistence disabled"), "{error}");
        assert!(agent.session_path.is_none());
        assert!(agent.session_header.is_none());
    }

    #[test]
    fn a_session_file_that_can_be_written_leaves_no_error() {
        let state = tempfile::tempdir().unwrap();
        let (mut agent, _dir) = test_agent();

        with_state_home(state.path(), || agent.new_session_file()).unwrap();

        let path = agent.session_path.clone().unwrap();
        assert!(path.exists());
        assert!(path.starts_with(state.path()));
        assert!(agent.session_header.is_some());
    }

    #[test]
    fn resolve_pwd_accepts_absolute_and_relative_dirs() {
        let d = tempfile::tempdir().unwrap();
        let sub = d.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let abs = Agent::resolve_pwd(std::path::Path::new("/"), d.path()).unwrap();
        assert!(abs.is_dir());
        let rel = Agent::resolve_pwd(&abs, std::path::Path::new("sub")).unwrap();
        assert_eq!(rel, sub.canonicalize().unwrap());
    }

    #[test]
    fn resolve_pwd_rejects_missing_paths_and_files() {
        let d = tempfile::tempdir().unwrap();
        assert!(Agent::resolve_pwd(d.path(), std::path::Path::new("nope")).is_err());
        let file = d.path().join("f.txt");
        std::fs::write(&file, "x").unwrap();
        assert!(Agent::resolve_pwd(d.path(), &file).is_err());
    }
}
