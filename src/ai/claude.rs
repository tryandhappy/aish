use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::types::{extract_commands, AiResponse, ContextMessage, SessionContext};
use super::{AiProvider, ProviderKind};

pub struct ClaudeProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl ClaudeProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model: "claude-sonnet-4-20250514".to_string(),
        }
    }
}

#[derive(Serialize)]
struct ClaudeRequest {
    model: String,
    max_tokens: u32,
    system: String,
    messages: Vec<ClaudeMessage>,
}

#[derive(Serialize)]
struct ClaudeMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ClaudeResponse {
    content: Vec<ClaudeContent>,
}

#[derive(Deserialize)]
struct ClaudeContent {
    text: Option<String>,
}

#[async_trait]
impl AiProvider for ClaudeProvider {
    async fn send_message(&self, context: &SessionContext) -> Result<AiResponse> {
        let messages: Vec<ClaudeMessage> = context
            .messages
            .iter()
            .map(|m| match m {
                ContextMessage::User { text } => ClaudeMessage {
                    role: "user".to_string(),
                    content: text.clone(),
                },
                ContextMessage::Assistant { text } => ClaudeMessage {
                    role: "assistant".to_string(),
                    content: text.clone(),
                },
                ContextMessage::CommandOutput { command, output } => ClaudeMessage {
                    role: "user".to_string(),
                    content: format!("[Command executed: {}]\nOutput:\n{}", command, output),
                },
            })
            .collect();

        let request = ClaudeRequest {
            model: self.model.clone(),
            max_tokens: 4096,
            system: context.system_prompt.clone(),
            messages,
        };

        let response = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Claude API error ({}): {}", status, body);
        }

        let claude_resp: ClaudeResponse = response.json().await?;
        let raw_text = claude_resp
            .content
            .into_iter()
            .filter_map(|c| c.text)
            .collect::<Vec<_>>()
            .join("\n");

        let (message, suggested_commands) = extract_commands(&raw_text);

        Ok(AiResponse {
            message,
            suggested_commands,
        })
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Claude
    }

    fn name(&self) -> &str {
        "Claude"
    }
}
