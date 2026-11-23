use std::collections::{HashMap, VecDeque};
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

const EYES: &str = "\u{1f440}";
const SIGNAL_DEPTH: usize = 32;
const SIGNALS_PER_WINDOW: usize = 4;
const SIGNAL_WINDOW: Duration = Duration::from_secs(5);
const TYPING_TICK: Duration = Duration::from_secs(8);
/// The deadline to beat is shorter than the tick that checks it, so a ping that
/// lands a moment after a tick is still due on the next one instead of drifting
/// past Discord's own 10 s expiry.
const TYPING_REFRESH: Duration = Duration::from_secs(6);
const TYPING_CAP: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ignore {
    FromBot,
    DisallowedUser(String),
    NoText,
    /// The message was accepted but never reached the agent, so no answer will
    /// ever come for it.
    NotDelivered(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound {
    Prompted { channel_id: String, text: String },
    Ignored(Ignore),
}

/// Any non-bot message from the allowed user is a prompt, in DMs and in server
/// channels alike. `channel_id` in the config only says where answers to
/// prompts with no Discord origin go, so no channel has to be configured for
/// the bot to hear it.
pub fn accept_message(d: &Value, allowed_user_id: &str) -> Inbound {
    let author = &d["author"];
    if author.get("bot").and_then(|b| b.as_bool()).unwrap_or(false) {
        return Inbound::Ignored(Ignore::FromBot);
    }
    let user_id = author.get("id").and_then(|i| i.as_str()).unwrap_or("");
    if !allowed_user_id.is_empty() && user_id != allowed_user_id {
        return Inbound::Ignored(Ignore::DisallowedUser(user_id.to_string()));
    }
    let text = d["content"].as_str().unwrap_or("").trim();
    if text.is_empty() {
        return Inbound::Ignored(Ignore::NoText);
    }
    Inbound::Prompted {
        channel_id: d["channel_id"].as_str().unwrap_or("").to_string(),
        text: text.to_string(),
    }
}

pub fn is_guild_message(d: &Value) -> bool {
    !d["guild_id"].is_null()
}

/// One-line hints for the failures that otherwise look like "the bot is
/// connected but ignores me"; each is printed once per gateway session.
#[derive(Debug, Default)]
pub struct Notices {
    missing_text: bool,
    disallowed_user: bool,
    unreadable_frame: bool,
    binary_frame: bool,
}

impl Notices {
    pub fn notice(&mut self, inbound: &Inbound, guild: bool) -> Option<String> {
        match inbound {
            Inbound::Ignored(Ignore::NoText) if guild && !self.missing_text => {
                self.missing_text = true;
                Some(
                    "discord: a server message arrived with no text, so Discord is not applying \
                     the Message Content Intent for this session; switch it on under Bot -> \
                     Privileged Gateway Intents in the developer portal"
                        .to_string(),
                )
            }
            Inbound::Ignored(Ignore::DisallowedUser(id)) if !self.disallowed_user => {
                self.disallowed_user = true;
                Some(format!(
                    "discord: ignoring messages from user id {id}; it does not match \
                     allowed_user_id (clear allowed_user_id to accept anyone)"
                ))
            }
            _ => None,
        }
    }

    /// A frame the bridge could not read at all. Nothing else says why a
    /// connected gateway appears to deliver nothing, so it is said once here.
    pub fn unreadable_frame(&mut self) -> Option<String> {
        (!self.unreadable_frame).then(|| {
            self.unreadable_frame = true;
            "discord: a gateway frame was not valid JSON, so it was skipped; if every frame is \
             like this the gateway is not speaking encoding=json"
                .to_string()
        })
    }

    pub fn binary_frame(&mut self) -> Option<String> {
        (!self.binary_frame).then(|| {
            self.binary_frame = true;
            "discord: the gateway sent a binary frame, which rho does not read; it only speaks \
             JSON, so a compressed or payload-encoded session delivers no messages"
                .to_string()
        })
    }
}

pub fn guild_summary(d: &Value) -> String {
    let name = d["name"].as_str().unwrap_or("unnamed");
    let id = d["id"].as_str().unwrap_or("?");
    let state = if d["unavailable"].as_bool().unwrap_or(false) {
        "unavailable"
    } else {
        "available"
    };
    format!("discord: server \"{name}\" ({id}) {state}")
}

/// Where an inbound message came from, in the words the transcript shows.
pub fn arrival(d: &Value, inbound: &Inbound, server: Option<&str>) -> String {
    let author = d["author"]["username"]
        .as_str()
        .or_else(|| d["author"]["id"].as_str())
        .unwrap_or("unknown");
    let channel = d["channel_id"].as_str().unwrap_or("?");
    let place = match server {
        Some(name) => format!("server \"{name}\" #{channel}"),
        None => format!("DM #{channel}"),
    };
    let who = format!("@{author} in {place}");
    match inbound {
        Inbound::Prompted { text, .. } => {
            format!("{who}: {} — queued as a prompt", quoted(text))
        }
        Inbound::Ignored(Ignore::FromBot) => format!("{who}: ignored, the author is a bot"),
        Inbound::Ignored(Ignore::DisallowedUser(id)) => {
            format!("{who}: ignored, user id {id} is not allowed_user_id")
        }
        Inbound::Ignored(Ignore::NoText) => {
            format!("{who}: arrived with no text content, so nothing was queued")
        }
        Inbound::Ignored(Ignore::NotDelivered(reason)) => {
            format!("{who}: not queued — {reason}")
        }
    }
}

const QUOTED_MAX: usize = 60;

/// Says how many of a courtesy Discord refused, so the count in the report is
/// not read as the number the user actually saw.
fn refusal_note(errors: usize) -> String {
    match errors {
        0 => String::new(),
        1 => " (1 refused)".to_string(),
        errors => format!(" ({errors} refused)"),
    }
}

fn quoted(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    let mut chars = flat.chars();
    let shortened: String = chars.by_ref().take(QUOTED_MAX).collect();
    if chars.next().is_some() {
        format!("\"{shortened}…\"")
    } else {
        format!("\"{shortened}\"")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub id: String,
    pub name: String,
    pub available: bool,
}

impl Server {
    fn label(&self) -> String {
        if self.available {
            format!("\"{}\" ({})", self.name, self.id)
        } else {
            format!("\"{}\" ({} unavailable)", self.name, self.id)
        }
    }
}

/// One line of the `/discord` report. `fault` says the line is reporting
/// something broken, so the transcript draws it as an error instead of as
/// routine news.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    pub text: String,
    pub fault: bool,
}

impl StatusLine {
    fn note(text: String) -> Self {
        Self { text, fault: false }
    }

    fn fault(text: String) -> Self {
        Self { text, fault: true }
    }
}

/// What the bridge knows about the live connection. The TUI shows it through
/// `/discord` because stderr is invisible while the transcript owns the screen.
#[derive(Debug, Default)]
pub struct Status {
    pub state: String,
    pub state_fault: bool,
    pub granted_intents: Option<u64>,
    pub message_content_granted: bool,
    pub servers: Vec<Server>,
    pub messages_seen: usize,
    pub bot_messages_seen: usize,
    pub prompts_queued: usize,
    pub replies_posted: usize,
    pub reactions: usize,
    pub reaction_errors: usize,
    pub typing: usize,
    pub typing_errors: usize,
    pub last_inbound: Option<String>,
    pub errors: usize,
    pub last_error: Option<String>,
}

impl Status {
    fn upsert_server(&mut self, server: Server) {
        match self.servers.iter_mut().find(|s| s.id == server.id) {
            Some(seen) => *seen = server,
            None => self.servers.push(server),
        }
    }

    fn server_name(&self, id: &str) -> Option<String> {
        self.servers
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.name.clone())
    }

    pub fn summary(&self, allowed_user_id: &str, fallback_channel: &str) -> String {
        self.lines(allowed_user_id, fallback_channel)
            .into_iter()
            .map(|line| line.text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn lines(&self, allowed_user_id: &str, fallback_channel: &str) -> Vec<StatusLine> {
        let state = if self.state.is_empty() {
            "connecting to the gateway".to_string()
        } else {
            self.state.clone()
        };
        let servers = if self.servers.is_empty() {
            "no servers visible: invite the bot to the server and give it View Channel (DMs need neither)".to_string()
        } else {
            format!(
                "servers: {}",
                self.servers
                    .iter()
                    .map(Server::label)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let allowed = if allowed_user_id.trim().is_empty() {
            "allowed_user_id is empty: anyone who can reach the bot can drive the agent".to_string()
        } else {
            format!("allowed_user_id: {}", allowed_user_id.trim())
        };
        let fallback = if fallback_channel.trim().is_empty() {
            "no fallback channel: prompts typed in rho are not mirrored to Discord".to_string()
        } else {
            format!("fallback channel: {}", fallback_channel.trim())
        };
        let intents = match self.granted_intents {
            Some(granted) if self.message_content_granted => {
                format!("intents {granted:#06x}, message content granted")
            }
            Some(granted) => {
                format!("intents {granted:#06x}, Message Content Intent NOT granted: server text arrives empty")
            }
            None => "intents not reported yet".to_string(),
        };
        let intents_missing = self.granted_intents.is_some() && !self.message_content_granted;
        let errors = if self.errors > 0 {
            StatusLine::fault(format!(
                "discord: {} error(s) reported, last error: {}",
                self.errors,
                self.last_error
                    .as_deref()
                    .map(|error| error.trim_start_matches("discord: "))
                    .unwrap_or("none")
            ))
        } else {
            StatusLine::note("discord: no errors reported".to_string())
        };
        let line = |text: String, fault: bool| StatusLine { text, fault };
        vec![
            line(format!("discord: {state}"), self.state_fault),
            line(format!("discord: {intents}"), intents_missing),
            line(format!("discord: {servers}"), false),
            line(
                format!(
                    "discord: {} message(s) in ({} from bots), {} queued as prompts, {} reply chunk(s) sent",
                    self.messages_seen, self.bot_messages_seen, self.prompts_queued, self.replies_posted
                ),
                false,
            ),
            line(
                format!(
                    "discord: last message in: {}",
                    self.last_inbound.as_deref().unwrap_or("none")
                ),
                false,
            ),
            line(
                format!(
                    "discord: {} reaction(s) added{}, {} typing indicator(s) sent{}",
                    self.reactions,
                    refusal_note(self.reaction_errors),
                    self.typing,
                    refusal_note(self.typing_errors)
                ),
                false,
            ),
            errors,
            line(format!("discord: {allowed}"), false),
            line(format!("discord: {fallback}"), false),
        ]
    }
}

/// The bridge's voice. A TUI owns the terminal, so its notes go into the
/// transcript as `Info`/`Fault` events; only headless runs (no TUI drawing over
/// stderr) write them to the console.
pub struct Bridge {
    events: broadcast::Sender<AgentEvent>,
    console: bool,
    allowed_user_id: String,
    fallback_channel: String,
    status: Mutex<Status>,
}

impl Bridge {
    pub fn new(
        config: &DiscordConfig,
        events: broadcast::Sender<AgentEvent>,
        console: bool,
    ) -> Self {
        Self {
            events,
            console,
            allowed_user_id: config.allowed_user_id.clone(),
            fallback_channel: config.channel_id.clone(),
            status: Mutex::new(Status {
                state: "connecting to the gateway".to_string(),
                ..Status::default()
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Status> {
        self.status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn emit(&self, fault: bool, message: &str) {
        if self.console {
            eprintln!("{message}");
            return;
        }
        let event = if fault {
            AgentEvent::Fault(message.to_string())
        } else {
            AgentEvent::Info(message.to_string())
        };
        let _ = self.events.send(event);
    }

    pub fn note(&self, message: &str) {
        self.emit(false, message);
    }

    /// Reports a failure the bridge hit: it lands in the transcript as an error
    /// line and is remembered for `/discord`, so a bridge that cannot deliver is
    /// never silent.
    pub fn fail(&self, message: &str) {
        {
            let mut status = self.lock();
            status.errors += 1;
            status.last_error = Some(message.to_string());
        }
        self.emit(true, message);
    }

    pub fn set_state(&self, state: &str, fault: bool) {
        let mut status = self.lock();
        status.state = state.to_string();
        status.state_fault = fault;
    }

    pub fn note_ready(&self, ready: &ReadyInfo) {
        {
            let mut status = self.lock();
            status.state = ready.summary();
            status.state_fault = ready.content_missing();
            status.granted_intents = ready.granted_intents;
            status.message_content_granted = ready
                .granted_intents
                .is_some_and(|granted| granted & GUILD_MESSAGE_CONTENT != 0);
        }
        let line = format!("discord gateway {}", ready.summary());
        if ready.content_missing() {
            self.fail(&line);
        } else {
            self.note(&line);
        }
    }

    pub fn note_server(&self, d: &Value) {
        let server = Server {
            id: d["id"].as_str().unwrap_or("?").to_string(),
            name: d["name"].as_str().unwrap_or("unnamed").to_string(),
            available: !d["unavailable"].as_bool().unwrap_or(false),
        };
        self.lock().upsert_server(server);
        self.note(&guild_summary(d));
    }

    pub fn server_of(&self, d: &Value) -> Option<String> {
        let id = d["guild_id"].as_str()?;
        let status = self.lock();
        Some(status.server_name(id).unwrap_or_else(|| format!("id {id}")))
    }

    /// Says what arrived and what rho did with it, once per message; bot posts
    /// are counted but only mentioned the first time so a busy channel cannot
    /// push the transcript along.
    pub fn note_inbound(&self, d: &Value, inbound: &Inbound) {
        let line = arrival(d, inbound, self.server_of(d).as_deref());
        let fault = matches!(inbound, Inbound::Ignored(Ignore::NotDelivered(_)));
        let mention = {
            let mut status = self.lock();
            status.messages_seen += 1;
            if matches!(inbound, Inbound::Ignored(Ignore::FromBot)) {
                status.bot_messages_seen += 1;
                status.bot_messages_seen == 1
            } else {
                if matches!(inbound, Inbound::Prompted { .. }) {
                    status.prompts_queued += 1;
                }
                status.last_inbound = Some(line.clone());
                true
            }
        };
        if mention {
            if fault {
                self.fail(&line);
            } else {
                self.note(&line);
            }
        }
    }

    pub fn note_answer(&self, channel_id: &str, chunks: usize) {
        let unit = if chunks == 1 { "chunk" } else { "chunks" };
        self.note(&format!(
            "discord: answering #{channel_id} with {chunks} {unit}"
        ));
    }

    pub fn note_reply_posted(&self, chunks: usize) {
        self.lock().replies_posted += chunks;
    }

    pub fn note_reaction(&self) {
        self.lock().reactions += 1;
    }

    /// The first refusal names the permission it wanted, because reactions are
    /// cosmetic and the user needs to know prompts are unaffected; the rest are
    /// only counted, so a busy channel cannot repeat the same complaint.
    pub fn note_reaction_failure(&self, target: &str, message: &str) {
        let first = {
            let mut status = self.lock();
            status.reaction_errors += 1;
            status.reaction_errors == 1
        };
        if first {
            self.fail(&format!(
                "discord: could not add a reaction to {target}: {message}; reactions need the \
                 Add Reactions and Read Message History permissions, prompts and replies work \
                 without them"
            ));
        }
    }

    pub fn note_typing(&self) {
        self.lock().typing += 1;
    }

    pub fn note_typing_failure(&self, target: &str, message: &str) {
        let first = {
            let mut status = self.lock();
            status.typing_errors += 1;
            status.typing_errors == 1
        };
        if first {
            self.fail(&format!(
                "discord: could not show typing in {target}: {message}"
            ));
        }
    }

    pub fn note_send_failure(&self, message: &str) {
        self.fail(&format!("discord: could not post a reply: {message}"));
    }

    pub fn summary(&self) -> String {
        let status = self.lock();
        status.summary(&self.allowed_user_id, &self.fallback_channel)
    }

    /// The `/discord` report, one line at a time and marked with whether it
    /// reports a fault, so the TUI can draw the broken parts as errors.
    pub fn report(&self) -> Vec<StatusLine> {
        let status = self.lock();
        status.lines(&self.allowed_user_id, &self.fallback_channel)
    }
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

/// A courtesy the bridge owes Discord for a prompt it accepted: eyes on the
/// message to say it was seen, and a typing indicator for as long as the
/// answer is still being written. They travel on their own bounded queue with
/// their own rate limit because they are not answers: dropping one must never
/// delay or displace a reply, and the gateway read loop must never wait on
/// Discord to post one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    React {
        channel_id: String,
        message_id: String,
    },
    TypingStart {
        channel_id: String,
    },
    TypingStop {
        channel_id: String,
    },
    /// A command finished without producing an answer (the turn was
    /// interrupted), so the prompt the agent was working on has to stop
    /// claiming an answer is on its way.
    TypingDropOldest,
    TypingClear,
}

/// What an accepted prompt needs from the rest of the bridge: the channel its
/// answer belongs to, and the courtesies Discord is told about while that
/// answer is being written. Both are keyed to the same prompt, so they travel
/// together.
#[derive(Clone)]
pub struct Intake {
    router: Arc<Router>,
    signals: Signals,
}

impl Intake {
    fn new(fallback_channel: &str, signals: Signals) -> Self {
        Self {
            router: Arc::new(Router::new(fallback_channel)),
            signals,
        }
    }

    /// The prompt is on its way to the agent: note where the answer goes, put
    /// eyes on the message and start claiming the channel is being typed in.
    fn claim(&self, channel_id: &str, message_id: &str) {
        self.router.expect_reply(channel_id);
        self.signals.react(channel_id, message_id);
        self.signals.typing_start(channel_id);
    }

    fn take(&self) -> Option<String> {
        self.router.take()
    }
}

/// The handle the gateway and the reply relay push signals through.
#[derive(Clone)]
pub struct Signals {
    inbox: mpsc::Sender<Signal>,
    bridge: Arc<Bridge>,
}

impl Signals {
    fn new(inbox: mpsc::Sender<Signal>, bridge: Arc<Bridge>) -> Self {
        Self { inbox, bridge }
    }

    pub fn react(&self, channel_id: &str, message_id: &str) {
        if channel_id.trim().is_empty() || message_id.trim().is_empty() {
            return;
        }
        self.push(Signal::React {
            channel_id: channel_id.to_string(),
            message_id: message_id.to_string(),
        });
    }

    pub fn typing_start(&self, channel_id: &str) {
        if channel_id.trim().is_empty() {
            return;
        }
        self.push(Signal::TypingStart {
            channel_id: channel_id.to_string(),
        });
    }

    pub fn typing_stop(&self, channel_id: &str) {
        if channel_id.trim().is_empty() {
            return;
        }
        self.push(Signal::TypingStop {
            channel_id: channel_id.to_string(),
        });
    }

    pub fn typing_drop_oldest(&self) {
        self.push(Signal::TypingDropOldest);
    }

    pub fn typing_clear(&self) {
        self.push(Signal::TypingClear);
    }

    /// Best effort by design: a stalled Discord REST call must not back the
    /// agent or the gateway up, so a full queue drops the signal and says so,
    /// because a silent courtesy looks like a bot that ignored you.
    fn push(&self, signal: Signal) {
        if self.inbox.try_send(signal).is_err() {
            self.bridge
                .note("discord: the reaction/typing queue is full, so a signal was dropped");
        }
    }
}

/// The channels the bot is currently "typing" in. An episode is opened per
/// prompt, not per channel, so a second prompt in the same channel keeps the
/// indicator up while the first answer is being posted.
#[derive(Debug, Default)]
pub struct TypingBoard {
    episodes: HashMap<String, Episode>,
}

#[derive(Debug, Clone, Copy)]
struct Episode {
    started: tokio::time::Instant,
    next_ping: tokio::time::Instant,
    owed: u32,
}

impl TypingBoard {
    /// Returns true when this opened a fresh episode, which is pinged at once.
    fn start(&mut self, channel_id: &str, now: tokio::time::Instant) -> bool {
        if let Some(episode) = self.episodes.get_mut(channel_id) {
            episode.owed += 1;
            return false;
        }
        self.episodes.insert(
            channel_id.to_string(),
            Episode {
                started: now,
                next_ping: now,
                owed: 1,
            },
        );
        true
    }

    /// Returns true when the channel stopped typing, which is when the last
    /// prompt owed an answer in it gets one.
    fn stop(&mut self, channel_id: &str) -> bool {
        let owed = match self.episodes.get_mut(channel_id) {
            Some(episode) => {
                episode.owed = episode.owed.saturating_sub(1);
                episode.owed
            }
            None => return false,
        };
        if owed > 0 {
            return false;
        }
        self.episodes.remove(channel_id);
        true
    }

    /// The channels due a ping, and the ones abandoned: the agent only ever
    /// owes an answer while it is busy, so an episode that has outlasted every
    /// plausible turn is a lost stop rather than work in progress.
    fn due(&mut self, now: tokio::time::Instant) -> (Vec<String>, Vec<String>) {
        let abandoned = self
            .episodes
            .iter()
            .filter(|(_, episode)| now.duration_since(episode.started) >= TYPING_CAP)
            .map(|(channel_id, _)| channel_id.clone())
            .collect::<Vec<_>>();
        for channel_id in &abandoned {
            self.episodes.remove(channel_id);
        }
        let mut due = self
            .episodes
            .iter()
            .filter(|(_, episode)| now >= episode.next_ping)
            .map(|(channel_id, _)| channel_id.clone())
            .collect::<Vec<_>>();
        due.sort();
        (due, abandoned)
    }

    fn pinged(&mut self, channel_id: &str, now: tokio::time::Instant) {
        if let Some(episode) = self.episodes.get_mut(channel_id) {
            episode.next_ping = now + TYPING_REFRESH;
        }
    }

    /// Drops the longest-open episode: the turn that ended without an answer
    /// was working the oldest prompt in the queue.
    fn drop_oldest(&mut self) -> Option<String> {
        let oldest = self
            .episodes
            .iter()
            .min_by_key(|(_, episode)| episode.started)?
            .0
            .clone();
        self.episodes.remove(&oldest);
        Some(oldest)
    }

    fn clear(&mut self) -> usize {
        let open = self.episodes.len();
        self.episodes.clear();
        open
    }

    pub fn open(&self) -> usize {
        self.episodes.len()
    }
}

/// Posts the signals: one reaction per accepted prompt, then a typing ping
/// every `TYPING_TICK` while an episode is open, since Discord lets its own
/// "typing" banner expire after about 10 s.
struct SignalTask<P: Poster> {
    poster: P,
    bucket: Bucket,
    bridge: Arc<Bridge>,
    board: TypingBoard,
}

impl SignalTask<RestPoster> {
    fn new(config: DiscordConfig, bridge: Arc<Bridge>) -> SignalTask<RestPoster> {
        SignalTask {
            poster: RestPoster {
                http: reqwest::Client::new(),
                config,
            },
            bucket: Bucket::new(SIGNALS_PER_WINDOW, SIGNAL_WINDOW),
            bridge,
            board: TypingBoard::default(),
        }
    }
}

impl<P: Poster> SignalTask<P> {
    async fn run(mut self, mut inbox: mpsc::Receiver<Signal>) {
        let mut tick = tokio::time::interval(TYPING_TICK);
        loop {
            tokio::select! {
                signal = inbox.recv() => {
                    let Some(signal) = signal else { break };
                    self.handle(signal).await;
                }
                _ = tick.tick() => self.ping_due().await,
            }
        }
    }

    async fn handle(&mut self, signal: Signal) {
        match signal {
            Signal::React {
                channel_id,
                message_id,
            } => {
                let target = format!("#{channel_id}");
                let call = Call::react(&channel_id, &message_id, EYES);
                match deliver(&self.poster, &mut self.bucket, call).await {
                    Delivery::Sent => self.bridge.note_reaction(),
                    Delivery::Failed(message) => {
                        self.bridge.note_reaction_failure(&target, &message)
                    }
                    Delivery::GaveUp { attempts, message } => self.bridge.note_reaction_failure(
                        &target,
                        &format!("{message} (after {attempts} attempts)"),
                    ),
                }
            }
            Signal::TypingStart { channel_id } => {
                if self.board.start(&channel_id, tokio::time::Instant::now()) {
                    self.ping(&channel_id).await;
                }
            }
            Signal::TypingStop { channel_id } => {
                self.board.stop(&channel_id);
            }
            Signal::TypingDropOldest => {
                if let Some(channel_id) = self.board.drop_oldest() {
                    self.bridge.note(&format!(
                        "discord: no answer came for #{channel_id}, so its typing indicator \
                         was dropped"
                    ));
                }
            }
            Signal::TypingClear => {
                self.board.clear();
            }
        }
    }

    async fn ping_due(&mut self) {
        let (due, abandoned) = self.board.due(tokio::time::Instant::now());
        for channel_id in abandoned {
            self.bridge.note(&format!(
                "discord: #{channel_id} has waited over {}s for an answer, so its typing \
                 indicator stopped",
                TYPING_CAP.as_secs()
            ));
        }
        for channel_id in due {
            self.ping(&channel_id).await;
        }
    }

    /// A typing ping is a heartbeat, not a message: one that fails is made up
    /// on the next tick, so it is tried once and never retried.
    async fn ping(&mut self, channel_id: &str) {
        let now = tokio::time::Instant::now();
        self.board.pinged(channel_id, now);
        self.bucket.acquire().await;
        if let Err((message, _)) = self.poster.send(&Call::typing(channel_id)).await {
            self.bridge
                .note_typing_failure(&format!("#{channel_id}"), &message);
        } else {
            self.bridge.note_typing();
        }
    }
}

/// Start the bridge and hand back the status the TUI reads for `/discord`.
/// `console` is true when no TUI is drawing on the terminal (print/headless
/// runs), which is the only case where notes go to stderr.
pub fn spawn(
    config: Config,
    agent_tx: mpsc::UnboundedSender<AgentCommand>,
    events: broadcast::Sender<AgentEvent>,
    console: bool,
) -> Arc<Bridge> {
    let bridge = Arc::new(Bridge::new(&config.discord, events.clone(), console));
    if config.discord.channel_id.is_empty() {
        bridge.note(
            "discord: channel_id is empty, prompts typed in the TUI will not be posted; \
             messages sent to the bot are still answered where they were sent",
        );
    }
    if config.discord.bot_token.trim().is_empty() {
        bridge.fail("discord: bot_token is empty, every reply will be rejected with 401");
    }
    if config.discord.allowed_user_id.trim().is_empty() {
        bridge.note(
            "discord: allowed_user_id is empty, so anyone who can DM the bot or who shares a \
             server with it can drive the agent; set discord.allowed_user_id to lock it down",
        );
    }
    let (outbox_tx, outbox_rx) = mpsc::channel::<Posted>(OUTBOX_DEPTH);
    let (signal_tx, signal_rx) = mpsc::channel::<Signal>(SIGNAL_DEPTH);
    let signals = Signals::new(signal_tx, bridge.clone());
    let intake = Intake::new(&config.discord.channel_id, signals);
    tokio::spawn(SignalTask::new(config.discord.clone(), bridge.clone()).run(signal_rx));
    tokio::spawn(reply_relay(
        events.subscribe(),
        outbox_tx,
        intake.clone(),
        bridge.clone(),
    ));
    tokio::spawn(Outbox::new(config.discord.clone(), outbox_rx, bridge.clone()).run());
    tokio::spawn(connect_loop(config, agent_tx, intake, bridge.clone()));
    bridge
}

async fn connect_loop(
    config: Config,
    agent_tx: mpsc::UnboundedSender<AgentCommand>,
    intake: Intake,
    bridge: Arc<Bridge>,
) {
    let mut session = GatewaySession::default();
    let mut backoff = Backoff::new(BACKOFF_START, BACKOFF_MAX);
    let mut intents = INTENTS;
    loop {
        bridge.set_state("connecting to the gateway", false);
        let outcome = gateway_session(
            &config,
            &agent_tx,
            &intake,
            &mut session,
            &mut backoff,
            intents,
            &bridge,
        )
        .await;
        match outcome {
            Ok(()) => session.clear(),
            Err(error) => {
                if let Some(leaner) = downgrade_intents(intents, error.close_code) {
                    intents = leaner;
                    session.clear();
                    bridge.set_state(
                        "reconnecting without the Message Content Intent: server text is empty",
                        true,
                    );
                    bridge.fail(
                        "discord: gateway rejected the privileged Message Content Intent (4013); \
                         reconnecting without it. Enable it on the bot in the Discord developer \
                         portal to read server channel text; DMs keep working.",
                    );
                    continue;
                }
                bridge.set_state(format!("disconnected: {}", error.message).as_str(), true);
                if error.reconnect == Reconnect::Fatal {
                    bridge.fail(&format!("discord gateway stopped: {}", error.message));
                    bridge.set_state("stopped: the bridge is not connected", true);
                    return;
                }
                if error.reconnect == Reconnect::Fresh {
                    session.clear();
                }
                bridge.fail(&format!("discord gateway error: {}", error.message));
            }
        }
        let wait = backoff.plan(jitter());
        bridge.note(&format!(
            "discord: reconnecting in {:.1}s",
            wait.as_secs_f64()
        ));
        tokio::time::sleep(wait).await;
    }
}

async fn gateway_session(
    config: &Config,
    agent_tx: &mpsc::UnboundedSender<AgentCommand>,
    intake: &Intake,
    session: &mut GatewaySession,
    backoff: &mut Backoff,
    intents: u64,
    bridge: &Bridge,
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

    let handshake =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake(&mut ws, session, intents)).await;
    match handshake {
        Ok(Ok(ready)) => bridge.note_ready(&ready),
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
    let mut notices = Notices::default();

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
                        let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                            if let Some(note) = notices.unreadable_frame() {
                                bridge.fail(&note);
                            }
                            continue;
                        };
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
                            GatewayOp::Dispatch if value["t"].as_str() == Some("GUILD_CREATE") => {
                                bridge.note_server(&value["d"]);
                            }
                            GatewayOp::Dispatch
                                if value["t"].as_str() == Some("MESSAGE_CREATE") =>
                            {
                                let inbound =
                                    route_message(&config.discord, &value["d"], agent_tx, intake);
                                bridge.note_inbound(&value["d"], &inbound);
                                let guild = is_guild_message(&value["d"]);
                                if let Some(note) = notices.notice(&inbound, guild) {
                                    match &inbound {
                                        Inbound::Ignored(Ignore::NoText) => bridge.fail(&note),
                                        _ => bridge.note(&note),
                                    }
                                }
                            }
                            _ => continue,
                        }
                    }
                    Message::Close(frame) => return Err(close_error(frame)),
                    Message::Binary(_) => {
                        if let Some(note) = notices.binary_frame() {
                            bridge.fail(&note);
                        }
                    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyInfo {
    pub resumed: bool,
    pub requested_intents: u64,
    pub granted_intents: Option<u64>,
}

impl ReadyInfo {
    /// True when rho asked for the privileged intent that carries server text
    /// and Discord did not apply it, which is why a connected bot can appear to
    /// ignore a server: the messages arrive with empty `content`.
    pub fn content_missing(&self) -> bool {
        self.requested_intents & GUILD_MESSAGE_CONTENT != 0
            && self
                .granted_intents
                .is_some_and(|granted| granted & GUILD_MESSAGE_CONTENT == 0)
    }

    /// Says out loud whether Discord actually granted the privileged intent, so
    /// a portal setting that was not saved (or a session that already degraded)
    /// is visible instead of looking like a bot that ignores the server.
    pub fn summary(&self) -> String {
        let mut status = if self.resumed {
            "resumed session".to_string()
        } else {
            "identified".to_string()
        };
        let Some(granted) = self.granted_intents else {
            return status;
        };
        let content = if granted & GUILD_MESSAGE_CONTENT != 0 {
            "message content granted"
        } else if self.content_missing() {
            "WARNING: Discord did not grant the Message Content Intent, server text is empty"
        } else {
            "rho is not requesting the Message Content Intent, server text is empty"
        };
        status.push_str(&format!(" (intents {:#06x}, {content})", granted));
        status
    }
}

async fn handshake(ws: &mut Ws, session: &mut GatewaySession, intents: u64) -> Gw<ReadyInfo> {
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
                return Ok(ReadyInfo {
                    resumed: false,
                    requested_intents: intents,
                    granted_intents: value["d"]["intents"].as_u64(),
                });
            }
            GatewayOp::Dispatch if value["t"].as_str() == Some("RESUMED") => {
                return Ok(ReadyInfo {
                    resumed: true,
                    requested_intents: intents,
                    granted_intents: None,
                });
            }
            _ => continue,
        }
    }
}

fn route_message(
    config: &DiscordConfig,
    d: &Value,
    agent_tx: &mpsc::UnboundedSender<AgentCommand>,
    intake: &Intake,
) -> Inbound {
    let inbound = accept_message(d, &config.allowed_user_id);
    let Inbound::Prompted { channel_id, text } = &inbound else {
        return inbound;
    };
    if let Err(e) = agent_tx.send(AgentCommand::Prompt { text: text.clone() }) {
        return Inbound::Ignored(Ignore::NotDelivered(format!(
            "the agent is not accepting prompts ({e})"
        )));
    }
    intake.claim(channel_id, d["id"].as_str().unwrap_or(""));
    inbound
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
    intake: Intake,
    bridge: Arc<Bridge>,
) {
    let mut answered = false;
    loop {
        let text = match events.recv().await {
            Ok(AgentEvent::AssistantDone { text }) | Ok(AgentEvent::Error(text)) => {
                answered = true;
                text
            }
            Ok(AgentEvent::Idle) => {
                // The agent is idle, so nothing is being written any more. If
                // no answer came for the prompt it was working (an interrupted
                // turn), that prompt still thinks it is owed one.
                if !answered {
                    intake.signals.typing_drop_oldest();
                }
                answered = false;
                continue;
            }
            Ok(_) => continue,
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                answered = false;
                intake.signals.typing_clear();
                bridge.fail(&format!(
                    "discord: fell behind the event stream and lost {skipped} event(s), an \
                     answer may never have been posted"
                ));
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => {
                intake.signals.typing_clear();
                bridge.fail("discord: the agent shut down, so no more replies will be posted");
                break;
            }
        };
        let Some(channel_id) = intake.take() else {
            continue;
        };
        let chunks = chunk_message(&text);
        if chunks.is_empty() {
            intake.signals.typing_stop(&channel_id);
            bridge.fail(&format!(
                "discord: the answer for #{channel_id} had no text, so nothing was posted"
            ));
            continue;
        }
        bridge.note_answer(&channel_id, chunks.len());
        let dropped = push_chunks(&outbox, &channel_id, &chunks);
        intake.signals.typing_stop(&channel_id);
        if dropped > 0 {
            bridge.fail(&format!(
                "discord: dropped {dropped} reply chunk(s) for #{channel_id}, outbox full"
            ));
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Posted {
    pub channel_id: String,
    pub text: String,
}

/// One Discord REST call. Answers, reactions and typing pings differ only in
/// verb, endpoint and body, so they share the retry and rate-limit machinery.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub verb: Verb,
    pub url: String,
    pub body: Option<Value>,
}

/// Discord answers anything else with a `405`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Post,
    Put,
}

impl Verb {
    pub fn as_method(self) -> reqwest::Method {
        match self {
            Verb::Post => reqwest::Method::POST,
            Verb::Put => reqwest::Method::PUT,
        }
    }
}

impl Call {
    pub fn message(channel_id: &str, text: &str) -> Self {
        Self {
            verb: Verb::Post,
            url: send_url(channel_id),
            body: Some(json!({ "content": text })),
        }
    }

    pub fn react(channel_id: &str, message_id: &str, emoji: &str) -> Self {
        Self {
            verb: Verb::Put,
            url: reaction_url(channel_id, message_id, emoji),
            body: None,
        }
    }

    pub fn typing(channel_id: &str) -> Self {
        Self {
            verb: Verb::Post,
            url: typing_url(channel_id),
            body: None,
        }
    }
}

trait Poster {
    async fn send(&self, call: &Call) -> Result<(), (String, Retry)>;
}

#[derive(Debug, PartialEq)]
pub enum Delivery {
    Sent,
    Failed(String),
    GaveUp { attempts: u32, message: String },
}

async fn deliver<P: Poster + ?Sized>(post: &P, bucket: &mut Bucket, call: Call) -> Delivery {
    let mut attempts = 0;
    loop {
        bucket.acquire().await;
        attempts += 1;
        match post.send(&call).await {
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
    bridge: Arc<Bridge>,
}

impl Outbox {
    fn new(config: DiscordConfig, outbox: mpsc::Receiver<Posted>, bridge: Arc<Bridge>) -> Self {
        Self {
            poster: RestPoster {
                http: reqwest::Client::new(),
                config,
            },
            bucket: Bucket::new(SENDS_PER_WINDOW, SEND_WINDOW),
            outbox,
            bridge,
        }
    }

    async fn run(mut self) {
        while let Some(posted) = self.outbox.recv().await {
            let target = format!("#{}", posted.channel_id.trim());
            let call = Call::message(&posted.channel_id, &posted.text);
            match deliver(&self.poster, &mut self.bucket, call).await {
                Delivery::Sent => self.bridge.note_reply_posted(1),
                Delivery::Failed(message) => {
                    self.bridge
                        .note_send_failure(&format!("{target}: {message}"));
                }
                Delivery::GaveUp { attempts, message } => {
                    self.bridge.note_send_failure(&format!(
                        "{target}: {message} (after {attempts} attempts)"
                    ));
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

/// The emoji goes before the literal `@me`, not after it.
pub fn reaction_url(channel_id: &str, message_id: &str, emoji: &str) -> String {
    format!(
        "https://discord.com/api/v10/channels/{}/messages/{}/reactions/{}/@me",
        channel_id.trim(),
        message_id.trim(),
        encode_emoji(emoji)
    )
}

pub fn typing_url(channel_id: &str) -> String {
    format!(
        "https://discord.com/api/v10/channels/{}/typing",
        channel_id.trim()
    )
}

/// Discord wants a unicode emoji percent-encoded in the reaction path, so the
/// eyes go out as `%F0%9F%91%80`.
pub fn encode_emoji(emoji: &str) -> String {
    let mut encoded = String::new();
    for byte in emoji.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

pub fn auth_header(bot_token: &str) -> String {
    format!("Bot {}", bot_token.trim())
}

impl Poster for RestPoster {
    async fn send(&self, call: &Call) -> Result<(), (String, Retry)> {
        let mut request = self.http.request(call.verb.as_method(), &call.url).header(
            reqwest::header::AUTHORIZATION,
            auth_header(&self.config.bot_token),
        );
        if let Some(body) = &call.body {
            request = request.json(body);
        }
        let response = request
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

    fn prompted(channel_id: &str, text: &str) -> Inbound {
        Inbound::Prompted {
            channel_id: channel_id.to_string(),
            text: text.to_string(),
        }
    }

    fn message(channel_id: &str, content: &str, author: Value) -> Value {
        json!({ "channel_id": channel_id, "content": content, "author": author })
    }

    #[test]
    fn ignores_bots_and_wrong_users() {
        let bot = message("c", "hello", json!({"bot": true, "id": "1"}));
        let user = message("c", "hello", json!({"bot": false, "id": "42"}));
        assert_eq!(accept_message(&bot, ""), Inbound::Ignored(Ignore::FromBot));
        assert_eq!(
            accept_message(&user, "99"),
            Inbound::Ignored(Ignore::DisallowedUser("42".to_string()))
        );
        assert_eq!(accept_message(&user, "42"), prompted("c", "hello"));
        assert_eq!(accept_message(&user, ""), prompted("c", "hello"));
    }

    #[test]
    fn every_channel_reaches_the_agent() {
        let dm = json!({"channel_id": "7", "content": "hi", "author": {"id": "42"}});
        let server = json!({
            "channel_id": "9", "guild_id": "1", "content": "build it",
            "author": {"id": "42"}
        });
        assert_eq!(accept_message(&dm, "42"), prompted("7", "hi"));
        assert_eq!(accept_message(&server, "42"), prompted("9", "build it"));
        assert!(!is_guild_message(&dm));
        assert!(is_guild_message(&server));
    }

    #[test]
    fn missing_server_text_is_blamed_on_the_intent() {
        let guild = json!({
            "channel_id": "9", "guild_id": "1", "content": "",
            "author": {"id": "42"}
        });
        let dm = json!({"channel_id": "7", "content": "", "author": {"id": "42"}});

        let mut notices = Notices::default();
        let note = notices
            .notice(&accept_message(&guild, "42"), is_guild_message(&guild))
            .unwrap();
        assert!(note.contains("Message Content Intent"));
        assert!(notices
            .notice(&accept_message(&guild, "42"), true)
            .is_none());
        assert!(notices
            .notice(&accept_message(&dm, "42"), is_guild_message(&dm))
            .is_none());
    }

    #[test]
    fn an_unknown_user_is_reported_once_with_its_id() {
        let stranger = json!({
            "channel_id": "9", "guild_id": "1", "content": "help",
            "author": {"id": "777"}
        });
        let mut notices = Notices::default();
        let note = notices
            .notice(&accept_message(&stranger, "42"), true)
            .unwrap();
        assert!(note.contains("777"));
        assert!(note.contains("allowed_user_id"));
        assert!(notices
            .notice(&accept_message(&stranger, "42"), true)
            .is_none());
    }

    #[test]
    fn accepted_messages_produce_no_notice() {
        let mut notices = Notices::default();
        assert!(notices.notice(&prompted("9", "build it"), true).is_none());
        assert!(notices
            .notice(&Inbound::Ignored(Ignore::FromBot), false)
            .is_none());
    }

    #[test]
    fn frames_the_bridge_cannot_read_are_admitted_once() {
        let mut notices = Notices::default();
        assert!(notices
            .unreadable_frame()
            .unwrap()
            .contains("not valid JSON"));
        assert!(notices.unreadable_frame().is_none());
        assert!(notices.binary_frame().unwrap().contains("binary frame"));
        assert!(notices.binary_frame().is_none());
    }

    #[test]
    fn servers_are_listed_with_their_state() {
        assert_eq!(
            guild_summary(&json!({"id": "1", "name": "Work"})),
            "discord: server \"Work\" (1) available"
        );
        assert_eq!(
            guild_summary(&json!({"id": "1", "unavailable": true})),
            "discord: server \"unnamed\" (1) unavailable"
        );
    }

    fn posted(content: &str) -> Value {
        json!({
            "channel_id": "9", "guild_id": "1", "content": content,
            "author": {"id": "42", "username": "alice"}
        })
    }

    fn direct(content: &str) -> Value {
        json!({
            "channel_id": "7", "content": content,
            "author": {"id": "42", "username": "alice"}
        })
    }

    fn bot_post() -> Value {
        json!({
            "channel_id": "9", "guild_id": "1", "content": "ping",
            "author": {"id": "3", "username": "robby", "bot": true}
        })
    }

    #[test]
    fn arrivals_say_where_the_message_came_from() {
        assert_eq!(
            arrival(&direct("build it"), &prompted("7", "build it"), None),
            "@alice in DM #7: \"build it\" — queued as a prompt"
        );
        assert_eq!(
            arrival(
                &posted("  fix\n the tests  "),
                &prompted("9", "fix the tests"),
                Some("Work")
            ),
            "@alice in server \"Work\" #9: \"fix the tests\" — queued as a prompt"
        );
        assert!(
            arrival(&posted(""), &Inbound::Ignored(Ignore::NoText), Some("Work"))
                .contains("no text content")
        );
        assert!(arrival(
            &posted("hi"),
            &Inbound::Ignored(Ignore::DisallowedUser("99".to_string())),
            None
        )
        .contains("99 is not allowed_user_id"));
    }

    #[test]
    fn long_arrivals_are_quoted_briefly() {
        let text = "x".repeat(300);
        let line = arrival(&direct(&text), &prompted("7", &text), None);
        assert!(line.contains(&format!("\"{}…\"", "x".repeat(60))), "{line}");
        assert!(line.chars().count() < 120, "{line}");
    }

    /// An intake for the fallback channel, with the signal queue left unread so
    /// a test can see which courtesies the bridge asked for.
    fn intake(fallback: &str) -> (Intake, mpsc::Receiver<Signal>) {
        let (signals, received) = signal_queue();
        (Intake::new(fallback, signals), received)
    }

    /// A signal queue the bridge can push into without a task reading it back.
    fn signal_queue() -> (Signals, mpsc::Receiver<Signal>) {
        let (events, _notes) = broadcast::channel(8);
        let (inbox, received) = mpsc::channel(SIGNAL_DEPTH);
        (
            Signals::new(inbox, Arc::new(Bridge::new(&config("9"), events, false))),
            received,
        )
    }

    fn bridge(console: bool) -> (Arc<Bridge>, broadcast::Receiver<AgentEvent>) {
        let (tx, rx) = broadcast::channel(16);
        let mut cfg = config("123");
        cfg.allowed_user_id = "42".to_string();
        (Arc::new(Bridge::new(&cfg, tx, console)), rx)
    }

    #[tokio::test]
    async fn bridge_notes_go_to_the_transcript_not_stderr() {
        let (bridge, mut rx) = bridge(false);
        bridge.note("discord: hello");
        assert!(matches!(
            rx.recv().await,
            Ok(AgentEvent::Info(text)) if text == "discord: hello"
        ));
    }

    #[tokio::test]
    async fn failures_reach_the_transcript_as_errors_and_are_remembered() {
        let (bridge, mut rx) = bridge(false);
        assert_eq!(bridge.report().iter().filter(|l| l.fault).count(), 0);

        bridge.fail("discord: could not post a reply: #9: discord REST 403: Missing Permissions");
        match rx.recv().await {
            Ok(AgentEvent::Fault(text)) => assert!(text.contains("403"), "{text}"),
            other => panic!("expected a bridge error, got {other:?}"),
        }

        let summary = bridge.summary();
        assert!(summary.contains("1 error(s) reported"), "{summary}");
        assert!(summary.contains("403: Missing Permissions"), "{summary}");
        let faults = bridge
            .report()
            .into_iter()
            .filter(|l| l.fault)
            .map(|l| l.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(faults.matches("discord:").count(), 1, "{faults}");
        assert!(faults.contains("403"), "{faults}");
    }

    #[tokio::test]
    async fn a_bridge_that_stopped_is_flagged_in_the_report() {
        let (bridge, _rx) = bridge(false);
        bridge.set_state("stopped: the bridge is not connected", true);
        assert!(
            bridge
                .report()
                .iter()
                .any(|l| l.fault && l.text.contains("stopped")),
            "{}",
            bridge.summary()
        );
    }

    #[tokio::test]
    async fn status_explains_a_server_channel_that_stays_unread() {
        let (bridge, _rx) = bridge(false);
        bridge.note_ready(&ReadyInfo {
            resumed: false,
            requested_intents: INTENTS,
            granted_intents: Some(INTENTS & !GUILD_MESSAGE_CONTENT),
        });
        bridge.note_server(&json!({"id": "1", "name": "Work"}));
        bridge.note_inbound(&posted(""), &Inbound::Ignored(Ignore::NoText));
        bridge.note_reply_posted(2);
        bridge.note_send_failure("send failed: 403 Forbidden");
        let summary = bridge.summary();
        assert!(summary.contains("identified"), "{summary}");
        assert!(
            summary.contains("Message Content Intent NOT granted"),
            "{summary}"
        );
        assert!(summary.contains("\"Work\" (1)"), "{summary}");
        assert!(summary.contains("1 message(s) in"), "{summary}");
        assert!(summary.contains("2 reply chunk(s) sent"), "{summary}");
        assert!(
            summary.contains("last message in: @alice in server \"Work\" #9"),
            "{summary}"
        );
        assert!(summary.contains("403 Forbidden"), "{summary}");
        assert!(summary.contains("allowed_user_id: 42"), "{summary}");
        assert!(summary.contains("fallback channel: 123"), "{summary}");
        let faults = bridge
            .report()
            .into_iter()
            .filter(|l| l.fault)
            .map(|l| l.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            faults.contains("Message Content Intent NOT granted"),
            "{faults}"
        );
    }

    #[tokio::test]
    async fn an_unseen_server_is_named_in_the_status() {
        let (bridge, _rx) = bridge(false);
        bridge.note_inbound(&posted("hello"), &prompted("9", "hello"));
        assert!(
            bridge.summary().contains("no servers visible"),
            "{}",
            bridge.summary()
        );
        bridge.status.lock().unwrap().upsert_server(Server {
            id: "1".to_string(),
            name: "Work".to_string(),
            available: false,
        });
        assert_eq!(bridge.server_of(&posted("x")).as_deref(), Some("Work"));
        assert!(bridge.summary().contains("(1 unavailable)"));
        assert!(bridge.summary().contains("intents not reported yet"));
    }

    #[tokio::test]
    async fn bot_traffic_is_counted_but_only_mentioned_once() {
        let (bridge, mut rx) = bridge(false);
        bridge.note_inbound(&bot_post(), &Inbound::Ignored(Ignore::FromBot));
        bridge.note_inbound(&bot_post(), &Inbound::Ignored(Ignore::FromBot));
        assert!(matches!(rx.recv().await, Ok(AgentEvent::Info(_))));
        assert!(rx.try_recv().is_err());

        bridge.note_inbound(&posted(""), &Inbound::Ignored(Ignore::NoText));
        bridge.note_inbound(&posted(""), &Inbound::Ignored(Ignore::NoText));
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());

        let status = bridge.status.lock().unwrap();
        assert_eq!(status.messages_seen, 4);
        assert_eq!(status.bot_messages_seen, 2);
        assert_eq!(status.prompts_queued, 0);
    }

    #[test]
    fn ready_says_whether_discord_granted_the_intent() {
        let granted = ReadyInfo {
            resumed: false,
            requested_intents: INTENTS,
            granted_intents: Some(INTENTS),
        };
        assert!(granted.summary().contains("message content granted"));

        let denied = ReadyInfo {
            granted_intents: Some(INTENTS & !GUILD_MESSAGE_CONTENT),
            ..granted.clone()
        };
        assert!(denied.summary().contains("did not grant"));

        let degraded = ReadyInfo {
            requested_intents: INTENTS & !GUILD_MESSAGE_CONTENT,
            ..denied.clone()
        };
        assert!(degraded.summary().contains("not requesting"));

        let resumed = ReadyInfo {
            resumed: true,
            granted_intents: None,
            ..granted
        };
        assert_eq!(resumed.summary(), "resumed session");
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
        assert_eq!(retry_policy(405, None, None), Retry::Stop);
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

    async fn next_bridge_error(notes: &mut broadcast::Receiver<AgentEvent>) -> String {
        loop {
            match notes.recv().await {
                Ok(AgentEvent::Fault(text)) => return text,
                Ok(_) => continue,
                Err(e) => panic!("the event stream ended: {e}"),
            }
        }
    }

    #[tokio::test]
    async fn an_answer_with_no_text_says_nothing_was_posted() {
        let (events, mut notes) = broadcast::channel(16);
        let (intake, _inbox) = intake("123");
        let bridge = Arc::new(Bridge::new(&config("123"), events.clone(), false));
        let (outbox, mut posted) = mpsc::channel(4);
        let relay_outbox = outbox.clone();
        tokio::spawn(reply_relay(
            events.subscribe(),
            relay_outbox,
            intake.clone(),
            bridge.clone(),
        ));

        events
            .send(AgentEvent::AssistantDone {
                text: "   ".to_string(),
            })
            .unwrap();

        let text = next_bridge_error(&mut notes).await;
        assert!(text.contains("no text"), "{text}");
        assert!(posted.try_recv().is_err());
        assert!(bridge.summary().contains("0 reply chunk(s) sent"));
    }

    #[tokio::test]
    async fn a_full_outbox_reports_the_chunks_it_drops() {
        let (events, mut notes) = broadcast::channel(16);
        let (intake, _inbox) = intake("123");
        let bridge = Arc::new(Bridge::new(&config("123"), events.clone(), false));
        let (outbox, mut posted) = mpsc::channel(1);
        let relay_outbox = outbox.clone();
        tokio::spawn(reply_relay(
            events.subscribe(),
            relay_outbox,
            intake.clone(),
            bridge.clone(),
        ));

        events
            .send(AgentEvent::AssistantDone {
                text: "x".repeat(2500),
            })
            .unwrap();

        let text = next_bridge_error(&mut notes).await;
        assert!(text.contains("dropped 1 reply chunk"), "{text}");
        assert!(text.contains("#123"), "{text}");
        assert_eq!(posted.try_recv().unwrap().text, "x".repeat(2000));
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
        async fn send(&self, _call: &Call) -> Result<(), (String, Retry)> {
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
            deliver(&poster, &mut bucket, Call::message("9", "hi")).await,
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
            deliver(&poster, &mut bucket, Call::message("9", "hi")).await,
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
            deliver(&poster, &mut bucket, Call::message("9", "hi")).await,
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
                deliver(&poster, &mut bucket, Call::message("9", "hi")).await,
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
    fn routes_messages_from_any_channel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (intake, _inbox) = intake("7");
        let message = json!({
            "channel_id": "9", "guild_id": "1", "content":  "  build it  ",
            "author": {"bot": false, "id": "42"}
        });

        assert_eq!(
            route_message(&config("9"), &message, &tx, &intake),
            prompted("9", "build it")
        );
        assert_eq!(
            rx.try_recv().unwrap().text_or_debug(),
            "build it".to_string()
        );

        assert_eq!(
            route_message(&config("7"), &message, &tx, &intake),
            prompted("9", "build it")
        );
        assert_eq!(
            rx.try_recv().unwrap().text_or_debug(),
            "build it".to_string()
        );

        let dm = json!({
            "channel_id": "9", "content": "dm prompt", "author": {"bot": false, "id": "42"}
        });
        assert_eq!(
            route_message(&config("7"), &dm, &tx, &intake),
            prompted("9", "dm prompt")
        );
        assert_eq!(
            rx.try_recv().unwrap().text_or_debug(),
            "dm prompt".to_string()
        );

        let restricted = DiscordConfig {
            allowed_user_id: "1".to_string(),
            ..config("9")
        };
        assert_eq!(
            route_message(&restricted, &message, &tx, &intake),
            Inbound::Ignored(Ignore::DisallowedUser("42".to_string()))
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn routed_messages_retarget_the_reply() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (intake, _inbox) = intake("7");
        let guild = json!({
            "channel_id": "9", "guild_id": "1", "content": "build it",
            "author": {"bot": false, "id": "42"}
        });

        assert_eq!(intake.take().as_deref(), Some("7"));
        route_message(&config("9"), &guild, &tx, &intake);
        assert_eq!(rx.try_recv().unwrap().text_or_debug(), "build it");
        assert_eq!(intake.take().as_deref(), Some("9"));

        let dm = json!({
            "channel_id": "77", "content": "hello", "author": {"bot": false, "id": "42"}
        });
        route_message(&config("9"), &dm, &tx, &intake);
        assert_eq!(rx.try_recv().unwrap().text_or_debug(), "hello");
        assert_eq!(intake.take().as_deref(), Some("77"));
        assert_eq!(intake.take().as_deref(), Some("7"));
    }

    #[test]
    fn ignored_messages_do_not_claim_an_answer() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (intake, _inbox) = intake("7");

        let blank = json!({
            "channel_id": "77", "guild_id": "1", "content": "   ",
            "author": {"bot": false, "id": "42"}
        });
        assert_eq!(
            route_message(&config("7"), &blank, &tx, &intake),
            Inbound::Ignored(Ignore::NoText)
        );

        let from_bot = json!({
            "channel_id": "77", "guild_id": "1", "content": "echo",
            "author": {"bot": true, "id": "42"}
        });
        assert_eq!(
            route_message(&config("7"), &from_bot, &tx, &intake),
            Inbound::Ignored(Ignore::FromBot)
        );

        assert!(rx.try_recv().is_err());
        assert_eq!(intake.take().as_deref(), Some("7"));
    }

    #[test]
    fn a_prompt_the_agent_will_not_take_is_reported_as_undelivered() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let (intake, _inbox) = intake("7");

        let inbound = route_message(&config("9"), &posted("build it"), &tx, &intake);
        let Inbound::Ignored(Ignore::NotDelivered(reason)) = &inbound else {
            panic!("expected an undelivered message, got {inbound:?}");
        };
        assert!(reason.contains("not accepting prompts"), "{reason}");
        let line = arrival(&posted("build it"), &inbound, Some("Work"));
        assert!(line.contains("not queued"), "{line}");
        assert!(line.contains("@alice"), "{line}");
        assert_eq!(intake.take().as_deref(), Some("7"));
    }

    #[tokio::test]
    async fn an_undelivered_message_is_an_error_and_not_a_queued_prompt() {
        let (bridge, mut rx) = bridge(false);
        let inbound = Inbound::Ignored(Ignore::NotDelivered(
            "the agent is not accepting prompts".to_string(),
        ));

        bridge.note_inbound(&posted("build it"), &inbound);

        match rx.recv().await {
            Ok(AgentEvent::Fault(text)) => assert!(text.contains("not queued"), "{text}"),
            other => panic!("expected a bridge error, got {other:?}"),
        }
        let status = bridge.status.lock().unwrap();
        assert_eq!(status.messages_seen, 1);
        assert_eq!(status.prompts_queued, 0);
        assert_eq!(status.errors, 1);
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

    #[test]
    fn eyes_and_typing_have_their_own_endpoints() {
        assert_eq!(encode_emoji("\u{1f440}"), "%F0%9F%91%80");
        assert_eq!(encode_emoji("rho:1"), "rho%3A1");
        assert_eq!(
            reaction_url("9", "55", EYES),
            "https://discord.com/api/v10/channels/9/messages/55/reactions/%F0%9F%91%80/@me"
        );
        assert_eq!(
            typing_url(" 9 "),
            "https://discord.com/api/v10/channels/9/typing"
        );
        assert_eq!(
            Call::typing("9"),
            Call {
                verb: Verb::Post,
                url: typing_url("9"),
                body: None
            }
        );
        assert_eq!(
            Call::react("9", "55", EYES),
            Call {
                verb: Verb::Put,
                url: reaction_url("9", "55", EYES),
                body: None
            }
        );
        assert_eq!(Call::message("9", "hi").verb, Verb::Post);
        assert_eq!(
            Call::message("9", "hi").body,
            Some(json!({ "content": "hi" }))
        );
    }

    #[test]
    fn an_accepted_prompt_asks_for_eyes_and_typing() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (intake, mut inbox) = intake("7");
        let message = json!({
            "id": "55", "channel_id": "9", "content": "build it",
            "author": {"bot": false, "id": "42"}
        });

        assert_eq!(
            route_message(&config("9"), &message, &tx, &intake),
            prompted("9", "build it")
        );
        assert_eq!(
            rx.try_recv().unwrap().text_or_debug(),
            "build it".to_string()
        );
        assert_eq!(
            inbox.try_recv().unwrap(),
            Signal::React {
                channel_id: "9".to_string(),
                message_id: "55".to_string()
            }
        );
        assert_eq!(
            inbox.try_recv().unwrap(),
            Signal::TypingStart {
                channel_id: "9".to_string()
            }
        );
    }

    #[test]
    fn a_message_that_is_not_a_prompt_asks_for_nothing() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (intake, mut inbox) = intake("7");
        let from_bot = json!({
            "id": "55", "channel_id": "9", "content": "echo",
            "author": {"bot": true, "id": "42"}
        });

        assert_eq!(
            route_message(&config("9"), &from_bot, &tx, &intake),
            Inbound::Ignored(Ignore::FromBot)
        );
        assert!(inbox.try_recv().is_err());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_prompt_in_the_same_channel_keeps_typing() {
        let now = tokio::time::Instant::now();
        let mut board = TypingBoard::default();
        assert!(board.start("9", now));
        assert!(!board.start("9", now + Duration::from_secs(1)));
        assert_eq!(board.open(), 1);
        assert!(!board.stop("9"));
        assert!(board.stop("9"));
        assert!(!board.stop("9"));
        assert_eq!(board.open(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn typing_pings_repeat_until_the_answer_lands() {
        let start = tokio::time::Instant::now();
        let mut board = TypingBoard::default();
        board.start("9", start);
        assert_eq!(board.due(start).0, vec!["9".to_string()]);
        board.pinged("9", start);
        assert!(board.due(start + TYPING_TICK / 2).0.is_empty());
        assert_eq!(board.due(start + TYPING_TICK).0, vec!["9".to_string()]);
        assert!(board.stop("9"));
        assert!(board.due(start + TYPING_TICK * 2).0.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn the_oldest_prompt_loses_its_indicator_when_no_answer_comes() {
        let start = tokio::time::Instant::now();
        let mut board = TypingBoard::default();
        board.start("9", start);
        board.start("77", start + Duration::from_secs(2));
        assert_eq!(board.drop_oldest().as_deref(), Some("9"));
        assert_eq!(board.drop_oldest().as_deref(), Some("77"));
        assert_eq!(board.drop_oldest(), None);
        board.start("9", start);
        board.start("77", start);
        assert_eq!(board.clear(), 2);
        assert_eq!(board.open(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_episode_outlasting_every_answer_is_abandoned() {
        let start = tokio::time::Instant::now();
        let mut board = TypingBoard::default();
        board.start("9", start);
        board.pinged("9", start);
        let (due, abandoned) = board.due(start + TYPING_CAP);
        assert!(due.is_empty());
        assert_eq!(abandoned, vec!["9".to_string()]);
        assert_eq!(board.open(), 0);
    }

    #[derive(Default)]
    struct RecordingPoster {
        calls: Mutex<Vec<Call>>,
    }

    impl RecordingPoster {
        fn count(&self, needle: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.url.contains(needle))
                .count()
        }

        fn url(&self, index: usize) -> String {
            self.calls.lock().unwrap()[index].url.clone()
        }

        fn verb(&self, index: usize) -> Verb {
            self.calls.lock().unwrap()[index].verb
        }
    }

    impl Poster for RecordingPoster {
        async fn send(&self, call: &Call) -> Result<(), (String, Retry)> {
            self.calls.lock().unwrap().push(call.clone());
            Ok(())
        }
    }

    impl<P: Poster + ?Sized> Poster for Arc<P> {
        async fn send(&self, call: &Call) -> Result<(), (String, Retry)> {
            (**self).send(call).await
        }
    }

    async fn wait_until(done: impl Fn() -> bool) {
        for _ in 0..200 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the signal task never caught up");
    }

    async fn next_bridge_note(notes: &mut broadcast::Receiver<AgentEvent>) -> String {
        loop {
            match notes.recv().await {
                Ok(AgentEvent::Info(text)) => return text,
                Ok(_) => continue,
                Err(e) => panic!("the event stream ended: {e}"),
            }
        }
    }

    fn signal_task(
        poster: Arc<RecordingPoster>,
    ) -> (
        SignalTask<Arc<RecordingPoster>>,
        Arc<Bridge>,
        broadcast::Receiver<AgentEvent>,
    ) {
        let (events, notes) = broadcast::channel(64);
        let bridge = Arc::new(Bridge::new(&config("123"), events, false));
        (
            SignalTask {
                poster,
                bucket: Bucket::new(SIGNALS_PER_WINDOW, SIGNAL_WINDOW),
                bridge: bridge.clone(),
                board: TypingBoard::default(),
            },
            bridge,
            notes,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn eyes_go_on_the_prompt_and_typing_keeps_up_until_it_is_answered() {
        let poster = Arc::new(RecordingPoster::default());
        let (task, bridge, _notes) = signal_task(poster.clone());
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(task.run(rx));

        tx.send(Signal::React {
            channel_id: "9".to_string(),
            message_id: "55".to_string(),
        })
        .await
        .unwrap();
        wait_until(|| poster.count("reactions") == 1).await;
        assert!(
            poster
                .url(0)
                .ends_with("messages/55/reactions/%F0%9F%91%80/@me"),
            "{}",
            poster.url(0)
        );
        assert_eq!(poster.verb(0), Verb::Put, "reactions are a PUT");

        tx.send(Signal::TypingStart {
            channel_id: "9".to_string(),
        })
        .await
        .unwrap();
        wait_until(|| poster.count("typing") == 1).await;

        tokio::time::sleep(TYPING_TICK + Duration::from_secs(1)).await;
        assert_eq!(poster.count("typing"), 2, "the indicator is kept alive");

        tx.send(Signal::TypingStop {
            channel_id: "9".to_string(),
        })
        .await
        .unwrap();
        tx.send(Signal::React {
            channel_id: "9".to_string(),
            message_id: "56".to_string(),
        })
        .await
        .unwrap();
        wait_until(|| poster.count("reactions") == 2).await;
        tokio::time::sleep(TYPING_TICK * 3).await;
        assert_eq!(poster.count("typing"), 2, "stopping really stops");

        let summary = bridge.summary();
        assert!(summary.contains("2 reaction(s) added"), "{summary}");
        assert!(summary.contains("2 typing indicator(s) sent"), "{summary}");
    }

    #[tokio::test(start_paused = true)]
    async fn an_abandoned_typing_episode_says_so() {
        let poster = Arc::new(RecordingPoster::default());
        let (task, bridge, mut notes) = signal_task(poster.clone());
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(task.run(rx));

        tx.send(Signal::TypingStart {
            channel_id: "9".to_string(),
        })
        .await
        .unwrap();
        wait_until(|| poster.count("typing") == 1).await;
        tx.send(Signal::TypingDropOldest).await.unwrap();
        let dropped = next_bridge_note(&mut notes).await;
        assert!(dropped.contains("#9"), "{dropped}");

        tx.send(Signal::TypingStart {
            channel_id: "77".to_string(),
        })
        .await
        .unwrap();
        tokio::time::sleep(TYPING_CAP + Duration::from_secs(1)).await;
        let abandoned = next_bridge_note(&mut notes).await;
        assert!(abandoned.contains("#77"), "{abandoned}");
        assert!(abandoned.contains("stopped"), "{abandoned}");
        assert!(
            !bridge.summary().contains("refused"),
            "{}",
            bridge.summary()
        );
    }

    #[tokio::test]
    async fn the_answer_stops_the_typing_indicator() {
        let (events, _notes) = broadcast::channel(16);
        let (outbox, mut posted) = mpsc::channel(4);
        let (intake, mut inbox) = intake("123");
        let bridge = Arc::new(Bridge::new(&config("123"), events.clone(), false));
        tokio::spawn(reply_relay(
            events.subscribe(),
            outbox,
            intake.clone(),
            bridge.clone(),
        ));

        events
            .send(AgentEvent::AssistantDone {
                text: "done".to_string(),
            })
            .unwrap();
        assert_eq!(
            next_signal(&mut inbox).await,
            Signal::TypingStop {
                channel_id: "123".to_string()
            }
        );
        assert_eq!(posted.recv().await.unwrap().text, "done");
    }

    #[tokio::test]
    async fn an_interrupted_turn_drops_the_indicator_it_owes() {
        let (events, _notes) = broadcast::channel(16);
        let (outbox, _posted) = mpsc::channel(4);
        let (intake, mut inbox) = intake("123");
        let bridge = Arc::new(Bridge::new(&config("123"), events.clone(), false));
        tokio::spawn(reply_relay(
            events.subscribe(),
            outbox,
            intake.clone(),
            bridge.clone(),
        ));

        events.send(AgentEvent::Idle).unwrap();
        assert_eq!(next_signal(&mut inbox).await, Signal::TypingDropOldest);

        events
            .send(AgentEvent::AssistantDone {
                text: "done".to_string(),
            })
            .unwrap();
        assert_eq!(
            next_signal(&mut inbox).await,
            Signal::TypingStop {
                channel_id: "123".to_string()
            }
        );
        events.send(AgentEvent::Idle).unwrap();
        expect_no_signal(&mut inbox).await;
    }

    async fn next_signal(inbox: &mut mpsc::Receiver<Signal>) -> Signal {
        tokio::time::timeout(Duration::from_secs(2), inbox.recv())
            .await
            .expect("no signal reached the queue")
            .expect("the signal queue closed")
    }

    async fn expect_no_signal(inbox: &mut mpsc::Receiver<Signal>) {
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(inbox.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_refused_reaction_is_complained_about_once() {
        let (bridge, mut notes) = bridge(false);
        bridge.note_reaction_failure("#9", "discord REST 403: Missing Permissions");
        bridge.note_reaction_failure("#9", "discord REST 403: Missing Permissions");
        assert!(matches!(
            notes.recv().await,
            Ok(AgentEvent::Fault(text)) if text.contains("Add Reactions")
        ));
        assert!(notes.try_recv().is_err());
        let summary = bridge.summary();
        assert!(
            summary.contains("0 reaction(s) added (2 refused)"),
            "{summary}"
        );
    }

    #[tokio::test]
    async fn a_refused_typing_indicator_is_complained_about_once() {
        let (bridge, mut notes) = bridge(false);
        bridge.note_typing_failure("#9", "discord REST 403: Missing Permissions");
        bridge.note_typing_failure("#9", "discord REST 403: Missing Permissions");
        assert!(matches!(notes.recv().await, Ok(AgentEvent::Fault(_))));
        assert!(notes.try_recv().is_err());
        assert!(
            bridge
                .summary()
                .contains("0 typing indicator(s) sent (2 refused)"),
            "{}",
            bridge.summary()
        );
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
