use std::io::{self, Read, Write};
use std::sync::OnceLock;
use unicode_width::UnicodeWidthChar;

const AI_COLOR_START: &str = "\x1b[48;5;238m\x1b[K";

#[cfg(unix)]
static ORIG_TERMIOS: OnceLock<libc::termios> = OnceLock::new();

/// 起動時にtermiosを保存する。main開始直後に呼ぶこと。
#[cfg(unix)]
pub fn save_terminal_settings() {
    use std::os::unix::io::AsRawFd;
    let fd = io::stdin().as_raw_fd();
    if let Some(t) = termios_get(fd) {
        let _ = ORIG_TERMIOS.set(t);
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
const AI_COLOR_END: &str = "\x1b[0m";

pub fn print_ai_thinking() {
    print!("\x1b[36mThinking...\x1b[0m\n");
    io::stdout().flush().ok();
}

pub fn print_ai_message(message: &str) {
    for line in message.lines() {
        print!("{}{}{}\n", AI_COLOR_START, line, AI_COLOR_END);
    }
    io::stdout().flush().ok();
}

pub fn print_ai_commands(commands: &[String]) {
    if commands.is_empty() {
        return;
    }
    print!("{}Proposed commands:{}\n", AI_COLOR_START, AI_COLOR_END);
    for (i, cmd) in commands.iter().enumerate() {
        print!("{}  {}: {}{}\n", AI_COLOR_START, i + 1, cmd, AI_COLOR_END);
    }
    io::stdout().flush().ok();
}

pub fn print_confirm_prompt(commands: &[String]) {
    if commands.is_empty() {
        return;
    }
    print_ai_commands(commands);
    print!("\x1b[43mExecute? (Y/n) \x1b[0m ");
    io::stdout().flush().ok();
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
    use std::os::unix::io::AsRawFd;

    let fd = io::stdin().as_raw_fd();

    // 現在のtermios設定を保存
    let orig = match termios_get(fd) {
        Some(t) => t,
        None => return read_line_cooked_unix(),
    };

    // rawモードに設定（エコーoff, canonical off）
    let mut raw = orig;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO);
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
        return read_line_cooked_unix();
    }

    let result = read_line_raw_loop();

    // termios設定を復元
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &orig) };

    result
}

#[cfg(unix)]
fn read_line_cooked_unix() -> Option<String> {
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => Some(line.trim_end_matches('\n').trim_end_matches('\r').to_string()),
        Err(_) => None,
    }
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
