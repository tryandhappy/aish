use crate::config::DisplayConfig;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use unicode_width::UnicodeWidthChar;

pub enum InputEvent {
    PtyData(Vec<u8>),
    Line(String),
    PassthroughEnded,
    #[allow(dead_code)]
    CtrlCExit,
}

static CTRL_C_COUNT: AtomicU32 = AtomicU32::new(0);
static MINIBUFFER_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn record_ctrl_c() {
    CTRL_C_COUNT.fetch_add(1, Ordering::Relaxed);
}

pub fn ctrl_c_count() -> u32 {
    CTRL_C_COUNT.load(Ordering::Relaxed)
}

pub fn minibuffer_active() -> bool {
    MINIBUFFER_ACTIVE.load(Ordering::Relaxed)
}

pub fn terminal_size() -> (u16, u16) {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let fd = io::stdout().as_raw_fd();
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_row > 0 {
            return (ws.ws_row, ws.ws_col);
        }
    }
    (24, 80)
}

pub enum InputRequest {
    Passthrough(String),
    ReadLine(String),
}

#[cfg(unix)]
static ORIG_TERMIOS: OnceLock<libc::termios> = OnceLock::new();

/// 起動時にtermiosを保存し、rawモードに設定する。main開始直後に呼ぶこと。
#[cfg(unix)]
pub fn save_terminal_settings() {
    use std::os::unix::io::AsRawFd;
    let fd = io::stdin().as_raw_fd();
    if let Some(t) = termios_get(fd) {
        let _ = ORIG_TERMIOS.set(t);
        // セッション全体でrawモードを維持する
        let mut raw = t;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) };
    }
}

#[cfg(not(unix))]
pub fn save_terminal_settings() {}

/// termiosを起動時の状態に復元する。終了時に呼ぶこと。
#[cfg(unix)]
pub fn restore_terminal_settings() {
    use std::os::unix::io::AsRawFd;
    if let Some(orig) = ORIG_TERMIOS.get() {
        let fd = io::stdin().as_raw_fd();
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, orig) };
    }
}

#[cfg(not(unix))]
pub fn restore_terminal_settings() {}
pub fn build_color_start(fg: &str, bg: &str) -> String {
    let erase = if bg.is_empty() { "" } else { "\x1b[K" };
    format!("{}{}{}", fg, bg, erase)
}

const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub struct Spinner {
    running: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Spinner {
    pub fn start(display: &DisplayConfig) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();
        let color = build_color_start(&display.thinking_foreground, &display.thinking_background);
        let message = display.thinking_message.clone();

        let handle = std::thread::spawn(move || {
            let mut stdout = io::stdout();
            let mut i = 0;
            while running_clone.load(Ordering::Relaxed) {
                let _ = write!(stdout, "\r{}{} {}\x1b[0m\x1b[K", color, SPINNER_FRAMES[i], message);
                let _ = stdout.flush();
                i = (i + 1) % SPINNER_FRAMES.len();
                std::thread::sleep(Duration::from_millis(80));
            }
            let _ = write!(stdout, "\r\x1b[K");
            let _ = stdout.flush();
        });

        Spinner {
            running,
            handle: Some(handle),
        }
    }

    pub fn stop(mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

pub fn print_ai_message(message: &str, display: &DisplayConfig) {
    let color = build_color_start(&display.ai_foreground, &display.ai_background);
    for line in message.lines() {
        print!("{}{}\x1b[0m\n", color, line);
    }
    io::stdout().flush().ok();
}

pub fn print_ai_commands(commands: &[String], display: &DisplayConfig) {
    if commands.is_empty() {
        return;
    }
    let color = build_color_start(&display.ai_foreground, &display.ai_background);
    print!("{}Proposed commands:\x1b[0m\n", color);
    for (i, cmd) in commands.iter().enumerate() {
        print!("{}  {}: {}\x1b[0m\n", color, i + 1, cmd);
    }
    io::stdout().flush().ok();
}

pub fn print_confirm_prompt(commands: &[String], display: &DisplayConfig) {
    if commands.is_empty() {
        return;
    }
    print_ai_commands(commands, display);
    let bg = &display.input_background;
    print!("{}Execute? (Y/n) \x1b[0m ", bg);
    io::stdout().flush().ok();
}

pub fn parse_confirm(input: &str) -> bool {
    let trimmed = input.trim();
    trimmed.is_empty() || trimmed.eq_ignore_ascii_case("y") || trimmed.eq_ignore_ascii_case("yes")
}

pub enum UserInput {
    AiPrompt(String),
    ClaudeHandover,
    ShellCommand(String),
    Exit,
}

pub fn parse_input(input: &str) -> UserInput {
    let trimmed = input.trim();

    if trimmed.eq_ignore_ascii_case("exit") {
        return UserInput::Exit;
    }

    if let Some(prompt) = trimmed.strip_prefix("@ai") {
        return UserInput::AiPrompt(prompt.trim().to_string());
    }

    if let Some(prompt) = trimmed.strip_prefix('?') {
        let prompt = prompt.trim();
        if prompt.eq_ignore_ascii_case("claude") {
            return UserInput::ClaudeHandover;
        }
        return UserInput::AiPrompt(prompt.to_string());
    }

    UserInput::ShellCommand(input.to_string())
}

/// 文字の表示幅を返す（全角=2, 半角=1, 制御文字=0）
fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// rawモードでライン編集を行い、全角文字のBS削除に対応する
pub fn read_line() -> Option<String> {
    #[cfg(unix)]
    {
        read_line_raw()
    }
    #[cfg(not(unix))]
    {
        read_line_cooked()
    }
}

#[cfg(not(unix))]
fn read_line_cooked() -> Option<String> {
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => Some(line.trim_end_matches('\n').trim_end_matches('\r').to_string()),
        Err(_) => None,
    }
}

#[cfg(unix)]
fn read_line_raw() -> Option<String> {
    // rawモードはセッション全体で維持されているため、ここでは設定・復元しない
    read_line_raw_loop()
}

#[cfg(unix)]
fn termios_get(fd: i32) -> Option<libc::termios> {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) == 0 {
            Some(t)
        } else {
            None
        }
    }
}

#[cfg(unix)]
fn read_line_raw_loop() -> Option<String> {
    read_line_raw_loop_from(String::new(), false)
}

#[cfg(unix)]
fn read_line_raw_loop_from(initial: String, minibuffer: bool) -> Option<String> {
    let mut line = initial;
    let mut stdout = io::stdout();
    let mut stdin = io::stdin();
    let mut buf = [0u8; 4];

    loop {
        let n = match stdin.read(&mut buf[..1]) {
            Ok(0) => {
                if line.is_empty() {
                    return None;
                }
                break;
            }
            Ok(n) => n,
            Err(_) => return None,
        };

        if n == 0 {
            continue;
        }

        let b = buf[0];

        match b {
            b'\n' | b'\r' => {
                if !minibuffer {
                    let _ = stdout.write_all(b"\x1b[0m\n");
                    let _ = stdout.flush();
                }
                break;
            }
            0x7f | 0x08 => {
                // Backspace (DEL or BS)
                if let Some(c) = line.pop() {
                    let w = char_width(c);
                    // カーソルをw列戻し、スペースで上書きし、再度戻す
                    for _ in 0..w {
                        let _ = stdout.write_all(b"\x08 \x08");
                    }
                    let _ = stdout.flush();
                }
            }
            0x03 => {
                // Ctrl-C
                if minibuffer {
                    return None;
                }
                record_ctrl_c();
                let _ = stdout.write_all(b"^C\n");
                let _ = stdout.flush();
                return Some(String::new());
            }
            0x04 => {
                // Ctrl-D (EOF)
                if line.is_empty() {
                    return None;
                }
            }
            0x15 => {
                // Ctrl-U: 行全体を削除
                let total_width: usize = line.chars().map(|c| char_width(c)).sum();
                for _ in 0..total_width {
                    let _ = stdout.write_all(b"\x08 \x08");
                }
                let _ = stdout.flush();
                line.clear();
            }
            0x17 => {
                // Ctrl-W: 直前の単語を削除
                // 末尾の空白を削除
                let mut erased_width = 0usize;
                while line.ends_with(' ') {
                    line.pop();
                    erased_width += 1;
                }
                // 非空白文字を削除
                while !line.is_empty() && !line.ends_with(' ') {
                    if let Some(c) = line.pop() {
                        erased_width += char_width(c);
                    }
                }
                for _ in 0..erased_width {
                    let _ = stdout.write_all(b"\x08 \x08");
                }
                let _ = stdout.flush();
            }
            0x1b => {
                // ESCシーケンス（矢印キー等）を読み飛ばす
                let mut seq = [0u8; 2];
                let _ = stdin.read(&mut seq);
                // 無視
            }
            _ if b < 0x20 => {
                // その他の制御文字は無視
            }
            _ => {
                // UTF-8マルチバイト文字の処理
                let byte_len = utf8_char_len(b);
                if byte_len > 1 {
                    // 残りのバイトを読む
                    let mut read_so_far = 1;
                    buf[0] = b;
                    while read_so_far < byte_len {
                        match stdin.read(&mut buf[read_so_far..byte_len]) {
                            Ok(n) if n > 0 => read_so_far += n,
                            _ => break,
                        }
                    }
                    if read_so_far == byte_len {
                        if let Ok(s) = std::str::from_utf8(&buf[..byte_len]) {
                            line.push_str(s);
                            let _ = stdout.write_all(&buf[..byte_len]);
                            let _ = stdout.flush();
                        }
                    }
                } else {
                    // ASCII文字
                    line.push(b as char);
                    let _ = stdout.write_all(&[b]);
                    let _ = stdout.flush();
                }
            }
        }
    }

    Some(line)
}

/// パススルーモードで入力を読む。Ctrl+/でミニバッファを開く。
/// それ以外のキー入力はPTYに直送される。
pub fn passthrough_read(tx: &Sender<InputEvent>, input_bg: &str, aish_label: &str) {
    #[cfg(unix)]
    {
        passthrough_read_unix(tx, input_bg, aish_label);
    }
    #[cfg(not(unix))]
    {
        let _ = (input_bg, aish_label);
        match read_line_cooked() {
            Some(line) => {
                let _ = tx.send(InputEvent::Line(line));
            }
            None => {}
        }
    }
}

#[cfg(unix)]
fn passthrough_read_unix(tx: &Sender<InputEvent>, input_bg: &str, aish_label: &str) {
    // rawモードはセッション全体で維持されているため、ここでは設定・復元しない
    passthrough_read_raw(tx, input_bg, aish_label);
    let _ = tx.send(InputEvent::PassthroughEnded);
}

/// ミニバッファをターミナル最下行に表示し、ユーザ入力を受け付ける。
/// 入力確定後にカーソルを復元し、InputEventを送信する。
#[cfg(unix)]
fn show_minibuffer(
    stdout: &mut io::Stdout,
    tx: &Sender<InputEvent>,
    input_bg: &str,
    aish_label: &str,
    cancel_shell: bool,
) {
    let (rows, _) = terminal_size();
    MINIBUFFER_ACTIVE.store(true, Ordering::Relaxed);

    // カーソル保存、最下行に移動、[aish]ラベル + 入力エリア描画
    let _ = write!(stdout, "\x1b7\x1b[{};1H\x1b[2K{}{}\x1b[K", rows, aish_label, input_bg);
    let _ = stdout.flush();

    let result = read_line_raw_loop_from(String::new(), true);

    // クリーンアップ: 最下行消去、カーソル復元
    let (rows, _) = terminal_size(); // リサイズ対応で再取得
    let _ = write!(stdout, "\x1b[0m\x1b[{};1H\x1b[2K\x1b8", rows);
    let _ = stdout.flush();
    MINIBUFFER_ACTIVE.store(false, Ordering::Relaxed);

    match result {
        Some(text) if text.trim().is_empty() => {
            // 空Enter → 何もしない
        }
        Some(text) => {
            // AIプロンプト
            if cancel_shell {
                let _ = tx.send(InputEvent::PtyData(vec![0x03]));
            }
            let _ = tx.send(InputEvent::Line(format!("? {}", text)));
        }
        None => {
            // キャンセル (Ctrl+C) — 何も送信しない
        }
    }
}

/// パススルーモードのrawキー入力処理。
/// Ctrl+/ でミニバッファを開き、それ以外はPTYに直送する。
#[cfg(unix)]
fn passthrough_read_raw(tx: &Sender<InputEvent>, input_bg: &str, aish_label: &str) {
    let mut stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut buf = [0u8; 1];
    let mut at_line_start = true;

    loop {
        match stdin.read(&mut buf) {
            Ok(0) => return,
            Ok(_) => {}
            Err(_) => return,
        }
        let b = buf[0];

        match b {
            0x1f => {
                // Ctrl+/ → ミニバッファを開く
                show_minibuffer(&mut stdout, tx, input_bg, aish_label, !at_line_start);
                return;
            }
            0x03 => {
                // Ctrl+C: PTYに送信して戻る
                let _ = tx.send(InputEvent::PtyData(vec![b]));
                return;
            }
            b'\r' | b'\n' => {
                let _ = tx.send(InputEvent::PtyData(vec![b]));
                return;
            }
            0x1b => {
                // ESCシーケンス
                if at_line_start {
                    // 行頭ではESCシーケンスを読み飛ばす(フォーカスイベント等)
                    let mut seq = [0u8; 1];
                    if stdin.read(&mut seq).is_ok() && seq[0] == b'[' {
                        loop {
                            match stdin.read(&mut seq) {
                                Ok(1) if seq[0] >= 0x40 && seq[0] <= 0x7E => break,
                                Ok(1) => continue,
                                _ => break,
                            }
                        }
                    }
                } else {
                    // 入力中はPTYに転送
                    let _ = tx.send(InputEvent::PtyData(vec![b]));
                    let mut seq = [0u8; 1];
                    if stdin.read(&mut seq).is_ok() {
                        let _ = tx.send(InputEvent::PtyData(vec![seq[0]]));
                        if seq[0] == b'[' {
                            loop {
                                match stdin.read(&mut seq) {
                                    Ok(1) => {
                                        let _ = tx.send(InputEvent::PtyData(vec![seq[0]]));
                                        if seq[0] >= 0x40 && seq[0] <= 0x7E {
                                            break;
                                        }
                                    }
                                    _ => break,
                                }
                            }
                        }
                    }
                }
            }
            _ if at_line_start && (b < 0x20 || b == 0x7f) => {
                // 行頭の制御文字は無視
            }
            _ => {
                // 通常文字(ASCII or UTF-8マルチバイト)をPTYに転送
                let byte_len = utf8_char_len(b);
                if byte_len > 1 {
                    let mut mb_buf = [0u8; 4];
                    mb_buf[0] = b;
                    let mut read_so_far = 1;
                    while read_so_far < byte_len {
                        match stdin.read(&mut mb_buf[read_so_far..byte_len]) {
                            Ok(n) if n > 0 => read_so_far += n,
                            _ => break,
                        }
                    }
                    let _ = tx.send(InputEvent::PtyData(mb_buf[..read_so_far].to_vec()));
                } else {
                    let _ = tx.send(InputEvent::PtyData(vec![b]));
                }
                at_line_start = false;
            }
        }
    }
}

/// UTF-8の先頭バイトから文字のバイト長を返す
fn utf8_char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b < 0xC0 {
        1 // continuation byte (invalid as start)
    } else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else {
        4
    }
}
