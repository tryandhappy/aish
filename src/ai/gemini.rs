use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::types::{extract_commands, AiResponse, ContextMessage, SessionContext};
use super::{AiProvider, ProviderKind};

pub struct GeminiProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl GeminiProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model: "gemini-2.0-flash".to_string(),
        }
    }
}

#[derive(Serialize)]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(rename = "systemInstruction")]
    system_instruction: GeminiContent,
}

#[derive(Serialize, Deserialize)]
struct GeminiContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    parts: Vec<GeminiPart>,
}

#[derive(Serialize, Deserialize)]
struct GeminiPart {
    text: String,
}

#[derive(Deserialize)]
struct GeminiResponse {
    candidates: Option<Vec<GeminiCandidate>>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    content: GeminiContent,
}

#[async_trait]
impl AiProvider for GeminiProvider {
    async fn send_message(&self, context: &SessionContext) -> Result<AiResponse> {
        let mut contents = Vec::new();

        for m in &context.messages {
            match m {
                ContextMessage::User { text } => {
                    contents.push(GeminiContent {
                        role: Some("user".to_string()),
                        parts: vec![GeminiPart { text: text.clone() }],
                    });
                }
                ContextMessage::Assistant { text } => {
                    contents.push(GeminiContent {
                        role: Some("model".to_string()),
                        parts: vec![GeminiPart { text: text.clone() }],
                    });
                }
                ContextMessage::CommandOutput { command, output } => {
                    contents.push(GeminiContent {
                        role: Some("user".to_string()),
                        parts: vec![GeminiPart {
                            text: format!("[Command executed: {}]\nOutput:\n{}", command, output),
                        }],
                    });
                }
            }
        }

        let request = GeminiRequest {
            contents,
            system_instruction: GeminiContent {
                role: None,
                parts: vec![GeminiPart {
                    text: context.system_prompt.clone(),
                }],
            },
        };

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.model, self.api_key
        );

        let response = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Gemini API error ({}): {}", status, body);
        }

        let gemini_resp: GeminiResponse = response.json().await?;
        let raw_text = gemini_resp
            .candidates
            .and_then(|c| c.into_iter().next())
            .map(|c| {
                c.content
                    .parts
                    .into_iter()
                    .map(|p| p.text)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();

        let (message, suggested_commands) = extract_commands(&raw_text);

        Ok(AiResponse {
            message,
            suggested_commands,
        })
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Gemini
    }

    fn name(&self) -> &str {
        "Gemini"
    }
}
