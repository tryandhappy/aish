use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use unicode_width::UnicodeWidthChar;

const AI_COLOR_START: &str = "\x1b[46m";
const AI_COLOR_END: &str = "\x1b[0m";

pub const INPUT_MODE_NORMAL: u8 = 0;
pub const INPUT_MODE_CONFIRM: u8 = 1;
pub const INPUT_MODE_LINE: u8 = 2;

pub enum InputEvent {
    RawBytes(Vec<u8>),
    AiPrompt(String),
    Confirmation(bool),
    Exit,
}

pub fn print_ai_message(message: &str) {
    print!("{}{}{}\n", AI_COLOR_START, message, AI_COLOR_END);
    io::stdout().flush().ok();
}

pub fn print_ai_commands(commands: &[String]) {
    if commands.is_empty() {
        return;
    }
    print!(
        "{}Proposed commands:{}\n",
        AI_COLOR_START, AI_COLOR_END
    );
    for (i, cmd) in commands.iter().enumerate() {
        print!(
            "{}  {}: {}{}\n",
            AI_COLOR_START,
            i + 1,
            cmd,
            AI_COLOR_END
        );
    }
    io::stdout().flush().ok();
}

pub fn print_confirm_prompt(commands: &[String]) {
    print_ai_commands(commands);
    print!("Execute? (Y/n) ");
    io::stdout().flush().ok();
}

// --- ターミナル rawモード制御 ---

pub struct RawModeGuard {
    #[cfg(unix)]
    original: libc::termios,
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = io::stdin().as_raw_fd();
            unsafe {
                libc::tcsetattr(fd, libc::TCSANOW, &self.original);
            }
        }
    }
}

pub fn enable_raw_mode() -> RawModeGuard {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let fd = io::stdin().as_raw_fd();
        unsafe {
            let mut original: libc::termios = std::mem::zeroed();
            libc::tcgetattr(fd, &mut original);
            let mut raw = original;
            // 入力: canonical, echo, シグナル, 拡張を無効化
            raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
            // 入力: フロー制御, CR→NL変換等を無効化
            raw.c_iflag &=
                !(libc::IXON | libc::ICRNL | libc::BRKINT | libc::INPCK | libc::ISTRIP);
            // 文字サイズ8ビット
            raw.c_cflag |= libc::CS8;
            // 出力: OPOSTは維持 (\n → \r\n 変換を保持)
            // 1バイトで即座にreadが返るようにする
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            libc::tcsetattr(fd, libc::TCSANOW, &raw);
            return RawModeGuard { original };
        }
    }
    #[cfg(not(unix))]
    {
        RawModeGuard {}
    }
}

// --- 入力パース (行モード用) ---

pub enum UserInput {
    AiPrompt(String),
    #[allow(dead_code)]
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
        return UserInput::AiPrompt(prompt.trim().to_string());
    }

    UserInput::ShellCommand(input.to_string())
}

// --- 入力ループ ---

pub fn run_input_loop(tx: mpsc::Sender<InputEvent>, input_mode: Arc<AtomicU8>) {
    let mut stdin = io::stdin();
    let mut buf = [0u8; 1];
    let mut at_line_start = true;
    let mut prefix_buf: Vec<u8> = Vec::new();
    let mut ai_mode = false;
    let mut ai_prompt = String::new();
    let mut utf8_buf: Vec<u8> = Vec::new();
    let mut pending: Option<u8> = None;

    loop {
        let mode = input_mode.load(Ordering::Relaxed);

        // 確認モード: Y/nの1バイトを読む
        if mode == INPUT_MODE_CONFIRM {
            let b = match read_one_byte(&mut stdin, &mut pending) {
                Some(b) => b,
                None => return,
            };
            let confirmed = matches!(b, b'\r' | b'Y' | b'y');
            print!("\n");
            io::stdout().flush().ok();
            let _ = tx.send(InputEvent::Confirmation(confirmed));
            input_mode.store(INPUT_MODE_NORMAL, Ordering::Relaxed);
            at_line_start = true;
            ai_mode = false;
            prefix_buf.clear();
            continue;
        }

        // 行モード (リモート終了モード): ローカルエコー付き行入力
        if mode == INPUT_MODE_LINE {
            match read_line_raw(&mut stdin, &mut pending) {
                Some(line) => match parse_input(&line) {
                    UserInput::Exit => {
                        let _ = tx.send(InputEvent::Exit);
                        return;
                    }
                    UserInput::AiPrompt(prompt) => {
                        let _ = tx.send(InputEvent::AiPrompt(prompt));
                    }
                    UserInput::ShellCommand(_) => {
                        print!("Cannot execute commands in remote-ended mode.\n");
                        io::stdout().flush().ok();
                    }
                },
                None => return,
            }
            continue;
        }

        // 通常モード: rawバイト処理
        let b = match read_one_byte(&mut stdin, &mut pending) {
            Some(b) => b,
            None => return,
        };

        // stdin.read()後にモードを再チェック (ブロック中にモード変更された場合)
        let current_mode = input_mode.load(Ordering::Relaxed);
        if current_mode != INPUT_MODE_NORMAL {
            // バイトを保持して正しいモードで処理し直す
            pending = Some(b);
            // AIモードやプリフィックスバッファの状態をリセット
            if ai_mode {
                ai_mode = false;
                ai_prompt.clear();
                utf8_buf.clear();
            }
            if !prefix_buf.is_empty() {
                // バッファ済みのプリフィックスはPTYに送信
                let bytes = std::mem::take(&mut prefix_buf);
                let _ = tx.send(InputEvent::RawBytes(bytes));
            }
            at_line_start = true;
            continue;
        }

        // AIプロンプト入力中
        if ai_mode {
            handle_ai_byte(
                b,
                &mut ai_prompt,
                &mut ai_mode,
                &mut at_line_start,
                &tx,
                &mut utf8_buf,
            );
            continue;
        }

        // 行頭: @ai / ? プリフィックス検出
        if at_line_start {
            prefix_buf.push(b);

            // "?" → AIモード開始
            if prefix_buf.len() == 1 && prefix_buf[0] == b'?' {
                print!("?");
                io::stdout().flush().ok();
                ai_mode = true;
                ai_prompt.clear();
                prefix_buf.clear();
                at_line_start = false;
                continue;
            }

            let target = b"@ai";

            // "@ai"のプリフィックスに一致中 → バッファリング継続
            if prefix_buf.len() <= target.len() {
                if prefix_buf[..] == target[..prefix_buf.len()] {
                    continue;
                }
                // 一致しない → バッファをPTYに転送
                let bytes = std::mem::take(&mut prefix_buf);
                at_line_start =
                    bytes.last().map_or(false, |&c| c == b'\r' || c == b'\n');
                let _ = tx.send(InputEvent::RawBytes(bytes));
                continue;
            }

            // "@ai" + 区切り文字チェック
            if prefix_buf[..3] == *target {
                let delim = prefix_buf[3];
                if delim == b' ' {
                    print!("@ai ");
                    io::stdout().flush().ok();
                    ai_mode = true;
                    ai_prompt.clear();
                    prefix_buf.clear();
                    at_line_start = false;
                    continue;
                }
                if delim == b'\r' || delim == b'\n' {
                    print!("@ai\n");
                    io::stdout().flush().ok();
                    prefix_buf.clear();
                    at_line_start = true;
                    // 空のAIプロンプト
                    let _ = tx.send(InputEvent::AiPrompt(String::new()));
                    continue;
                }
            }

            // 一致しない → バッファをPTYに転送
            let bytes = std::mem::take(&mut prefix_buf);
            at_line_start =
                bytes.last().map_or(false, |&c| c == b'\r' || c == b'\n');
            let _ = tx.send(InputEvent::RawBytes(bytes));
        } else {
            // パススルー: そのままPTYへ転送
            if b == b'\r' || b == b'\n' {
                at_line_start = true;
            }
            let _ = tx.send(InputEvent::RawBytes(vec![b]));
        }
    }
}

/// stdinから1バイト読む (pendingがあればそちらを優先)
fn read_one_byte(stdin: &mut io::Stdin, pending: &mut Option<u8>) -> Option<u8> {
    if let Some(b) = pending.take() {
        return Some(b);
    }
    let mut buf = [0u8; 1];
    match stdin.read(&mut buf) {
        Ok(n) if n >= 1 => Some(buf[0]),
        _ => None,
    }
}

/// AIプロンプト入力中の1バイト処理
fn handle_ai_byte(
    b: u8,
    ai_prompt: &mut String,
    ai_mode: &mut bool,
    at_line_start: &mut bool,
    tx: &mpsc::Sender<InputEvent>,
    utf8_buf: &mut Vec<u8>,
) {
    match b {
        b'\r' => {
            // Enter: AIプロンプト送信
            print!("\n");
            io::stdout().flush().ok();
            let prompt = std::mem::take(ai_prompt);
            *ai_mode = false;
            *at_line_start = true;
            utf8_buf.clear();
            let _ = tx.send(InputEvent::AiPrompt(prompt));
        }
        0x7f | 0x08 => {
            // Backspace: AIプロンプトの文字のみ削除 (プロンプトより前には戻らない)
            if let Some(removed) = ai_prompt.pop() {
                let width = removed.width().unwrap_or(1);
                for _ in 0..width {
                    print!("\x08 \x08");
                }
                io::stdout().flush().ok();
            }
        }
        0x03 | 0x1b => {
            // Ctrl+C / ESC: AIモードをキャンセル
            print!("\n");
            io::stdout().flush().ok();
            ai_prompt.clear();
            *ai_mode = false;
            *at_line_start = true;
            utf8_buf.clear();
        }
        _ => {
            // UTF-8マルチバイト対応
            utf8_buf.push(b);
            if let Ok(s) = std::str::from_utf8(utf8_buf) {
                ai_prompt.push_str(s);
                print!("{}", s);
                io::stdout().flush().ok();
                utf8_buf.clear();
            } else if utf8_buf.len() >= 4 {
                // 不正なUTF-8 → 破棄
                utf8_buf.clear();
            }
        }
    }
}

/// rawモードでの行入力 (リモート終了モード用)
fn read_line_raw(stdin: &mut io::Stdin) -> Option<String> {
    let mut line = String::new();
    let mut buf = [0u8; 1];
    let mut utf8_buf: Vec<u8> = Vec::new();

    loop {
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) if n >= 1 => match buf[0] {
                b'\r' | b'\n' => {
                    print!("\n");
                    io::stdout().flush().ok();
                    return Some(line);
                }
                0x7f | 0x08 => {
                    // Backspace: 入力文字のみ削除
                    if let Some(removed) = line.pop() {
                        let width = removed.width().unwrap_or(1);
                        for _ in 0..width {
                            print!("\x08 \x08");
                        }
                        io::stdout().flush().ok();
                    }
                }
                b => {
                    utf8_buf.push(b);
                    if let Ok(s) = std::str::from_utf8(&utf8_buf) {
                        line.push_str(s);
                        print!("{}", s);
                        io::stdout().flush().ok();
                        utf8_buf.clear();
                    } else if utf8_buf.len() >= 4 {
                        utf8_buf.clear();
                    }
                }
            },
            Ok(_) => return None,
        }
    }
}
