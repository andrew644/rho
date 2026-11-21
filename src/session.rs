use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::types::Message;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionHeader {
    pub id: String,
    pub cwd: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub created: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionRecord {
    Header(SessionHeader),
    Message(Message),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub header: SessionHeader,
    pub message_count: usize,
    pub first_prompt: String,
}

pub fn sessions_root() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rho")
        .join("sessions")
}

pub fn sessions_dir_for(cwd: &Path) -> PathBuf {
    sessions_root().join(sanitize_dir_name(&cwd.display().to_string()))
}

pub fn sanitize_dir_name(path: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for c in path.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "root".to_string()
    } else {
        trimmed
    }
}

fn generate_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:x}-{:x}", now.as_secs(), now.subsec_nanos())
}

fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, m, d) = civil_from_days(secs.div_euclid(86400) as i64);
    let sod = secs % 86400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

impl SessionHeader {
    pub fn new(cwd: &Path, model: &str) -> Self {
        Self {
            id: generate_id(),
            cwd: cwd.display().to_string(),
            model: model.to_string(),
            name: None,
            created: timestamp(),
        }
    }
}

pub fn create_session_file(cwd: &Path, header: &SessionHeader) -> Result<PathBuf> {
    let dir = sessions_dir_for(cwd);
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let path = dir.join(format!("{}.jsonl", header.id));
    fs::write(&path, "")?;
    save_session(&path, header, &[])?;
    Ok(path)
}

pub fn save_session(path: &Path, header: &SessionHeader, messages: &[Message]) -> Result<()> {
    let mut text = serde_json::to_string(&SessionRecord::Header(header.clone()))?;
    text.push('\n');
    for m in messages {
        text.push_str(&serde_json::to_string(&SessionRecord::Message(m.clone()))?);
        text.push('\n');
    }
    fs::write(path, text).with_context(|| format!("failed to write session {}", path.display()))
}

pub fn load_session(path: &Path) -> Result<(SessionHeader, Vec<Message>)> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read session {}", path.display()))?;
    let mut header: Option<SessionHeader> = None;
    let mut messages = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let record: SessionRecord = serde_json::from_str(line)
            .with_context(|| format!("bad session record in {}", path.display()))?;
        match record {
            SessionRecord::Header(h) => header = Some(h),
            SessionRecord::Message(m) => {
                if m.role != "system" {
                    messages.push(m);
                }
            }
        }
    }
    let header = header.context("session file has no header record")?;
    Ok((header, messages))
}

pub fn list_sessions(cwd: &Path, limit: usize) -> Vec<SessionInfo> {
    list_sessions_in(&sessions_dir_for(cwd), limit)
}

pub fn list_sessions_in(dir: &Path, limit: usize) -> Vec<SessionInfo> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut sessions: Vec<SessionInfo> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| {
            let (header, messages) = load_session(&e.path()).ok()?;
            let first_prompt = messages
                .iter()
                .find(|m| m.role == "user")
                .and_then(|m| m.content.clone())
                .unwrap_or_default()
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(80)
                .collect();
            Some(SessionInfo {
                path: e.path(),
                header,
                message_count: messages.len(),
                first_prompt,
            })
        })
        .collect();
    sessions.sort_by(|a, b| b.header.created.cmp(&a.header.created));
    sessions.truncate(limit);
    sessions
}

pub fn latest_session(cwd: &Path) -> Option<SessionInfo> {
    list_sessions(cwd, 1).into_iter().next()
}

pub fn update_name(path: &Path, name: &str) -> Result<()> {
    let (mut header, messages) = load_session(path)?;
    header.name = if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    };
    save_session(path, &header, &messages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_dir_names() {
        assert_eq!(sanitize_dir_name("/home/u/my.repo"), "home-u-my-repo");
        assert_eq!(sanitize_dir_name("///"), "root");
        assert_eq!(sanitize_dir_name("a--b"), "a-b");
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s1.jsonl");
        let header = SessionHeader {
            id: "s1".into(),
            cwd: "/tmp".into(),
            model: "m".into(),
            name: Some("work".into()),
            created: "2026-01-01T00:00:00Z".into(),
        };
        let messages = vec![
            Message::system("ignored on load"),
            Message::user("hi"),
            Message::assistant("hello"),
        ];
        save_session(&path, &header, &messages).unwrap();
        let (loaded_header, loaded_messages) = load_session(&path).unwrap();
        assert_eq!(loaded_header, header);
        assert_eq!(loaded_messages.len(), 2);
        assert_eq!(loaded_messages[0].content.as_deref(), Some("hi"));
    }

    #[test]
    fn load_fails_without_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s2.jsonl");
        fs::write(&path, "{\"role\":\"user\",\"content\":\"x\"}\n").unwrap();
        assert!(load_session(&path).is_err());
    }

    #[test]
    fn update_name_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s3.jsonl");
        let header = SessionHeader {
            id: "s3".into(),
            cwd: "/tmp".into(),
            model: "m".into(),
            name: None,
            created: "2026-01-02T00:00:00Z".into(),
        };
        save_session(&path, &header, &[Message::user("a")]).unwrap();
        update_name(&path, "renamed").unwrap();
        let (h, msgs) = load_session(&path).unwrap();
        assert_eq!(h.name.as_deref(), Some("renamed"));
        assert_eq!(msgs.len(), 1);
        update_name(&path, "").unwrap();
        assert_eq!(load_session(&path).unwrap().0.name, None);
    }

    #[test]
    fn lists_sessions_newest_first() {
        let root = tempfile::tempdir().unwrap();
        let cwd_dir = root.path().join("sessions");
        fs::create_dir_all(&cwd_dir).unwrap();
        let write = |id: &str, created: &str, name: Option<&str>| {
            let header = SessionHeader {
                id: id.into(),
                cwd: "/tmp/proj".into(),
                model: "m".into(),
                name: name.map(Into::into),
                created: created.into(),
            };
            save_session(
                &cwd_dir.join(format!("{id}.jsonl")),
                &header,
                &[Message::user("first question here")],
            )
            .unwrap();
        };
        write("old", "2026-01-01T00:00:00Z", None);
        write("new", "2026-02-01T00:00:00Z", Some("named"));
        let sessions = list_sessions_in(&cwd_dir, 10);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].header.id, "new");
        assert_eq!(sessions[0].first_prompt, "first question here");
        assert!(list_sessions_in(&root.path().join("missing"), 10).is_empty());
    }

    #[test]
    fn timestamps_look_sane() {
        let ts = timestamp();
        assert!(ts.ends_with('Z'), "{ts}");
        assert_eq!(ts.len(), 20, "{ts}");
        assert!(ts.starts_with("20"), "{ts}");
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(10957), (2000, 1, 1));
    }
}
