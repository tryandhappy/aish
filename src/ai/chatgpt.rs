use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::types::{extract_commands, AiResponse, ContextMessage, SessionContext};
use super::{AiProvider, ProviderKind};

pub struct ChatGptProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl ChatGptProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model: "gpt-4o".to_string(),
        }
    }
}

#[derive(Serialize)]
struct ChatGptRequest {
    model: String,
    messages: Vec<ChatGptMessage>,
    max_tokens: u32,
}

#[derive(Serialize)]
struct ChatGptMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatGptResponse {
    choices: Vec<ChatGptChoice>,
}

#[derive(Deserialize)]
struct ChatGptChoice {
    message: ChatGptMessageResp,
}

#[derive(Deserialize)]
struct ChatGptMessageResp {
    content: Option<String>,
}

#[async_trait]
impl AiProvider for ChatGptProvider {
    async fn send_message(&self, context: &SessionContext) -> Result<AiResponse> {
        let mut messages = vec![ChatGptMessage {
            role: "system".to_string(),
            content: context.system_prompt.clone(),
        }];

        for m in &context.messages {
            match m {
                ContextMessage::User { text } => {
                    messages.push(ChatGptMessage {
                        role: "user".to_string(),
                        content: text.clone(),
                    });
                }
                ContextMessage::Assistant { text } => {
                    messages.push(ChatGptMessage {
                        role: "assistant".to_string(),
                        content: text.clone(),
                    });
                }
                ContextMessage::CommandOutput { command, output } => {
                    messages.push(ChatGptMessage {
                        role: "user".to_string(),
                        content: format!("[Command executed: {}]\nOutput:\n{}", command, output),
                    });
                }
            }
        }

        let request = ChatGptRequest {
            model: self.model.clone(),
            messages,
            max_tokens: 4096,
        };

        let response = self
            .client
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("ChatGPT API error ({}): {}", status, body);
        }

        let gpt_resp: ChatGptResponse = response.json().await?;
        let raw_text = gpt_resp
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default();

        let (message, suggested_commands) = extract_commands(&raw_text);

        Ok(AiResponse {
            message,
            suggested_commands,
        })
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::ChatGpt
    }

    fn name(&self) -> &str {
        "ChatGPT"
    }
}
