use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::agent::{AgentCommand, AgentEvent};
use crate::config::{Config, DiscordConfig};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
const GATEWAY_QUERY: &str = "v=10&encoding=json";

const GUILDS: u64 = 1 << 0;
const GUILD_MESSAGE_CONTENT: u64 = 1 << 10;
const GUILD_MESSAGES: u64 = 1 << 11;
const DIRECT_MESSAGES: u64 = 1 << 12;
const INTENTS: u64 = GUILDS | GUILD_MESSAGE_CONTENT | GUILD_MESSAGES | DIRECT_MESSAGES;
const DISALLOWED_INTENTS: u16 = 4013;

const DISCORD_MESSAGE_LIMIT: usize = 2000;

const SENDS_PER_WINDOW: usize = 4;
const SEND_WINDOW: Duration = Duration::from_secs(5);
const OUTBOX_DEPTH: usize = 16;
const MAX_SEND_ATTEMPTS: u32 = 3;
const RETRY_WAIT: Duration = Duration::from_secs(1);
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

pub fn should_respond(author: &Value, channel_id: &str, allowed_user_id: &str) -> bool {
    if author.get("bot").and_then(|b| b.as_bool()).unwrap_or(false) {
        return false;
    }
    if !allowed_user_id.is_empty() {
        let id = author.get("id").and_then(|i| i.as_str()).unwrap_or("");
        if id != allowed_user_id {
            return false;
        }
    }
    let _ = channel_id;
    true
}

pub fn chunk_message(text: &str) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut rest = text;
    while rest.len() > DISCORD_MESSAGE_LIMIT {
        let mut cut = DISCORD_MESSAGE_LIMIT;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        chunks.push(rest[..cut].to_string());
        rest = rest[cut..].trim_start();
    }
    if !rest.is_empty() {
        chunks.push(rest.to_string());
    }
    chunks
}

pub fn push_chunks(outbox: &mpsc::Sender<Posted>, channel_id: &str, chunks: &[String]) -> usize {
    let mut dropped = 0;
    for text in chunks {
        let posted = Posted {
            channel_id: channel_id.to_string(),
            text: text.clone(),
        };
        if outbox.try_send(posted).is_err() {
            dropped += 1;
        }
    }
    dropped
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GatewayOp {
    Hello { heartbeat_ms: u64 },
    HeartbeatRequest,
    Ack,
    Invalid { resumable: bool },
    Dispatch,
    Ignored,
}

pub fn classify(value: &Value) -> GatewayOp {
    match value["op"].as_u64() {
        Some(0) if value["t"].is_string() => GatewayOp::Dispatch,
        Some(1) => GatewayOp::HeartbeatRequest,
        Some(9) => GatewayOp::Invalid {
            resumable: value["d"].as_bool().unwrap_or(true),
        },
        Some(10) => GatewayOp::Hello {
            heartbeat_ms: value["d"]["heartbeat_interval"].as_u64().unwrap_or(0),
        },
        Some(11) => GatewayOp::Ack,
        _ => GatewayOp::Ignored,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reconnect {
    Resume,
    Fresh,
    Fatal,
}

pub fn reconnect_for_close(code: u16) -> Reconnect {
    match code {
        4004 | 4010 | 4011 | 4013 | 4014 => Reconnect::Fatal,
        4007 | 4008 | 4009 | 4012 => Reconnect::Fresh,
        _ => Reconnect::Resume,
    }
}

#[derive(Debug)]
pub struct GatewayError {
    pub message: String,
    pub reconnect: Reconnect,
    pub close_code: u16,
}

impl GatewayError {
    fn new(message: impl Into<String>, reconnect: Reconnect) -> Self {
        Self {
            message: message.into(),
            reconnect,
            close_code: 0,
        }
    }

    fn closed(message: impl Into<String>, code: u16) -> Self {
        Self {
            message: message.into(),
            reconnect: reconnect_for_close(code),
            close_code: code,
        }
    }
}

/// Discord rejects `IDENTIFY` with `4013` when a privileged intent is not
/// enabled in the developer portal. Dropping it keeps DMs working, so the
/// bridge degrades instead of stopping; there is nothing leaner to give up.
pub fn downgrade_intents(intents: u64, close_code: u16) -> Option<u64> {
    if close_code == DISALLOWED_INTENTS && intents & GUILD_MESSAGE_CONTENT != 0 {
        Some(intents & !GUILD_MESSAGE_CONTENT)
    } else {
        None
    }
}

impl fmt::Display for GatewayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GatewayError {}

impl From<anyhow::Error> for GatewayError {
    fn from(err: anyhow::Error) -> Self {
        GatewayError::new(err.to_string(), Reconnect::Fresh)
    }
}

type Gw<T> = Result<T, GatewayError>;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct GatewaySession {
    pub session_id: Option<String>,
    pub resume_url: Option<String>,
    pub seq: u64,
}

impl GatewaySession {
    fn resumable(&self) -> bool {
        self.session_id.is_some()
    }

    fn clear(&mut self) {
        *self = GatewaySession::default();
    }

    fn endpoint(&self) -> String {
        match &self.resume_url {
            Some(url) => resume_endpoint(url),
            None => GATEWAY_URL.to_string(),
        }
    }
}

pub fn resume_endpoint(base: &str) -> String {
    if base.contains('?') {
        return base.to_string();
    }
    format!("{}/?{}", base.trim_end_matches('/'), GATEWAY_QUERY)
}

#[derive(Debug)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    current: Duration,
}

impl Backoff {
    pub fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max,
            current: initial,
        }
    }

    pub fn plan(&mut self, factor: f64) -> Duration {
        let wait = scale(self.current, factor);
        self.current = (self.current * 2).min(self.max);
        wait
    }

    pub fn reset(&mut self) {
        self.current = self.initial;
    }
}

fn scale(duration: Duration, factor: f64) -> Duration {
    let millis = duration.as_secs_f64() * 1000.0 * factor.clamp(0.0, 2.0);
    Duration::from_millis(millis.max(0.0) as u64)
}

pub fn first_beat_delay(interval: Duration, factor: f64) -> Duration {
    scale(interval, factor.clamp(0.0, 1.0)).max(Duration::from_millis(50))
}

pub fn entropy() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos % 1_000_000) as f64 / 1_000_000.0
}

fn jitter() -> f64 {
    0.5 + 0.5 * entropy()
}

#[derive(Debug)]
pub struct Bucket {
    capacity: usize,
    window: Duration,
    used: usize,
    opened: Option<tokio::time::Instant>,
}

impl Bucket {
    pub fn new(capacity: usize, window: Duration) -> Self {
        Self {
            capacity: capacity.max(1),
            window,
            used: 0,
            opened: None,
        }
    }

    async fn acquire(&mut self) {
        loop {
            match self.opened {
                None => {
                    self.opened = Some(tokio::time::Instant::now());
                    self.used = 1;
                    return;
                }
                Some(opened) => {
                    let elapsed = opened.elapsed();
                    if elapsed >= self.window {
                        self.opened = Some(tokio::time::Instant::now());
                        self.used = 1;
                        return;
                    }
                    if self.used < self.capacity {
                        self.used += 1;
                        return;
                    }
                    tokio::time::sleep(self.window - elapsed).await;
                }
            }
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Retry {
    Stop,
    Wait(Duration),
}

pub fn retry_policy(status: u16, retry_after: Option<&str>, reset_after: Option<&str>) -> Retry {
    if status == 429 {
        let seconds = parse_seconds(retry_after)
            .or_else(|| parse_seconds(reset_after))
            .unwrap_or(RETRY_WAIT.as_secs_f64());
        Retry::Wait(
            Duration::from_secs_f64(seconds).clamp(Duration::from_millis(50), MAX_RETRY_WAIT),
        )
    } else if (500..=599).contains(&status) {
        Retry::Wait(RETRY_WAIT)
    } else {
        Retry::Stop
    }
}

fn parse_seconds(raw: Option<&str>) -> Option<f64> {
    raw?.trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0)
}

/// Decides which Discord channel an agent answer belongs to. Prompts run one
/// at a time in the agent's queue, so the channel each accepted Discord
/// message arrived in is queued and handed back in the same order; answers to
/// prompts that never came through Discord go to the configured channel.
#[derive(Debug)]
pub struct Router {
    waiting: Mutex<VecDeque<String>>,
    fallback: String,
}

impl Router {
    pub fn new(fallback: &str) -> Self {
        Self {
            waiting: Mutex::new(VecDeque::new()),
            fallback: fallback.trim().to_string(),
        }
    }

    pub fn expect_reply(&self, channel_id: &str) {
        let channel_id = channel_id.trim();
        if channel_id.is_empty() {
            return;
        }
        self.lock().push_back(channel_id.to_string());
    }

    pub fn take(&self) -> Option<String> {
        let mut waiting = self.lock();
        match waiting.pop_front() {
            Some(channel_id) => Some(channel_id),
            None => (!self.fallback.is_empty()).then(|| self.fallback.clone()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<String>> {
        self.waiting
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub fn spawn(
    config: Config,
    agent_tx: mpsc::UnboundedSender<AgentCommand>,
    events: broadcast::Sender<AgentEvent>,
) {
    if config.discord.channel_id.is_empty() {
        eprintln!(
            "discord: channel_id is empty, prompts typed in the TUI will not be posted; \
             messages sent to the bot are still answered where they were sent"
        );
    }
    if config.discord.bot_token.trim().is_empty() {
        eprintln!("discord: bot_token is empty, replies will be rejected with 401");
    }
    let router = Arc::new(Router::new(&config.discord.channel_id));
    let (outbox_tx, outbox_rx) = mpsc::channel::<Posted>(OUTBOX_DEPTH);
    tokio::spawn(reply_relay(events.subscribe(), outbox_tx, router.clone()));
    tokio::spawn(Outbox::new(config.discord.clone(), outbox_rx).run());
    tokio::spawn(connect_loop(config, agent_tx, router));
}

async fn connect_loop(
    config: Config,
    agent_tx: mpsc::UnboundedSender<AgentCommand>,
    router: Arc<Router>,
) {
    let mut session = GatewaySession::default();
    let mut backoff = Backoff::new(BACKOFF_START, BACKOFF_MAX);
    let mut intents = INTENTS;
    loop {
        let outcome = gateway_session(
            &config,
            &agent_tx,
            &router,
            &mut session,
            &mut backoff,
            intents,
        )
        .await;
        match outcome {
            Ok(()) => session.clear(),
            Err(error) => {
                if let Some(leaner) = downgrade_intents(intents, error.close_code) {
                    intents = leaner;
                    session.clear();
                    eprintln!(
                        "discord: gateway rejected the privileged Message Content Intent (4013); \
                         reconnecting without it. Enable it on the bot in the Discord developer \
                         portal to read server channel text; DMs keep working."
                    );
                    continue;
                }
                match error.reconnect {
                    Reconnect::Fatal => {
                        eprintln!("discord gateway stopped: {}", error.message);
                        return;
                    }
                    Reconnect::Fresh => {
                        session.clear();
                        eprintln!("discord gateway error: {}", error.message);
                    }
                    Reconnect::Resume => {
                        eprintln!("discord gateway error: {}", error.message);
                    }
                }
            }
        }
        let wait = backoff.plan(jitter());
        eprintln!("discord: reconnecting in {:.1}s", wait.as_secs_f64());
        tokio::time::sleep(wait).await;
    }
}

async fn gateway_session(
    config: &Config,
    agent_tx: &mpsc::UnboundedSender<AgentCommand>,
    router: &Router,
    session: &mut GatewaySession,
    backoff: &mut Backoff,
    intents: u64,
) -> Gw<()> {
    let endpoint = session.endpoint();
    let want_resume = session.resumable();
    let (mut ws, _) = connect_async(&endpoint)
        .await
        .map_err(|e| GatewayError::new(e.to_string(), reconnect_after_connect(want_resume)))?;

    let hello: Value = read_json(&mut ws).await?;
    let GatewayOp::Hello { heartbeat_ms } = classify(&hello) else {
        return Err(GatewayError::new(
            "expected HELLO from gateway",
            Reconnect::Fresh,
        ));
    };
    if heartbeat_ms == 0 {
        return Err(GatewayError::new(
            "no heartbeat_interval in HELLO",
            Reconnect::Fresh,
        ));
    }
    let interval = Duration::from_millis(heartbeat_ms);

    if want_resume {
        let resume = json!({
            "op": 6,
            "d": {
                "token": config.discord.bot_token,
                "session_id": session.session_id,
                "seq": session.seq,
            }
        });
        send_json(&mut ws, &resume).await?;
    } else {
        let identify = json!({
            "op": 2,
            "d": {
                "token": config.discord.bot_token,
                "intents": intents,
                "properties": { "os": "linux", "browser": "rho", "device": "rho" },
            }
        });
        send_json(&mut ws, &identify).await?;
    }

    let handshake = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake(&mut ws, session)).await;
    match handshake {
        Ok(Ok(status)) => eprintln!("discord gateway {status}"),
        Ok(Err(error)) => return Err(error),
        Err(_) => {
            return Err(GatewayError::new(
                "timed out waiting for READY/RESUMED",
                Reconnect::Fresh,
            ))
        }
    }
    backoff.reset();

    let first = first_beat_delay(interval, entropy());
    let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + first, interval);
    let mut awaiting_ack = false;

    loop {
        tokio::select! {
            _ = beat.tick() => {
                if awaiting_ack {
                    return Err(GatewayError::new("heartbeat was not acknowledged", Reconnect::Resume));
                }
                heartbeat(&mut ws, session.seq).await?;
                awaiting_ack = true;
            }
            frame = ws.next() => {
                let frame = frame.ok_or_else(|| GatewayError::new("gateway closed", Reconnect::Resume))?;
                match frame.map_err(|e| GatewayError::new(e.to_string(), Reconnect::Resume))? {
                    Message::Text(text) => {
                        let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                        if let Some(seq) = value["s"].as_u64() {
                            session.seq = seq;
                        }
                        match classify(&value) {
                            GatewayOp::Ack => awaiting_ack = false,
                            GatewayOp::HeartbeatRequest => {
                                heartbeat(&mut ws, session.seq).await?;
                            }
                            GatewayOp::Invalid { resumable } => {
                                return Err(GatewayError::new(
                                    "invalid session",
                                    if resumable { Reconnect::Resume } else { Reconnect::Fresh },
                                ));
                            }
                            GatewayOp::Dispatch
                                if value["t"].as_str() == Some("MESSAGE_CREATE") =>
                            {
                                route_message(&config.discord, &value["d"], agent_tx, router);
                            }
                            _ => continue,
                        }
                    }
                    Message::Close(frame) => return Err(close_error(frame)),
                    _ => continue,
                }
            }
        }
    }
}

fn reconnect_after_connect(want_resume: bool) -> Reconnect {
    if want_resume {
        Reconnect::Resume
    } else {
        Reconnect::Fresh
    }
}

async fn handshake(ws: &mut Ws, session: &mut GatewaySession) -> Gw<String> {
    loop {
        let value: Value = read_json(ws).await?;
        if let Some(seq) = value["s"].as_u64() {
            session.seq = seq;
        }
        match classify(&value) {
            GatewayOp::Invalid { resumable } => {
                return Err(GatewayError::new(
                    "invalid session during handshake",
                    if resumable {
                        Reconnect::Resume
                    } else {
                        Reconnect::Fresh
                    },
                ));
            }
            GatewayOp::Dispatch if value["t"].as_str() == Some("READY") => {
                let Some(id) = value["d"]["session_id"].as_str() else {
                    return Err(GatewayError::new(
                        "READY without session_id",
                        Reconnect::Fresh,
                    ));
                };
                session.session_id = Some(id.to_string());
                session.resume_url = value["d"]["resume_gateway_url"]
                    .as_str()
                    .filter(|url| !url.is_empty())
                    .map(|url| url.to_string());
                return Ok("identified".to_string());
            }
            GatewayOp::Dispatch if value["t"].as_str() == Some("RESUMED") => {
                return Ok("resumed session".to_string());
            }
            _ => continue,
        }
    }
}

fn route_message(
    config: &DiscordConfig,
    d: &Value,
    agent_tx: &mpsc::UnboundedSender<AgentCommand>,
    router: &Router,
) {
    let channel_id = d["channel_id"].as_str().unwrap_or("");
    let relevant =
        config.channel_id.is_empty() || config.channel_id == channel_id || d["guild_id"].is_null();
    if !relevant {
        return;
    }
    if !should_respond(&d["author"], channel_id, &config.allowed_user_id) {
        return;
    }
    let content = d["content"].as_str().unwrap_or("").trim();
    if content.is_empty() {
        return;
    }
    if agent_tx
        .send(AgentCommand::Prompt {
            text: content.to_string(),
        })
        .is_ok()
    {
        router.expect_reply(channel_id);
    }
}

async fn heartbeat(ws: &mut Ws, seq: u64) -> Gw<()> {
    send_json(ws, &json!({ "op": 1, "d": seq })).await
}

async fn send_json(ws: &mut Ws, payload: &Value) -> Gw<()> {
    ws.send(Message::Text(payload.to_string().into()))
        .await
        .map_err(|e| GatewayError::new(format!("send failed: {e}"), Reconnect::Resume))
}

fn close_error(
    frame: Option<tokio_tungstenite::tungstenite::protocol::CloseFrame>,
) -> GatewayError {
    let (code, reason) = match &frame {
        Some(frame) => (u16::from(frame.code), frame.reason.to_string()),
        None => (0, String::new()),
    };
    GatewayError::closed(format!("gateway closed ({code}) {reason}"), code)
}

async fn read_json<T: serde::de::DeserializeOwned>(ws: &mut Ws) -> anyhow::Result<T> {
    while let Some(frame) = ws.next().await {
        match frame? {
            Message::Text(text) => {
                if let Ok(value) = serde_json::from_str::<T>(text.as_str()) {
                    return Ok(value);
                }
            }
            Message::Close(frame) => anyhow::bail!("gateway closed: {frame:?}"),
            _ => {}
        }
    }
    anyhow::bail!("gateway stream ended")
}

async fn reply_relay(
    mut events: broadcast::Receiver<AgentEvent>,
    outbox: mpsc::Sender<Posted>,
    router: Arc<Router>,
) {
    loop {
        let text = match events.recv().await {
            Ok(AgentEvent::AssistantDone { text }) | Ok(AgentEvent::Error(text)) => text,
            Ok(_) => continue,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        };
        let Some(channel_id) = router.take() else {
            continue;
        };
        let dropped = push_chunks(&outbox, &channel_id, &chunk_message(&text));
        if dropped > 0 {
            eprintln!("discord: dropped {dropped} reply chunk(s), outbox full");
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Posted {
    pub channel_id: String,
    pub text: String,
}

trait Poster {
    async fn post(&self, channel_id: &str, text: &str) -> Result<(), (String, Retry)>;
}

#[derive(Debug, PartialEq)]
pub enum Delivery {
    Sent,
    Failed(String),
    GaveUp { attempts: u32, message: String },
}

async fn deliver<P: Poster + ?Sized>(
    post: &P,
    bucket: &mut Bucket,
    channel_id: &str,
    text: &str,
) -> Delivery {
    let mut attempts = 0;
    loop {
        bucket.acquire().await;
        attempts += 1;
        match post.post(channel_id, text).await {
            Ok(()) => return Delivery::Sent,
            Err((message, Retry::Stop)) => return Delivery::Failed(message),
            Err((message, Retry::Wait(wait))) => {
                if attempts >= MAX_SEND_ATTEMPTS {
                    return Delivery::GaveUp { attempts, message };
                }
                tokio::time::sleep(wait).await;
            }
        }
    }
}

struct Outbox {
    poster: RestPoster,
    bucket: Bucket,
    outbox: mpsc::Receiver<Posted>,
}

impl Outbox {
    fn new(config: DiscordConfig, outbox: mpsc::Receiver<Posted>) -> Self {
        Self {
            poster: RestPoster {
                http: reqwest::Client::new(),
                config,
            },
            bucket: Bucket::new(SENDS_PER_WINDOW, SEND_WINDOW),
            outbox,
        }
    }

    async fn run(mut self) {
        while let Some(posted) = self.outbox.recv().await {
            match deliver(
                &self.poster,
                &mut self.bucket,
                &posted.channel_id,
                &posted.text,
            )
            .await
            {
                Delivery::Sent => {}
                Delivery::Failed(message) => eprintln!("discord send failed: {message}"),
                Delivery::GaveUp { attempts, message } => {
                    eprintln!("discord send failed after {attempts} attempts: {message}")
                }
            }
        }
    }
}

struct RestPoster {
    http: reqwest::Client,
    config: DiscordConfig,
}

pub fn send_url(channel_id: &str) -> String {
    format!(
        "https://discord.com/api/v10/channels/{}/messages",
        channel_id.trim()
    )
}

pub fn auth_header(bot_token: &str) -> String {
    format!("Bot {}", bot_token.trim())
}

impl Poster for RestPoster {
    async fn post(&self, channel_id: &str, text: &str) -> Result<(), (String, Retry)> {
        let response = self
            .http
            .post(send_url(channel_id))
            .header(
                reqwest::header::AUTHORIZATION,
                auth_header(&self.config.bot_token),
            )
            .json(&json!({ "content": text }))
            .send()
            .await
            .map_err(|e| (e.to_string(), Retry::Wait(RETRY_WAIT)))?;
        let status = response.status().as_u16();
        if (200..=299).contains(&status) {
            return Ok(());
        }
        let retry_after = header(response.headers(), "retry-after");
        let reset_after = header(response.headers(), "x-ratelimit-reset-after");
        let body = response.text().await.unwrap_or_default();
        Err((
            format!("discord REST {status}: {}", body.trim()),
            retry_policy(status, retry_after.as_deref(), reset_after.as_deref()),
        ))
    }
}

fn header(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;

    fn config(channel_id: &str) -> DiscordConfig {
        DiscordConfig {
            enabled: true,
            bot_token: "tok".to_string(),
            channel_id: channel_id.to_string(),
            allowed_user_id: String::new(),
        }
    }

    #[test]
    fn ignores_bots_and_wrong_users() {
        let bot = json!({"bot": true, "id": "1"});
        let user = json!({"bot": false, "id": "42"});
        assert!(!should_respond(&bot, "c", ""));
        assert!(should_respond(&user, "c", ""));
        assert!(should_respond(&user, "c", "42"));
        assert!(!should_respond(&user, "c", "99"));
    }

    #[test]
    fn chunks_long_messages() {
        let text = "x".repeat(4500);
        let chunks = chunk_message(&text);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 2000);
        assert_eq!(chunks[2].len(), 500);
        assert!(chunk_message("  ").is_empty());
        assert_eq!(chunk_message("hi"), vec!["hi".to_string()]);
    }

    #[test]
    fn chunks_respect_char_boundaries() {
        let text = "é".repeat(1500);
        let chunks = chunk_message(&text);
        assert_eq!(chunks.concat(), text);
        assert_eq!(
            chunks.iter().map(|c| c.chars().count()).sum::<usize>(),
            1500
        );
    }

    #[test]
    fn classifies_gateway_opcodes() {
        assert_eq!(
            classify(&json!({"op":10,"d":{"heartbeat_interval":41250}})),
            GatewayOp::Hello {
                heartbeat_ms: 41250
            }
        );
        assert_eq!(classify(&json!({"op":11})), GatewayOp::Ack);
        assert_eq!(classify(&json!({"op":1})), GatewayOp::HeartbeatRequest);
        assert_eq!(
            classify(&json!({"op":9,"d":true})),
            GatewayOp::Invalid { resumable: true }
        );
        assert_eq!(
            classify(&json!({"op":9,"d":false})),
            GatewayOp::Invalid { resumable: false }
        );
        assert_eq!(
            classify(&json!({"op":0,"t":"MESSAGE_CREATE","d":{}})),
            GatewayOp::Dispatch
        );
        assert_eq!(classify(&json!({"op":7})), GatewayOp::Ignored);
        assert_eq!(
            classify(&json!({"op":10,"d":{}})),
            GatewayOp::Hello { heartbeat_ms: 0 }
        );
    }

    #[test]
    fn close_codes_pick_reconnect_mode() {
        assert_eq!(reconnect_for_close(4004), Reconnect::Fatal);
        assert_eq!(reconnect_for_close(4014), Reconnect::Fatal);
        assert_eq!(reconnect_for_close(4009), Reconnect::Fresh);
        assert_eq!(reconnect_for_close(4007), Reconnect::Fresh);
        assert_eq!(reconnect_for_close(1006), Reconnect::Resume);
        assert_eq!(reconnect_for_close(4000), Reconnect::Resume);
    }

    #[test]
    fn resume_endpoint_keeps_query_and_adds_when_missing() {
        assert_eq!(
            resume_endpoint("wss://gateway.discord.gg"),
            "wss://gateway.discord.gg/?v=10&encoding=json"
        );
        assert_eq!(
            resume_endpoint("wss://gateway-4.discord.gg/?v=10&encoding=json"),
            "wss://gateway-4.discord.gg/?v=10&encoding=json"
        );
    }

    #[test]
    fn session_endpoint_falls_back_to_primary() {
        let mut session = GatewaySession::default();
        assert_eq!(session.endpoint(), GATEWAY_URL);
        assert!(!session.resumable());
        session.session_id = Some("s".to_string());
        session.resume_url = Some("wss://gateway-2.discord.gg".to_string());
        assert!(session.resumable());
        assert_eq!(
            session.endpoint(),
            "wss://gateway-2.discord.gg/?v=10&encoding=json"
        );
        session.clear();
        assert_eq!(session, GatewaySession::default());
    }

    #[test]
    fn backoff_doubles_caps_and_resets() {
        let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));
        assert_eq!(backoff.plan(1.0), Duration::from_secs(1));
        assert_eq!(backoff.plan(1.0), Duration::from_secs(2));
        assert_eq!(backoff.plan(1.0), Duration::from_secs(4));
        assert_eq!(backoff.plan(1.0), Duration::from_secs(8));
        assert_eq!(backoff.plan(1.0), Duration::from_secs(16));
        assert_eq!(backoff.plan(1.0), Duration::from_secs(30));
        assert_eq!(backoff.plan(1.0), Duration::from_secs(30));
        backoff.reset();
        assert_eq!(backoff.plan(1.0), Duration::from_secs(1));
    }

    #[test]
    fn jitter_scales_without_going_negative() {
        assert_eq!(
            scale(Duration::from_secs(4), 0.5),
            Duration::from_millis(2000)
        );
        assert_eq!(scale(Duration::from_secs(4), -3.0), Duration::ZERO);
        assert_eq!(
            first_beat_delay(Duration::from_millis(41250), 1.0),
            Duration::from_millis(41250)
        );
        assert_eq!(
            first_beat_delay(Duration::from_millis(10), 0.0),
            Duration::from_millis(50)
        );
        assert!((0.0..1.0).contains(&entropy()));
    }

    #[tokio::test(start_paused = true)]
    async fn bucket_allows_burst_then_waits_for_window() {
        let mut bucket = Bucket::new(4, Duration::from_secs(5));
        let start = tokio::time::Instant::now();
        for _ in 0..4 {
            bucket.acquire().await;
        }
        assert_eq!(start.elapsed(), Duration::ZERO);
        bucket.acquire().await;
        assert!(start.elapsed() >= Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn bucket_window_reopens_after_idle() {
        let mut bucket = Bucket::new(2, Duration::from_secs(5));
        bucket.acquire().await;
        bucket.acquire().await;
        tokio::time::sleep(Duration::from_secs(6)).await;
        let start = tokio::time::Instant::now();
        bucket.acquire().await;
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[test]
    fn retry_policy_follows_discord_headers() {
        assert_eq!(
            retry_policy(429, Some("2.5"), None),
            Retry::Wait(Duration::from_secs_f64(2.5))
        );
        assert_eq!(
            retry_policy(429, None, Some("0.4")),
            Retry::Wait(Duration::from_millis(400))
        );
        assert_eq!(
            retry_policy(429, Some("nonsense"), None),
            Retry::Wait(RETRY_WAIT)
        );
        assert_eq!(
            retry_policy(429, Some("600"), None),
            Retry::Wait(MAX_RETRY_WAIT)
        );
        assert_eq!(retry_policy(500, None, None), Retry::Wait(RETRY_WAIT));
        assert_eq!(retry_policy(502, None, None), Retry::Wait(RETRY_WAIT));
        assert_eq!(retry_policy(403, Some("5"), None), Retry::Stop);
        assert_eq!(retry_policy(401, None, None), Retry::Stop);
        assert_eq!(retry_policy(200, None, None), Retry::Stop);
    }

    #[tokio::test]
    async fn outbox_drops_instead_of_blocking() {
        let (tx, mut rx) = mpsc::channel(2);
        let chunks = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(push_chunks(&tx, "9", &chunks), 1);
        assert_eq!(rx.len(), 2);
        assert_eq!(
            rx.try_recv().unwrap(),
            Posted {
                channel_id: "9".to_string(),
                text: "a".to_string()
            }
        );
        assert_eq!(rx.len(), 1);

        drop(rx);
        assert_eq!(push_chunks(&tx, "9", &chunks), chunks.len());
    }

    struct ScriptedPoster {
        script: RefCell<VecDeque<Result<(), (String, Retry)>>>,
        attempts: Cell<u32>,
    }

    impl ScriptedPoster {
        fn new(script: Vec<Result<(), (String, Retry)>>) -> Self {
            Self {
                script: RefCell::new(script.into()),
                attempts: Cell::new(0),
            }
        }

        fn wait(message: &str, seconds: f64) -> Result<(), (String, Retry)> {
            Err((
                message.to_string(),
                Retry::Wait(Duration::from_secs_f64(seconds)),
            ))
        }
    }

    impl Poster for ScriptedPoster {
        async fn post(&self, _channel_id: &str, _text: &str) -> Result<(), (String, Retry)> {
            self.attempts.set(self.attempts.get() + 1);
            self.script
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err(("script empty".to_string(), Retry::Stop)))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn send_retries_until_the_rate_limit_clears() {
        let poster = ScriptedPoster::new(vec![
            ScriptedPoster::wait("discord REST 429: slow down", 2.0),
            ScriptedPoster::wait("discord REST 502: bad gateway", 1.0),
            Ok(()),
        ]);
        let mut bucket = Bucket::new(SENDS_PER_WINDOW, SEND_WINDOW);
        assert_eq!(
            deliver(&poster, &mut bucket, "9", "hi").await,
            Delivery::Sent
        );
        assert_eq!(poster.attempts.get(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn send_gives_up_after_max_attempts() {
        let poster = ScriptedPoster::new(vec![
            ScriptedPoster::wait("discord REST 429: slow down", 1.0),
            ScriptedPoster::wait("discord REST 429: slow down", 1.0),
            ScriptedPoster::wait("discord REST 429: slow down", 1.0),
        ]);
        let mut bucket = Bucket::new(SENDS_PER_WINDOW, SEND_WINDOW);
        assert_eq!(
            deliver(&poster, &mut bucket, "9", "hi").await,
            Delivery::GaveUp {
                attempts: MAX_SEND_ATTEMPTS,
                message: "discord REST 429: slow down".to_string(),
            }
        );
        assert_eq!(poster.attempts.get(), MAX_SEND_ATTEMPTS);
    }

    #[tokio::test(start_paused = true)]
    async fn client_errors_are_not_retried() {
        let poster = ScriptedPoster::new(vec![Err((
            "discord REST 403: missing permissions".to_string(),
            Retry::Stop,
        ))]);
        let mut bucket = Bucket::new(SENDS_PER_WINDOW, SEND_WINDOW);
        assert_eq!(
            deliver(&poster, &mut bucket, "9", "hi").await,
            Delivery::Failed("discord REST 403: missing permissions".to_string())
        );
        assert_eq!(poster.attempts.get(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn every_attempt_costs_a_token() {
        let poster = ScriptedPoster::new(vec![Ok(()), Ok(()), Ok(()), Ok(()), Ok(())]);
        let mut bucket = Bucket::new(2, Duration::from_secs(5));
        let start = tokio::time::Instant::now();
        for _ in 0..5 {
            assert_eq!(
                deliver(&poster, &mut bucket, "9", "hi").await,
                Delivery::Sent
            );
        }
        assert_eq!(poster.attempts.get(), 5);
        assert!(start.elapsed() >= Duration::from_secs(5));
    }

    #[test]
    fn rest_uses_the_bot_authorization_scheme() {
        assert_eq!(auth_header("tok"), "Bot tok");
        assert_eq!(auth_header("  tok \n"), "Bot tok");
        assert_eq!(
            send_url("123"),
            "https://discord.com/api/v10/channels/123/messages"
        );
    }

    #[test]
    fn routes_only_relevant_messages() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let router = Router::new("7");
        let message = json!({
            "channel_id": "9", "guild_id": "1", "content":  "  build it  ",
            "author": {"bot": false, "id": "42"}
        });

        route_message(&config("9"), &message, &tx, &router);
        assert_eq!(
            rx.try_recv().unwrap().text_or_debug(),
            "build it".to_string()
        );

        route_message(&config("7"), &message, &tx, &router);
        assert!(rx.try_recv().is_err());

        let dm = json!({
            "channel_id": "9", "content": "dm prompt", "author": {"bot": false, "id": "42"}
        });
        route_message(&config("7"), &dm, &tx, &router);
        assert_eq!(
            rx.try_recv().unwrap().text_or_debug(),
            "dm prompt".to_string()
        );

        let restricted = DiscordConfig {
            allowed_user_id: "1".to_string(),
            ..config("9")
        };
        route_message(&restricted, &message, &tx, &router);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn routed_messages_retarget_the_reply() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let router = Router::new("7");
        let guild = json!({
            "channel_id": "9", "guild_id": "1", "content": "build it",
            "author": {"bot": false, "id": "42"}
        });

        assert_eq!(router.take().as_deref(), Some("7"));
        route_message(&config("9"), &guild, &tx, &router);
        assert_eq!(rx.try_recv().unwrap().text_or_debug(), "build it");
        assert_eq!(router.take().as_deref(), Some("9"));

        let dm = json!({
            "channel_id": "77", "content": "hello", "author": {"bot": false, "id": "42"}
        });
        route_message(&config("9"), &dm, &tx, &router);
        assert_eq!(rx.try_recv().unwrap().text_or_debug(), "hello");
        assert_eq!(router.take().as_deref(), Some("77"));
        assert_eq!(router.take().as_deref(), Some("7"));
    }

    #[test]
    fn ignored_messages_do_not_claim_an_answer() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let router = Router::new("7");

        let other_channel = json!({
            "channel_id": "9", "guild_id": "1", "content": "nope",
            "author": {"bot": false, "id": "42"}
        });
        route_message(&config("7"), &other_channel, &tx, &router);

        let blank = json!({
            "channel_id": "77", "guild_id": "1", "content": "",
            "author": {"bot": false, "id": "42"}
        });
        route_message(&config("7"), &blank, &tx, &router);

        let from_bot = json!({
            "channel_id": "77", "guild_id": "1", "content": "echo",
            "author": {"bot": true, "id": "42"}
        });
        route_message(&config("7"), &from_bot, &tx, &router);

        assert!(rx.try_recv().is_err());
        assert_eq!(router.take().as_deref(), Some("7"));
    }

    #[test]
    fn answers_go_back_to_the_channel_that_asked() {
        let router = Router::new("7");
        assert_eq!(router.take().as_deref(), Some("7"));

        router.expect_reply("9");
        assert_eq!(router.take().as_deref(), Some("9"));
        assert_eq!(router.take().as_deref(), Some("7"));

        router.expect_reply("9");
        router.expect_reply("99");
        assert_eq!(router.take().as_deref(), Some("9"));
        assert_eq!(router.take().as_deref(), Some("99"));
        assert_eq!(router.take().as_deref(), Some("7"));
    }

    #[test]
    fn without_a_configured_channel_only_discord_gets_answers() {
        let router = Router::new("");
        assert_eq!(router.take(), None);
        router.expect_reply("9");
        assert_eq!(router.take().as_deref(), Some("9"));
        assert_eq!(router.take(), None);

        router.expect_reply("   ");
        assert_eq!(router.take(), None);
    }

    #[test]
    fn identify_asks_for_the_message_content_intent() {
        assert_eq!(INTENTS & GUILD_MESSAGE_CONTENT, GUILD_MESSAGE_CONTENT);
        assert_eq!(INTENTS & DIRECT_MESSAGES, DIRECT_MESSAGES);
        assert_eq!(INTENTS & GUILD_MESSAGES, GUILD_MESSAGES);
    }

    #[test]
    fn disallowed_intents_drops_only_the_privileged_bit() {
        let leaner = downgrade_intents(INTENTS, DISALLOWED_INTENTS);
        assert_eq!(leaner, Some(INTENTS & !GUILD_MESSAGE_CONTENT));
        assert_eq!(downgrade_intents(leaner.unwrap(), DISALLOWED_INTENTS), None);
        assert_eq!(downgrade_intents(INTENTS, 4014), None);
        assert_eq!(downgrade_intents(INTENTS, 0), None);
    }

    #[test]
    fn close_errors_carry_the_code_that_decides_reconnect() {
        let frame = CloseFrame {
            code: CloseCode::from(DISALLOWED_INTENTS),
            reason: "Disallowed intents".into(),
        };
        let error = close_error(Some(frame));
        assert_eq!(error.close_code, DISALLOWED_INTENTS);
        assert_eq!(error.reconnect, Reconnect::Fatal);
        assert_eq!(close_error(None).close_code, 0);
    }

    impl AgentCommand {
        fn text_or_debug(&self) -> String {
            match self {
                AgentCommand::Prompt { text } => text.clone(),
                other => format!("{other:?}"),
            }
        }
    }
}
