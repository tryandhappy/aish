use std::time::{SystemTime, UNIX_EPOCH};

/// AI 提案コマンドを「ラッパ + 終了マーカー」で包んで PTY に送る形式に変換する。
///
/// 形式:
/// ```sh
/// { <cmd>; }; printf '\n__AISH_DONE_<id>_%03d__\n' "$?"
/// ```
///
/// マーカー方式が壊れる形（ヒアドキュメント、末尾 `&`、未閉じクォート、
/// 行末バックスラッシュ）は検出して `None` を返す。呼び出し側は従来の
/// 500ms 無音ヒューリスティックにフォールバックする。
///
/// 戻り値は `(PTY に送る文字列, マーカー id)` のペア。
pub fn wrap_command(cmd: &str) -> Option<(String, String)> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return None;
    }
    let cmd_clean = cmd.trim_end_matches(';').trim_end();
    if cmd_clean.is_empty() {
        return None;
    }
    if !is_safe_for_marker(cmd_clean) {
        return None;
    }
    let id = make_marker_id();
    let wrapped = format!(
        "{{ {cmd_clean}; }}; printf '\\n__AISH_DONE_{id}_%03d__\\n' \"$?\"\n"
    );
    Some((wrapped, id))
}

fn make_marker_id() -> String {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{pid:08x}{nanos:016x}")
}

fn is_safe_for_marker(cmd: &str) -> bool {
    if cmd.is_empty() {
        return false;
    }
    // 行末バックスラッシュ（行継続）
    if cmd.ends_with('\\') {
        return false;
    }
    // ヒアドキュメント（簡易判定: `<<` を含む）
    if cmd.contains("<<") {
        return false;
    }
    // 末尾 `&`（バックグラウンド）。`&&` は OK
    let trimmed = cmd.trim_end_matches(';').trim_end();
    if trimmed.ends_with('&') && !trimmed.ends_with("&&") {
        return false;
    }
    // クォートのバランス
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' if !in_single => {
                chars.next(); // 次の1文字をエスケープとしてスキップ
            }
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            _ => {}
        }
    }
    !in_single && !in_double
}

/// PTY 出力ストリームから終了マーカーを検出するスキャナ。
///
/// `feed(data)` で PTY 出力を流し込むと、画面に表示すべきバイト列を返す
/// （マーカー行は除去されている）。マーカーが見つかると `marker_found()`
/// が true になり、`exit_code()` で終了コードが取れる。
///
/// マーカーがチャンク境界を跨いで分割されても検出できるよう、内部に
/// 短い pending バッファを持つ（最大 ~50 バイト）。
pub struct MarkerScanner {
    /// 探索パターン: `\n__AISH_DONE_<id>_`
    pattern: Vec<u8>,
    /// マーカー検出までの未確定バイト
    pending: Vec<u8>,
    /// 検出後の exit code
    found_rc: Option<u32>,
}

impl MarkerScanner {
    pub fn new(id: &str) -> Self {
        let pattern = format!("\n__AISH_DONE_{id}_").into_bytes();
        Self {
            pattern,
            pending: Vec::new(),
            found_rc: None,
        }
    }

    pub fn marker_found(&self) -> bool {
        self.found_rc.is_some()
    }

    pub fn exit_code(&self) -> Option<u32> {
        self.found_rc
    }

    /// 入力データを処理し、画面・リングバッファに渡すべきバイト列を返す。
    /// マーカー前後の改行は `\n` / `\r\n` のどちらにも対応する（PTY の OPOST
    /// が `\n` を `\r\n` に変換するため、実環境では `\r\n` で届く）。
    pub fn feed(&mut self, data: &[u8]) -> Vec<u8> {
        if self.found_rc.is_some() {
            return data.to_vec();
        }
        self.pending.extend_from_slice(data);

        if let Some(idx) = find_subseq(&self.pending, &self.pattern) {
            let suffix_start = idx + self.pattern.len();
            // パターン後に必要な最小バイト: 3 桁 rc + "__" + "\n" (= 6)。
            // "__\r\n" の場合は 7 バイト。
            if self.pending.len() < suffix_start + 6 {
                return self.flush_safe();
            }
            let rc_bytes = &self.pending[suffix_start..suffix_start + 3];
            let rc_str = match std::str::from_utf8(rc_bytes) {
                Ok(s) => s,
                Err(_) => return self.flush_safe(),
            };
            let rc: u32 = match rc_str.parse() {
                Ok(n) => n,
                Err(_) => return self.flush_safe(),
            };
            if &self.pending[suffix_start + 3..suffix_start + 5] != b"__" {
                return self.flush_safe();
            }
            // "__" の直後は "\n" または "\r\n"
            let nl_pos = suffix_start + 5;
            let after_start = match self.pending.get(nl_pos) {
                Some(&b'\n') => nl_pos + 1,
                Some(&b'\r') => {
                    if self.pending.len() < nl_pos + 2 {
                        // \r まで来たがまだ \n を見ていない、次の feed まで待つ
                        return self.flush_safe();
                    }
                    if self.pending[nl_pos + 1] != b'\n' {
                        return self.flush_safe();
                    }
                    nl_pos + 2
                }
                _ => return self.flush_safe(),
            };
            // パターン直前の `\r` も除去（OPOST の `\n` → `\r\n` 対策）
            let before_end = if idx > 0 && self.pending[idx - 1] == b'\r' {
                idx - 1
            } else {
                idx
            };
            // マーカー確定
            self.found_rc = Some(rc);
            let before = self.pending[..before_end].to_vec();
            let after = self.pending[after_start..].to_vec();
            self.pending.clear();
            let mut out = before;
            out.extend_from_slice(&after);
            return out;
        }

        self.flush_safe()
    }

    /// マーカーが分割されている可能性を考慮して、末尾の一定バイトは
    /// pending に残したまま、それ以前を出力する。
    fn flush_safe(&mut self) -> Vec<u8> {
        // pattern + (rc + "__\r\n") + 直前の \r = pattern.len() + 8
        let keep = (self.pattern.len() + 8).min(self.pending.len());
        let split_at = self.pending.len() - keep;
        let out = self.pending[..split_at].to_vec();
        self.pending.drain(..split_at);
        out
    }

    /// 残バッファを全て吐き出す（中断・タイムアウト時用）。
    #[allow(dead_code)]
    pub fn flush_remaining(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

fn find_subseq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// PTY が我々の送信したラッパコマンドをそのまま echo して返してくる
/// 1 行ぶんを画面表示から取り除くためのスキッパ。
///
/// 「指定数の改行を見るまで全バイトを捨てる」というシンプルな実装。
/// シェルの syntax highlighting や色コードが echo に挟まっても影響しない。
///
/// 上限 (`max_bytes`) を設けてあり、想定外に長い echo を受け取った場合は
/// スキップを諦めて passthrough する（`stty -echo` 等で echo 無効時の保険）。
pub struct EchoSkipper {
    newlines_remaining: usize,
    bytes_skipped: usize,
    max_bytes: usize,
    skipping: bool,
}

const DEFAULT_MAX_ECHO_BYTES: usize = 4096;

impl EchoSkipper {
    pub fn new(newlines_to_skip: usize) -> Self {
        Self::with_max_bytes(newlines_to_skip, DEFAULT_MAX_ECHO_BYTES)
    }

    pub fn with_max_bytes(newlines_to_skip: usize, max_bytes: usize) -> Self {
        Self {
            newlines_remaining: newlines_to_skip,
            bytes_skipped: 0,
            max_bytes,
            skipping: newlines_to_skip > 0,
        }
    }

    pub fn feed(&mut self, data: &[u8]) -> Vec<u8> {
        if !self.skipping {
            return data.to_vec();
        }
        let mut out = Vec::new();
        for (i, &b) in data.iter().enumerate() {
            if self.skipping {
                if b == b'\n' {
                    self.newlines_remaining -= 1;
                    if self.newlines_remaining == 0 {
                        self.skipping = false;
                    }
                }
                self.bytes_skipped += 1;
                if self.bytes_skipped >= self.max_bytes {
                    // 想定より長い echo: 諦めて passthrough
                    self.skipping = false;
                }
            } else {
                out.extend_from_slice(&data[i..]);
                break;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_simple_command() {
        let (w, id) = wrap_command("ls -la").unwrap();
        assert!(w.starts_with("{ ls -la; }; printf '\\n__AISH_DONE_"));
        assert!(w.contains(&format!("__AISH_DONE_{}_", id)));
        assert!(w.ends_with("\"$?\"\n"));
        assert_eq!(id.len(), 24);
    }

    #[test]
    fn wrap_strips_trailing_semicolons() {
        let (w, _id) = wrap_command("ls;").unwrap();
        assert!(w.starts_with("{ ls; }"));
    }

    #[test]
    fn wrap_rejects_unbalanced_double_quote() {
        assert!(wrap_command(r#"echo "hello"#).is_none());
    }

    #[test]
    fn wrap_rejects_unbalanced_single_quote() {
        assert!(wrap_command("echo 'hello").is_none());
    }

    #[test]
    fn wrap_accepts_balanced_quotes() {
        assert!(wrap_command(r#"echo "hello""#).is_some());
        assert!(wrap_command("echo 'hello'").is_some());
        assert!(wrap_command(r#"echo "it's ok""#).is_some());
    }

    #[test]
    fn wrap_rejects_trailing_backslash() {
        assert!(wrap_command("echo hello \\").is_none());
    }

    #[test]
    fn wrap_rejects_heredoc() {
        assert!(wrap_command("cat <<EOF").is_none());
        assert!(wrap_command("cat <<-EOF").is_none());
    }

    #[test]
    fn wrap_rejects_trailing_background() {
        assert!(wrap_command("sleep 30 &").is_none());
        assert!(wrap_command("ls; sleep 30 &").is_none());
    }

    #[test]
    fn wrap_accepts_logical_and() {
        assert!(wrap_command("apt update && apt upgrade").is_some());
    }

    #[test]
    fn wrap_rejects_empty() {
        assert!(wrap_command("").is_none());
        assert!(wrap_command("   ").is_none());
        assert!(wrap_command(";").is_none());
    }

    fn collect_all(scanner: &mut MarkerScanner, chunks: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for c in chunks {
            out.extend_from_slice(&scanner.feed(c));
        }
        out.extend_from_slice(&scanner.flush_remaining());
        out
    }

    #[test]
    fn scanner_passthrough_when_no_marker() {
        let mut s = MarkerScanner::new("abc");
        let out = collect_all(&mut s, &[b"hello world"]);
        assert_eq!(out, b"hello world");
        assert!(!s.marker_found());
    }

    #[test]
    fn scanner_detects_complete_marker() {
        let mut s = MarkerScanner::new("abc");
        let out = s.feed(b"output\n\n__AISH_DONE_abc_000__\nprompt$ ");
        assert!(s.marker_found());
        assert_eq!(s.exit_code(), Some(0));
        assert_eq!(out, b"output\nprompt$ ");
    }

    #[test]
    fn scanner_detects_split_marker() {
        let mut s = MarkerScanner::new("abc");
        let mut out = Vec::new();
        out.extend_from_slice(&s.feed(b"output\n\n__AISH_DON"));
        out.extend_from_slice(&s.feed(b"E_abc_042__\nprompt$ "));
        assert!(s.marker_found());
        assert_eq!(s.exit_code(), Some(42));
        assert_eq!(out, b"output\nprompt$ ");
    }

    #[test]
    fn scanner_handles_marker_immediately_after_echo() {
        // コマンドが出力ゼロ。echo の直後にマーカー
        let mut s = MarkerScanner::new("abc");
        let out = s.feed(b"sleep 30; }; printf ...\nsleep_output\n\n__AISH_DONE_abc_000__\nprompt$ ");
        assert!(s.marker_found());
        assert_eq!(s.exit_code(), Some(0));
        assert_eq!(out, b"sleep 30; }; printf ...\nsleep_output\nprompt$ ");
    }

    #[test]
    fn scanner_passthrough_after_marker_found() {
        let mut s = MarkerScanner::new("abc");
        let _ = s.feed(b"foo\n\n__AISH_DONE_abc_000__\nbar");
        let out = s.feed(b"baz");
        assert_eq!(out, b"baz");
    }

    #[test]
    fn scanner_handles_high_exit_code() {
        let mut s = MarkerScanner::new("abc");
        let _ = s.feed(b"out\n\n__AISH_DONE_abc_130__\n");
        assert_eq!(s.exit_code(), Some(130));
    }

    #[test]
    fn scanner_ignores_marker_with_wrong_id() {
        let mut s = MarkerScanner::new("abc");
        let out = collect_all(&mut s, &[b"out\n\n__AISH_DONE_xyz_000__\n"]);
        assert!(!s.marker_found());
        assert_eq!(out, b"out\n\n__AISH_DONE_xyz_000__\n");
    }

    #[test]
    fn scanner_handles_byte_at_a_time_feed() {
        let input = b"out\n\n__AISH_DONE_abc_007__\ntail";
        let mut s = MarkerScanner::new("abc");
        let mut out = Vec::new();
        for &b in input {
            out.extend_from_slice(&s.feed(&[b]));
        }
        out.extend_from_slice(&s.flush_remaining());
        assert!(s.marker_found());
        assert_eq!(s.exit_code(), Some(7));
        assert_eq!(out, b"out\ntail");
    }

    #[test]
    fn scanner_detects_marker_with_crlf_line_endings() {
        // PTY の OPOST が \n を \r\n に翻訳した実環境を模す
        let mut s = MarkerScanner::new("abc");
        let out = s.feed(b"output\r\n\r\n__AISH_DONE_abc_000__\r\nprompt$ ");
        assert!(s.marker_found());
        assert_eq!(s.exit_code(), Some(0));
        // マーカー直前の \r も除去されるので before-bytes は "output\r\n"
        assert_eq!(out, b"output\r\nprompt$ ");
    }

    #[test]
    fn scanner_detects_marker_with_crlf_split_at_rc() {
        // マーカーが rc 桁の途中で分割される
        let mut s = MarkerScanner::new("abc");
        let mut out = Vec::new();
        out.extend_from_slice(&s.feed(b"out\r\n\r\n__AISH_DONE_abc_04"));
        out.extend_from_slice(&s.feed(b"2__\r\nprompt$ "));
        assert!(s.marker_found());
        assert_eq!(s.exit_code(), Some(42));
        assert_eq!(out, b"out\r\nprompt$ ");
    }

    #[test]
    fn scanner_detects_marker_with_crlf_split_at_cr() {
        // \r まで届いて \n がまだ来ていないケース
        let mut s = MarkerScanner::new("abc");
        let mut out = Vec::new();
        out.extend_from_slice(&s.feed(b"x\r\n\r\n__AISH_DONE_abc_000__\r"));
        assert!(!s.marker_found());
        out.extend_from_slice(&s.feed(b"\nprompt$ "));
        assert!(s.marker_found());
        assert_eq!(s.exit_code(), Some(0));
        assert_eq!(out, b"x\r\nprompt$ ");
    }

    #[test]
    fn scanner_handles_crlf_byte_at_a_time() {
        let input = b"x\r\n\r\n__AISH_DONE_abc_007__\r\ntail";
        let mut s = MarkerScanner::new("abc");
        let mut out = Vec::new();
        for &b in input {
            out.extend_from_slice(&s.feed(&[b]));
        }
        out.extend_from_slice(&s.flush_remaining());
        assert!(s.marker_found());
        assert_eq!(s.exit_code(), Some(7));
        assert_eq!(out, b"x\r\ntail");
    }

    #[test]
    fn echo_skipper_skips_first_line_with_lf() {
        let mut s = EchoSkipper::new(1);
        let out = s.feed(b"echo_line\ncmd_output\n");
        assert_eq!(out, b"cmd_output\n");
    }

    #[test]
    fn echo_skipper_skips_first_line_with_crlf() {
        let mut s = EchoSkipper::new(1);
        let out = s.feed(b"echo\r\noutput\r\n");
        assert_eq!(out, b"output\r\n");
    }

    #[test]
    fn echo_skipper_split_across_feeds() {
        let mut s = EchoSkipper::new(1);
        let mut out = Vec::new();
        out.extend_from_slice(&s.feed(b"partial_echo"));
        out.extend_from_slice(&s.feed(b"_more\nrest"));
        assert_eq!(out, b"rest");
    }

    #[test]
    fn echo_skipper_zero_count_is_passthrough() {
        let mut s = EchoSkipper::new(0);
        let out = s.feed(b"all_passthrough\n");
        assert_eq!(out, b"all_passthrough\n");
    }

    #[test]
    fn echo_skipper_passthrough_after_finishing() {
        let mut s = EchoSkipper::new(1);
        let _ = s.feed(b"line1\n");
        let out = s.feed(b"more_data");
        assert_eq!(out, b"more_data");
    }

    #[test]
    fn echo_skipper_byte_at_a_time() {
        let mut s = EchoSkipper::new(1);
        let mut out = Vec::new();
        for &b in b"abc\r\ndef" {
            out.extend_from_slice(&s.feed(&[b]));
        }
        assert_eq!(out, b"def");
    }

    #[test]
    fn echo_skipper_gives_up_at_max_bytes() {
        // echo 無効環境（stty -echo 等）の保険: max_bytes を超えたら諦めて passthrough
        let mut s = EchoSkipper::with_max_bytes(1, 5);
        let out = s.feed(b"123456789");
        // 1〜5 はスキップ、6〜9 は passthrough
        assert_eq!(out, b"6789");
    }
}
