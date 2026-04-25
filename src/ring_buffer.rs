const DEFAULT_CAPACITY: usize = 1024 * 1024; // 1MB

pub struct RingBuffer {
    data: Vec<u8>,
    capacity: usize,
    write_pos: usize,
    // 累計書き込みバイト数。リング位置とは独立に単調増加する。
    total_written: u64,
    // 直近の mark_sent 時点の total_written。
    sent_written: u64,
}

impl RingBuffer {
    pub fn new() -> Self {
        Self {
            data: vec![0u8; DEFAULT_CAPACITY],
            capacity: DEFAULT_CAPACITY,
            write_pos: 0,
            total_written: 0,
            sent_written: 0,
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

    fn unsent_len(&self) -> usize {
        let unsent = self.total_written.saturating_sub(self.sent_written);
        // 上書きされた古いデータには遡れないので capacity でクランプ。
        (unsent.min(self.capacity as u64)) as usize
    }

    pub fn get_unsent(&self) -> String {
        let amount = self.unsent_len();
        if amount == 0 {
            return String::new();
        }
        let bytes = self.read_tail(amount);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    pub fn mark_sent(&mut self) {
        self.sent_written = self.total_written;
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

    #[test]
    fn test_append_and_get() {
        let mut buf = RingBuffer::new();
        buf.append(b"hello world");
        assert_eq!(buf.get_unsent(), "hello world");
    }

    #[test]
    fn test_mark_sent() {
        let mut buf = RingBuffer::new();
        buf.append(b"first");
        buf.mark_sent();
        buf.append(b" second");
        assert_eq!(buf.get_unsent(), " second");
    }

    #[test]
    fn test_strip_ansi() {
        let mut buf = RingBuffer::new();
        buf.append(b"\x1b[31mred text\x1b[0m");
        assert_eq!(buf.get_unsent(), "red text");
    }

    #[test]
    fn test_mark_sent_after_full_does_not_starve() {
        // 回帰テスト: バッファ満杯状態で mark_sent された後、
        // 後続の append が必ず未送信として見える。
        let mut buf = RingBuffer::new();
        let chunk = vec![b'x'; buf.capacity];
        buf.append(&chunk);
        buf.mark_sent();
        buf.append(b"new data");
        assert_eq!(buf.get_unsent(), "new data");
    }

    #[test]
    fn test_unsent_capped_at_capacity() {
        let mut buf = RingBuffer::new();
        let chunk = vec![b'a'; buf.capacity * 3];
        buf.append(&chunk);
        let unsent = buf.get_unsent();
        assert_eq!(unsent.len(), buf.capacity);
        assert!(unsent.bytes().all(|b| b == b'a'));
    }

    #[test]
    fn test_wraparound_returns_correct_tail() {
        let mut buf = RingBuffer::new();
        // capacity 直前まで埋めてから mark_sent する。
        let first = vec![b'A'; buf.capacity - 5];
        buf.append(&first);
        buf.mark_sent();
        // ラップアラウンドを跨ぐ書き込み。
        buf.append(b"BBBBBBBB");
        assert_eq!(buf.get_unsent(), "BBBBBBBB");
    }

    #[test]
    fn test_repeated_mark_sent_cycles() {
        // 大量データを何サイクルも流しても、その都度 unsent が見える。
        let mut buf = RingBuffer::new();
        for i in 0..5 {
            let payload = format!("chunk-{}", i);
            buf.append(payload.as_bytes());
            assert_eq!(buf.get_unsent(), payload);
            buf.mark_sent();
            assert_eq!(buf.get_unsent(), "");
        }
    }
}
