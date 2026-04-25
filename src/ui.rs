use crate::config::DisplayConfig;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
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

static MINIBUFFER_ACTIVE: AtomicBool = AtomicBool::new(false);
static SIGWINCH_RECEIVED: AtomicBool = AtomicBool::new(false);
static TERM_ROWS: AtomicU16 = AtomicU16::new(24);
static STATUS_BAR_LABEL: OnceLock<String> = OnceLock::new();
static STATUS_BAR_COLOR: OnceLock<String> = OnceLock::new();

pub fn minibuffer_active() -> bool {
    MINIBUFFER_ACTIVE.load(Ordering::Relaxed)
}

/// stdin から利用可能なバイトをノンブロッキングで取得する。
/// AI 提案コマンドの完了待ち中に、ユーザのキー入力 / Ctrl+C / パスワード入力等を
/// PTY に転送するために使う。`BufReader` をバイパスして fd 0 を直接読む。
#[cfg(unix)]
pub fn drain_stdin_nonblocking() -> Vec<u8> {
    use std::os::unix::io::{AsRawFd, FromRawFd};
    let fd = io::stdin().as_raw_fd();
    let mut stdin = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(0) });
    let mut out = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
        if ret <= 0 || (pfd.revents & libc::POLLIN) == 0 {
            break;
        }
        match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    out
}

#[cfg(not(unix))]
pub fn drain_stdin_nonblocking() -> Vec<u8> {
    Vec::new()
}

pub fn record_sigwinch() {
    SIGWINCH_RECEIVED.store(true, Ordering::Relaxed);
}

pub fn check_and_clear_sigwinch() -> bool {
    SIGWINCH_RECEIVED.swap(false, Ordering::Relaxed)
}

/// 初回: 改行でステータスバー用の1行を確保してからスクロール領域を設定し描画する
pub fn setup_status_bar(rows: u16, label: &str, color: &str) {
    TERM_ROWS.store(rows, Ordering::Relaxed);
    let _ = STATUS_BAR_LABEL.set(label.to_string());
    let _ = STATUS_BAR_COLOR.set(color.to_string());
    let mut stdout = io::stdout();
    let scroll_bottom = rows.saturating_sub(1).max(1);
    let _ = write!(stdout,
        "\n\x1b[1;{}r\x1b[{};1H{}{}\x1b[K\x1b[0m\x1b[{};1H",
        scroll_bottom, rows, color, label, scroll_bottom
    );
    let _ = stdout.flush();
}

/// リサイズ時: スクロール領域を再設定しステータスバーを再描画する（改行なし）。
/// シェル側のカーソル位置を壊さないよう save/restore で囲む。
pub fn resize_status_bar(rows: u16) {
    TERM_ROWS.store(rows, Ordering::Relaxed);
    let label = STATUS_BAR_LABEL.get().map(|s| s.as_str()).unwrap_or("");
    let color = STATUS_BAR_COLOR.get().map(|s| s.as_str()).unwrap_or("");
    let mut stdout = io::stdout();
    let scroll_bottom = rows.saturating_sub(1).max(1);
    let _ = write!(stdout,
        "\x1b7\x1b[1;{}r\x1b[{};1H{}{}\x1b[K\x1b[0m\x1b8",
        scroll_bottom, rows, color, label
    );
    let _ = stdout.flush();
}

/// ステータスバーを再描画する（ラベル・色はstaticから取得、カーソル移動のみ）
fn redraw_status_bar(stdout: &mut io::Stdout) {
    let rows = TERM_ROWS.load(Ordering::Relaxed);
    let label = STATUS_BAR_LABEL.get().map(|s| s.as_str()).unwrap_or("");
    let color = STATUS_BAR_COLOR.get().map(|s| s.as_str()).unwrap_or("");
    let _ = write!(stdout,
        "\x1b[{};1H{}{}\x1b[K\x1b[0m",
        rows, color, label
    );
    let _ = stdout.flush();
}

/// ステータスバー内容のみ再描画（DECSTBM変更なし、カーソル位置保全）
#[allow(dead_code)]
pub fn refresh_status_bar() {
    let rows = TERM_ROWS.load(Ordering::Relaxed);
    let label = STATUS_BAR_LABEL.get().map(|s| s.as_str()).unwrap_or("");
    let color = STATUS_BAR_COLOR.get().map(|s| s.as_str()).unwrap_or("");
    let mut stdout = io::stdout();
    let _ = write!(stdout,
        "\x1b7\x1b[{};1H{}{}\x1b[K\x1b[0m\x1b8",
        rows, color, label
    );
    let _ = stdout.flush();
}

/// スクロール領域をリセットしステータスバーをクリアする
pub fn cleanup_status_bar(rows: u16) {
    let mut stdout = io::stdout();
    // スクロール領域リセット→ステータスバー行クリア
    let _ = write!(stdout, "\x1b[r\x1b[{};1H\x1b[2K", rows);
    let _ = stdout.flush();
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
                let rows = TERM_ROWS.load(Ordering::Relaxed);
                let _ = write!(stdout,
                    "\x1b7\x1b[{};1H{}{} {}\x1b[0m\x1b[K\x1b8",
                    rows, color, SPINNER_FRAMES[i], message
                );
                let _ = stdout.flush();
                i = (i + 1) % SPINNER_FRAMES.len();
                std::thread::sleep(Duration::from_millis(80));
            }
            // ステータスバーを元に戻す（カーソル位置を保全）
            let _ = write!(stdout, "\x1b7");
            redraw_status_bar(&mut stdout);
            let _ = write!(stdout, "\x1b8");
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

pub fn print_single_confirm_prompt(
    cmd: &str,
    index: usize,
    total: usize,
    display: &DisplayConfig,
) {
    let color = &display.confirm_color;
    print!(
        "{}Execute [{}/{}]: {} (Y/n)\x1b[0m ",
        color, index, total, cmd
    );
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
                // Ctrl-C: 入力をキャンセルして None を返す。aish 自体は終了しない。
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

/// ANSIエスケープを除去して可視幅（表示カラム数）を返す
fn visible_width(s: &str) -> usize {
    let stripped = strip_ansi_escapes::strip(s.as_bytes());
    std::str::from_utf8(&stripped)
        .unwrap_or("")
        .chars()
        .map(char_width)
        .sum()
}

/// 入力を可視行にレイアウトする。
/// 各要素は (start_char, end_char_exclusive, is_first_on_logical_line) を表す。
/// cursor_vline, cursor_vcol はカーソルの可視行インデックスと左端からのカラムオフセット。
fn compute_visual_layout(
    chars: &[char],
    cursor_pos: usize,
    avail_first: usize,
    avail_cont: usize,
) -> (Vec<(usize, usize, bool)>, usize, usize) {
    let mut vlines: Vec<(usize, usize, bool)> = Vec::new();
    let mut cursor_vline = 0usize;
    let mut cursor_vcol = 0usize;
    let mut line_start = 0usize;
    let mut col_used = 0usize;
    let mut is_first = true;

    for i in 0..chars.len() {
        let c = chars[i];
        let avail = if is_first { avail_first } else { avail_cont };

        if c == '\n' {
            if i == cursor_pos {
                cursor_vline = vlines.len();
                cursor_vcol = col_used;
            }
            vlines.push((line_start, i, is_first));
            line_start = i + 1;
            col_used = 0;
            is_first = true;
            continue;
        }

        let w = char_width(c);
        if col_used > 0 && col_used + w > avail {
            vlines.push((line_start, i, is_first));
            line_start = i;
            col_used = 0;
            is_first = false;
        }

        if i == cursor_pos {
            cursor_vline = vlines.len();
            cursor_vcol = col_used;
        }
        col_used += w;
    }

    if cursor_pos >= chars.len() {
        cursor_vline = vlines.len();
        cursor_vcol = col_used;
    }
    vlines.push((line_start, chars.len(), is_first));

    (vlines, cursor_vline, cursor_vcol)
}

/// aishプロンプト（ミニバッファ）を現在の状態で再描画する。
/// 入力長に応じて縦方向に拡張し、DECSTBMを動的に調整する。
/// 戻り値: 新しくミニバッファが占有する行数。
#[cfg(unix)]
fn redraw_minibuffer(
    stdout: &mut io::Stdout,
    term_rows: u16,
    term_cols: u16,
    max_rows: u16,
    label: &str,
    label_width: usize,
    input_bg: &str,
    chars: &[char],
    cursor_pos: usize,
    rows_used: &mut u16,
) {
    let total_cols = term_cols as usize;
    let avail_first = total_cols.saturating_sub(label_width).max(1);
    let indent_width = label_width;
    let avail_cont = total_cols.saturating_sub(indent_width).max(1);

    let (vlines, cvline, cvcol) =
        compute_visual_layout(chars, cursor_pos, avail_first, avail_cont);
    let total_vlines = vlines.len();
    let visible_count = total_vlines.min(max_rows as usize).max(1);

    // 総行数が max_rows を超える場合、カーソル行が見える位置までスクロール
    let scroll_top = if total_vlines > visible_count {
        let min_top = cvline.saturating_sub(visible_count - 1);
        let max_top = total_vlines - visible_count;
        min_top.min(max_top)
    } else {
        0
    };

    let new_rows_used = visible_count as u16;

    // DECSTBM を更新（シュリンクする場合は不要になった行を消去）
    if new_rows_used != *rows_used {
        if new_rows_used < *rows_used {
            let clear_from = term_rows - *rows_used + 1;
            let clear_to = term_rows - new_rows_used;
            for r in clear_from..=clear_to {
                let _ = write!(stdout, "\x1b[{};1H\x1b[2K", r);
            }
        }
        let scroll_bottom = term_rows.saturating_sub(new_rows_used).max(1);
        let _ = write!(stdout, "\x1b[1;{}r", scroll_bottom);
        *rows_used = new_rows_used;
    }

    let start_row = term_rows - new_rows_used + 1;

    for disp in 0..visible_count {
        let vi = scroll_top + disp;
        let row = start_row + disp as u16;
        let (s, e, is_first_line) = vlines[vi];
        let _ = write!(stdout, "\x1b[{};1H\x1b[0m\x1b[2K", row);
        if is_first_line {
            let _ = write!(stdout, "{}", label);
        } else {
            // 継続行はラベル幅ぶん空白でインデント
            for _ in 0..indent_width {
                let _ = stdout.write_all(b" ");
            }
        }
        let _ = write!(stdout, "{}", input_bg);
        let line_str: String = chars[s..e].iter().collect();
        let _ = stdout.write_all(line_str.as_bytes());
        let _ = write!(stdout, "\x1b[K");
    }

    let cursor_display_line = cvline - scroll_top;
    let cursor_row = start_row + cursor_display_line as u16;
    let prefix_w = if vlines[cvline].2 {
        label_width
    } else {
        indent_width
    };
    let cursor_col = prefix_w + cvcol + 1;
    let _ = write!(stdout, "\x1b[{};{}H", cursor_row, cursor_col);
    let _ = stdout.flush();
}

/// aishプロンプト用のマルチラインエディタ。
/// 矢印キー/Home/End/Delete/BSによる編集、履歴ナビゲーション、
/// Alt+Enter / Shift+Enter (CSI u) による改行挿入をサポートする。
/// 入力長に応じて縦方向に拡張し、最大 max_rows 行まで表示する。
/// 戻り値は (入力テキスト, 最終的に占有した行数)。
#[cfg(unix)]
fn read_minibuffer_line(
    stdout: &mut io::Stdout,
    term_rows: u16,
    term_cols: u16,
    max_rows: u16,
    label: &str,
    label_width: usize,
    input_bg: &str,
) -> (Option<String>, u16) {
    use std::os::unix::io::{AsRawFd, FromRawFd};
    let mut stdin = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(0) });
    let mut buf = [0u8; 4];

    let mut chars: Vec<char> = Vec::new();
    let mut cursor_pos: usize = 0;
    let mut rows_used: u16 = 1;

    // 履歴ナビゲーション
    let hist_len = prompt_history().lock().map_or(0, |h| h.len());
    let mut hist_idx: usize = hist_len;
    let mut saved_input: Vec<char> = Vec::new();

    redraw_minibuffer(
        stdout, term_rows, term_cols, max_rows, label, label_width, input_bg,
        &chars, cursor_pos, &mut rows_used,
    );

    loop {
        let n = match stdin.read(&mut buf[..1]) {
            Ok(0) => {
                let text = if chars.is_empty() {
                    None
                } else {
                    Some(chars.iter().collect())
                };
                return (text, rows_used);
            }
            Ok(n) => n,
            Err(_) => return (None, rows_used),
        };
        if n == 0 {
            continue;
        }
        let b = buf[0];

        match b {
            b'\n' | b'\r' => {
                let s: String = chars.iter().collect();
                if s.trim() == "exit" {
                    return (None, rows_used);
                }
                return (Some(s), rows_used);
            }
            0x7f | 0x08 => {
                if cursor_pos > 0 {
                    cursor_pos -= 1;
                    chars.remove(cursor_pos);
                }
            }
            0x1f => return (None, rows_used), // Ctrl+/ でキャンセル
            0x03 => return (None, rows_used), // Ctrl-C
            0x04 => {
                if chars.is_empty() {
                    return (None, rows_used);
                }
                if cursor_pos < chars.len() {
                    chars.remove(cursor_pos);
                }
            }
            0x01 => cursor_pos = 0,
            0x05 => cursor_pos = chars.len(),
            0x02 => {
                if cursor_pos > 0 {
                    cursor_pos -= 1;
                }
            }
            0x06 => {
                if cursor_pos < chars.len() {
                    cursor_pos += 1;
                }
            }
            0x15 => {
                chars.drain(..cursor_pos);
                cursor_pos = 0;
            }
            0x0b => {
                chars.truncate(cursor_pos);
            }
            0x17 => {
                let mut end = cursor_pos;
                while end > 0 && chars[end - 1] == ' ' {
                    end -= 1;
                }
                while end > 0 && chars[end - 1] != ' ' {
                    end -= 1;
                }
                chars.drain(end..cursor_pos);
                cursor_pos = end;
            }
            0x1b => {
                let fd = (&*stdin).as_raw_fd();
                let mut pollfd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut pollfd, 1, 50) };
                if ready <= 0 {
                    return (None, rows_used); // 単独ESCでキャンセル
                }
                let mut first = [0u8; 1];
                if stdin.read(&mut first).is_err() {
                    continue;
                }
                match first[0] {
                    b'\r' | b'\n' => {
                        // Alt+Enter: 改行を挿入
                        chars.insert(cursor_pos, '\n');
                        cursor_pos += 1;
                    }
                    b'[' | b'O' => {
                        // CSI/SS3 パラメータと終端を読む
                        let mut params: Vec<u8> = Vec::new();
                        let mut final_byte: u8 = 0;
                        loop {
                            let mut c = [0u8; 1];
                            match stdin.read(&mut c) {
                                Ok(1) => {
                                    if c[0] >= 0x40 && c[0] <= 0x7E {
                                        final_byte = c[0];
                                        break;
                                    }
                                    params.push(c[0]);
                                }
                                _ => break,
                            }
                        }
                        // Shift+Enter / modifier+Enter の CSI u 形式: ESC [ 13 ; N u
                        // 修飾キーありに限定するため "13;" で始まるものだけマッチ
                        // (プレーンEnterが \x1b[13u で届いた場合は else 側へ流す)
                        if final_byte == b'u' && params.starts_with(b"13;") {
                            chars.insert(cursor_pos, '\n');
                            cursor_pos += 1;
                        } else {
                            match (params.as_slice(), final_byte) {
                                (b"", b'D') => {
                                    if cursor_pos > 0 {
                                        cursor_pos -= 1;
                                    }
                                }
                                (b"", b'C') => {
                                    if cursor_pos < chars.len() {
                                        cursor_pos += 1;
                                    }
                                }
                                (b"", b'H') | (b"1", b'~') | (b"7", b'~') => cursor_pos = 0,
                                (b"", b'F') | (b"4", b'~') | (b"8", b'~') => {
                                    cursor_pos = chars.len()
                                }
                                (b"3", b'~') => {
                                    if cursor_pos < chars.len() {
                                        chars.remove(cursor_pos);
                                    }
                                }
                                (b"", b'A') | (b"", b'B') => {
                                    if let Ok(history) = prompt_history().lock() {
                                        let new_idx = if final_byte == b'A' {
                                            if hist_idx > 0 {
                                                hist_idx - 1
                                            } else {
                                                hist_idx
                                            }
                                        } else if hist_idx < hist_len {
                                            hist_idx + 1
                                        } else {
                                            hist_idx
                                        };
                                        if new_idx != hist_idx {
                                            if hist_idx == hist_len {
                                                saved_input = chars.clone();
                                            }
                                            hist_idx = new_idx;
                                            chars = if hist_idx < hist_len {
                                                history[hist_idx].chars().collect()
                                            } else {
                                                saved_input.clone()
                                            };
                                            cursor_pos = chars.len();
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {} // 未知のエスケープは無視
                }
            }
            _ if b < 0x20 => {}
            _ => {
                let byte_len = utf8_char_len(b);
                buf[0] = b;
                let mut read_so_far = 1;
                while read_so_far < byte_len {
                    match stdin.read(&mut buf[read_so_far..byte_len]) {
                        Ok(n) if n > 0 => read_so_far += n,
                        _ => break,
                    }
                }
                if read_so_far == byte_len {
                    if let Ok(s) = std::str::from_utf8(&buf[..byte_len]) {
                        for c in s.chars() {
                            chars.insert(cursor_pos, c);
                            cursor_pos += 1;
                        }
                    }
                }
            }
        }

        redraw_minibuffer(
            stdout, term_rows, term_cols, max_rows, label, label_width, input_bg,
            &chars, cursor_pos, &mut rows_used,
        );
    }
}

/// aishプロンプトをステータスバー行に表示し、ユーザ入力を受け付ける。
/// 入力確定後にステータスバーを復元し、InputEventを送信する。
/// 入力が長いとき縦方向に拡張し、終了時に DECSTBM を元に戻す。
#[cfg(unix)]
fn show_minibuffer(
    stdout: &mut io::Stdout,
    tx: &Sender<InputEvent>,
    input_bg: &str,
    aish_label: &str,
    cancel_shell: bool,
) {
    MINIBUFFER_ACTIVE.store(true, Ordering::Relaxed);
    let rows = TERM_ROWS.load(Ordering::Relaxed);
    let (_, cols) = terminal_size();
    let label_width = visible_width(aish_label);
    // 最大ミニバッファ行数: 端末高さの半分、かつ1以上
    let max_rows = (rows / 2).max(1);

    // カーソル保存
    let _ = write!(stdout, "\x1b7");
    let _ = stdout.flush();

    let (result, rows_used) = read_minibuffer_line(
        stdout, rows, cols, max_rows, aish_label, label_width, input_bg,
    );

    // DECSTBM を元に戻す (1..rows-1)、ミニバッファが使用した追加行をクリア
    let scroll_bottom = rows.saturating_sub(1).max(1);
    let _ = write!(stdout, "\x1b[0m\x1b[1;{}r", scroll_bottom);
    if rows_used > 1 {
        let clear_from = rows - rows_used + 1;
        let clear_to = rows - 1;
        for r in clear_from..=clear_to {
            let _ = write!(stdout, "\x1b[{};1H\x1b[2K", r);
        }
    }
    // ステータスバーを復元→カーソル復元
    redraw_status_bar(stdout);
    let _ = write!(stdout, "\x1b8");
    let _ = stdout.flush();
    MINIBUFFER_ACTIVE.store(false, Ordering::Relaxed);

    match result {
        Some(text) if text.trim().is_empty() => {
            // 空Enter → 何もしない（ステータスバーは復元済み）
            if cancel_shell {
                let _ = tx.send(InputEvent::PtyData(vec![0x03]));
            }
        }
        Some(text) => {
            // 履歴に追加（重複は追加しない）
            if let Ok(mut history) = prompt_history().lock() {
                if history.last().map_or(true, |last| last != &text) {
                    history.push(text.clone());
                }
            }
            // スクロールエリアにプロンプト内容をエコー表示
            // 複数行入力は各論理行の先頭に [aish] ラベルを付ける
            let _ = write!(stdout, "\n");
            for (i, line) in text.split('\n').enumerate() {
                if i > 0 {
                    let _ = write!(stdout, "\n");
                }
                let _ = write!(stdout, "{}{}\x1b[K\x1b[0m", aish_label, line);
            }
            let _ = write!(stdout, "\n");
            let _ = stdout.flush();
            if cancel_shell {
                let _ = tx.send(InputEvent::PtyData(vec![0x03]));
            }
            let _ = tx.send(InputEvent::AiPrompt(text));
        }
        None => {
            // キャンセル (ESC/Ctrl+C/Ctrl+//exit) — ステータスバーは復元済み
            if cancel_shell {
                let _ = tx.send(InputEvent::PtyData(vec![0x03]));
            }
        }
    }
}

/// パススルーモードのrawキー入力処理。
/// Ctrl+/ でaishプロンプトを開き、それ以外はPTYに直送する。
#[cfg(unix)]
fn passthrough_read_raw(tx: &Sender<InputEvent>, input_bg: &str, aish_label: &str) {
    use std::os::unix::io::{AsRawFd, FromRawFd};
    // io::stdin()はBufReaderを内包しており、poll()と併用するとデータ喪失する。
    // ManuallyDropでfd 0を直接読み取り、BufReaderをバイパスする。
    let mut stdin = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(0) });
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
                // ESC: 後続バイトをpollで時間制限つきに読み取る。
                // 単独ESCのときは50ms待って後続が無ければESC単体としてPTYに転送する。
                let fd = (&*stdin).as_raw_fd();
                let mut pollfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
                let ready = unsafe { libc::poll(&mut pollfd, 1, 50) };
                let mut seq_bytes = vec![0x1b_u8];
                if ready > 0 {
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
