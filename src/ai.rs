use serde::Deserialize;
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

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

pub const CANCELLED: &str = "Cancelled";

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

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Claude Codeをインタラクティブモードで起動し、セッションを引き継ぐ
    pub fn launch_interactive(&self) -> Result<std::process::ExitStatus, Box<dyn std::error::Error>> {
        let sid = self.session_id.as_ref().ok_or("No session ID")?;
        let status = Command::new("claude")
            .args(["--resume", sid])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()?;
        Ok(status)
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

        let mut child = if let Some(ref sid) = self.session_id {
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
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?
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
                        "{} コマンドを提案してください。直接実行しないでください。コマンドは;で結合せず個別に提案してください。ただし&&や||による条件付き実行は1つのコマンドとして維持してください。",
                        self.system_prompt
                    ),
                    "--json-schema",
                    AI_RESPONSE_SCHEMA,
                    &prompt,
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?
        };

        // stdout/stderrを別スレッドで読み取り
        let child_stdout = child.stdout.take().unwrap();
        let child_stderr = child.stderr.take().unwrap();

        let stdout_handle = thread::spawn(move || {
            let mut buf = Vec::new();
            let mut r = child_stdout;
            let _ = r.read_to_end(&mut buf);
            buf
        });

        let stderr_handle = thread::spawn(move || {
            let mut buf = Vec::new();
            let mut r = child_stderr;
            let _ = r.read_to_end(&mut buf);
            buf
        });

        // 子プロセス完了を待ちつつ、Ctrl+Cをチェック
        let status = loop {
            match child.try_wait()? {
                Some(status) => break status,
                None => {
                    if check_stdin_cancel() {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = stdout_handle.join();
                        let _ = stderr_handle.join();
                        return Err(CANCELLED.into());
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
        };

        let stdout_bytes = stdout_handle.join().unwrap_or_default();
        let stderr_bytes = stderr_handle.join().unwrap_or_default();
        let stdout = String::from_utf8_lossy(&stdout_bytes);
        let stderr = String::from_utf8_lossy(&stderr_bytes);

        if !status.success() {
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

/// stdinからCtrl+C (0x03) が入力されているかノンブロッキングでチェック
#[cfg(unix)]
fn check_stdin_cancel() -> bool {
    use std::os::unix::io::AsRawFd;
    let fd = std::io::stdin().as_raw_fd();
    let mut found = false;
    loop {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
        if ret <= 0 || (pfd.revents & libc::POLLIN) == 0 {
            break;
        }
        let mut buf = [0u8; 1];
        match std::io::stdin().read(&mut buf) {
            Ok(1) if buf[0] == 0x03 => found = true,
            Ok(1) => {} // Ctrl+C以外は破棄
            _ => break,
        }
    }
    found
}

#[cfg(not(unix))]
fn check_stdin_cancel() -> bool {
    false
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
