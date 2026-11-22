use rho::agent::{Agent, AgentCommand, AgentEvent};
use rho::client::Client;
use rho::config::Config;
use std::time::Duration;

fn config() -> Config {
    Config::load().expect("requires ~/.config/rho/config.yaml")
}

#[tokio::test]
#[ignore]
async fn live_simple_chat() {
    let config = config();
    let client = Client::new(&config.provider.base_url, &config.provider.api_key);
    let reply = client
        .stream_chat(
            &config.model.id,
            &[rho::types::Message::user(
                "Reply with exactly: PONG. No other text.",
            )],
            &[],
            200,
            Some(0.0),
            None,
            None,
        )
        .await
        .unwrap();
    assert!(
        reply.content.contains("PONG"),
        "got: {:?}",
        reply.content
    );
}

#[tokio::test]
#[ignore]
async fn live_agent_uses_write_tool() {
    let config = config();
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = tokio::sync::broadcast::channel(256);
    let (agent, _cancel) = Agent::spawn(config, dir.path().to_path_buf(), tx);
    agent
        .send(AgentCommand::Prompt {
            text: "Create a file named hello.txt whose only content is: hello rho. \
                   Then reply with exactly: DONE"
                .to_string(),
        })
        .unwrap();

    let timeout = tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            let event = rx.recv().await.unwrap();
            match event {
                AgentEvent::AssistantDone { text } => return Some(text),
                AgentEvent::Error(e) => panic!("agent error: {e}"),
                _ => {}
            }
        }
    })
    .await;

    let final_text = timeout.expect("timed out").expect("no final text");
    let written = std::fs::read_to_string(dir.path().join("hello.txt")).unwrap();
    assert_eq!(written.trim(), "hello rho");
    assert!(final_text.contains("DONE"), "got: {final_text}");
}

#[tokio::test]
#[ignore]
async fn live_length_truncation_retries_once() {
    let mut config = config();
    config.model.max_tokens = 32;
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = tokio::sync::broadcast::channel(256);
    let (agent, _cancel) = Agent::spawn(config, dir.path().to_path_buf(), tx);
    agent
        .send(AgentCommand::Prompt {
            text: "Count from 1 to 1000 separated by commas. No prose.".to_string(),
        })
        .unwrap();

    let mut saw_retry_note = false;
    let mut saw_recovery_failure = false;
    let outcome = tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            match rx.recv().await.unwrap() {
                AgentEvent::Info(msg) if msg.contains("compacting and retrying") => {
                    saw_retry_note = true;
                }
                AgentEvent::Error(msg) if msg.contains("compact-and-retry") => {
                    saw_recovery_failure = true;
                }
                AgentEvent::Idle => return,
                _ => {}
            }
        }
    })
    .await;

    outcome.expect("timed out");
    assert!(saw_retry_note, "expected one compact-and-retry note");
    assert!(
        saw_recovery_failure,
        "expected recovery failure notice if the retry truncated again"
    );
}
