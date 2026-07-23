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
    // win32-input-mode (ESC [ Vk ; Sc ; Uc ; Kd ; Cs ; Rc _)。Windows Terminal + PowerShell
    // では PSReadLine がこのモードを有効化するため、キー入力が KEY_EVENT でなくこの
    // シーケンスのバイト列で aish に届く (SPEC §15.13)。final byte `_`(0x5F) は 0x40..=0x7E に
    // 含まれ decode_csi が既に終端として読み切っているので、ここは params の解釈だけを担う。
    if final_byte == b'_' {
        if let Some(tok) = classify_win32_input_mode(params) {
            return tok;
        }
    }
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

/// win32-input-mode (`ESC [ Vk ; Sc ; Uc ; Kd ; Cs ; Rc _`) のパラメータを `Tok` に変換する。
/// `#[cfg]` を付けない純関数 (Unix では `ESC[..._` が来ず無害。無条件にすることで ubuntu CI で
/// golden test が回る)。**raw は呼び出し側の `InEvent` に保持されるので passthrough は生の
/// win32 バイトをそのまま PTY へ転送でき、PowerShell / 子 TUI がそれを復号する** (透明性の根幹)。
///
/// key-down (Kd != 0) だけ `Some(Tok)` を返す。key-up と解釈不能は `None` を返し、呼び出し側の
/// `EscSeq` フォールバックに委ねる (全 tok 消費者が `EscSeq` を無視し、passthrough は raw を送る
/// ので 1 キー = down/up 2 連でも二重入力にならず down/up 整合も保たれる → 新 variant 不要)。
fn classify_win32_input_mode(params: &[u8]) -> Option<Tok> {
    let s = std::str::from_utf8(params).ok()?;
    // 各フィールドを数値化 (空は 0 = win32-input-mode の省略既定)。非数値が混じる = win32 でない
    // → `?` で None にして EscSeq へフォールバック (別種の `ESC[..._` を誤解釈しない安全側)。
    let mut fields: Vec<u32> = Vec::new();
    for part in s.split(';') {
        let t = part.trim();
        fields.push(if t.is_empty() { 0 } else { t.parse().ok()? });
    }
    // 最低 Vk;Sc;Uc;Kd の 4 フィールドが無ければ win32-input-mode とみなさない。
    if fields.len() < 4 {
        return None;
    }
    let vk = fields[0];
    let uc = fields[2];
    let kd = fields[3];
    let cs = fields.get(4).copied().unwrap_or(0);

    // key-up は無視 (down だけ Tok 化)。
    if kd == 0 {
        return None;
    }
    const LEFT_CTRL_PRESSED: u32 = 0x0008;
    const RIGHT_CTRL_PRESSED: u32 = 0x0004;
    let ctrl = cs & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0;

    // Ctrl+/ (エントリキー)。VK_OEM_2(0xBF) または uChar が 0x1f/0x2f。term/windows.rs の
    // pump 正規化と同一値・同一条件 (US=0x1f / JIS=0x2f / VK 経路)。
    if ctrl && (vk == 0xBF || uc == 0x1F || uc == 0x2F) {
        return Some(Tok::Ctrl(0x1f));
    }
    // Vk による特殊キー (Enter/Backspace/Esc は制御文字フィルタより先に判定する — さもないと
    // uChar<0x20 の下の分岐に飲まれる。next_event の順序トラップと同じ理由)。
    match vk {
        0x0D => return Some(Tok::Enter),     // VK_RETURN
        0x08 => return Some(Tok::Backspace), // VK_BACK
        0x1B => return Some(Tok::Esc),       // VK_ESCAPE
        0x25 => return Some(Tok::Left),      // VK_LEFT
        0x26 => return Some(Tok::Up),        // VK_UP
        0x27 => return Some(Tok::Right),     // VK_RIGHT
        0x28 => return Some(Tok::Down),      // VK_DOWN
        0x24 => return Some(Tok::Home),      // VK_HOME
        0x23 => return Some(Tok::End),       // VK_END
        0x2E => return Some(Tok::Delete),    // VK_DELETE
        _ => {}
    }
    // Ctrl+英字等の C0 制御文字 (uChar が 0x01..=0x1f)。
    if uc != 0 && uc < 0x20 {
        return Some(Tok::Ctrl(uc as u8));
    }
    // 印字可能文字 (BMP)。非 BMP=サロゲート (1 record=1 UTF-16 unit) は稀なので None →
    // EscSeq にフォールバック (passthrough は raw で無事)。制約として文書化。
    if let Some(c) = char::from_u32(uc) {
        if !c.is_control() {
            return Some(Tok::Char(c));
        }
    }
    None
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
// b < 0x80 (ASCII) と継続バイト (b < 0xC0, 先頭としては不正) はどちらも長さ 1。
// 意図的に同一の分岐なので if_same_then_else は許容する。
#[allow(clippy::if_same_then_else)]
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

/// Windows のコンソール入力バイト源。実体は `crate::term::windows` の入力ポンプ
/// (`ReadConsoleInputW` → UTF-8 化キュー) への薄い委譲で、Unix の `Fd0Source` と
/// バイト互換のストリームを返す (VT 入力モードで ESC シーケンスも同形)。
#[cfg(windows)]
#[derive(Default)]
pub struct ConsoleSource;

#[cfg(windows)]
impl ConsoleSource {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(windows)]
impl ByteSource for ConsoleSource {
    fn read_byte(&mut self, timeout_ms: i32) -> Option<u8> {
        crate::term::read_stdin_byte(timeout_ms)
    }
}

/// UI 層が使う「現在プラットフォームの stdin バイト源」。
/// framing (byte→Tok) は本モジュールに集約し、OS 差はこの型だけに閉じる。
#[cfg(unix)]
pub type StdinSource = Fd0Source;
#[cfg(windows)]
pub type StdinSource = ConsoleSource;

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

    // ---- win32-input-mode (ESC[Vk;Sc;Uc;Kd;Cs;Rc_) ----
    // Windows Terminal + PowerShell の PSReadLine が有効化するモード。実測 (2026-07):
    // Ctrl+/ は ESC[191;53;0;1;40;1_ (Vk=191=0xBF, Cs=40=0x28 に LEFT_CTRL_PRESSED)。

    #[test]
    fn win32_ctrl_slash_via_vk_oem2() {
        // Ctrl+/ = VK_OEM_2(191) + Ctrl (Cs=8)、uChar=0。エントリキー 0x1f に正規化。
        assert_eq!(decode_all(b"\x1b[191;53;0;1;8;1_"), vec![Tok::Ctrl(0x1f)]);
        // 実測の Cs=40 (0x28 = NUMLOCK_ON|LEFT_CTRL) でも拾う。
        assert_eq!(decode_all(b"\x1b[191;53;0;1;40;1_"), vec![Tok::Ctrl(0x1f)]);
    }

    #[test]
    fn win32_ctrl_slash_via_uchar() {
        // US 配列で uChar=0x1f、JIS 等で uChar=0x2f のまま届くケース (Vk は 0 でも拾う)。
        assert_eq!(decode_all(b"\x1b[0;0;31;1;8;1_"), vec![Tok::Ctrl(0x1f)]); // 0x1f
        assert_eq!(decode_all(b"\x1b[0;0;47;1;8;1_"), vec![Tok::Ctrl(0x1f)]); // 0x2f
    }

    #[test]
    fn win32_printable_char() {
        // 'a' = Vk=0x41, uChar=0x61, key-down, 修飾なし。
        assert_eq!(decode_all(b"\x1b[65;30;97;1;0;1_"), vec![Tok::Char('a')]);
        // space。
        assert_eq!(decode_all(b"\x1b[32;57;32;1;0;1_"), vec![Tok::Char(' ')]);
    }

    #[test]
    fn win32_special_keys_by_vk() {
        assert_eq!(decode_all(b"\x1b[13;28;13;1;0;1_"), vec![Tok::Enter]);
        assert_eq!(decode_all(b"\x1b[8;14;8;1;0;1_"), vec![Tok::Backspace]);
        assert_eq!(decode_all(b"\x1b[27;1;27;1;0;1_"), vec![Tok::Esc]);
        assert_eq!(decode_all(b"\x1b[37;75;0;1;0;1_"), vec![Tok::Left]);
        assert_eq!(decode_all(b"\x1b[38;72;0;1;0;1_"), vec![Tok::Up]);
        assert_eq!(decode_all(b"\x1b[39;77;0;1;0;1_"), vec![Tok::Right]);
        assert_eq!(decode_all(b"\x1b[40;80;0;1;0;1_"), vec![Tok::Down]);
        assert_eq!(decode_all(b"\x1b[36;71;0;1;0;1_"), vec![Tok::Home]);
        assert_eq!(decode_all(b"\x1b[35;79;0;1;0;1_"), vec![Tok::End]);
        assert_eq!(decode_all(b"\x1b[46;83;0;1;0;1_"), vec![Tok::Delete]);
    }

    #[test]
    fn win32_ctrl_letters() {
        // Ctrl+C = Vk=0x43('C'), uChar=0x03, Ctrl。転送/中断に使う 0x03。
        assert_eq!(decode_all(b"\x1b[67;46;3;1;8;1_"), vec![Tok::Ctrl(0x03)]);
        // Ctrl+D = uChar=0x04。
        assert_eq!(decode_all(b"\x1b[68;32;4;1;8;1_"), vec![Tok::Ctrl(0x04)]);
    }

    #[test]
    fn win32_key_up_is_ignored() {
        // key-up (Kd=0) は Tok を生まず EscSeq に落ちる (二重入力防止)。
        assert_eq!(decode_all(b"\x1b[65;30;97;0;0;1_"), vec![Tok::EscSeq]);
    }

    #[test]
    fn win32_malformed_falls_back_to_escseq() {
        // フィールド不足 (< 4) は win32 とみなさず EscSeq。
        assert_eq!(decode_all(b"\x1b[1;2_"), vec![Tok::EscSeq]);
        // 非数値混じり (param 域に残る 0x3A ':' 等) も EscSeq。英字はそもそも CSI 終端
        // (0x40..=0x7E) 扱いになり params に入らないので、param 域の記号で検証する。
        assert_eq!(decode_all(b"\x1b[1;2;:;4_"), vec![Tok::EscSeq]);
    }

    #[test]
    fn win32_raw_is_preserved_for_passthrough() {
        // Ctrl+/ を tok 化しても raw は生の win32 バイト列を保持する
        // (passthrough は raw を PowerShell へ転送する)。
        let mut src = SliceSource::new(b"\x1b[191;53;0;1;8;1_");
        let ev = next_event(&mut src);
        assert_eq!(ev.tok, Tok::Ctrl(0x1f));
        assert_eq!(ev.raw, b"\x1b[191;53;0;1;8;1_");
    }
}
