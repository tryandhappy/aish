use serde::Deserialize;
use std::process::Command;

const AI_RESPONSE_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "message": { "type": "string", "description": "ユーザへの説明" },
    "commands": {
      "type": "array",
      "items": { "type": "string" },
      "description": "実行を提案するコマンドのリスト(空配列も可)"
    }
  },
  "required": ["message", "commands"]
}"#;

#[derive(Debug, Deserialize)]
pub struct AiResponse {
    pub message: String,
    pub commands: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ClaudeJsonOutput {
    pub result: String,
    pub session_id: String,
}

pub struct AiSession {
    session_id: Option<String>,
    system_prompt: String,
}

impl AiSession {
    pub fn new(system_prompt: &str) -> Self {
        Self {
            session_id: None,
            system_prompt: system_prompt.to_string(),
        }
    }

    pub fn send(
        &mut self,
        terminal_context: &str,
        user_prompt: &str,
    ) -> Result<AiResponse, Box<dyn std::error::Error>> {
        let prompt = if terminal_context.is_empty() {
            user_prompt.to_string()
        } else {
            format!(
                "```terminal\n{}\n```\n\n{}",
                terminal_context, user_prompt
            )
        };

        let output = if let Some(ref sid) = self.session_id {
            Command::new("claude")
                .args([
                    "-p",
                    "--resume",
                    sid,
                    "--output-format",
                    "json",
                    "--json-schema",
                    AI_RESPONSE_SCHEMA,
                    &prompt,
                ])
                .output()?
        } else {
            Command::new("claude")
                .args([
                    "-p",
                    "--output-format",
                    "json",
                    "--disallowedTools",
                    "Bash,Edit,Write,Read",
                    "--append-system-prompt",
                    &format!(
                        "{} コマンドを提案してください。直接実行しないでください。",
                        self.system_prompt
                    ),
                    "--json-schema",
                    AI_RESPONSE_SCHEMA,
                    &prompt,
                ])
                .output()?
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("claude command failed: {}", stderr).into());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let claude_output: ClaudeJsonOutput = serde_json::from_str(&stdout)?;

        if self.session_id.is_none() {
            self.session_id = Some(claude_output.session_id.clone());
        }

        let ai_response: AiResponse = serde_json::from_str(&claude_output.result)?;
        Ok(ai_response)
    }
}

pub fn check_claude_installed() -> bool {
    Command::new("claude")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
