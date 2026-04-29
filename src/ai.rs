use crate::config::LogConfig;
use serde::Deserialize;
use std::io::{Read, Write};
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

/// claude CLI 呼び出し時に常に拒否するツール群。
/// aish は提案ベースで動くので、AI が直接実行・編集するツールは無効化する。
const DISALLOWED_TOOLS: &str = "Bash,Edit,Write,Read";

pub const CANCELLED: &str = "Cancelled";

#[derive(Debug, Deserialize)]
pub struct AiResponse {
    pub message: String,
    pub commands: Vec<String>,
}

pub struct AiSession {
    session_id: Option<String>,
    system_prompt: String,
    log_path: Option<String>,
}

impl AiSession {
    pub fn new(system_prompt: &str, language: &str, log_config: &LogConfig) -> Self {
        let log_path = if log_config.enabled {
            let path = expand_tilde(&log_config.path);
            Some(path)
        } else {
            None
        };
        let system_prompt = if language.is_empty() {
            system_prompt.to_string()
        } else {
            format!("{system_prompt} Respond in {language}.")
        };
        Self {
            session_id: None,
            system_prompt,
            log_path,
        }
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
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
                "```terminal\n{terminal_context}\n```\n\n{user_prompt}"
            )
        };

        // 共通フラグ + 初回 vs resume の差分を組み立てる。
        // 安全制約 (--disallowedTools) と出力形式は毎回明示する。
        // --append-system-prompt は append 動作のため初回のみ（resume でも付けると二重に追加される）。
        let mut args: Vec<String> = vec!["-p".to_string()];

        if let Some(ref sid) = self.session_id {
            args.push("--resume".to_string());
            args.push(sid.clone());
        } else {
            let system = format!(
                "{} コマンドを提案してください。直接実行しないでください。1度のレスポンスで提案するコマンドは1つだけにしてください。複数のステップが必要な場合は、実行結果を確認してから次のコマンドを提案してください。&&や||による条件付き実行は1つのコマンドとして維持してください。",
                self.system_prompt
            );
            args.push("--append-system-prompt".to_string());
            args.push(system);
        }

        args.push("--output-format".to_string());
        args.push("json".to_string());
        args.push("--disallowedTools".to_string());
        args.push(DISALLOWED_TOOLS.to_string());
        args.push("--json-schema".to_string());
        args.push(AI_RESPONSE_SCHEMA.to_string());
        // prompt は引数ではなく stdin で渡す。
        // ターミナルコンテキストを含む prompt が ARG_MAX (~2MB) を超えると
        // execve() が E2BIG (`Argument list too long`, os error 7) で失敗するため。

        let mut child = Command::new("claude")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        write_log(&self.log_path, &format!("claude {}", shell_join(&args)));
        write_log(&self.log_path, &format!("[prompt via stdin]\n{prompt}"));

        // prompt を子プロセスの stdin に書き込み、EOF を伝えるために close する。
        // close しないと claude は入力待ちで永遠にブロックする。
        {
            let mut stdin = child.stdin.take().expect("stdin should be piped");
            stdin.write_all(prompt.as_bytes())?;
            // stdin はスコープを抜けて drop されると close される
        }

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

        write_log(&self.log_path, stdout.trim());
        if !stderr.trim().is_empty() {
            write_log(&self.log_path, &format!("[stderr]\n{}", stderr.trim()));
        }

        if !status.success() {
            return Err(format!("claude command failed: {stderr}").into());
        }

        let stdout_trimmed = stdout.trim();
        if stdout_trimmed.is_empty() {
            return Err(format!("claude returned empty output. stderr: {stderr}").into());
        }

        // claude CLIの出力にJSON以外のテキストが含まれる場合があるため、
        // JSON部分を抽出する
        let json_str = extract_json(stdout_trimmed)
            .ok_or_else(|| format!("No JSON found in claude output: {stdout_trimmed}"))?;

        let claude_output: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| format!("Failed to parse claude output: {e}\nRaw: {stdout_trimmed}"))?;

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
                        "claude returned empty result.\nFull output: {stdout_trimmed}"
                    ).into());
                }
                serde_json::from_str::<AiResponse>(s).unwrap_or_else(|_| AiResponse {
                    message: s.to_string(),
                    commands: vec![],
                })
            }
            _ => {
                return Err(format!(
                    "Unexpected result from claude.\nFull output: {stdout_trimmed}"
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

/// ~をホームディレクトリに展開する
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().to_string();
        }
    }
    path.to_string()
}

/// ログエントリをファイルに追記する。log_pathがNoneなら何もしない。
fn write_log(log_path: &Option<String>, entry: &str) {
    let path = match log_path {
        Some(p) => p,
        None => return,
    };
    let path = std::path::Path::new(path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let now = timestamp_local();
        let _ = writeln!(file, "=== {now} ===");
        let _ = writeln!(file, "{entry}");
        let _ = writeln!(file);
    }
}

/// ローカルタイムのタイムスタンプを返す (YYYY-MM-DD HH:MM:SS)
fn timestamp_local() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // ローカルTZオフセットを取得
    let offset = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = now as libc::time_t;
        libc::localtime_r(&t, &mut tm);
        tm.tm_gmtoff
    };
    let local = now as i64 + offset;
    let secs = local % 60;
    let mins = (local / 60) % 60;
    let hrs = (local / 3600) % 24;
    // 日付計算（簡易: days since epoch → year/month/day）
    let days = local / 86400;
    let (y, m, d) = days_to_ymd(days);
    format!("{y:04}-{m:02}-{d:02} {hrs:02}:{mins:02}:{secs:02}")
}

fn days_to_ymd(days: i64) -> (i64, i64, i64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// 引数をシェル表示用に結合する（スペースを含む引数はクォート）
fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.contains(' ') || a.contains('"') || a.contains('\n') {
                format!("'{}'", a.replace('\'', "'\\''"))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_pure() {
        assert_eq!(extract_json(r#"{"a":1}"#), Some(r#"{"a":1}"#));
    }

    #[test]
    fn extract_json_with_prefix_and_suffix() {
        let s = "noise before {\"a\":1} noise after";
        assert_eq!(extract_json(s), Some(r#"{"a":1}"#));
    }

    #[test]
    fn extract_json_nested_object() {
        let s = r#"{"a":{"b":[1,2]},"c":"d"}"#;
        assert_eq!(extract_json(s), Some(s));
    }

    #[test]
    fn extract_json_brace_inside_string() {
        // 文字列内の { } を depth に算入してはいけない。
        let s = r#"{"msg":"open { close }"}"#;
        assert_eq!(extract_json(s), Some(s));
    }

    #[test]
    fn extract_json_escaped_quote() {
        // 文字列内のエスケープ済み " を文字列終端と誤認しない。
        let s = r#"{"msg":"say \"hi\" {"}"#;
        assert_eq!(extract_json(s), Some(s));
    }

    #[test]
    fn extract_json_returns_none_when_unbalanced() {
        assert_eq!(extract_json(r#"{"a":1"#), None);
    }

    #[test]
    fn extract_json_returns_none_when_no_brace() {
        assert_eq!(extract_json("plain text"), None);
    }

    #[test]
    fn extract_json_picks_first_balanced_object() {
        // 複数の独立オブジェクトが並んでいた場合、最初のバランス済みを返す。
        let s = r#"{"a":1}{"b":2}"#;
        assert_eq!(extract_json(s), Some(r#"{"a":1}"#));
    }
}
