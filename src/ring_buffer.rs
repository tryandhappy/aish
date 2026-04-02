const DEFAULT_CAPACITY: usize = 1024 * 1024; // 1MB

pub struct RingBuffer {
    data: Vec<u8>,
    capacity: usize,
    write_pos: usize,
    len: usize,
    sent_pos: usize,
}

impl RingBuffer {
    pub fn new() -> Self {
        Self {
            data: vec![0u8; DEFAULT_CAPACITY],
            capacity: DEFAULT_CAPACITY,
            write_pos: 0,
            len: 0,
            sent_pos: 0,
        }
    }

    pub fn append(&mut self, input: &[u8]) {
        let stripped = strip_ansi_escapes::strip(input);
        let bytes = &stripped;

        if bytes.len() >= self.capacity {
            let start = bytes.len() - self.capacity;
            self.data[..self.capacity].copy_from_slice(&bytes[start..]);
            self.write_pos = 0;
            self.len = self.capacity;
            self.sent_pos = 0;
            return;
        }

        for &b in bytes.iter() {
            self.data[self.write_pos] = b;
            self.write_pos = (self.write_pos + 1) % self.capacity;
        }

        self.len = (self.len + bytes.len()).min(self.capacity);

        if self.len == self.capacity {
            let unsent_len = self.unsent_len();
            if unsent_len > self.capacity {
                self.sent_pos = 0;
            }
        }
    }

    fn unsent_len(&self) -> usize {
        if self.len == 0 {
            return 0;
        }
        let total_written = self.len;
        let sent = self.sent_pos;
        if sent > total_written {
            total_written
        } else {
            total_written - sent
        }
    }

    pub fn get_unsent(&self) -> String {
        let unsent = self.unsent_len();
        if unsent == 0 {
            return String::new();
        }

        let start = if self.len < self.capacity {
            (self.write_pos + self.capacity - unsent) % self.capacity
        } else {
            (self.write_pos + self.capacity - unsent) % self.capacity
        };

        let mut result = Vec::with_capacity(unsent);
        for i in 0..unsent {
            result.push(self.data[(start + i) % self.capacity]);
        }

        String::from_utf8_lossy(&result).to_string()
    }

    pub fn mark_sent(&mut self) {
        self.sent_pos = self.len;
    }

    pub fn get_all(&self) -> String {
        if self.len == 0 {
            return String::new();
        }

        let start = if self.len < self.capacity {
            0
        } else {
            self.write_pos
        };

        let mut result = Vec::with_capacity(self.len);
        for i in 0..self.len {
            result.push(self.data[(start + i) % self.capacity]);
        }

        String::from_utf8_lossy(&result).to_string()
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
}
