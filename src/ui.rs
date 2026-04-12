use crate::config::DisplayConfig;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use unicode_width::UnicodeWidthChar;

static PROMPT_HISTORY: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn prompt_history() -> &'static Mutex<Vec<String>> {
    PROMPT_HISTORY.get_or_init(|| Mutex::new(Vec::new()))
}

pub enum InputEvent {
    PtyData(Vec<u8>),
    Line(String),
    AiPrompt(String),
    PassthroughEnded,
    #[allow(dead_code)]
    CtrlCExit,
}

static CTRL_C_COUNT: AtomicU32 = AtomicU32::new(0);
static MINIBUFFER_ACTIVE: AtomicBool = AtomicBool::new(false);
static PASSTHROUGH_EXIT: AtomicBool = AtomicBool::new(false);
static SIGWINCH_RECEIVED: AtomicBool = AtomicBool::new(false);

pub fn record_ctrl_c() {
    CTRL_C_COUNT.fetch_add(1, Ordering::Relaxed);
}

pub fn ctrl_c_count() -> u32 {
    CTRL_C_COUNT.load(Ordering::Relaxed)
}

pub fn minibuffer_active() -> bool {
    MINIBUFFER_ACTIVE.load(Ordering::Relaxed)
}

pub fn request_passthrough_exit() {
    PASSTHROUGH_EXIT.store(true, Ordering::Relaxed);
}

pub fn record_sigwinch() {
    SIGWINCH_RECEIVED.store(true, Ordering::Relaxed);
}

pub fn check_and_clear_sigwinch() -> bool {
    SIGWINCH_RECEIVED.swap(false, Ordering::Relaxed)
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
pub fn build_color_start(color: &str) -> String {
    if color.is_empty() {
        return String::new();
    }
    format!("{}\x1b[K", color)
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
        let color = build_color_start(&display.thinking_color);
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
    let color = build_color_start(&display.ai_color);
    for line in message.lines() {
        print!("{}{}\x1b[K\x1b[0m\n", color, line);
    }
    io::stdout().flush().ok();
}

pub fn print_ai_commands(commands: &[String], display: &DisplayConfig) {
    if commands.is_empty() {
        return;
    }
    let color = build_color_start(&display.ai_color);
    print!("{}Proposed commands:\x1b[K\x1b[0m\n", color);
    for (i, cmd) in commands.iter().enumerate() {
        print!("{}  {}: {}\x1b[K\x1b[0m\n", color, i + 1, cmd);
    }
    io::stdout().flush().ok();
}

pub fn print_confirm_prompt(commands: &[String], display: &DisplayConfig) {
    if commands.is_empty() {
        return;
    }
    print_ai_commands(commands, display);
    let color = &display.confirm_color;
    print!("{}Execute? (Y/n) \x1b[0m ", color);
    io::stdout().flush().ok();
}

pub fn parse_confirm(input: &str) -> bool {
    let trimmed = input.trim();
    trimmed.is_empty() || trimmed.eq_ignore_ascii_case("y") || trimmed.eq_ignore_ascii_case("yes")
}

pub enum UserInput {
    ShellCommand(String),
    Exit,
}

pub fn parse_input(input: &str) -> UserInput {
    let trimmed = input.trim();

    if trimmed.eq_ignore_ascii_case("exit") {
        return UserInput::Exit;
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
    use std::os::unix::io::FromRawFd;
    let mut line = initial;
    let mut stdout = io::stdout();
    // io::stdin()はBufReaderを内包しており、poll()と併用するとデータ喪失する。
    // ManuallyDropでfd 0を直接読み取り、BufReaderをバイパスする。
    let mut stdin = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(0) });
    let mut buf = [0u8; 4];

    // 履歴ナビゲーション用（minibufferのみ）
    let history = prompt_history().lock().ok();
    let hist_len = history.as_ref().map_or(0, |h| h.len());
    drop(history);
    let mut hist_idx: usize = hist_len; // 末尾=新規入力
    let mut saved_input = String::new(); // 新規入力の退避用

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
                if minibuffer && line.trim() == "exit" {
                    return None;
                }
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
            0x1f => {
                // Ctrl+/: aishプロンプト中ならキャンセル
                if minibuffer {
                    return None;
                }
            }
            0x03 => {
                // Ctrl-C
                let _ = stdout.write_all(b"\n");
                let _ = stdout.flush();
                return None;
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
                // ESC: 後続バイトがあればエスケープシーケンス（矢印キー等）、なければ単独ESC
                use std::os::unix::io::AsRawFd;
                let fd = (&*stdin).as_raw_fd();
                let mut pollfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
                let ready = unsafe { libc::poll(&mut pollfd, 1, 50) };
                if ready > 0 {
                    let mut seq = [0u8; 2];
                    let _ = stdin.read(&mut seq);
                    // aishプロンプトで上下キー → 履歴ナビゲーション
                    if minibuffer && seq[0] == b'[' && (seq[1] == b'A' || seq[1] == b'B') {
                        if let Ok(history) = prompt_history().lock() {
                            let new_idx = if seq[1] == b'A' {
                                // Up: 古い方へ
                                if hist_idx > 0 { hist_idx - 1 } else { hist_idx }
                            } else {
                                // Down: 新しい方へ
                                if hist_idx < hist_len { hist_idx + 1 } else { hist_idx }
                            };
                            if new_idx != hist_idx {
                                // 現在の入力が新規入力なら退避
                                if hist_idx == hist_len {
                                    saved_input = line.clone();
                                }
                                hist_idx = new_idx;
                                // 現在行をクリア
                                let old_width: usize = line.chars().map(|c| char_width(c)).sum();
                                for _ in 0..old_width {
                                    let _ = stdout.write_all(b"\x08 \x08");
                                }
                                // 履歴またはsaved_inputで置換
                                line = if hist_idx < hist_len {
                                    history[hist_idx].clone()
                                } else {
                                    saved_input.clone()
                                };
                                let _ = stdout.write_all(line.as_bytes());
                                let _ = stdout.flush();
                            }
                        }
                    }
                    // それ以外のエスケープシーケンスは無視
                } else {
                    // 単独ESC → キャンセル
                    return None;
                }
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

/// パススルーモードで入力を読む。Ctrl+/でaishプロンプトを開く。
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

/// aishプロンプトをターミナル最下行に表示し、ユーザ入力を受け付ける。
/// aishプロンプトを現在のカーソル位置にインライン表示し、ユーザ入力を受け付ける。
/// 入力確定後にInputEventを送信する。
#[cfg(unix)]
fn show_minibuffer(
    stdout: &mut io::Stdout,
    tx: &Sender<InputEvent>,
    input_bg: &str,
    aish_label: &str,
    cancel_shell: bool,
) {
    MINIBUFFER_ACTIVE.store(true, Ordering::Relaxed);

    // 改行してaishプロンプトをインライン表示（\x1b[K で背景色を行末まで伸ばす）
    let _ = write!(stdout, "\r\n{}{}\x1b[K", aish_label, input_bg);
    let _ = stdout.flush();

    let result = read_line_raw_loop_from(String::new(), true);

    // 入力行のリセット
    let _ = write!(stdout, "\x1b[0m");
    let _ = stdout.flush();
    MINIBUFFER_ACTIVE.store(false, Ordering::Relaxed);

    match result {
        Some(text) if text.trim().is_empty() => {
            // 空Enter → 行をクリアしてシェルプロンプト再表示
            let _ = write!(stdout, "\r\x1b[2K");
            let _ = stdout.flush();
            let _ = tx.send(InputEvent::PtyData(vec![0x03]));
        }
        Some(text) => {
            // 履歴に追加（重複は追加しない）
            if let Ok(mut history) = prompt_history().lock() {
                if history.last().map_or(true, |last| last != &text) {
                    history.push(text.clone());
                }
            }
            // AIプロンプト — [aish] プロンプト を描画済みなので改行のみ
            let _ = write!(stdout, "\r\n");
            let _ = stdout.flush();
            if cancel_shell {
                let _ = tx.send(InputEvent::PtyData(vec![0x03]));
            }
            let _ = tx.send(InputEvent::AiPrompt(text));
        }
        None => {
            // キャンセル (ESC/Ctrl+C/Ctrl+//exit) — 行をクリアしてシェルプロンプト再表示
            let _ = write!(stdout, "\r\x1b[2K");
            let _ = stdout.flush();
            let _ = tx.send(InputEvent::PtyData(vec![0x03]));
        }
    }
}

/// パススルーモードのrawキー入力処理。
/// Ctrl+/ でaishプロンプトを開き、それ以外はPTYに直送する。
#[cfg(unix)]
fn passthrough_read_raw(tx: &Sender<InputEvent>, input_bg: &str, aish_label: &str) {
    use std::os::unix::io::AsRawFd;
    let mut stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut buf = [0u8; 1];
    let mut at_line_start = true;
    let fd = stdin.as_raw_fd();
    PASSTHROUGH_EXIT.store(false, Ordering::Relaxed);

    loop {
        // poll()でフラグチェックの機会を作る（100ms間隔）
        let mut pollfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let ready = unsafe { libc::poll(&mut pollfd, 1, 100) };
        if PASSTHROUGH_EXIT.load(Ordering::Relaxed) {
            return;
        }
        if ready <= 0 {
            continue;
        }
        match stdin.read(&mut buf) {
            Ok(0) => return,
            Ok(_) => {}
            Err(_) => return,
        }
        let b = buf[0];

        match b {
            0x1f => {
                // Ctrl+/ → aishプロンプトを開く
                show_minibuffer(&mut stdout, tx, input_bg, aish_label, !at_line_start);
                return;
            }
            0x03 => {
                // Ctrl+C: PTYに送信
                let _ = tx.send(InputEvent::PtyData(vec![b]));
                at_line_start = true;
            }
            b'\r' | b'\n' => {
                let _ = tx.send(InputEvent::PtyData(vec![b]));
                at_line_start = true;
            }
            0x1b => {
                // ESCシーケンスを読み取る
                let mut seq_bytes = vec![0x1b_u8];
                let mut seq = [0u8; 1];
                if stdin.read(&mut seq).is_ok() {
                    seq_bytes.push(seq[0]);
                    if seq[0] == b'[' {
                        // CSIシーケンス: 終端文字(0x40-0x7E)まで読む
                        loop {
                            match stdin.read(&mut seq) {
                                Ok(1) => {
                                    seq_bytes.push(seq[0]);
                                    if seq[0] >= 0x40 && seq[0] <= 0x7E {
                                        break;
                                    }
                                }
                                _ => break,
                            }
                        }
                    }
                }
                // フォーカスイベント(ESC[I, ESC[O)は破棄、それ以外はPTYに転送
                let is_focus = seq_bytes.len() == 3
                    && seq_bytes[1] == b'['
                    && (seq_bytes[2] == b'I' || seq_bytes[2] == b'O');
                if !is_focus {
                    let _ = tx.send(InputEvent::PtyData(seq_bytes));
                    at_line_start = false;
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
