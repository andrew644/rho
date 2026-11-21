use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

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
    pub channel_id: String,
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
        Self { max_turns_per_run: default_max_turns() }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Config {
    pub provider: ProviderConfig,
    pub model: ModelConfig,
    #[serde(default = "default_context_window")]
    pub context_window: u32,
    #[serde(default)]
    pub discord: DiscordConfig,
    #[serde(default)]
    pub loop_config: LoopConfig,
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
  channel_id: "123"
  allowed_user_id: "456"
loop_config:
  max_turns_per_run: 10
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
        assert!(cfg.discord.enabled);
        assert_eq!(cfg.discord.channel_id, "123");
        assert_eq!(cfg.loop_config.max_turns_per_run, 10);
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
        assert!(!cfg.discord.enabled);
        assert_eq!(cfg.loop_config.max_turns_per_run, 64);
    }

    #[test]
    fn missing_file_is_an_error() {
        let err = Config::load_from(std::path::Path::new("/nonexistent/rho/config.yaml"));
        assert!(err.is_err());
    }
}
