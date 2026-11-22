use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use rho::agent::{Agent, AgentCommand, AgentEvent};
use rho::config::{
    CompactionConfig, Config, DiscordConfig, LoopConfig, ModelConfig, ProviderConfig, ThemeName,
};

const TRUNCATED_TOOL_CALL_SSE: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",",
    "\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"ls -la /very\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
    "data: [DONE]\n\n"
);

const PLAIN_SSE: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"RECOVERED\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n"
);

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Value> {
    let mut buf = vec![0u8; 256 * 1024];
    let mut filled = 0usize;
    let mut body_start = 0usize;
    let mut expected = usize::MAX;
    loop {
        if filled == buf.len() {
            return None;
        }
        let read = socket.read(&mut buf[filled..]).await.ok()?;
        if read == 0 {
            return None;
        }
        filled += read;
        if expected == usize::MAX {
            let head = String::from_utf8_lossy(&buf[..filled]).to_lowercase();
            let pos = head.find("\r\n\r\n")?;
            let length = head[..pos]
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))?
                .trim()
                .parse::<usize>()
                .ok()?;
            body_start = pos + 4;
            expected = body_start + length;
        }
        if filled >= expected {
            return serde_json::from_slice(&buf[body_start..expected]).ok();
        }
    }
}

async fn respond(socket: &mut tokio::net::TcpStream, sse: &str) {
    let body = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{sse}"
    );
    let _ = socket.write_all(body.as_bytes()).await;
    let _ = socket.shutdown().await;
}

fn config(base_url: String) -> Config {
    Config {
        provider: ProviderConfig {
            base_url,
            api_key: "test".to_string(),
        },
        model: ModelConfig {
            id: "mock".to_string(),
            max_tokens: 64,
            models: vec![],
        },
        context_window: 200_000,
        theme: ThemeName::Dark,
        discord: DiscordConfig::default(),
        loop_config: LoopConfig::default(),
        compaction: CompactionConfig::default(),
    }
}

fn sendable_problems(messages: &[Value]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    for message in messages {
        let role = message["role"].as_str().unwrap_or("");
        match role {
            "tool" => {
                let Some(id) = message["tool_call_id"].as_str() else {
                    problems.push("tool result without tool_call_id".to_string());
                    continue;
                };
                if !pending.iter().any(|pending| pending == id) {
                    problems.push(format!("orphan tool result for {id}"));
                }
                pending.retain(|pending| pending != id);
                if message.get("content").is_none() {
                    problems.push(format!("tool result {id} without content"));
                }
            }
            "assistant" => {
                if !pending.is_empty() {
                    problems.push(format!("unanswered tool calls: {pending:?}"));
                    pending.clear();
                }
                let calls = message
                    .get("tool_calls")
                    .and_then(|calls| calls.as_array())
                    .cloned();
                if message.get("content").is_none() && calls.is_none() {
                    problems.push("assistant message without content or tool_calls".to_string());
                }
                pending = calls
                    .unwrap_or_default()
                    .iter()
                    .map(|call| call["id"].as_str().unwrap_or_default().to_string())
                    .collect();
            }
            _ => {
                if !pending.is_empty() {
                    problems.push(format!("unanswered tool calls: {pending:?}"));
                    pending.clear();
                }
            }
        }
    }
    if !pending.is_empty() {
        problems.push(format!(
            "unanswered tool calls at end of request: {pending:?}"
        ));
    }
    problems
}

#[tokio::test]
async fn truncated_tool_call_keeps_session_usable() {
    let state = std::env::temp_dir().join(format!("rho-sessions-{}", std::process::id()));
    std::fs::create_dir_all(&state).unwrap();
    std::env::set_var("XDG_STATE_HOME", &state);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let problems: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (server_requests, server_problems) = (requests.clone(), problems.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let Some(body) = read_request(&mut socket).await else {
                continue;
            };
            let index = {
                let mut seen = server_requests.lock().unwrap();
                let index = seen.len();
                seen.push(body.clone());
                index
            };
            let messages = body["messages"].as_array().cloned().unwrap_or_default();
            server_problems
                .lock()
                .unwrap()
                .extend(sendable_problems(&messages));
            let sse = if index < 2 {
                TRUNCATED_TOOL_CALL_SSE
            } else {
                PLAIN_SSE
            };
            respond(&mut socket, sse).await;
        }
    });

    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = tokio::sync::broadcast::channel(256);
    let (agent, _cancel) = Agent::spawn(
        config(format!("http://{addr}/v1")),
        dir.path().to_path_buf(),
        tx,
    );

    agent
        .send(AgentCommand::Prompt {
            text: "run the command".to_string(),
        })
        .unwrap();

    let mut saw_compact_note = false;
    let mut saw_truncated_note = false;
    let mut saw_error_note = false;
    let first_turn = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match rx.recv().await.unwrap() {
                AgentEvent::Info(message) if message.contains("compacting and retrying") => {
                    saw_compact_note = true;
                }
                AgentEvent::Error(message) if message.contains("truncated before completion") => {
                    saw_truncated_note = true;
                }
                AgentEvent::Error(message) if message.contains("compact-and-retry") => {
                    saw_error_note = true;
                }
                AgentEvent::Idle => break,
                _ => {}
            }
        }
    })
    .await;

    first_turn.expect("first turn timed out");
    assert!(saw_compact_note, "expected a compact-and-retry note");
    assert!(saw_truncated_note, "expected the truncated-response note");
    assert!(saw_error_note, "expected recovery to be reported as failed");
    assert_eq!(requests.lock().unwrap().len(), 2);

    agent
        .send(AgentCommand::Prompt {
            text: "continue".to_string(),
        })
        .unwrap();

    let reply = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let AgentEvent::AssistantDone { text } = rx.recv().await.unwrap() {
                return text;
            }
        }
    })
    .await
    .expect("follow-up prompt timed out");

    assert_eq!(reply, "RECOVERED");
    assert_eq!(requests.lock().unwrap().len(), 3);
    assert!(
        problems.lock().unwrap().is_empty(),
        "invalid requests: {:?}",
        problems.lock().unwrap()
    );
}

const OVERFLOW_BODY: &str = "{\"error\":{\"message\":\"This model's maximum context length is 8192 tokens. However, your request has 12000 input tokens.\"}}";

const SUMMARIZER_SSE: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"## Goal\\nkeep working\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n"
);

const FIRST_SSE: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"FIRST\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n"
);

#[tokio::test]
async fn context_overflow_compacts_and_retries() {
    let state = std::env::temp_dir().join(format!("rho-sessions-b-{}", std::process::id()));
    std::fs::create_dir_all(&state).unwrap();
    std::env::set_var("XDG_STATE_HOME", &state);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let problems: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (server_requests, server_problems) = (requests.clone(), problems.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let Some(body) = read_request(&mut socket).await else {
                continue;
            };
            let messages = body["messages"].as_array().cloned().unwrap_or_default();
            server_problems
                .lock()
                .unwrap()
                .extend(sendable_problems(&messages));
            let index = {
                let mut seen = server_requests.lock().unwrap();
                let index = seen.len();
                seen.push(body);
                index
            };
            match index {
                0 => respond(&mut socket, FIRST_SSE).await,
                1 => {
                    let response = format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\n\
                         Connection: close\r\n\r\n{OVERFLOW_BODY}"
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                }
                2 => respond(&mut socket, SUMMARIZER_SSE).await,
                _ => respond(&mut socket, PLAIN_SSE).await,
            }
        }
    });

    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = tokio::sync::broadcast::channel(256);
    let mut config = config(format!("http://{addr}/v1"));
    config.compaction.keep_recent_tokens = 50;
    let (agent, _cancel) = Agent::spawn(config, dir.path().to_path_buf(), tx);

    agent
        .send(AgentCommand::Prompt {
            text: "first question".to_string(),
        })
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let AgentEvent::AssistantDone { text } = rx.recv().await.unwrap() {
                break text;
            }
        }
    })
    .await
    .expect("first turn timed out");
    assert_eq!(first, "FIRST");

    agent
        .send(AgentCommand::Prompt {
            text: "second question".repeat(80),
        })
        .unwrap();

    let mut saw_overflow_note = false;
    let mut saw_compaction = false;
    let mut errors: Vec<String> = Vec::new();
    let second = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match rx.recv().await.unwrap() {
                AgentEvent::Info(message) if message.contains("context overflow") => {
                    saw_overflow_note = true;
                }
                AgentEvent::Info(message) if message.contains("compacted") => {
                    saw_compaction = true;
                }
                AgentEvent::Error(message) => errors.push(message),
                AgentEvent::AssistantDone { text } => break text,
                _ => {}
            }
        }
    })
    .await
    .expect("second turn timed out");

    assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    assert!(saw_overflow_note, "expected a context overflow note");
    assert!(saw_compaction, "expected compaction to shrink history");
    assert_eq!(second, "RECOVERED");
    assert_eq!(requests.lock().unwrap().len(), 4);
    assert!(
        problems.lock().unwrap().is_empty(),
        "invalid requests: {:?}",
        problems.lock().unwrap()
    );
}
