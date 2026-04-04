use crate::config::DisplayConfig;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::OnceLock;
use unicode_width::UnicodeWidthChar;

pub enum InputEvent {
    PtyData(Vec<u8>),
    Line(String),
    PassthroughEnded,
    #[allow(dead_code)]
    CtrlCExit,
}

static CTRL_C_COUNT: AtomicU32 = AtomicU32::new(0);

pub fn record_ctrl_c() {
    CTRL_C_COUNT.fetch_add(1, Ordering::Relaxed);
}

pub fn ctrl_c_count() -> u32 {
    CTRL_C_COUNT.load(Ordering::Relaxed)
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
fn color_to_fg(color: &str) -> String {
    if color.is_empty() {
        return String::new();
    }
    match color {
        "black" => "\x1b[30m".to_string(),
        "red" => "\x1b[31m".to_string(),
        "green" => "\x1b[32m".to_string(),
        "yellow" => "\x1b[33m".to_string(),
        "blue" => "\x1b[34m".to_string(),
        "magenta" => "\x1b[35m".to_string(),
        "cyan" => "\x1b[36m".to_string(),
        "white" => "\x1b[37m".to_string(),
        n => n
            .parse::<u8>()
            .ok()
            .map(|num| format!("\x1b[38;5;{}m", num))
            .unwrap_or_default(),
    }
}

fn color_to_bg(color: &str) -> String {
    if color.is_empty() {
        return String::new();
    }
    match color {
        "black" => "\x1b[40m".to_string(),
        "red" => "\x1b[41m".to_string(),
        "green" => "\x1b[42m".to_string(),
        "yellow" => "\x1b[43m".to_string(),
        "blue" => "\x1b[44m".to_string(),
        "magenta" => "\x1b[45m".to_string(),
        "cyan" => "\x1b[46m".to_string(),
        "white" => "\x1b[47m".to_string(),
        n => n
            .parse::<u8>()
            .ok()
            .map(|num| format!("\x1b[48;5;{}m", num))
            .unwrap_or_default(),
    }
}

pub fn build_color_start(fg: &str, bg: &str) -> String {
    let fg_code = color_to_fg(fg);
    let bg_code = color_to_bg(bg);
    let erase = if bg.is_empty() { "" } else { "\x1b[K" };
    format!("{}{}{}", fg_code, bg_code, erase)
}

pub fn print_ai_thinking(display: &DisplayConfig) {
    let color = build_color_start(&display.thinking_foreground, &display.thinking_background);
    print!("{}{}\x1b[0m\n", color, display.thinking_message);
    io::stdout().flush().ok();
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
    print!("\x1b[43mExecute? (Y/n) \x1b[0m ");
    io::stdout().flush().ok();
}

/// last_lineがシェルプロンプトらしいかを判定する。
pub fn looks_like_prompt(last_line: &[u8]) -> bool {
    let stripped = strip_ansi_escapes::strip(last_line);
    let s = String::from_utf8_lossy(&stripped);
    let trimmed = s.trim_end();
    if trimmed.is_empty() {
        return false;
    }
    trimmed.ends_with('$')
        || trimmed.ends_with('#')
        || trimmed.ends_with('%')
        || trimmed.ends_with('>')
}

pub fn parse_confirm(input: &str) -> bool {
    let trimmed = input.trim();
    trimmed.is_empty() || trimmed.eq_ignore_ascii_case("y") || trimmed.eq_ignore_ascii_case("yes")
}

pub enum UserInput {
    AiPrompt(String),
    AiAnalyze,
    ShellCommand(String),
    Exit,
}

const AI_ANALYZE_MARKER: &str = "\x1f";

pub fn parse_input(input: &str) -> UserInput {
    if input == AI_ANALYZE_MARKER {
        return UserInput::AiAnalyze;
    }

    let trimmed = input.trim();

    if trimmed.eq_ignore_ascii_case("exit") {
        return UserInput::Exit;
    }

    if let Some(prompt) = trimmed.strip_prefix("@ai") {
        return UserInput::AiPrompt(prompt.trim().to_string());
    }

    if let Some(prompt) = trimmed.strip_prefix('?') {
        return UserInput::AiPrompt(prompt.trim().to_string());
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
    let mut line = String::new();
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
                // Enter
                let _ = stdout.write_all(b"\n");
                let _ = stdout.flush();
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
            0x1f => {
                // Ctrl+? : AI分析
                let _ = stdout.write_all(b"\n");
                let _ = stdout.flush();
                return Some("\x1f".to_string());
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

/// パススルーモードで入力を読む。AIプレフィックス検出付き。
/// キー入力はPTYに直送され、@aiや?が検出された場合はAI入力モードに切り替わる。
pub fn passthrough_read(tx: &Sender<InputEvent>) {
    #[cfg(unix)]
    {
        passthrough_read_unix(tx);
    }
    #[cfg(not(unix))]
    {
        match read_line_cooked() {
            Some(line) => {
                let _ = tx.send(InputEvent::Line(line));
            }
            None => {}
        }
    }
}

#[cfg(unix)]
fn passthrough_read_unix(tx: &Sender<InputEvent>) {
    // rawモードはセッション全体で維持されているため、ここでは設定・復元しない
    passthrough_read_raw(tx);
    let _ = tx.send(InputEvent::PassthroughEnded);
}

#[cfg(unix)]
fn passthrough_read_raw(tx: &Sender<InputEvent>) {
    let mut stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut detect_buf: Vec<u8> = Vec::new();
    let mut buf = [0u8; 1];

    #[derive(PartialEq)]
    enum State {
        LineStart,
        DetectAt,
        DetectAtA,
        DetectAtAi,
        Passthrough,
    }
    let mut state = State::LineStart;

    loop {
        match stdin.read(&mut buf) {
            Ok(0) => return,
            Ok(_) => {}
            Err(_) => return,
        }
        let b = buf[0];

        match state {
            State::LineStart => match b {
                b'?' => {
                    let _ = stdout.write_all(b"?");
                    let _ = stdout.flush();
                    let rest = read_line_raw_loop().unwrap_or_default();
                    let _ = tx.send(InputEvent::Line(format!("?{}", rest)));
                    return;
                }
                b'@' => {
                    detect_buf.clear();
                    detect_buf.push(b);
                    state = State::DetectAt;
                }
                0x1f => {
                    let _ = stdout.write_all(b"\n");
                    let _ = stdout.flush();
                    let _ = tx.send(InputEvent::Line("\x1f".to_string()));
                    return;
                }
                0x03 => {
                    let _ = tx.send(InputEvent::PtyData(vec![b]));
                    return;
                }
                b'\r' | b'\n' => {
                    let _ = tx.send(InputEvent::PtyData(vec![b]));
                    return;
                }
                _ => {
                    let _ = tx.send(InputEvent::PtyData(vec![b]));
                    state = State::Passthrough;
                }
            },
            State::DetectAt => match b {
                b'a' | b'A' => {
                    detect_buf.push(b);
                    state = State::DetectAtA;
                }
                0x7f | 0x08 => {
                    detect_buf.clear();
                    state = State::LineStart;
                }
                0x03 => {
                    detect_buf.clear();
                    let _ = tx.send(InputEvent::PtyData(vec![b]));
                    return;
                }
                b'\r' | b'\n' => {
                    detect_buf.push(b);
                    let _ = tx.send(InputEvent::PtyData(detect_buf.clone()));
                    detect_buf.clear();
                    return;
                }
                _ => {
                    detect_buf.push(b);
                    let _ = tx.send(InputEvent::PtyData(detect_buf.clone()));
                    detect_buf.clear();
                    state = State::Passthrough;
                }
            },
            State::DetectAtA => match b {
                b'i' | b'I' => {
                    detect_buf.push(b);
                    state = State::DetectAtAi;
                }
                0x7f | 0x08 => {
                    detect_buf.pop();
                    state = State::DetectAt;
                }
                0x03 => {
                    detect_buf.clear();
                    let _ = tx.send(InputEvent::PtyData(vec![b]));
                    return;
                }
                b'\r' | b'\n' => {
                    detect_buf.push(b);
                    let _ = tx.send(InputEvent::PtyData(detect_buf.clone()));
                    detect_buf.clear();
                    return;
                }
                _ => {
                    detect_buf.push(b);
                    let _ = tx.send(InputEvent::PtyData(detect_buf.clone()));
                    detect_buf.clear();
                    state = State::Passthrough;
                }
            },
            State::DetectAtAi => match b {
                b' ' | b'\t' => {
                    let _ = stdout.write_all(b"@ai ");
                    let _ = stdout.flush();
                    let rest = read_line_raw_loop().unwrap_or_default();
                    let _ = tx.send(InputEvent::Line(format!("@ai {}", rest)));
                    return;
                }
                b'\r' | b'\n' => {
                    let _ = stdout.write_all(b"@ai\n");
                    let _ = stdout.flush();
                    let _ = tx.send(InputEvent::Line("@ai".to_string()));
                    return;
                }
                0x7f | 0x08 => {
                    detect_buf.pop();
                    state = State::DetectAtA;
                }
                0x03 => {
                    detect_buf.clear();
                    let _ = tx.send(InputEvent::PtyData(vec![b]));
                    return;
                }
                _ => {
                    detect_buf.push(b);
                    let _ = tx.send(InputEvent::PtyData(detect_buf.clone()));
                    detect_buf.clear();
                    state = State::Passthrough;
                }
            },
            State::Passthrough => {
                if b == 0x1f {
                    let _ = stdout.write_all(b"\n");
                    let _ = stdout.flush();
                    let _ = tx.send(InputEvent::Line("\x1f".to_string()));
                    return;
                }
                let _ = tx.send(InputEvent::PtyData(vec![b]));
                if b == b'\r' || b == b'\n' {
                    return;
                }
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
