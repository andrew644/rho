use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::clipboard::ClipboardMode;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ProviderConfig {
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ModelConfig {
    pub id: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default)]
    pub models: Vec<String>,
}

fn default_max_tokens() -> u32 {
    4096
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct DiscordConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub bot_token: String,
    #[serde(default)]
    pub allowed_user_id: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct LoopConfig {
    #[serde(default = "default_max_turns")]
    pub max_turns_per_run: usize,
}

fn default_max_turns() -> usize {
    64
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            max_turns_per_run: default_max_turns(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CompactionConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_reserve_tokens")]
    pub reserve_tokens: usize,
    #[serde(default = "default_keep_recent_tokens")]
    pub keep_recent_tokens: usize,
}

fn default_true() -> bool {
    true
}

fn default_reserve_tokens() -> usize {
    crate::compaction::RESERVE_TOKENS
}

fn default_keep_recent_tokens() -> usize {
    crate::compaction::KEEP_RECENT_TOKENS
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: default_reserve_tokens(),
            keep_recent_tokens: default_keep_recent_tokens(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ThemeName {
    #[default]
    Dark,
    Light,
    Ansi,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Config {
    pub provider: ProviderConfig,
    pub model: ModelConfig,
    #[serde(default = "default_context_window")]
    pub context_window: u32,
    #[serde(default)]
    pub theme: ThemeName,
    #[serde(default)]
    pub clipboard: ClipboardMode,
    #[serde(default)]
    pub discord: DiscordConfig,
    #[serde(default)]
    pub loop_config: LoopConfig,
    #[serde(default)]
    pub compaction: CompactionConfig,
}

fn default_context_window() -> u32 {
    200000
}

impl Config {
    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rho")
            .join("config.yaml")
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        Self::load_from(&path)
    }

    pub fn load_from(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config from {}", path.display()))?;
        let mut cfg: Config = serde_yaml::from_str(&text)
            .with_context(|| format!("failed to parse config from {}", path.display()))?;
        cfg.provider.base_url = cfg.provider.base_url.trim_end_matches('/').to_string();
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let yaml = r#"
provider:
  base_url: http://localhost:8000/v1/
  api_key: "1234"
model:
  id: qwen4exp
  max_tokens: 2048
context_window: 100000
discord:
  enabled: true
  bot_token: tok
  allowed_user_id: "456"
loop_config:
  max_turns_per_run: 10
compaction:
  enabled: false
  reserve_tokens: 8192
  keep_recent_tokens: 4096
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, yaml).unwrap();
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.provider.base_url, "http://localhost:8000/v1");
        assert_eq!(cfg.provider.api_key, "1234");
        assert_eq!(cfg.model.id, "qwen4exp");
        assert_eq!(cfg.model.max_tokens, 2048);
        assert_eq!(cfg.context_window, 100000);
        assert_eq!(cfg.theme, ThemeName::Dark);
        assert!(cfg.discord.enabled);
        assert_eq!(cfg.discord.allowed_user_id, "456");
        assert_eq!(cfg.loop_config.max_turns_per_run, 10);
        assert!(!cfg.compaction.enabled);
        assert_eq!(cfg.compaction.reserve_tokens, 8192);
        assert_eq!(cfg.compaction.keep_recent_tokens, 4096);
    }

    #[test]
    fn a_config_naming_the_channel_replies_used_to_fall_back_to_still_loads() {
        let yaml = r#"
provider:
  base_url: http://localhost:8000/v1
model:
  id: test-model
discord:
  enabled: true
  bot_token: tok
  channel_id: "123"
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, yaml).unwrap();
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.discord.enabled);
    }

    #[test]
    fn applies_defaults() {
        let yaml = r#"
provider:
  base_url: http://localhost:8000/v1
model:
  id: test-model
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, yaml).unwrap();
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.model.max_tokens, 4096);
        assert_eq!(cfg.context_window, 200000);
        assert_eq!(cfg.theme, ThemeName::Dark);
        assert!(!cfg.discord.enabled);
        assert_eq!(cfg.loop_config.max_turns_per_run, 64);
        assert!(cfg.compaction.enabled);
        assert_eq!(cfg.compaction.reserve_tokens, 16_384);
        assert_eq!(cfg.compaction.keep_recent_tokens, 20_000);
        assert!(cfg.model.models.is_empty());
        assert_eq!(cfg.clipboard, ClipboardMode::Auto);
    }

    #[test]
    fn parses_theme() {
        let yaml = r#"
provider:
  base_url: http://localhost:8000/v1
model:
  id: test-model
theme: light
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, yaml).unwrap();
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.theme, ThemeName::Light);
    }

    #[test]
    fn parses_clipboard_mode() {
        let yaml = r#"
provider:
  base_url: http://localhost:8000/v1
model:
  id: test-model
clipboard: terminal
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, yaml).unwrap();
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.clipboard, ClipboardMode::Terminal);
    }

    #[test]
    fn missing_file_is_an_error() {
        let err = Config::load_from(std::path::Path::new("/nonexistent/rho/config.yaml"));
        assert!(err.is_err());
    }
}
