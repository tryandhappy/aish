use std::io::Read;

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
}
