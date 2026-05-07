use serde::Deserialize;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

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

const AI_COLOR_START: &str = "\x1b[46m";
const AI_COLOR_END: &str = "\x1b[0m";

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

        let mut cmd = Command::new("claude");

        if let Some(ref sid) = self.session_id {
            cmd.args(["-p", "--resume", sid]);
        } else {
            cmd.args([
                "-p",
                "--disallowedTools",
                "Bash,Edit,Write,Read",
                "--append-system-prompt",
                &format!(
                    "{} コマンドを提案してください。直接実行しないでください。",
                    self.system_prompt
                ),
            ]);
        }

        cmd.args([
            "--output-format",
            "stream-json",
            "--json-schema",
            AI_RESPONSE_SCHEMA,
            &prompt,
        ]);

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().unwrap();
        let reader = BufReader::new(stdout);

        let mut result_json = String::new();
        let mut session_id = String::new();
        let mut displayed = false;

        for line in reader.lines() {
            let line = line?;
            if line.is_empty() {
                continue;
            }

            let event: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let subtype = event.get("subtype").and_then(|v| v.as_str()).unwrap_or("");

            match event_type {
                "assistant" => {
                    if let Some(text) = event.get("text").and_then(|v| v.as_str()) {
                        if subtype == "thinking" {
                            // 思考内容をリアルタイム表示
                            if !displayed {
                                print!("{}", AI_COLOR_START);
                                displayed = true;
                            }
                            print!("{}", text);
                            std::io::stdout().flush().ok();
                        }
                        // subtype "text" は JSON Schema のレスポンス構築なので非表示
                    }
                }
                "result" => {
                    result_json = event
                        .get("result")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    session_id = event
                        .get("session_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                }
                _ => {}
            }
        }

        if displayed {
            print!("{}\n", AI_COLOR_END);
            std::io::stdout().flush().ok();
        }

        let status = child.wait()?;

        if !status.success() {
            let mut stderr_buf = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                use std::io::Read;
                stderr.read_to_string(&mut stderr_buf).ok();
            }
            return Err(format!("claude command failed: {}", stderr_buf).into());
        }

        if result_json.is_empty() {
            return Err("claude returned no result".into());
        }

        if self.session_id.is_none() {
            self.session_id = Some(session_id);
        }

        let ai_response: AiResponse = serde_json::from_str(&result_json).map_err(|e| {
            format!(
                "Failed to parse AI response: {}\nresult: {}",
                e, result_json
            )
        })?;
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
