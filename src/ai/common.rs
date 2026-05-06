use super::types::{AiError, AiResponse};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// stdinからCtrl+C (0x03) が入力されているかノンブロッキングでチェック
#[cfg(unix)]
pub(crate) fn check_stdin_cancel() -> bool {
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
pub(crate) fn check_stdin_cancel() -> bool {
    false
}

/// stdout から最外のJSONオブジェクトを抽出する。
/// claude CLIがJSON前後にテキストを出力する場合に対応。
pub(crate) fn extract_json(s: &str) -> Option<&str> {
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

/// ~をホームディレクトリに展開する
pub(crate) fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().to_string();
        }
    }
    path.to_string()
}

/// ログエントリをファイルに追記する。log_pathがNoneなら何もしない。
pub(crate) fn write_log(log_path: &Option<String>, entry: &str) {
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
pub(crate) fn timestamp_local() -> String {
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
pub(crate) fn shell_join(args: &[String]) -> String {
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

/// system_prompt に language 指示を末尾連結する。
/// language が空なら system_prompt をそのまま返す。
pub(crate) fn build_system_prompt(prompt: &str, language: &str) -> String {
    if language.is_empty() {
        prompt.to_string()
    } else {
        format!("{prompt} Respond in {language}.")
    }
}

/// JSON Schema フラグを持たない backend (codex/gemini/qwen) 向けの system prompt。
/// JSON 単独出力を強く指示し、aish の信頼原則 (提案ベース・無書き込み・ツール非使用) を埋め込む。
pub(crate) fn build_proposal_system_prompt(base: &str, language: &str) -> String {
    let lang_part = if language.is_empty() {
        String::new()
    } else {
        format!(" Respond in {language}.")
    };
    format!(
        "{base}{lang_part}\n\n\
         重要 (安全制約):\n\
         - あなたは aish の提案エンジンです。**いかなるツール呼び出しも行わないでください**。\
         shell exec, file read, file write, web search, code interpreter 等のいずれも禁止です。\n\
         - ターミナルの内容は既に下記 ```terminal``` ブロックに含まれています。\
         追加の情報収集はせず、与えられた情報だけで判断してください。\n\
         - コマンドは「提案のみ」行ってください。\
         直接の実行・ファイル編集・サーバへの書き込みは一切行わないでください。\
         実行可否は aish が画面でユーザに確認します。\n\n\
         応答ルール:\n\
         - 1度のレスポンスで提案するコマンドは1つだけにしてください。複数ステップが必要な場合は、\
         実行結果を確認してから次のコマンドを提案してください。\n\
         - &&や||による条件付き実行は1つのコマンドとして維持してください。\n\n\
         出力フォーマット: 必ず以下の JSON だけを 1 つ出力してください。\
         前後に説明文・コードフェンス・追加テキストを付けないでください。\n\
         {{\"message\": \"ユーザへの説明\", \"commands\": [\"提案コマンド\"]}}\n\
         追加のコマンド提案が不要な場合は commands を [] にしてください。"
    )
}

/// system + 過去ターン履歴 + terminal context + user prompt を 1 本のプロンプトに連結する。
/// session resume が無い backend (codex/gemini/qwen) で毎回フルコンテキストを送るための整形。
/// `history` は (user_prompt, ai_message) の時系列リスト。空でも OK。
pub(crate) fn build_full_prompt(
    system: &str,
    history: &[(String, String)],
    terminal_context: &str,
    user_prompt: &str,
) -> String {
    let history_block = if history.is_empty() {
        String::new()
    } else {
        let mut s = String::from("## これまでの会話\n\n");
        for (i, (u, a)) in history.iter().enumerate() {
            s.push_str(&format!("### Turn {}\nユーザ: {u}\nAI: {a}\n\n", i + 1));
        }
        s.push_str("## 現在のターン\n\n");
        s
    };
    let context_block = if terminal_context.is_empty() {
        String::new()
    } else {
        format!("```terminal\n{terminal_context}\n```\n\n")
    };
    format!("{system}\n\n{history_block}{context_block}{user_prompt}")
}

/// 直近 `max_turns` 件だけ残すように履歴をトリムする。
/// 古いターンから捨てる。
pub(crate) fn trim_history(history: &mut Vec<(String, String)>, max_turns: usize) {
    while history.len() > max_turns {
        history.remove(0);
    }
}

/// `extra_args` から `-m <X>` / `--model <X>` / `-m=X` / `--model=X` を検出してモデル名を返す。
/// CLI ごとの差異を気にせず単純に `-m` か `--model` を見るだけの軽量パーサ。
pub(crate) fn extract_model_from_args(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "-m" || arg == "--model" {
            return iter.next().cloned();
        }
        if let Some(rest) = arg.strip_prefix("--model=") {
            return Some(rest.to_string());
        }
        if let Some(rest) = arg.strip_prefix("-m=") {
            return Some(rest.to_string());
        }
    }
    None
}

/// `override_model` が `Some` のときに限り、`extra_args` から既存の `-m` / `--model` 指定
/// (空白区切り・= 区切り両方) を除去し、末尾に `--model <name>` を追加する。
/// `None` のときは `extra_args` をそのまま返す (既存の `-m` 指定を尊重)。
/// `--model` / `[ai].model` で指定された値を最終的に CLI に渡すためのユーティリティ。
pub(crate) fn override_model_in_args(
    extra_args: &[String],
    override_model: Option<&str>,
) -> Vec<String> {
    let Some(m) = override_model else {
        return extra_args.to_vec();
    };
    let mut out = Vec::with_capacity(extra_args.len() + 2);
    let mut i = 0;
    while i < extra_args.len() {
        let a = &extra_args[i];
        if a == "-m" || a == "--model" {
            i += 2; // フラグ + 値の 2 個をスキップ (値が無くても安全に進む)
            continue;
        }
        if a.starts_with("-m=") || a.starts_with("--model=") {
            i += 1;
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    out.push("--model".to_string());
    out.push(m.to_string());
    out
}

/// 子プロセスを spawn し stdin に prompt を流して stdout 全体を返す。
/// Ctrl+C でキャンセル、stderr はログに記録。
pub(crate) fn run_cli_capture_stdout(
    program: &str,
    args: &[String],
    stdin_input: &str,
    log_path: &Option<String>,
) -> Result<String, AiError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    write_log(log_path, &format!("{program} {}", shell_join(args)));
    write_log(log_path, &format!("[prompt via stdin]\n{stdin_input}"));

    {
        let mut stdin = child.stdin.take().expect("stdin should be piped");
        stdin.write_all(stdin_input.as_bytes())?;
        // drop で close → EOF
    }

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

    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None => {
                if check_stdin_cancel() {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_handle.join();
                    let _ = stderr_handle.join();
                    return Err(AiError::Cancelled);
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    };

    let stdout_bytes = stdout_handle.join().unwrap_or_default();
    let stderr_bytes = stderr_handle.join().unwrap_or_default();
    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();

    write_log(log_path, stdout.trim());
    if !stderr.trim().is_empty() {
        write_log(log_path, &format!("[stderr]\n{}", stderr.trim()));
    }

    if !status.success() {
        return Err(AiError::NonZeroExit { stderr });
    }

    if stdout.trim().is_empty() {
        return Err(AiError::EmptyOutput { stderr });
    }

    Ok(stdout)
}

/// 出力テキストから AiResponse を抽出する。JSON が見つからなければ全文を message としてフォールバック。
/// JSON Schema 強制が無い backend で使う。
pub(crate) fn parse_ai_response_lossy(raw: &str) -> AiResponse {
    if let Some(json_str) = extract_json(raw) {
        if let Ok(resp) = serde_json::from_str::<AiResponse>(json_str) {
            return resp;
        }
    }
    AiResponse {
        message: raw.trim().to_string(),
        commands: Vec::new(),
    }
}

/// `~/.aish/tmp/` 以下にユニークな一時ファイルパスを生成する。実ファイルは作らない。
/// codex の `--output-last-message` 用。プロセスID + 単調カウンタで衝突回避。
pub(crate) fn unique_tmp_path(suffix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = if let Some(home) = dirs::home_dir() {
        home.join(".aish").join("tmp")
    } else {
        std::env::temp_dir()
    };
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("aish-{pid}-{n}{suffix}"))
        .to_string_lossy()
        .to_string()
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

    #[test]
    fn build_system_prompt_appends_language() {
        assert_eq!(
            build_system_prompt("base.", "Japanese"),
            "base. Respond in Japanese."
        );
    }

    #[test]
    fn build_system_prompt_skips_when_empty_language() {
        assert_eq!(build_system_prompt("base.", ""), "base.");
    }

    #[test]
    fn extract_model_short_separated() {
        let args = vec!["-m".into(), "gpt-4".into()];
        assert_eq!(extract_model_from_args(&args), Some("gpt-4".into()));
    }

    #[test]
    fn extract_model_long_separated() {
        let args = vec!["--model".into(), "claude-sonnet-4-6".into()];
        assert_eq!(extract_model_from_args(&args), Some("claude-sonnet-4-6".into()));
    }

    #[test]
    fn extract_model_short_eq() {
        let args = vec!["-m=gpt-4".into()];
        assert_eq!(extract_model_from_args(&args), Some("gpt-4".into()));
    }

    #[test]
    fn extract_model_long_eq() {
        let args = vec!["--model=gemini-2.5-pro".into()];
        assert_eq!(extract_model_from_args(&args), Some("gemini-2.5-pro".into()));
    }

    #[test]
    fn extract_model_absent() {
        let args = vec!["-s".into(), "read-only".into()];
        assert_eq!(extract_model_from_args(&args), None);
    }

    #[test]
    fn extract_model_dangling_short_returns_none() {
        let args = vec!["-m".into()];
        assert_eq!(extract_model_from_args(&args), None);
    }

    #[test]
    fn override_model_inserts_when_args_have_no_model() {
        let args: Vec<String> = vec!["-s".into(), "read-only".into()];
        let out = override_model_in_args(&args, Some("haiku"));
        assert_eq!(
            out,
            vec![
                "-s".to_string(),
                "read-only".to_string(),
                "--model".to_string(),
                "haiku".to_string()
            ]
        );
    }

    #[test]
    fn override_model_replaces_existing_short_separated() {
        let args: Vec<String> = vec!["-m".into(), "old-model".into(), "-s".into(), "ro".into()];
        let out = override_model_in_args(&args, Some("new-model"));
        assert_eq!(
            out,
            vec!["-s", "ro", "--model", "new-model"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn override_model_replaces_existing_long_eq() {
        let args: Vec<String> = vec!["--model=old".into(), "--other".into()];
        let out = override_model_in_args(&args, Some("new"));
        assert_eq!(out, vec!["--other", "--model", "new"]);
    }

    #[test]
    fn override_model_none_passes_through_but_strips_nothing() {
        let args: Vec<String> = vec!["-m".into(), "keep".into(), "-x".into()];
        let out = override_model_in_args(&args, None);
        // None なら extra_args をそのまま素通し (既存の -m は残る)。
        assert_eq!(out, vec!["-m", "keep", "-x"]);
    }
}
