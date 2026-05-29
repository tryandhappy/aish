//! 低レベル端末入力のデコード層。
//!
//! aish の信頼の根幹 (通常動作中は PTY に独自バイトを挿入しない / 承認したコマンド
//! だけを実行する) を守るため、stdin から読んだ **元バイト列 (`raw`) を主役**として保持し、
//! キー分類 (`Tok`) は副次情報として添える。passthrough はこの `raw` をそのまま PTY に
//! 転送する (`Char` を再エンコードしない: invalid UTF-8 / Alt+非ASCII / paste / マウス
//! シーケンスで壊れるため)。
//!
//! このモジュールは fd 0 の framing (ESC/CSI/SS3/UTF-8/poll+timeout) を **唯一の場所** に
//! 集約する。confirm / passthrough / minibuffer の 3 つの入力ループはすべて
//! [`next_event`] を消費する薄い層になる。バイト列 → `Tok` の golden test (本ファイル末尾)
//! が「Enter が制御文字フィルタに飲まれる」等の回帰を構造的に防ぐ。
//!
//! **どの read もブロッキングにしない** (最初の 1 byte を除く)。partial sequence
//! (mouse tracking 中のフォーカス切替で断片送信される等) が来ても stdin read が固まらない。

/// ESC シーケンス継続バイトの poll timeout (ms)。
const POLL_TIMEOUT_MS: i32 = 50;
/// CSI シーケンスの最大長 (壊れた入力で raw が無限に伸びるのを防ぐ fail-safe)。
const MAX_SEQ_LEN: usize = 64;

/// 1 イベント分のデコード結果。
/// `raw` は消費した元バイト列そのまま (passthrough はこれを無加工で PTY に送る)。
/// `tok` はキー分類 (confirm / minibuffer が利用)。
#[derive(Debug, Clone, PartialEq)]
pub struct InEvent {
    pub raw: Vec<u8>,
    pub tok: Tok,
}

/// キー分類。`raw` を主役とし、これは副次情報。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tok {
    /// 表示可能な 1 文字 (ASCII / UTF-8 マルチバイト確定文字)。raw に元バイト列。
    Char(char),
    /// Enter (LF `0x0a` / CR `0x0d`)。
    Enter,
    /// Alt+Enter (ESC + CR/LF)。minibuffer では改行挿入。
    AltEnter,
    /// 修飾付き Enter の CSI u 形式 (`ESC [ 13 ; N u`)。minibuffer では改行挿入。
    ModEnter,
    /// Backspace (`0x7f` / `0x08`)。
    Backspace,
    /// その他の制御文字 (`0x00`..=`0x1f` のうち上記以外)。値は元バイト。
    /// 例: Ctrl+C=`0x03`, Ctrl+D=`0x04`, Ctrl+/=`0x1f`, Ctrl+A=`0x01`, Tab=`0x09` 等。
    Ctrl(u8),
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Delete,
    /// 単独 ESC (後続バイトが poll timeout 内に来なかった)。
    Esc,
    /// 解釈しない CSI/SS3/ESC シーケンス (完全 / 不完全)。raw に全バイト。
    EscSeq,
    /// フォーカス in/out (`ESC [ I` / `ESC [ O`)。
    FocusIn,
    FocusOut,
    /// bracketed paste 開始 / 終了 (`ESC [ 200 ~` / `ESC [ 201 ~`)。
    /// minibuffer はこの間の改行を送信ではなくバッファ挿入 (複数行入力) として扱う。
    PasteStart,
    PasteEnd,
    /// デコードできなかった生バイト (invalid UTF-8 lead 等)。raw に元バイト。
    Bytes,
    /// 入力終端 (EOF / fd エラー)。
    Eof,
}

/// 1 バイトずつ供給するバイト源。`timeout_ms < 0` はブロッキング (バイトが来るまで待つ)。
/// `None` はブロッキング時は EOF/エラー、timeout 指定時は timeout/EOF/エラーを表す。
pub trait ByteSource {
    fn read_byte(&mut self, timeout_ms: i32) -> Option<u8>;
}

/// 1 イベントをデコードする。最初のバイトはブロッキングで読み、後続 (ESC/UTF-8 継続) は
/// `POLL_TIMEOUT_MS` 付きで読む。
pub fn next_event(src: &mut impl ByteSource) -> InEvent {
    let b0 = match src.read_byte(-1) {
        Some(b) => b,
        None => {
            return InEvent {
                raw: Vec::new(),
                tok: Tok::Eof,
            }
        }
    };
    let raw = vec![b0];
    // 順序が load-bearing: Enter (0x0a/0x0d) と Backspace (0x08) と ESC (0x1b) を
    // `b < 0x20` の制御文字フィルタより **先** に判定する。さもないと Enter 等が
    // Ctrl(_) に飲まれる (過去 2 回再発した「Enter が効かない」バグ。golden test 参照)。
    match b0 {
        0x0a | 0x0d => InEvent {
            raw,
            tok: Tok::Enter,
        },
        0x08 | 0x7f => InEvent {
            raw,
            tok: Tok::Backspace,
        },
        0x1b => decode_escape(src, raw),
        b if b < 0x20 => InEvent {
            raw,
            tok: Tok::Ctrl(b),
        },
        b if b < 0x80 => InEvent {
            raw,
            tok: Tok::Char(b as char),
        },
        _ => decode_utf8(src, raw),
    }
}

fn decode_escape(src: &mut impl ByteSource, mut raw: Vec<u8>) -> InEvent {
    let b1 = match src.read_byte(POLL_TIMEOUT_MS) {
        Some(b) => b,
        None => return InEvent { raw, tok: Tok::Esc }, // 単独 ESC
    };
    raw.push(b1);
    match b1 {
        b'\r' | b'\n' => InEvent {
            raw,
            tok: Tok::AltEnter,
        },
        b'[' => decode_csi(src, raw),
        b'O' => decode_ss3(src, raw),
        _ => InEvent {
            raw,
            tok: Tok::EscSeq,
        }, // ESC + 任意 (Alt+key 等)
    }
}

fn decode_csi(src: &mut impl ByteSource, mut raw: Vec<u8>) -> InEvent {
    // raw = [ESC, '[']。パラメータ + 終端 (0x40..=0x7E) を poll 付きで読み切る。
    let mut params: Vec<u8> = Vec::new();
    let mut final_byte: u8 = 0;
    while raw.len() < MAX_SEQ_LEN {
        match src.read_byte(POLL_TIMEOUT_MS) {
            Some(c) => {
                raw.push(c);
                if (0x40..=0x7E).contains(&c) {
                    final_byte = c;
                    break;
                }
                params.push(c);
            }
            None => break, // 不完全: 溜めた raw のまま返す
        }
    }
    let tok = classify_csi(&params, final_byte);
    InEvent { raw, tok }
}

fn classify_csi(params: &[u8], final_byte: u8) -> Tok {
    // フォーカスイベント (ESC [ I / ESC [ O)
    if params.is_empty() && final_byte == b'I' {
        return Tok::FocusIn;
    }
    if params.is_empty() && final_byte == b'O' {
        return Tok::FocusOut;
    }
    // 修飾付き Enter の CSI u (ESC [ 13 ; N u)。"13;" 始まりに限定
    // (プレーン Enter が \x1b[13u で届いた場合は EscSeq へ流す)。
    if final_byte == b'u' && params.starts_with(b"13;") {
        return Tok::ModEnter;
    }
    match (params, final_byte) {
        // bracketed paste マーカー (端末が ESC[?2004h 有効時にペースト本文を囲む)
        (b"200", b'~') => Tok::PasteStart,
        (b"201", b'~') => Tok::PasteEnd,
        (b"", b'A') => Tok::Up,
        (b"", b'B') => Tok::Down,
        (b"", b'C') => Tok::Right,
        (b"", b'D') => Tok::Left,
        (b"", b'H') | (b"1", b'~') | (b"7", b'~') => Tok::Home,
        (b"", b'F') | (b"4", b'~') | (b"8", b'~') => Tok::End,
        (b"3", b'~') => Tok::Delete,
        _ => Tok::EscSeq,
    }
}

fn decode_ss3(src: &mut impl ByteSource, mut raw: Vec<u8>) -> InEvent {
    // SS3 (ESC O <1 byte>): Home/End/F1-F4 / アプリケーションカーソルモードの矢印。
    // 1 byte 追読みしないと vim 等で ESC O と続く文字が分割解釈される。
    let tok = match src.read_byte(POLL_TIMEOUT_MS) {
        Some(c) => {
            raw.push(c);
            match c {
                b'H' => Tok::Home,
                b'F' => Tok::End,
                b'A' => Tok::Up,
                b'B' => Tok::Down,
                b'C' => Tok::Right,
                b'D' => Tok::Left,
                _ => Tok::EscSeq, // F1-F4 (P/Q/R/S) 等
            }
        }
        None => Tok::EscSeq, // ESC O 単独 (不完全)
    };
    InEvent { raw, tok }
}

fn decode_utf8(src: &mut impl ByteSource, mut raw: Vec<u8>) -> InEvent {
    let len = utf8_char_len(raw[0]);
    if len <= 1 {
        // 不正な先頭バイト (継続バイト等) → デコード不能
        return InEvent {
            raw,
            tok: Tok::Bytes,
        };
    }
    while raw.len() < len {
        match src.read_byte(POLL_TIMEOUT_MS) {
            Some(c) => raw.push(c),
            None => break,
        }
    }
    if raw.len() == len {
        if let Ok(s) = std::str::from_utf8(&raw) {
            if let Some(c) = s.chars().next() {
                return InEvent {
                    raw,
                    tok: Tok::Char(c),
                };
            }
        }
    }
    InEvent {
        raw,
        tok: Tok::Bytes,
    }
}

/// UTF-8 の先頭バイトから文字のバイト長を返す。
fn utf8_char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b < 0xC0 {
        1 // 継続バイト (先頭としては不正)
    } else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else {
        4
    }
}

// ---- fd 0 の実バイト源 ----

/// fd 0 を `ManuallyDrop` で借りて raw モードで 1 byte ずつ読むバイト源。
/// `io::stdin()` の BufReader をバイパスする (poll と併用するとデータ喪失するため)。
/// raw モードはセッション全体で維持済み (`save_terminal_settings`)。
#[cfg(unix)]
pub struct Fd0Source {
    stdin: std::mem::ManuallyDrop<std::fs::File>,
    fd: i32,
}

#[cfg(unix)]
impl Fd0Source {
    pub fn new() -> Self {
        use std::os::unix::io::FromRawFd;
        let stdin = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(0) });
        Self { stdin, fd: 0 }
    }
}

#[cfg(unix)]
impl Default for Fd0Source {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(unix)]
impl ByteSource for Fd0Source {
    fn read_byte(&mut self, timeout_ms: i32) -> Option<u8> {
        use std::io::Read;
        loop {
            let mut pollfd = libc::pollfd {
                fd: self.fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
            if ready < 0 {
                // EINTR (SIGWINCH 等): ブロッキング時は再試行 (std の File::read 相当)、
                // timeout 指定時は「来なかった」扱いで打ち切る。
                let interrupted =
                    std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted;
                if timeout_ms < 0 && interrupted {
                    continue;
                }
                return None;
            }
            if ready == 0 {
                return None; // timeout (timeout_ms < 0 では起きない)
            }
            let mut byte = [0u8; 1];
            match self.stdin.read(&mut byte) {
                Ok(1) => return Some(byte[0]),
                Ok(_) => return None, // EOF
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted && timeout_ms < 0 => {
                    continue;
                }
                Err(_) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用バイト源。バイトを順に返し、尽きたら `None` (= timeout/EOF をエミュレート)。
    /// 端末では完全な ESC シーケンスは 1 バーストで届くので、この「尽きたら None」モデルで
    /// 単独 ESC / partial CSI の timeout も表現できる。
    struct SliceSource {
        bytes: Vec<u8>,
        pos: usize,
    }
    impl SliceSource {
        fn new(bytes: &[u8]) -> Self {
            Self {
                bytes: bytes.to_vec(),
                pos: 0,
            }
        }
    }
    impl ByteSource for SliceSource {
        fn read_byte(&mut self, _timeout_ms: i32) -> Option<u8> {
            let b = self.bytes.get(self.pos).copied();
            if b.is_some() {
                self.pos += 1;
            }
            b
        }
    }

    fn decode_all(bytes: &[u8]) -> Vec<Tok> {
        let mut src = SliceSource::new(bytes);
        let mut out = Vec::new();
        loop {
            let ev = next_event(&mut src);
            if ev.tok == Tok::Eof {
                break;
            }
            out.push(ev.tok);
        }
        out
    }

    #[test]
    fn enter_lf_and_cr() {
        assert_eq!(decode_all(b"\n"), vec![Tok::Enter]);
        assert_eq!(decode_all(b"\r"), vec![Tok::Enter]);
    }

    #[test]
    fn enter_is_not_swallowed_by_control_filter() {
        // 回帰防止: Enter (0x0a/0x0d) が制御文字フィルタ (b < 0x20) に飲まれない。
        // 過去 2 回再発した「Enter キーが効かない」バグの byte レベル網。
        let mut src = SliceSource::new(b"\r");
        let ev = next_event(&mut src);
        assert_eq!(ev.tok, Tok::Enter);
        assert_eq!(ev.raw, b"\r");
    }

    #[test]
    fn ascii_chars() {
        assert_eq!(decode_all(b"y"), vec![Tok::Char('y')]);
        assert_eq!(decode_all(b"Y"), vec![Tok::Char('Y')]);
        assert_eq!(decode_all(b" "), vec![Tok::Char(' ')]);
    }

    #[test]
    fn control_chars() {
        assert_eq!(decode_all(&[0x03]), vec![Tok::Ctrl(0x03)]); // Ctrl+C
        assert_eq!(decode_all(&[0x04]), vec![Tok::Ctrl(0x04)]); // Ctrl+D
        assert_eq!(decode_all(&[0x1f]), vec![Tok::Ctrl(0x1f)]); // Ctrl+/
        assert_eq!(decode_all(&[0x01]), vec![Tok::Ctrl(0x01)]); // Ctrl+A
        assert_eq!(decode_all(&[0x09]), vec![Tok::Ctrl(0x09)]); // Tab
        assert_eq!(decode_all(&[0x08]), vec![Tok::Backspace]);
        assert_eq!(decode_all(&[0x7f]), vec![Tok::Backspace]);
    }

    #[test]
    fn csi_arrows_and_nav() {
        assert_eq!(decode_all(b"\x1b[A"), vec![Tok::Up]);
        assert_eq!(decode_all(b"\x1b[B"), vec![Tok::Down]);
        assert_eq!(decode_all(b"\x1b[C"), vec![Tok::Right]);
        assert_eq!(decode_all(b"\x1b[D"), vec![Tok::Left]);
        assert_eq!(decode_all(b"\x1b[H"), vec![Tok::Home]);
        assert_eq!(decode_all(b"\x1b[F"), vec![Tok::End]);
        assert_eq!(decode_all(b"\x1b[3~"), vec![Tok::Delete]);
        assert_eq!(decode_all(b"\x1b[1~"), vec![Tok::Home]);
        assert_eq!(decode_all(b"\x1b[7~"), vec![Tok::Home]);
        assert_eq!(decode_all(b"\x1b[4~"), vec![Tok::End]);
        assert_eq!(decode_all(b"\x1b[8~"), vec![Tok::End]);
    }

    #[test]
    fn ss3_keys() {
        assert_eq!(decode_all(b"\x1bOH"), vec![Tok::Home]);
        assert_eq!(decode_all(b"\x1bOF"), vec![Tok::End]);
        assert_eq!(decode_all(b"\x1bOA"), vec![Tok::Up]);
        assert_eq!(decode_all(b"\x1bOD"), vec![Tok::Left]);
        // F1 (ESC O P) 等は EscSeq (minibuffer は無視、passthrough は raw 転送)
        assert_eq!(decode_all(b"\x1bOP"), vec![Tok::EscSeq]);
    }

    #[test]
    fn focus_events() {
        assert_eq!(decode_all(b"\x1b[I"), vec![Tok::FocusIn]);
        assert_eq!(decode_all(b"\x1b[O"), vec![Tok::FocusOut]);
    }

    #[test]
    fn bracketed_paste_markers() {
        assert_eq!(decode_all(b"\x1b[200~"), vec![Tok::PasteStart]);
        assert_eq!(decode_all(b"\x1b[201~"), vec![Tok::PasteEnd]);
    }

    #[test]
    fn bracketed_paste_sequence() {
        // 複数行ペースト: 改行 (\r) はマーカー間に Enter として届く。
        // minibuffer 側がこの Enter を「送信」ではなく「改行挿入」に振り分ける。
        assert_eq!(
            decode_all(b"\x1b[200~ab\rcd\x1b[201~"),
            vec![
                Tok::PasteStart,
                Tok::Char('a'),
                Tok::Char('b'),
                Tok::Enter,
                Tok::Char('c'),
                Tok::Char('d'),
                Tok::PasteEnd,
            ]
        );
    }

    #[test]
    fn modified_enter_csi_u() {
        // ESC [ 13 ; 2 u (Shift+Enter 等) → ModEnter
        assert_eq!(decode_all(b"\x1b[13;2u"), vec![Tok::ModEnter]);
        // プレーン Enter の CSI u (13u, "13;" でない) は ModEnter にしない
        assert_eq!(decode_all(b"\x1b[13u"), vec![Tok::EscSeq]);
    }

    #[test]
    fn alt_enter() {
        assert_eq!(decode_all(b"\x1b\r"), vec![Tok::AltEnter]);
        assert_eq!(decode_all(b"\x1b\n"), vec![Tok::AltEnter]);
    }

    #[test]
    fn lone_esc() {
        assert_eq!(decode_all(b"\x1b"), vec![Tok::Esc]);
    }

    #[test]
    fn partial_csi_times_out_to_escseq() {
        // 終端が来ないまま尽きる (timeout) → 溜めた raw が EscSeq として返る (ハングしない)
        let mut src = SliceSource::new(b"\x1b[1");
        let ev = next_event(&mut src);
        assert_eq!(ev.tok, Tok::EscSeq);
        assert_eq!(ev.raw, b"\x1b[1");
    }

    #[test]
    fn ime_confirmed_multibyte() {
        // 全角 ｙ (U+FF59), ひらがな あ (U+3042) / ん (U+3093) — IME 確定文字
        assert_eq!(decode_all("ｙ".as_bytes()), vec![Tok::Char('ｙ')]);
        assert_eq!(decode_all("あ".as_bytes()), vec![Tok::Char('あ')]);
        assert_eq!(decode_all("ん".as_bytes()), vec![Tok::Char('ん')]);
    }

    #[test]
    fn raw_is_preserved() {
        // passthrough は raw を無加工転送するので、raw が元バイト列と一致すること。
        let mut src = SliceSource::new("あ".as_bytes());
        let ev = next_event(&mut src);
        assert_eq!(ev.tok, Tok::Char('あ'));
        assert_eq!(ev.raw, "あ".as_bytes());

        let mut src = SliceSource::new(b"\x1b[A");
        let ev = next_event(&mut src);
        assert_eq!(ev.tok, Tok::Up);
        assert_eq!(ev.raw, b"\x1b[A");

        let mut src = SliceSource::new(b"\x1b[I");
        let ev = next_event(&mut src);
        assert_eq!(ev.tok, Tok::FocusIn);
        assert_eq!(ev.raw, b"\x1b[I");
    }

    #[test]
    fn invalid_utf8_lead_is_bytes() {
        // 継続バイトが先頭に来た等 → Bytes (raw のみ、passthrough は転送)
        assert_eq!(decode_all(&[0x80]), vec![Tok::Bytes]);
    }

    #[test]
    fn sequence_of_events() {
        // 連続入力が正しく区切られる
        assert_eq!(
            decode_all(b"y\r\x1b[A"),
            vec![Tok::Char('y'), Tok::Enter, Tok::Up]
        );
        // confirm の代表入力
        assert_eq!(decode_all(b"Yn"), vec![Tok::Char('Y'), Tok::Char('n')]);
    }
}
