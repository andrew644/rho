use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;

use crate::client::{Client, StreamEvent};
use crate::config::Config;
use crate::loops::{LoopMode, LoopSpec};
use crate::prompt::build_system_prompt;
use crate::tools::{all_tools, Tool, ToolResult};
use crate::types::Message;

#[derive(Debug)]
pub enum AgentCommand {
    Prompt { text: String },
    Loop { spec: LoopSpec },
    Interrupt,
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    ReasoningDelta(String),
    TextDelta(String),
    ToolStarted { name: String, args: String },
    ToolFinished { name: String, is_error: bool, output: String },
    AssistantDone { text: String },
    LoopStarted { description: String },
    LoopIterationDone { iterations: usize },
    LoopFinished { iterations: usize, reason: String },
    Idle,
    Error(String),
}

pub struct Agent {
    client: Client,
    config: Config,
    tools: Vec<Arc<dyn Tool>>,
    history: Vec<Message>,
    cancel: Arc<AtomicBool>,
}

impl Agent {
    pub fn spawn(config: Config, cwd: PathBuf, events: UnboundedSender<AgentEvent>) -> (UnboundedSender<AgentCommand>, Arc<AtomicBool>) {
        let cancel = Arc::new(AtomicBool::new(false));
        let mut agent = Agent {
            client: Client::new(&config.provider.base_url, &config.provider.api_key),
            config,
            tools: all_tools(cwd.clone()).into_iter().map(Arc::from).collect(),
            history: vec![Message::system(build_system_prompt(&cwd))],
            cancel: cancel.clone(),
        };
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
                }
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

    async fn run_turn(&mut self, user_text: &str, events: &UnboundedSender<AgentEvent>) {
        self.history.push(Message::user(user_text));
        if let Err(e) = self.drive(events).await {
            let _ = events.send(AgentEvent::Error(e.to_string()));
        }
    }

    async fn drive(&mut self, events: &UnboundedSender<AgentEvent>) -> anyhow::Result<()> {
        let specs = self.tools.iter().map(|t| t.spec()).collect::<Vec<_>>();
        for _ in 0..self.config.loop_config.max_turns_per_run {
            if self.cancelled() {
                return Ok(());
            }
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
            let assistant = self
                .client
                .stream_chat(
                    &self.config.model.id,
                    &self.history,
                    &specs,
                    self.config.model.max_tokens,
                    None,
                    Some(stx),
                )
                .await?;
            forwarder.abort();

            if assistant.finish_reason.as_deref() == Some("length") {
                let _ = events.send(AgentEvent::Error(
                    "response truncated at max_tokens".to_string(),
                ));
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
                let result = self.execute_tool(&call.function.name, &call.function.arguments).await;
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

    async fn execute_tool(
        &self,
        name: &str,
        raw_args: &str,
    ) -> Result<ToolResult, String> {
        let tool = self
            .tool(name)
            .ok_or_else(|| format!("unknown tool: {name}"))?;
        let args: serde_json::Value = serde_json::from_str(raw_args)
            .unwrap_or_else(|_| json!({}));
        tokio::task::spawn_blocking(move || tool.execute(&args))
            .await
            .map_err(|e| format!("tool task failed: {e}"))
    }

    async fn run_loop(&mut self, spec: &LoopSpec, events: &UnboundedSender<AgentEvent>) {
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
            .stream_chat(&self.config.model.id, &messages, &[], 8, Some(0.0), None)
            .await
            .map_err(|e| e.to_string())?;
        Ok(answer.content.trim().to_uppercase().starts_with("YES"))
    }
}
