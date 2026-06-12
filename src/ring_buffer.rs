use crate::ai::BackendKind;
use std::collections::HashMap;

const DEFAULT_CAPACITY: usize = 1024 * 1024; // 1MB

pub struct RingBuffer {
    data: Vec<u8>,
    capacity: usize,
    write_pos: usize,
    // 累計書き込みバイト数。リング位置とは独立に単調増加する。
    total_written: u64,
    // backend ごとの「最後に送った位置 (total_written 値)」。
    // キー = BackendKind::ordinal()。entry が無いキー = 0 (起動以降全部を catch-up 対象)。
    // 旧 `[u64; BackendKind::COUNT]` から HashMap 化したのは Generic backend を
    // ordinal=6+ で可変個数サポートするため。
    sent_marks: HashMap<usize, u64>,
}

impl RingBuffer {
    pub fn new() -> Self {
        Self {
            data: vec![0u8; DEFAULT_CAPACITY],
            capacity: DEFAULT_CAPACITY,
            write_pos: 0,
            total_written: 0,
            sent_marks: HashMap::new(),
        }
    }

    pub fn append(&mut self, input: &[u8]) {
        let stripped = strip_ansi_escapes::strip(input);
        for &b in stripped.iter() {
            self.data[self.write_pos] = b;
            self.write_pos = (self.write_pos + 1) % self.capacity;
        }
        self.total_written = self.total_written.saturating_add(stripped.len() as u64);
    }

    /// AI 注釈など、ANSI escape を含まない前提のテキストを追記する。
    /// 内部実装は `append` と同じ (strip しても変わらない) が、呼び出し側の意図を明確にする。
    pub fn append_text(&mut self, text: &str) {
        self.append(text.as_bytes());
    }

    fn unsent_len_for(&self, kind: BackendKind) -> usize {
        let sent = self.sent_marks.get(&kind.ordinal()).copied().unwrap_or(0);
        let unsent = self.total_written.saturating_sub(sent);
        // 上書きされた古いデータには遡れないので capacity でクランプ。
        (unsent.min(self.capacity as u64)) as usize
    }

    pub fn get_unsent_for(&self, kind: BackendKind) -> String {
        let amount = self.unsent_len_for(kind);
        if amount == 0 {
            return String::new();
        }
        let bytes = self.read_tail(amount);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    pub fn mark_sent_for(&mut self, kind: BackendKind) {
        self.sent_marks.insert(kind.ordinal(), self.total_written);
    }

    /// AI 1 ターン分の注釈 (送信 prompt / 応答 message / 提案 commands) を append し、
    /// 直後に `mark_sent_for(kind)` するところまでを 1 メソッドで行う不可分操作。
    ///
    /// 「append してから mark」の順序を呼び出し側に委ねると、逆順 (mark → append) に
    /// 書かれたとき current AI が自分の発話を次ターンの catch-up で再受信してループする。
    /// この順序不変条件はここに閉じ、main loop にばらの append + mark を再導入しないこと。
    /// 注釈ラベル (`[aish→…]` / `[ai/…]` / `[ai/… suggests]`) のフォーマットもここが唯一の定義。
    pub fn record_ai_exchange(
        &mut self,
        kind: BackendKind,
        sent_prompt: &str,
        message: &str,
        commands: &[String],
    ) {
        let kind_label = kind.as_str();
        self.append_text(&format!("\n[aish→{kind_label}]> {sent_prompt}\n"));
        self.append_text(&format!("[ai/{kind_label}]> {message}\n"));
        if !commands.is_empty() {
            self.append_text(&format!(
                "[ai/{kind_label} suggests] {}\n",
                commands.join(" ; ")
            ));
        }
        self.mark_sent_for(kind);
    }

    /// 全 backend の cursor を末尾に進める。`/clear` 用。
    /// native (`all_native()`) と `all_generics()` 両方を更新する。
    /// まだ HashMap に entry が無い backend にも entry を作って総書き込み量と同期させる
    /// (= 次回 send 時に「過去ログ無し」状態でスタート)。
    pub fn mark_sent_all(&mut self) {
        for k in BackendKind::all_native() {
            self.sent_marks.insert(k.ordinal(), self.total_written);
        }
        for k in BackendKind::all_generics() {
            self.sent_marks.insert(k.ordinal(), self.total_written);
        }
    }

    /// リング末尾から `amount` バイトを線形バッファとして取り出す。
    fn read_tail(&self, amount: usize) -> Vec<u8> {
        let start = (self.write_pos + self.capacity - amount) % self.capacity;
        let mut out = Vec::with_capacity(amount);
        if start + amount <= self.capacity {
            out.extend_from_slice(&self.data[start..start + amount]);
        } else {
            let first = self.capacity - start;
            out.extend_from_slice(&self.data[start..]);
            out.extend_from_slice(&self.data[..amount - first]);
        }
        out
    }

    #[allow(dead_code)]
    pub fn get_all(&self) -> String {
        let amount = (self.total_written.min(self.capacity as u64)) as usize;
        if amount == 0 {
            return String::new();
        }
        let bytes = self.read_tail(amount);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANY: BackendKind = BackendKind::Claude;

    #[test]
    fn test_append_and_get() {
        let mut buf = RingBuffer::new();
        buf.append(b"hello world");
        assert_eq!(buf.get_unsent_for(ANY), "hello world");
    }

    #[test]
    fn test_mark_sent() {
        let mut buf = RingBuffer::new();
        buf.append(b"first");
        buf.mark_sent_for(ANY);
        buf.append(b" second");
        assert_eq!(buf.get_unsent_for(ANY), " second");
    }

    #[test]
    fn test_strip_ansi() {
        let mut buf = RingBuffer::new();
        buf.append(b"\x1b[31mred text\x1b[0m");
        assert_eq!(buf.get_unsent_for(ANY), "red text");
    }

    #[test]
    fn test_mark_sent_after_full_does_not_starve() {
        let mut buf = RingBuffer::new();
        let chunk = vec![b'x'; buf.capacity];
        buf.append(&chunk);
        buf.mark_sent_for(ANY);
        buf.append(b"new data");
        assert_eq!(buf.get_unsent_for(ANY), "new data");
    }

    #[test]
    fn test_unsent_capped_at_capacity() {
        let mut buf = RingBuffer::new();
        let chunk = vec![b'a'; buf.capacity * 3];
        buf.append(&chunk);
        let unsent = buf.get_unsent_for(ANY);
        assert_eq!(unsent.len(), buf.capacity);
        assert!(unsent.bytes().all(|b| b == b'a'));
    }

    #[test]
    fn test_wraparound_returns_correct_tail() {
        let mut buf = RingBuffer::new();
        let first = vec![b'A'; buf.capacity - 5];
        buf.append(&first);
        buf.mark_sent_for(ANY);
        buf.append(b"BBBBBBBB");
        assert_eq!(buf.get_unsent_for(ANY), "BBBBBBBB");
    }

    #[test]
    fn test_repeated_mark_sent_cycles() {
        let mut buf = RingBuffer::new();
        for i in 0..5 {
            let payload = format!("chunk-{i}");
            buf.append(payload.as_bytes());
            assert_eq!(buf.get_unsent_for(ANY), payload);
            buf.mark_sent_for(ANY);
            assert_eq!(buf.get_unsent_for(ANY), "");
        }
    }

    #[test]
    fn test_per_backend_independence() {
        // A.mark_sent_for は B に影響しない。
        let mut buf = RingBuffer::new();
        buf.append(b"shared-1");
        buf.mark_sent_for(BackendKind::Claude);
        // Claude は送信済み、他はまだ全部見える。
        assert_eq!(buf.get_unsent_for(BackendKind::Claude), "");
        assert_eq!(buf.get_unsent_for(BackendKind::Codex), "shared-1");
        assert_eq!(buf.get_unsent_for(BackendKind::Gemini), "shared-1");
        assert_eq!(buf.get_unsent_for(BackendKind::Qwen), "shared-1");

        buf.append(b"; shared-2");
        // Claude は差分のみ、他は全部。
        assert_eq!(buf.get_unsent_for(BackendKind::Claude), "; shared-2");
        assert_eq!(buf.get_unsent_for(BackendKind::Codex), "shared-1; shared-2");
    }

    #[test]
    fn test_switch_to_unused_backend_gets_full_catchup() {
        // 初期値 0 なので、まだ使われていない backend は append された全部を受け取る。
        let mut buf = RingBuffer::new();
        buf.append(b"hello");
        buf.mark_sent_for(BackendKind::Claude);
        buf.append(b" world");
        // Codex は一度も mark していないので、起動以降の全部が catch-up に乗る。
        assert_eq!(buf.get_unsent_for(BackendKind::Codex), "hello world");
    }

    #[test]
    fn test_mark_sent_all_resets_every_cursor() {
        let mut buf = RingBuffer::new();
        buf.append(b"history");
        // どの native backend もまだ何も見ていない状態。
        for k in BackendKind::all_native() {
            assert_eq!(buf.get_unsent_for(k), "history");
        }
        buf.mark_sent_all();
        for k in BackendKind::all_native() {
            assert_eq!(buf.get_unsent_for(k), "");
        }
        buf.append(b"after-clear");
        for k in BackendKind::all_native() {
            assert_eq!(buf.get_unsent_for(k), "after-clear");
        }
    }

    #[test]
    fn test_generic_backend_independent_cursors() {
        // Generic(0) と Generic(1) が独立して catch-up 履歴を持つことを検証。
        // init_generics は呼ばない (registry を介さず ordinal だけで動作)。
        let mut buf = RingBuffer::new();
        buf.append(b"alpha");
        buf.mark_sent_for(BackendKind::Generic(0));
        // Generic(0) は送信済み、Generic(1) はまだ全部見える。
        assert_eq!(buf.get_unsent_for(BackendKind::Generic(0)), "");
        assert_eq!(buf.get_unsent_for(BackendKind::Generic(1)), "alpha");

        buf.append(b"-beta");
        assert_eq!(buf.get_unsent_for(BackendKind::Generic(0)), "-beta");
        assert_eq!(buf.get_unsent_for(BackendKind::Generic(1)), "alpha-beta");
    }

    #[test]
    fn record_ai_exchange_marks_current_but_not_others() {
        let mut buf = RingBuffer::new();
        buf.record_ai_exchange(
            BackendKind::Claude,
            "disk full?",
            "df を見ます",
            &["df -h".to_string()],
        );
        // current は自分の発話を catch-up で再受信しない。
        assert_eq!(buf.get_unsent_for(BackendKind::Claude), "");
        // 他 backend には 3 注釈が全部見える。
        let other = buf.get_unsent_for(BackendKind::Codex);
        assert!(other.contains("[aish→claude]> disk full?"));
        assert!(other.contains("[ai/claude]> df を見ます"));
        assert!(other.contains("[ai/claude suggests] df -h"));
    }

    #[test]
    fn record_ai_exchange_format_is_stable() {
        // 注釈フォーマットの golden 固定。変更すると他 backend の catch-up 文脈や
        // SPEC.md の注釈仕様とズレるので、意図的な変更時のみテストごと更新する。
        let mut buf = RingBuffer::new();
        buf.record_ai_exchange(
            BackendKind::Claude,
            "p",
            "m",
            &["a".to_string(), "b".to_string()],
        );
        assert_eq!(
            buf.get_unsent_for(BackendKind::Codex),
            "\n[aish→claude]> p\n[ai/claude]> m\n[ai/claude suggests] a ; b\n"
        );
    }

    #[test]
    fn record_ai_exchange_omits_suggests_when_no_commands() {
        let mut buf = RingBuffer::new();
        buf.record_ai_exchange(BackendKind::Claude, "p", "m", &[]);
        let other = buf.get_unsent_for(BackendKind::Codex);
        assert!(!other.contains("suggests"));
        assert_eq!(other, "\n[aish→claude]> p\n[ai/claude]> m\n");
    }

    #[test]
    fn test_native_and_generic_share_no_state() {
        // native の Claude と Generic(0) は独立。
        let mut buf = RingBuffer::new();
        buf.append(b"shared");
        buf.mark_sent_for(BackendKind::Claude);
        assert_eq!(buf.get_unsent_for(BackendKind::Claude), "");
        assert_eq!(buf.get_unsent_for(BackendKind::Generic(0)), "shared");
    }
}
