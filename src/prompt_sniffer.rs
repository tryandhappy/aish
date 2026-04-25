use std::collections::BTreeSet;

/// 既定のプロンプト終端文字。`:` は ssh の password prompt 等で誤検出するので除外。
const DEFAULT_TERMINATORS: &[char] = &['$', '#', '>', '%', '➜', '❯', '»'];

/// PTY 出力末尾を保持するバッファサイズ。
/// 長すぎる prompt は通常存在しないので 256 バイトで十分。
const TAIL_BUFFER_BYTES: usize = 256;

/// PTY 出力末尾を観察してプロンプト戻りを検出するスニッファ。
///
/// 「人間がプロンプトを目視で確認している」のと同じ要領で、ANSI 除去後の
/// 末尾行が `[終端文字][空白]+` で終わっていればプロンプト戻りとみなす。
/// サーバ側に何も書き込まず、純粋に passive。
///
/// 多段 SSH (`ssh hostA → ssh hostB`) や `sudo bash` で PS1 が動的に変わっても、
/// `$` `#` `>` `%` 等の終端文字は共通しているので追従できる。observe された
/// 終端文字は学習セットに加わるので、`❯` 等の特殊終端も 1 度観察すれば
/// 次から追従する。
pub struct PromptSniffer {
    terminators: BTreeSet<char>,
    /// PTY 出力末尾の ANSI 除去後バッファ
    tail_stripped: Vec<u8>,
}

impl PromptSniffer {
    pub fn new() -> Self {
        Self {
            terminators: DEFAULT_TERMINATORS.iter().copied().collect(),
            tail_stripped: Vec::new(),
        }
    }

    /// PTY 出力を流し込む。tail を更新するだけで、画面表示等は別経路で行う。
    pub fn feed(&mut self, data: &[u8]) {
        let stripped = strip_ansi_escapes::strip(data);
        self.tail_stripped.extend_from_slice(&stripped);
        if self.tail_stripped.len() > TAIL_BUFFER_BYTES {
            let drop = self.tail_stripped.len() - TAIL_BUFFER_BYTES;
            self.tail_stripped.drain(..drop);
        }
    }

    /// 末尾がプロンプトの形 `[終端文字][空白]+` で終わっているか。
    pub fn matches_prompt(&self) -> bool {
        let s = String::from_utf8_lossy(&self.tail_stripped);
        let last_line = s.rsplit('\n').next().unwrap_or(&s);
        if !last_line.ends_with(' ') {
            return false;
        }
        let before_space = last_line.trim_end_matches(' ');
        match before_space.chars().last() {
            Some(c) => self.terminators.contains(&c),
            None => false,
        }
    }

    /// 完了確定時に呼ぶ。マッチした終端文字を学習セットに追加する。
    /// すでに登録済みなら何もしない。次回以降、特殊な終端文字でも追従できるようになる。
    pub fn record_match(&mut self) {
        let s = String::from_utf8_lossy(&self.tail_stripped);
        let last_line = s.rsplit('\n').next().unwrap_or(&s);
        let before_space = last_line.trim_end_matches(' ');
        if let Some(c) = before_space.chars().last() {
            self.terminators.insert(c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(input: &[u8]) -> PromptSniffer {
        let mut s = PromptSniffer::new();
        s.feed(input);
        s
    }

    #[test]
    fn matches_dollar_prompt() {
        assert!(fed(b"user@host:~$ ").matches_prompt());
    }

    #[test]
    fn matches_hash_prompt() {
        assert!(fed(b"[root@host tmp]# ").matches_prompt());
    }

    #[test]
    fn matches_greater_prompt() {
        assert!(fed(b"PS> ").matches_prompt());
    }

    #[test]
    fn matches_percent_prompt() {
        assert!(fed(b"% ").matches_prompt());
    }

    #[test]
    fn does_not_match_oh_my_zsh_themed_by_default() {
        // robbyrussell など装飾の強い oh-my-zsh テーマは末尾が `) ` や `~ ` 等、
        // 既定の終端セットに含まれない文字で終わる。最初は検出しない。
        // 利用者は `record_match` で学習させて次回以降追従させる想定。
        assert!(!fed("➜  myrepo git:(main) ".as_bytes()).matches_prompt());
        assert!(!fed("➜  ~ ".as_bytes()).matches_prompt());
    }

    #[test]
    fn matches_starship_arrow() {
        assert!(fed("❯ ".as_bytes()).matches_prompt());
    }

    #[test]
    fn matches_with_multiple_trailing_spaces() {
        assert!(fed(b"user@host:~$   ").matches_prompt());
    }

    #[test]
    fn rejects_colon_to_avoid_ssh_password_prompt() {
        // ssh の `password: ` を完了と誤検出しない
        assert!(!fed(b"user@host's password: ").matches_prompt());
    }

    #[test]
    fn rejects_question_mark() {
        // 確認系 `(y/n)? ` 等も完了とみなさない
        assert!(!fed(b"Continue? ").matches_prompt());
    }

    #[test]
    fn rejects_terminator_without_trailing_space() {
        assert!(!fed(b"user@host:~$").matches_prompt());
    }

    #[test]
    fn rejects_only_whitespace() {
        assert!(!fed(b"   ").matches_prompt());
    }

    #[test]
    fn rejects_empty() {
        assert!(!PromptSniffer::new().matches_prompt());
    }

    #[test]
    fn handles_ansi_color_codes() {
        // \x1b[32m...\x1b[0m を含む彩色プロンプト
        let input = b"\x1b[01;32muser@host\x1b[00m:\x1b[01;34m~\x1b[00m$ ";
        assert!(fed(input).matches_prompt());
    }

    #[test]
    fn handles_multi_line_output_then_prompt() {
        let input = b"line1\nline2\nuser@host:~$ ";
        assert!(fed(input).matches_prompt());
    }

    #[test]
    fn output_then_prompt_uses_last_line_only() {
        // 途中の行が `: ` で終わっていても、最後の行が prompt なら検出
        let input = b"foo: bar\nuser@host:~$ ";
        assert!(fed(input).matches_prompt());
    }

    #[test]
    fn middle_of_output_does_not_match() {
        // 出力途中（プロンプトに戻る前）は false
        assert!(!fed(b"running command...").matches_prompt());
    }

    #[test]
    fn tail_buffer_truncates_at_capacity() {
        let mut s = PromptSniffer::new();
        let big = vec![b'x'; TAIL_BUFFER_BYTES * 3];
        s.feed(&big);
        assert!(s.tail_stripped.len() <= TAIL_BUFFER_BYTES);
        // 末尾が x の連続なので prompt にはならない
        assert!(!s.matches_prompt());
    }

    #[test]
    fn byte_at_a_time_feed_works() {
        let mut s = PromptSniffer::new();
        for b in b"user@host:~$ " {
            s.feed(&[*b]);
        }
        assert!(s.matches_prompt());
    }

    #[test]
    fn learns_new_terminator() {
        let mut s = PromptSniffer::new();
        // ❤ は既定セットに含まれない
        s.feed("user@host:~❤ ".as_bytes());
        assert!(!s.matches_prompt());
        // 偽だが、強制的に学習させてみる（実運用では検出後に呼ぶ）
        s.record_match();
        // 学習後はその終端で検出可能
        assert!(s.matches_prompt());
    }

    #[test]
    fn record_match_is_idempotent_on_known_terminator() {
        let mut s = PromptSniffer::new();
        s.feed(b"user@host:~$ ");
        let count_before = s.terminators.len();
        s.record_match();
        // $ は既定セットなので増えない
        assert_eq!(s.terminators.len(), count_before);
    }

    #[test]
    fn nested_ssh_then_sudo_scenario() {
        // 外側 bash → ssh で内側 → sudo で root
        let mut s = PromptSniffer::new();
        // 外側プロンプト
        s.feed(b"userA@hostA:~$ ");
        assert!(s.matches_prompt());
        s.record_match();
        // ssh コマンドの出力 → 内側プロンプト
        s.feed(b"\nLast login: ...\nuserB@hostB:~$ ");
        assert!(s.matches_prompt());
        s.record_match();
        // sudo の出力 → root プロンプト
        s.feed(b"\n[root@hostB ~]# ");
        assert!(s.matches_prompt());
        s.record_match();
        // exit で外側に戻る
        s.feed(b"\nuserA@hostA:~$ ");
        assert!(s.matches_prompt());
    }

    #[test]
    fn output_with_newline_at_end_no_prompt() {
        // 末尾が改行のみで終わっていてプロンプトはまだ → false
        assert!(!fed(b"command output\n").matches_prompt());
    }
}
