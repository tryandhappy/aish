use crate::ai::BackendKind;

const DEFAULT_CAPACITY: usize = 1024 * 1024; // 1MB

pub struct RingBuffer {
    data: Vec<u8>,
    capacity: usize,
    write_pos: usize,
    // 累計書き込みバイト数。リング位置とは独立に単調増加する。
    total_written: u64,
    // backend ごとの「最後に送った位置 (total_written 値)」。
    // index = BackendKind::ordinal()。初期値 0 = aish 起動時から全部を catch-up 対象に。
    sent_marks: [u64; BackendKind::COUNT],
}

impl RingBuffer {
    pub fn new() -> Self {
        Self {
            data: vec![0u8; DEFAULT_CAPACITY],
            capacity: DEFAULT_CAPACITY,
            write_pos: 0,
            total_written: 0,
            sent_marks: [0u64; BackendKind::COUNT],
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
        let unsent = self
            .total_written
            .saturating_sub(self.sent_marks[kind.ordinal()]);
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
        self.sent_marks[kind.ordinal()] = self.total_written;
    }

    /// 全 backend の cursor を末尾に進める。`/clear` 用。
    pub fn mark_sent_all(&mut self) {
        for slot in self.sent_marks.iter_mut() {
            *slot = self.total_written;
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
            let payload = format!("chunk-{}", i);
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
        assert_eq!(
            buf.get_unsent_for(BackendKind::Codex),
            "shared-1; shared-2"
        );
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
        // どの backend もまだ何も見ていない状態。
        for k in BackendKind::all() {
            assert_eq!(buf.get_unsent_for(k), "history");
        }
        buf.mark_sent_all();
        for k in BackendKind::all() {
            assert_eq!(buf.get_unsent_for(k), "");
        }
        buf.append(b"after-clear");
        for k in BackendKind::all() {
            assert_eq!(buf.get_unsent_for(k), "after-clear");
        }
    }
}
