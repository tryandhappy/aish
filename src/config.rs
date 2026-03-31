use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;

use crate::ai::ProviderKind;

#[derive(Deserialize, Default, Debug)]
pub struct Config {
    pub anthropic_api_key: Option<String>,
    pub openai_api_key: Option<String>,
    pub google_api_key: Option<String>,
    pub default_ai: Option<String>,
    #[serde(default)]
    pub ssh: SshConfig,
}

#[derive(Deserialize, Default, Debug)]
pub struct SshConfig {
    pub identity_file: Option<String>,
    pub extra_args: Option<Vec<String>>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let config_path = Self::config_path();
        let mut config = if config_path.exists() {
            let text = std::fs::read_to_string(&config_path)?;
            toml::from_str(&text)?
        } else {
            Config::default()
        };

        // Environment variables override file values
        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            config.anthropic_api_key = Some(key);
        }
        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            config.openai_api_key = Some(key);
        }
        if let Ok(key) = std::env::var("GOOGLE_API_KEY") {
            config.google_api_key = Some(key);
        }

        Ok(config)
    }

    pub fn api_key_for(&self, provider: ProviderKind) -> Option<&str> {
        match provider {
            ProviderKind::Claude => self.anthropic_api_key.as_deref(),
            ProviderKind::ChatGpt => self.openai_api_key.as_deref(),
            ProviderKind::Gemini => self.google_api_key.as_deref(),
        }
    }

    fn config_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".aish")
            .join("config.toml")
    }
}
