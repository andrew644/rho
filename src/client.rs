use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::types::{AssistantMessage, FunctionCall, Message, ToolCall, ToolSpec, Usage};

#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallStarted {
        name: String,
    },
    Done {
        finish_reason: Option<String>,
        usage: Usage,
    },
}

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: u16,
    pub text: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "model API returned {}: {}", self.status, self.text)
    }
}

impl std::error::Error for ApiError {}

const OVERFLOW_MARKERS: &[&str] = &[
    "prompt is too long",
    "prompt too long",
    "prompt exceeds max length",
    "request_too_large",
    "input is too long for requested model",
    "exceeds the context window",
    "maximum context length",
    "context window exceeded",
    "context window exceeds limit",
    "exceeds the maximum number of tokens allowed",
    "maximum prompt length is",
    "reduce the length of the messages",
    "exceeds the maximum allowed input length",
    "is longer than the model",
    "exceeds the limit of",
    "exceeds the available context size",
    "greater than the context length",
    "exceeded model token limit",
    "too large for model with",
    "but the configured context size is",
    "model_context_window_exceeded",
    "exceeded max context length",
    "range of input length should be",
    "context_length_exceeded",
    "context length exceeded",
    "too many tokens",
    "token limit exceeded",
];

const NON_OVERFLOW_MARKERS: &[&str] = &[
    "rate limit",
    "too many requests",
    "throttling error:",
    "service unavailable:",
];

impl ApiError {
    pub fn is_context_overflow(&self) -> bool {
        if self.status == 413 {
            return true;
        }
        let text = self.text.to_lowercase();
        if NON_OVERFLOW_MARKERS
            .iter()
            .any(|marker| text.contains(marker))
        {
            return false;
        }
        OVERFLOW_MARKERS.iter().any(|marker| text.contains(marker))
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct Chunk {
    choices: Vec<ChunkChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct ChunkChoice {
    #[serde(default)]
    delta: ChunkDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ChunkDelta {
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    index: usize,
    id: Option<String>,
    function: Option<FunctionNameArgs>,
}

#[derive(Debug, Deserialize)]
struct FunctionNameArgs {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Default)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
    announced: bool,
}

#[derive(Default)]
pub struct AssistantAccumulator {
    content: String,
    reasoning: String,
    tool_calls: BTreeMap<usize, PendingToolCall>,
    usage: Usage,
    finish_reason: Option<String>,
}

impl AssistantAccumulator {
    pub(crate) fn push_chunk(&mut self, chunk: &Chunk) -> (Vec<String>, Vec<String>, Vec<usize>) {
        let mut text = Vec::new();
        let mut reasoning = Vec::new();
        let mut new_tools = Vec::new();
        if let Some(usage) = &chunk.usage {
            self.usage = usage.clone();
        }
        for choice in &chunk.choices {
            if choice.finish_reason.is_some() {
                self.finish_reason = choice.finish_reason.clone();
            }
            if let Some(c) = choice.delta.content.as_deref().filter(|c| !c.is_empty()) {
                self.content.push_str(c);
                text.push(c.to_string());
            }
            if let Some(r) = choice
                .delta
                .reasoning_content
                .as_deref()
                .filter(|r| !r.is_empty())
            {
                self.reasoning.push_str(r);
                reasoning.push(r.to_string());
            }
            for tc in choice.delta.tool_calls.iter().flatten() {
                let entry = self.tool_calls.entry(tc.index).or_default();
                if let Some(id) = &tc.id {
                    entry.id = id.clone();
                }
                if let Some(f) = &tc.function {
                    if let Some(n) = &f.name {
                        entry.name.push_str(n);
                    }
                    if let Some(a) = &f.arguments {
                        entry.arguments.push_str(a);
                    }
                }
                if !entry.announced && !entry.name.is_empty() {
                    entry.announced = true;
                    new_tools.push(tc.index);
                }
            }
        }
        (text, reasoning, new_tools)
    }

    pub fn finish_reason(&self) -> Option<String> {
        self.finish_reason.clone()
    }

    pub fn finish(self) -> AssistantMessage {
        AssistantMessage {
            content: self.content,
            reasoning: self.reasoning,
            tool_calls: self
                .tool_calls
                .into_iter()
                .map(|(idx, p)| ToolCall {
                    id: if p.id.is_empty() {
                        format!("toolcall_{idx}")
                    } else {
                        p.id
                    },
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: p.name,
                        arguments: p.arguments,
                    },
                })
                .collect(),
            finish_reason: self.finish_reason,
            usage: self.usage,
        }
    }
}

pub(crate) fn parse_sse_line(line: &str) -> Result<Option<Chunk>> {
    let Some(data) = line.trim().strip_prefix("data:") else {
        return Ok(None);
    };
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return Ok(None);
    }
    let chunk: Chunk =
        serde_json::from_str(data).with_context(|| format!("bad SSE data: {data}"))?;
    Ok(Some(chunk))
}

impl Client {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key: api_key.into(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn stream_chat(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        max_tokens: u32,
        temperature: Option<f32>,
        events: Option<UnboundedSender<StreamEvent>>,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<AssistantMessage> {
        let mut body = serde_json::json!({
            "model": model,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
            "max_tokens": max_tokens,
        });
        if !tools.is_empty() {
            body["tools"] = serde_json::json!(tools);
            body["tool_choice"] = serde_json::json!("auto");
        }
        if let Some(t) = temperature {
            body["temperature"] = serde_json::json!(t);
        }

        let mut resp = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("request to model failed")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow::Error::new(ApiError {
                status: status.as_u16(),
                text,
            }));
        }

        let mut acc = AssistantAccumulator::default();
        let mut buf = String::new();
        let mut announced_tools: std::collections::HashSet<usize> =
            std::collections::HashSet::new();

        while let Some(bytes) = resp.chunk().await? {
            if cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst)) {
                bail!("interrupted");
            }
            let text = String::from_utf8_lossy(&bytes);
            buf.push_str(&text);
            while let Some(pos) = buf.find('\n') {
                let line: String = buf.drain(..=pos).collect();
                if let Some(chunk) = parse_sse_line(&line)? {
                    let (deltas, reasoning, new_tools) = acc.push_chunk(&chunk);
                    if let Some(tx) = &events {
                        for r in reasoning {
                            let _ = tx.send(StreamEvent::ReasoningDelta(r));
                        }
                        for d in deltas {
                            let _ = tx.send(StreamEvent::TextDelta(d));
                        }
                        for idx in new_tools {
                            if announced_tools.insert(idx) {
                                if let Some(p) = acc.tool_calls.get(&idx) {
                                    let _ = tx.send(StreamEvent::ToolCallStarted {
                                        name: p.name.clone(),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        if !buf.trim().is_empty() {
            if let Some(chunk) = parse_sse_line(&buf)? {
                acc.push_chunk(&chunk);
            }
        }

        let finish_reason = acc.finish_reason();
        let message = acc.finish();
        if let Some(tx) = &events {
            let _ = tx.send(StreamEvent::Done {
                finish_reason,
                usage: message.usage.clone(),
            });
        }
        if message.content.is_empty()
            && message.tool_calls.is_empty()
            && message.finish_reason.is_none()
        {
            bail!("model returned an empty response");
        }
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(json: serde_json::Value) -> Chunk {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn ignores_non_data_lines() {
        assert!(parse_sse_line("").unwrap().is_none());
        assert!(parse_sse_line(": comment").unwrap().is_none());
        assert!(parse_sse_line("data: [DONE]").unwrap().is_none());
    }

    #[test]
    fn parses_data_line() {
        let c = parse_sse_line(
            r#"data: {"choices":[{"delta":{"content":"hi"},"finish_reason":null}]}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(c.choices[0].delta.content.as_deref(), Some("hi"));
    }

    #[test]
    fn accumulates_text_and_reasoning() {
        let mut acc = AssistantAccumulator::default();
        acc.push_chunk(&chunk(serde_json::json!({
            "choices": [{"delta": {"content": "Hel", "reasoning_content": "th"}}]
        })));
        acc.push_chunk(&chunk(serde_json::json!({
            "choices": [{"delta": {"content": "lo"}, "finish_reason": "stop"}]
        })));
        let m = acc.finish();
        assert_eq!(m.content, "Hello");
        assert_eq!(m.reasoning, "th");
        assert_eq!(m.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn accumulates_split_tool_calls() {
        let mut acc = AssistantAccumulator::default();
        let (_, _, first_new) = acc.push_chunk(&chunk(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_1", "function": {"name": "read", "arguments": ""}},
                {"index": 1, "id": null, "function": {"name": "bash", "arguments": "{\"co"}}
            ]}}]
        })));
        assert_eq!(first_new, vec![0, 1]);
        let (text, _, new_tools) = acc.push_chunk(&chunk(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 1, "id": null, "function": {"name": null, "arguments": "mmand\":\"ls\"}"}}
            ]}, "finish_reason": "tool_calls"}]
        })));
        assert!(text.is_empty());
        assert!(new_tools.is_empty());
        let m = acc.finish();
        assert_eq!(m.tool_calls.len(), 2);
        assert_eq!(m.tool_calls[0].function.name, "read");
        assert_eq!(m.tool_calls[0].id, "call_1");
        assert_eq!(m.tool_calls[1].id, "toolcall_1");
        assert_eq!(m.tool_calls[1].function.name, "bash");
        assert_eq!(m.tool_calls[1].function.arguments, r#"{"command":"ls"}"#);
        assert_eq!(m.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn captures_usage() {
        let mut acc = AssistantAccumulator::default();
        acc.push_chunk(&chunk(serde_json::json!({
            "choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        })));
        assert_eq!(acc.finish().usage.prompt_tokens, 10);
    }

    fn api_error(status: u16, text: &str) -> ApiError {
        ApiError {
            status,
            text: text.to_string(),
        }
    }

    #[test]
    fn detects_context_overflow_errors() {
        assert!(api_error(400, "This model's maximum context length is 8192 tokens. However, your request has 12000 input tokens.").is_context_overflow());
        assert!(
            api_error(400, "prompt is too long: 213462 tokens > 200000 maximum")
                .is_context_overflow()
        );
        assert!(api_error(400, "Range of input length should be [1, 8192]").is_context_overflow());
        assert!(api_error(
            400,
            "llama.cpp: the request exceeds the available context size, try increasing it"
        )
        .is_context_overflow());
        assert!(api_error(413, "failed to parse request").is_context_overflow());
    }

    #[test]
    fn ignores_non_overflow_errors() {
        assert!(
            !api_error(400, "invalid tool message: message must contain 'content'")
                .is_context_overflow()
        );
        assert!(!api_error(
            429,
            "Too many tokens: rate limit exceeded, too many requests"
        )
        .is_context_overflow());
        assert!(!api_error(500, "internal server error").is_context_overflow());
    }
}
