pub mod chatgpt;
pub mod claude;
pub mod gemini;
pub mod types;

use std::collections::HashMap;
use std::fmt;

use anyhow::{bail, Result};
use async_trait::async_trait;

use crate::config::Config;
use types::{AiResponse, SessionContext};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    Claude,
    ChatGpt,
    Gemini,
}

impl ProviderKind {
    pub fn from_name(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "claude" => Some(Self::Claude),
            "chatgpt" => Some(Self::ChatGpt),
            "gemini" => Some(Self::Gemini),
            _ => None,
        }
    }

    pub fn all() -> &'static [ProviderKind] {
        &[Self::Claude, Self::ChatGpt, Self::Gemini]
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Claude => write!(f, "Claude"),
            Self::ChatGpt => write!(f, "ChatGPT"),
            Self::Gemini => write!(f, "Gemini"),
        }
    }
}

#[async_trait]
pub trait AiProvider: Send + Sync {
    async fn send_message(&self, context: &SessionContext) -> Result<AiResponse>;
    fn kind(&self) -> ProviderKind;
    fn name(&self) -> &str;
}

pub struct AiManager {
    providers: HashMap<ProviderKind, Box<dyn AiProvider>>,
    active: ProviderKind,
}

impl AiManager {
    pub fn new(config: &Config) -> Result<Self> {
        let mut providers: HashMap<ProviderKind, Box<dyn AiProvider>> = HashMap::new();

        if let Some(ref key) = config.anthropic_api_key {
            providers.insert(
                ProviderKind::Claude,
                Box::new(claude::ClaudeProvider::new(key.clone())),
            );
        }
        if let Some(ref key) = config.openai_api_key {
            providers.insert(
                ProviderKind::ChatGpt,
                Box::new(chatgpt::ChatGptProvider::new(key.clone())),
            );
        }
        if let Some(ref key) = config.google_api_key {
            providers.insert(
                ProviderKind::Gemini,
                Box::new(gemini::GeminiProvider::new(key.clone())),
            );
        }

        // Determine default AI
        let default_kind = config
            .default_ai
            .as_deref()
            .and_then(ProviderKind::from_name)
            .unwrap_or(ProviderKind::Claude);

        let active = if providers.contains_key(&default_kind) {
            default_kind
        } else if let Some(kind) = providers.keys().next().copied() {
            kind
        } else {
            bail!("APIキーが設定されていません。環境変数 (ANTHROPIC_API_KEY, OPENAI_API_KEY, GOOGLE_API_KEY) または ~/.aish/config.toml で設定してください。");
        };

        Ok(Self { providers, active })
    }

    pub fn active_provider(&self) -> &dyn AiProvider {
        self.providers[&self.active].as_ref()
    }

    pub fn active_kind(&self) -> ProviderKind {
        self.active
    }

    pub fn switch(&mut self, kind: ProviderKind) -> Result<()> {
        if !self.providers.contains_key(&kind) {
            bail!(
                "{} のAPIキーが設定されていません。",
                kind
            );
        }
        self.active = kind;
        Ok(())
    }

    pub fn get_provider(&self, kind: ProviderKind) -> Option<&dyn AiProvider> {
        self.providers.get(&kind).map(|p| p.as_ref())
    }

    pub fn available_providers(&self) -> Vec<ProviderKind> {
        self.providers.keys().copied().collect()
    }
}
