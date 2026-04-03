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

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            return Err(format!("claude command failed: {}", stderr).into());
        }

        let stdout_trimmed = stdout.trim();
        if stdout_trimmed.is_empty() {
            return Err(format!("claude returned empty output. stderr: {}", stderr).into());
        }

        // claude CLIの出力にJSON以外のテキストが含まれる場合があるため、
        // JSON部分を抽出する
        let json_str = extract_json(stdout_trimmed)
            .ok_or_else(|| format!("No JSON found in claude output: {}", stdout_trimmed))?;

        let claude_output: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| format!("Failed to parse claude output: {}\nRaw: {}", e, stdout_trimmed))?;

        if let Some(sid) = claude_output["session_id"].as_str() {
            if self.session_id.is_none() {
                self.session_id = Some(sid.to_string());
            }
        }

        // --json-schema使用時はstructured_outputにレスポンスが入る
        // structured_outputがなければresultにフォールバック
        let result_value = if claude_output["structured_output"].is_object() {
            &claude_output["structured_output"]
        } else {
            &claude_output["result"]
        };

        let ai_response = match result_value {
            serde_json::Value::Object(_) => {
                serde_json::from_value::<AiResponse>(result_value.clone())
                    .unwrap_or_else(|_| AiResponse {
                        message: result_value.to_string(),
                        commands: vec![],
                    })
            }
            serde_json::Value::String(s) => {
                let s = s.trim();
                if s.is_empty() {
                    return Err(format!(
                        "claude returned empty result.\nFull output: {}",
                        stdout_trimmed
                    ).into());
                }
                serde_json::from_str::<AiResponse>(s).unwrap_or_else(|_| AiResponse {
                    message: s.to_string(),
                    commands: vec![],
                })
            }
            _ => {
                return Err(format!(
                    "Unexpected result from claude.\nFull output: {}",
                    stdout_trimmed
                ).into());
            }
        };
        Ok(ai_response)
    }
}

/// stdout から最外のJSONオブジェクトを抽出する。
/// claude CLIがJSON前後にテキストを出力する場合に対応。
fn extract_json(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape_next = false;
    for (i, ch) in s[start..].char_indices() {
        if escape_next {
            escape_next = false;
            continue;
        }
        match ch {
            '\\' if in_string => escape_next = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..start + i + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

pub fn check_claude_installed() -> bool {
    Command::new("claude")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
