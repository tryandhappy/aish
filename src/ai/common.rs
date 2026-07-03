use super::types::{AiError, AiResponse};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// stdinからCtrl+C (0x03) が入力されているかノンブロッキングでチェック。
/// `std::io::stdin().read()` は lock + 内部バッファ経由なので、入力スレッド側との
/// 競合や 1 byte 取り損ねで Ctrl+C を見逃すことがあった。`libc::read` で生 fd から
/// 直接 1 byte ずつ読むことで、単発の Ctrl+C でも確実に検出する。
#[cfg(unix)]
pub(crate) fn check_stdin_cancel() -> bool {
    let fd = libc::STDIN_FILENO;
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
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 1) };
        match n {
            1 if buf[0] == 0x03 => found = true,
            1 => {} // Ctrl+C 以外の入力は破棄 (キャンセル中のキー入力をシェルへ渡さない)
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

/// Claude Code 用の system prompt。
/// 元々は base + language のみで CLI フラグ (`--disallowedTools` / `--output-format json` / `--json-schema`)
/// に安全制約と JSON 出力指示を委ねていたが、defense-in-depth のため他 backend と同じ
/// `build_system_prompt` の内容 (安全制約 + JSON フォーマット指示) を Claude にも適用する。
/// CLI フラグによる構造的制約と prompt レベルの指示の二段構え。
pub(crate) fn build_system_prompt_claude(prompt: &str, language: &str) -> String {
    build_system_prompt(prompt, language)
}

/// 既定の system prompt。JSON Schema / ツール禁止フラグを持たない backend (codex/gemini/qwen)
/// 向けに、安全制約と JSON 単独出力指示を埋め込む。
/// Claude では `build_system_prompt_claude` を使う。
pub(crate) fn build_system_prompt(base: &str, language: &str) -> String {
    let lang_part = if language.is_empty() {
        String::new()
    } else {
        format!(" Respond in {language}.")
    };
    format!(
        "{base}{lang_part}\n\n\
         重要:\n\
         - あなたはLinux/ルータ管理の専門家です。\n\
         - SSH/Terminalの内容を把握しています。\n\
         - ユーザの指示に従いコマンドを考えて提案してください。\n\
         - コマンドは出力フォーマットの「提案コマンド」で提案します。\n\
         - 提案コマンドはユーザが実行するかどうか確認します。\n\
         - **いかなるツール呼び出しも直接行わないでください**。必ず提案コマンドを使用してください。\n\
         - shell exec, file read, file write, code interpreter 等のいずれも禁止です。\n\
         - 情報収集もかならず提案コマンドを使用してください。\n\
         - あなたが直接、端末の情報を収集・閲覧・操作・書込・編集・実行等するのは禁止です。\n\
         - ターミナルの内容は既に下記 ```terminal``` ブロックに含まれています。\n\
         - コマンドは「提案のみ」行ってください。\n\n\
         応答ルール:\n\
         - 独立した複数のコマンドを ; で1つに連結せず、commands 配列の別々の要素に分割してください。\n\
         - ただし &&・|| による条件付き実行や、for/while/until/case/if 等の制御構文に含まれる ; は1つのコマンドとして維持してください。\n\
         - 1つのコマンドが複数行になる場合 (heredoc やスクリプト等) は、無理に1行へ詰めず改行をそのまま保持して1要素にしてください。\n\n\
         出力フォーマット: 必ず以下の JSON だけを 1 つ出力してください。\
         前後に説明文・コードフェンス・追加テキストを付けないでください。\n\
         {{\"message\": \"ユーザへの説明\", \"commands\": [\"提案コマンド\"]}}\n\
         追加のコマンド提案が不要な場合は commands を [] にしてください。\n\
         実行したいコマンドがあれば必ず commands 配列に入れてください。\
         message 中にコマンドの説明やコードブロックが出てきても構いませんが、\
         実行を意図したコマンドは必ず commands 配列に含めてください。"
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

/// `/model` `/effort` ピッカーに出す候補リストを解決する。
/// 優先順位: static list 非空 → static / 取得コマンド 非空 → 実行結果 / それ以外 → 組み込み既定。
/// （`config::OptionLists` + backend ごとの組み込み既定をまとめる窓口）
pub(crate) fn resolve_option_list(
    static_list: &[String],
    command: &str,
    builtin: &[&str],
    log_path: &Option<String>,
) -> Vec<String> {
    if !static_list.is_empty() {
        return static_list.to_vec();
    }
    if !command.is_empty() {
        return run_option_command(command, log_path);
    }
    builtin.iter().map(|s| s.to_string()).collect()
}

/// 候補取得コマンドをローカルシェルで実行し stdout を 1 行 1 候補で解釈する。
/// クライアント側でのみ実行され、サーバ書き込みや承認フローには関与しない。
/// 失敗時は空 Vec（候補なし扱い）。
fn run_option_command(command: &str, log_path: &Option<String>) -> Vec<String> {
    #[cfg(unix)]
    let (program, args) = ("sh", vec!["-c".to_string(), command.to_string()]);
    #[cfg(not(unix))]
    let (program, args) = ("cmd", vec!["/C".to_string(), command.to_string()]);
    match run_cli_capture_stdout(program, &args, "", log_path) {
        Ok(out) => out
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// 子プロセスを spawn し stdin に prompt を流して stdout 全体を返す。
/// Ctrl+C でキャンセル、stderr はログに記録。
pub(crate) fn run_cli_capture_stdout(
    program: &str,
    args: &[String],
    stdin_input: &str,
    log_path: &Option<String>,
) -> Result<String, AiError> {
    run_cli_capture_stdout_env(program, args, stdin_input, &BTreeMap::new(), log_path)
}

/// `run_cli_capture_stdout` の環境変数追加版。generic recipe の `env` 用
/// (例: opencode の `OPENCODE_CONFIG_CONTENT`)。透明性のため env もログに記録する。
pub(crate) fn run_cli_capture_stdout_env(
    program: &str,
    args: &[String],
    stdin_input: &str,
    envs: &BTreeMap<String, String>,
    log_path: &Option<String>,
) -> Result<String, AiError> {
    let mut child = Command::new(program)
        .args(args)
        .envs(envs)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if !envs.is_empty() {
        let joined = envs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ");
        write_log(log_path, &format!("[env] {joined}"));
    }
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

/// JSONL (1 行 1 JSON object) 形式の出力から、指定 type のオブジェクトの指定パスの文字列を抽出する。
///
/// `content_spec` / `session_spec` はいずれも `"<type>:<dot-path>"` 形式:
/// - `"assistant.message:data.content"` → `type` が `"assistant.message"` の行で、`data.content` を取得
/// - `"result:sessionId"` → `type` が `"result"` の行で、`sessionId` を取得
///
/// content は **最後にマッチした** non-empty 値を採用 (multi-turn JSONL に備える)。
/// session は最後にマッチした値を採用。
/// spec が空文字なら該当パスは探さず `None` を返す。
///
/// 行が JSON として parse できなかった / `type` が一致しない / パス先が文字列でない場合はスキップ。
///
/// 戻り値: `(content, session_id)` どちらも `Option<String>`。
pub(crate) fn parse_jsonl_with_paths(
    raw: &str,
    content_spec: &str,
    session_spec: &str,
) -> (Option<String>, Option<String>) {
    let content_split = split_type_path(content_spec);
    let session_split = split_type_path(session_spec);
    let mut content: Option<String> = None;
    let mut session_id: Option<String> = None;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let type_str = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if let Some((want_type, path)) = &content_split {
            if type_str == want_type {
                if let Some(s) = get_str_at_dot_path(&obj, path) {
                    if !s.is_empty() {
                        content = Some(s.to_string());
                    }
                }
            }
        }
        if let Some((want_type, path)) = &session_split {
            if type_str == want_type {
                if let Some(s) = get_str_at_dot_path(&obj, path) {
                    if !s.is_empty() {
                        session_id = Some(s.to_string());
                    }
                }
            }
        }
    }
    (content, session_id)
}

/// `"type:dot.path"` を `("type", "dot.path")` に分割。空文字なら None。
fn split_type_path(spec: &str) -> Option<(String, String)> {
    if spec.is_empty() {
        return None;
    }
    let (t, p) = spec.split_once(':')?;
    if t.is_empty() || p.is_empty() {
        return None;
    }
    Some((t.to_string(), p.to_string()))
}

/// `serde_json::Value` から `"a.b.c"` 形式のパスをたどって `&str` を取り出す。
/// 途中でオブジェクトでなくなった / 終端が文字列でない場合は None。
fn get_str_at_dot_path<'a>(obj: &'a serde_json::Value, path: &str) -> Option<&'a str> {
    let mut cur = obj;
    for segment in path.split('.') {
        cur = cur.get(segment)?;
    }
    cur.as_str()
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
    fn build_system_prompt_claude_matches_generic_builder() {
        // Claude 版は build_system_prompt と完全に同じ出力。defense-in-depth で
        // 同じ安全制約・JSON 指示を CLI フラグと併用する。
        assert_eq!(
            build_system_prompt_claude("base.", "Japanese"),
            build_system_prompt("base.", "Japanese")
        );
        assert_eq!(
            build_system_prompt_claude("base.", ""),
            build_system_prompt("base.", "")
        );
    }

    #[test]
    fn build_system_prompt_claude_appends_language() {
        // base + " Respond in {language}." が先頭にあること。
        let s = build_system_prompt_claude("base.", "Japanese");
        assert!(s.starts_with("base. Respond in Japanese.\n\n"), "got: {s}");
    }

    #[test]
    fn build_system_prompt_claude_skips_when_empty_language() {
        // 空 language なら "Respond in ..." 句なし。
        let s = build_system_prompt_claude("base.", "");
        assert!(s.starts_with("base.\n\n"), "got: {s}");
        assert!(!s.contains("Respond in"), "got: {s}");
    }

    #[test]
    fn extract_model_short_separated() {
        let args = vec!["-m".into(), "gpt-4".into()];
        assert_eq!(extract_model_from_args(&args), Some("gpt-4".into()));
    }

    #[test]
    fn extract_model_long_separated() {
        let args = vec!["--model".into(), "claude-sonnet-4-6".into()];
        assert_eq!(
            extract_model_from_args(&args),
            Some("claude-sonnet-4-6".into())
        );
    }

    #[test]
    fn extract_model_short_eq() {
        let args = vec!["-m=gpt-4".into()];
        assert_eq!(extract_model_from_args(&args), Some("gpt-4".into()));
    }

    #[test]
    fn extract_model_long_eq() {
        let args = vec!["--model=gemini-2.5-pro".into()];
        assert_eq!(
            extract_model_from_args(&args),
            Some("gemini-2.5-pro".into())
        );
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
    fn jsonl_extracts_content_and_session() {
        let jsonl = concat!(
            r#"{"type":"session.start","data":{}}"#,
            "\n",
            r#"{"type":"user.message","data":{"content":"hi"}}"#,
            "\n",
            r#"{"type":"assistant.message","data":{"content":"hello world"}}"#,
            "\n",
            r#"{"type":"result","sessionId":"abc-123"}"#,
            "\n",
        );
        let (content, sid) =
            parse_jsonl_with_paths(jsonl, "assistant.message:data.content", "result:sessionId");
        assert_eq!(content.as_deref(), Some("hello world"));
        assert_eq!(sid.as_deref(), Some("abc-123"));
    }

    #[test]
    fn jsonl_last_content_wins() {
        let jsonl = concat!(
            r#"{"type":"assistant.message","data":{"content":"first"}}"#,
            "\n",
            r#"{"type":"assistant.message","data":{"content":"second"}}"#,
            "\n",
        );
        let (content, _) = parse_jsonl_with_paths(jsonl, "assistant.message:data.content", "");
        assert_eq!(content.as_deref(), Some("second"));
    }

    #[test]
    fn jsonl_skips_malformed_lines() {
        let jsonl = concat!(
            "garbage\n",
            r#"{"type":"assistant.message","data":{"content":"ok"}}"#,
            "\n",
            "{broken\n",
            r#"{"type":"result","sessionId":"sid"}"#,
            "\n",
        );
        let (content, sid) =
            parse_jsonl_with_paths(jsonl, "assistant.message:data.content", "result:sessionId");
        assert_eq!(content.as_deref(), Some("ok"));
        assert_eq!(sid.as_deref(), Some("sid"));
    }

    #[test]
    fn jsonl_empty_spec_returns_none_for_that_field() {
        let jsonl = r#"{"type":"assistant.message","data":{"content":"ok"}}"#;
        let (content, sid) = parse_jsonl_with_paths(jsonl, "", "result:sessionId");
        assert!(content.is_none());
        assert!(sid.is_none());
    }

    #[test]
    fn jsonl_empty_content_not_adopted() {
        let jsonl = r#"{"type":"assistant.message","data":{"content":""}}"#;
        let (content, _) = parse_jsonl_with_paths(jsonl, "assistant.message:data.content", "");
        assert!(content.is_none());
    }

    #[test]
    fn jsonl_dot_path_traverses_nested() {
        let jsonl = r#"{"type":"x","a":{"b":{"c":"deep"}}}"#;
        let (content, _) = parse_jsonl_with_paths(jsonl, "x:a.b.c", "");
        assert_eq!(content.as_deref(), Some("deep"));
    }

    #[test]
    fn jsonl_path_missing_returns_none() {
        let jsonl = r#"{"type":"x","a":{}}"#;
        let (content, _) = parse_jsonl_with_paths(jsonl, "x:a.missing", "");
        assert!(content.is_none());
    }

    #[test]
    fn jsonl_malformed_spec_silently_ignored() {
        // ":" 無い spec / 片側空 / 全部空 はそれぞれ None 扱い (panic しない)。
        let jsonl = r#"{"type":"x","a":"v"}"#;
        assert_eq!(parse_jsonl_with_paths(jsonl, "no_colon", "").0, None);
        assert_eq!(parse_jsonl_with_paths(jsonl, ":a", "").0, None);
        assert_eq!(parse_jsonl_with_paths(jsonl, "x:", "").0, None);
        assert_eq!(parse_jsonl_with_paths(jsonl, "", "").0, None);
    }
}
