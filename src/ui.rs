use crate::ai::BackendKind;
use crate::config::DisplayConfig;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use unicode_width::UnicodeWidthChar;

// 低レベル入力の framing は crate::input に集約。confirm / passthrough / minibuffer は
// next_event を消費する薄い層になる (fd 0 の直接読みはここから無くなる)。
#[cfg(unix)]
use crate::input::{self, Fd0Source, Tok};

static PROMPT_HISTORY: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn prompt_history() -> &'static Mutex<Vec<String>> {
    PROMPT_HISTORY.get_or_init(|| Mutex::new(Vec::new()))
}

pub enum InputEvent {
    PtyData(Vec<u8>),
    /// パススルーモードで Enter 確定された行 (Windows fallback 専用)。
    /// Unix では `passthrough_read_raw` が PtyData / AiPrompt を直接 emit するので使われない。
    #[allow(dead_code)]
    Line(String),
    AiPrompt(String),
    PassthroughEnded,
    /// `ReadLine` 中にユーザが Ctrl+C を押した (もしくは EOF)。
    /// 確認プロンプト中なら「残りコマンドを全部キャンセル」を意味する。
    ReadLineCancelled,
    /// `ReadConfirmKey` でユーザが Yes/No/All のいずれかを 1 キーで選んだ。
    Confirm(ConfirmChoice),
}

static MINIBUFFER_ACTIVE: AtomicBool = AtomicBool::new(false);
static SIGWINCH_RECEIVED: AtomicBool = AtomicBool::new(false);

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

/// ターミナルの "aish 動作中" 表示を OSC で設定する。
/// - OSC 0/1/2: アイコン名 / ウィンドウタイトル
/// - OSC 10/11/12: 前景 / 背景 / カーソル色 (色指定が空でない場合のみ送る)
///
/// PTY のコンテンツ領域には一切干渉しないため、fullscreen アプリやスクロール領域と衝突しない。
pub fn setup_terminal_indicator(title: &str, fg_color: &str, bg_color: &str, cursor_color: &str) {
    let mut stdout = io::stdout();
    // OSC 0: icon name + window title (両方をまとめてセット)
    let _ = write!(stdout, "\x1b]0;{title}\x07");
    // OSC 1: icon name (タブ名のみのターミナル向け)
    let _ = write!(stdout, "\x1b]1;{title}\x07");
    // OSC 2: window title (タイトルバーのみのターミナル向け)
    let _ = write!(stdout, "\x1b]2;{title}\x07");
    if !fg_color.is_empty() {
        let _ = write!(stdout, "\x1b]10;{fg_color}\x07");
    }
    if !bg_color.is_empty() {
        let _ = write!(stdout, "\x1b]11;{bg_color}\x07");
    }
    if !cursor_color.is_empty() {
        let _ = write!(stdout, "\x1b]12;{cursor_color}\x07");
    }
    let _ = stdout.flush();
}

/// 起動時に OSC 0/1/2 で設定したタイトルと OSC 10/11/12 で設定した色をリセットする。
/// OSC 110/111/112 はそれぞれ前景/背景/カーソル色をターミナルのデフォルトに戻す。
pub fn cleanup_terminal_indicator() {
    let mut stdout = io::stdout();
    // タイトルを空文字でクリア (シェル側のプロンプトが上書きする想定)
    let _ = write!(stdout, "\x1b]0;\x07");
    let _ = write!(stdout, "\x1b]1;\x07");
    let _ = write!(stdout, "\x1b]2;\x07");
    // 色をデフォルトに戻す
    let _ = write!(stdout, "\x1b]110\x07");
    let _ = write!(stdout, "\x1b]111\x07");
    let _ = write!(stdout, "\x1b]112\x07");
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
    /// Y/n/a 確認プロンプト用。1 キー (Enter 不要) で確定する。
    /// IME 経由の全角・ひらがな確定文字 (`ｙ` / `ｎ` / `あ` 等) も受理する。
    ReadConfirmKey,
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
        // セッション全体でrawモードを維持する。
        // cfmakeraw 相当: 端末→aish への入力を完全に raw 化する。
        // 特に ICRNL を落とさないと CR が NL に変換されて PTY に届き、
        // prompt_toolkit 系の選択ピッカー (`<c-m>` のみを Enter にバインド) で
        // Enter が効かなくなる (aws configure sso のアカウント選択画面で再現)。
        // OPOST (c_oflag) は触らない: aish 自身が writeln!(stdout) で `\n` だけ
        // 書く箇所があり、端末側の NL→CRLF 変換に依存しているため。
        let mut raw = t;
        raw.c_iflag &= !(libc::IGNBRK
            | libc::BRKINT
            | libc::PARMRK
            | libc::ISTRIP
            | libc::INLCR
            | libc::IGNCR
            | libc::ICRNL
            | libc::IXON);
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
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
    format!("{color}\x1b[K")
}

/// AI バックエンド種別ごとの識別色 (256-color)。未知名は orange にフォールバック。
/// Generic backend は `name = "generic:<provider-name>"` 形式で渡される想定で、
/// 表からは引かず、呼び出し側 (`backend_color_for_kind`) が
/// `BackendKind::generic_meta()` を直接見て recipe.color を取り出す。
fn backend_color_code(name: &str) -> u8 {
    match name {
        "claude" => 208, // orange (Anthropic 寄り)
        "codex" => 39,   // cyan-blue
        "gemini" => 135, // purple
        "qwen" => 198,   // pink-magenta
        "cursor" => 220, // amber/gold (Cursor brand 寄り)
        "copilot" => 41, // emerald green (GitHub 寄り)
        _ => 208,
    }
}

/// `BackendKind` から 256-color コードを引く。Generic は recipe.color を返す。
fn backend_color_for_kind(kind: BackendKind) -> u8 {
    if let Some(meta) = kind.generic_meta() {
        return meta.recipe.color;
    }
    backend_color_code(kind.as_str())
}

/// 起動時のバナーを表示する。
/// ロゴ (aish ASCII アート) は Sunset 配色 (黄→橙→赤橙→マゼンタ) の truecolor。
/// 続く status 行でバージョン + バックエンド名 (バックエンド別色) + (あれば) モデル名 +
/// (あれば) effort + キーヒント。
pub fn print_startup_banner(
    kind: BackendKind,
    model: Option<&str>,
    effort: Option<&str>,
    version: &str,
) {
    // Sunset gradient: A=yellow-orange / I=orange / S=red-orange / H=magenta
    let c_a = "\x1b[38;2;255;200;40m";
    let c_i = "\x1b[38;2;255;140;0m";
    let c_s = "\x1b[38;2;255;80;40m";
    let c_h = "\x1b[38;2;220;40;100m";
    let backend_col = backend_color_for_kind(kind);
    let backend_color = format!("\x1b[1;38;5;{backend_col}m");
    let backend_name = kind.as_str();
    let dim = "\x1b[38;5;245m";
    let model_color = "\x1b[38;5;250m";
    let reset = "\x1b[0m";

    println!("  {c_a}▄▀█{reset} {c_i}█{reset} {c_s}█▀{reset} {c_h}█░█{reset}  ");
    print!("  {c_a}█▀█{reset} {c_i}█{reset} {c_s}▄█{reset} {c_h}█▀█{reset}  ");
    print!("  {dim}v{version} · {reset}{backend_color}{backend_name}{reset}");
    if let Some(m) = model {
        print!(" {dim}·{reset} {model_color}{m}{reset}");
    }
    if let Some(e) = effort {
        print!(" {dim}·{reset} {model_color}{e}{reset}");
    }
    println!(" {dim}· (Ctrl+/){reset}");
    let _ = io::stdout().flush();
}

const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub struct Spinner {
    running: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Spinner {
    pub fn start(
        display: &DisplayConfig,
        kind: BackendKind,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();
        let color = build_color_start(&display.thinking_color);
        // 思考中バックエンド情報を `[backend model effort] ` プレフィックスとして付ける。
        // backend 名のみ backend 色 (起動バナーと統一)、続けて `\x1b[0m` でリセットしてから
        // thinking 色 (`color`) を再付与し、model/effort/メッセージを thinking 色に戻す。
        // model/effort が None の欄はスペースごと省略する。
        let backend_seq = format!("\x1b[1;38;5;{}m", backend_color_for_kind(kind));
        let mut prefix = format!("[{backend_seq}{}\x1b[0m{color}", kind.as_str());
        if let Some(m) = model {
            prefix.push(' ');
            prefix.push_str(m);
        }
        if let Some(e) = effort {
            prefix.push(' ');
            prefix.push_str(e);
        }
        prefix.push_str("] ");
        let message = format!("{prefix}{}", display.thinking_message);

        let handle = std::thread::spawn(move || {
            let mut stdout = io::stdout();
            let mut i = 0;
            // 現在カーソルがある行に \r で戻りながらフレームを更新する。
            // Spinner は AI 思考中に呼ばれ、直前は minibuffer のエコー出力後の
            // 新しい行頭にカーソルがあるので、その行をスピナー専用に使う。
            while running_clone.load(Ordering::Relaxed) {
                let _ = write!(
                    stdout,
                    "\r{}{} {}\x1b[0m\x1b[K",
                    color, SPINNER_FRAMES[i], message
                );
                let _ = stdout.flush();
                i = (i + 1) % SPINNER_FRAMES.len();
                std::thread::sleep(Duration::from_millis(80));
            }
            // 終了時は行をクリアして次の出力 (AI 応答) が同じ行に出るようにする
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

/// AI 由来の 1 論理行 (改行を含まない前提) 内の制御文字を caret 記法で可視化する。
/// C0 (0x00-0x1f) → `^X` (X = c + 0x40)、DEL (0x7f) → `^?`、その他 `char::is_control()`
/// (C1 等) も `^?` に潰す。印字可能文字はそのまま。ESC は `^[`、CR は `^M`、TAB は `^I`。
///
/// 目的: AI が返す message / commands を端末に出すとき、`\r` や `\x1b[2K` 等で確認画面の
/// 見た目を送信バイトとズラす偽装 (= ユーザが見て承認した物 ≠ 実際に送る物) を防ぐ。
/// `\n` は呼び出し側が `.lines()` で分割済みなので通常ここには来ないが、来ても `^J` に可視化される。
fn visualize_control_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            match c {
                '\x7f' => out.push_str("^?"),
                c if (c as u32) < 0x20 => {
                    out.push('^');
                    // 0x00..=0x1f → '@'(0x40)..='_'(0x5f)
                    out.push((b'@' + c as u8) as char);
                }
                // C1 等の非 ASCII 制御文字はまとめて `^?` に潰す (稀)。
                _ => out.push_str("^?"),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// シェルへ送る 1 コマンドとして不正な制御文字 (改行/CR/ESC/NUL/TAB/その他 C0/DEL/C1) を含むか。
/// 正当な単一行シェルコマンドに制御文字は不要なので、含むものは承認 UI に載せず実行も拒否する
/// (`src/main.rs` の実行ループ先頭ガード)。これにより `pty.write` には制御文字フリーの
/// コマンドだけが到達し、「画面で承認した物 = サーバで実行される物」が保たれる。
pub fn command_has_control_chars(cmd: &str) -> bool {
    cmd.chars().any(|c| c.is_control())
}

/// 制御文字を含むため実行を拒否したコマンドを、可視化したうえで理由付きで表示する。
pub fn print_rejected_command(cmd: &str, display: &DisplayConfig) {
    let color = build_color_start(&display.confirm_color);
    let safe = visualize_control_line(cmd);
    println!("\n{color}制御文字を含むため実行しません: {safe}\x1b[K\x1b[0m");
    io::stdout().flush().ok();
}

pub fn print_ai_message(message: &str, kind: BackendKind, display: &DisplayConfig) {
    let color = build_color_start(&display.ai_color);
    let label = format!("[ai/{}]> ", kind.as_str());
    let mut first = true;
    for line in message.lines() {
        // AI 出力は未信頼。行内の制御文字を可視化してから描画する。
        let line = visualize_control_line(line);
        if first {
            println!("{color}{label}{line}\x1b[K\x1b[0m");
            first = false;
        } else {
            println!("{color}{line}\x1b[K\x1b[0m");
        }
    }
    if first {
        // message が空でも ラベルだけは出す。
        println!("{color}{label}\x1b[K\x1b[0m");
    }
    io::stdout().flush().ok();
}

/// slash command (/effort, /model, /ai 等) の処理結果を表示する。
/// AI 応答とは色を変えて識別しやすくする (dim gray)。
pub fn print_slash_result(message: &str) {
    for line in message.lines() {
        println!("\x1b[38;5;245m{line}\x1b[K\x1b[0m");
    }
    io::stdout().flush().ok();
}

pub fn print_ai_commands(commands: &[String], display: &DisplayConfig) {
    if commands.is_empty() {
        return;
    }
    let color = build_color_start(&display.ai_color);
    println!("{color}Proposed commands:\x1b[K\x1b[0m");
    for (i, cmd) in commands.iter().enumerate() {
        // AI 提案コマンドは未信頼。制御文字を可視化してから描画する。
        let cmd = visualize_control_line(cmd);
        println!("{}  {}: {}\x1b[K\x1b[0m", color, i + 1, cmd);
    }
    io::stdout().flush().ok();
}

pub fn print_single_confirm_prompt(cmd: &str, index: usize, total: usize, display: &DisplayConfig) {
    let color = &display.confirm_color;
    // 残コマンドがある (= 最後ではない) ときだけ [Y/n/a] を出す。
    // a = 残り全部を自動承認 (apt / sudo の慣習)。
    let options = if index < total { "Y/n/a" } else { "Y/n" };
    // "Exec?" をオレンジ文字+暗い茶色背景 (prompt_color 系) で区別する試行。
    // 終了は再度 confirm_color を適用して元の薄黄/グレーに戻す。
    // 選択肢 [Y/n] / [Y/n/a] は bold + reverse で強調。
    let label_on = "\x1b[38;5;208;48;2;50;35;20m";
    let hl_on = "\x1b[1;7m";
    // 先頭に改行を入れて、確認プロンプトを必ず行頭から開始する。
    // 2つ目以降は直前にシェルプロンプト (`user@host:~$ `) が描画されるため、
    // これを入れないと混ざってしまう。1つ目の前は空行になるが
    // `Proposed commands:` リストとの区切りになり視認性が上がる。
    // cmd 前後のスペースは色を付けないように、各セグメント境界で \x1b[0m リセットする。
    // cmd は AI 由来 (未信頼) なので制御文字を可視化し、確認画面の見た目を送信バイトと一致させる。
    let cmd = visualize_control_line(cmd);
    print!("\n{color}{label_on}Exec?\x1b[0m {color}{cmd}\x1b[0m {color}{hl_on}[{options}]\x1b[0m ");
    io::stdout().flush().ok();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmChoice {
    Yes,
    No,
    All,
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

/// Windows fallback: cooked モードで 1 行読む。passthrough_read の non-unix 経路で使う。
#[cfg(not(unix))]
fn read_line_cooked() -> Option<String> {
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => Some(
            line.trim_end_matches('\n')
                .trim_end_matches('\r')
                .to_string(),
        ),
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

/// Y/n/a 確認プロンプトで「1 キー押下で即確定」する入力読み取り。
///
/// raw mode で 1 byte ずつ読み、UTF-8 マルチバイト文字はデコードして
/// `match_confirm_char` で判定する。IME 経由の全角・ひらがな確定文字も受理する
/// (`ｙ` `ｎ` `あ` 等)。詳細は `match_confirm_char` のテーブルを参照。
///
/// 戻り値:
/// - `Some(ConfirmChoice)`: ユーザが Y/N/A 相当のキーを押した
/// - `None`: Ctrl+C / Ctrl+D (EOF) / ESC 単独 → 「残り全部キャンセル」
///
/// 未知キー / 制御文字は無視して次のキーを待つ (打ち間違いで意図せず No に
/// なるのを避けるため)。raw mode は ECHO off なので、マッチした文字のみ
/// stdout に echo する。
pub fn read_confirm_key() -> Option<ConfirmChoice> {
    #[cfg(unix)]
    {
        read_confirm_key_unix()
    }
    #[cfg(not(unix))]
    {
        read_confirm_key_cooked()
    }
}

/// 押されたキーをそのまま `\n` 付きでターミナルに描画する。
/// raw mode は ECHO off なので、確定したことをユーザに見せるために手動で echo する。
/// `y` / `Y` などは押下の大小をそのまま反映し、`match` で大文字固定にはしない。
fn echo_confirm(c: char) {
    let mut stdout = io::stdout();
    // CLAUDE.md: echo_confirm は match を持たず write!("{c}\x1b[0m\n") だけ (writeln! に置換しない)。
    #[allow(clippy::write_with_newline)]
    let _ = write!(stdout, "{c}\x1b[0m\n");
    let _ = stdout.flush();
}

/// 1 文字を Yes / No / All / なし にマッピングする。
/// ASCII y/Y/n/N/a/A + IME 経由の全角・ひらがな確定文字をサポート。
/// Enter (`\n` / `\r`) と Space はデフォルト Yes 扱い。
fn match_confirm_char(c: char) -> Option<ConfirmChoice> {
    match c {
        // Yes: ASCII / 全角小文字 / 全角大文字 / Enter / Space
        'y' | 'Y' | 'ｙ' | 'Ｙ' | '\n' | '\r' | ' ' => Some(ConfirmChoice::Yes),
        // No: ASCII / 全角小文字 / 全角大文字 / ひらがな「ん」(romaji "n" 確定の自然結果)
        'n' | 'N' | 'ｎ' | 'Ｎ' | 'ん' => Some(ConfirmChoice::No),
        // All: ASCII / 全角小文字 / 全角大文字 / ひらがな「あ」(romaji "a" 確定の自然結果)
        'a' | 'A' | 'ａ' | 'Ａ' | 'あ' => Some(ConfirmChoice::All),
        _ => None,
    }
}

#[cfg(unix)]
fn read_confirm_key_unix() -> Option<ConfirmChoice> {
    // 入力の framing は crate::input に集約済み。ここは Tok を解釈するだけの薄い層。
    // Enter が制御文字フィルタに飲まれる順序トラップは next_event 側で型として解消済み。
    let mut src = Fd0Source::new();
    loop {
        let ev = input::next_event(&mut src);
        match ev.tok {
            Tok::Eof => return None,
            // Ctrl+C / Ctrl+D / 単独 ESC: 残り全部キャンセル。
            // **必ず改行を出してから抜ける**。これをしないと、直後にメインループが
            // 送るシェルプロンプトのリフレッシュ (bash の `\r` + プロンプト文字列;
            // しかも先頭の改行は drain 側で除去される) が、カーソルがまだ
            // `Exec? ... [Y/n/a] ` 行末にあるためその行を上書きして消してしまう
            // (ユーザ報告: キャンセルで最終行がプロンプトに上書きされる)。Y/n や
            // Ctrl+C は元々 echo で改行が入るのでクリーンだったが、ESC だけ
            // 「echo はしない」で改行が無く上書きしていた。ここで揃える。
            Tok::Ctrl(0x03) | Tok::Ctrl(0x04) | Tok::Esc => {
                let mut stdout = io::stdout();
                let _ = stdout.write_all(b"\n");
                let _ = stdout.flush();
                return None;
            }
            // Enter = デフォルト Yes。入力 char が無いのでデフォルト表記の 'Y' を echo。
            Tok::Enter => {
                echo_confirm('Y');
                return Some(ConfirmChoice::Yes);
            }
            // マッチする文字なら確定 (Space も match_confirm_char で Yes 扱い)。
            // 未知文字は無視して再読み取り (打ち間違いで意図せず No になる事故を避ける)。
            Tok::Char(c) => {
                if let Some(choice) = match_confirm_char(c) {
                    echo_confirm(c);
                    return Some(choice);
                }
            }
            // 矢印キー等のシーケンス・修飾キー・その他制御文字は無視して待つ。
            _ => {}
        }
    }
}

#[cfg(not(unix))]
fn read_confirm_key_cooked() -> Option<ConfirmChoice> {
    // Windows fallback: cooked mode で 1 行読み、先頭の有効文字でマッチ。
    // raw 1 キー読みは Windows では仕組みが違うのでここでは Enter 確定を許容する。
    loop {
        let mut line = String::new();
        match io::stdin().read_line(&mut line) {
            Ok(0) => return None,
            Ok(_) => {}
            Err(_) => return None,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Some(ConfirmChoice::Yes);
        }
        if let Some(c) = trimmed.chars().next() {
            if let Some(choice) = match_confirm_char(c) {
                return Some(choice);
            }
        }
        // 未知入力は再読み取り
    }
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
// i は chars の index 兼 cursor 位置比較 (i == cursor_pos) / vlines 記録に使うので enumerate には置換しない。
#[allow(clippy::needless_range_loop)]
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

/// DSR (Device Status Report) `\x1b[6n` の応答 `\x1b[{row};{col}R` をパースする。
/// 受信バッファの末尾が `R` で、`[` 以降に `{row};{col}` の形が含まれること。
fn parse_dsr_response(buf: &[u8]) -> Option<(u16, u16)> {
    if !buf.ends_with(b"R") {
        return None;
    }
    let lb = buf.iter().position(|&b| b == b'[')?;
    let body = &buf[lb + 1..buf.len() - 1];
    let s = std::str::from_utf8(body).ok()?;
    let (r, c) = s.split_once(';')?;
    Some((r.trim().parse().ok()?, c.trim().parse().ok()?))
}

/// `\x1b[6n` (DSR) を端末に送り、応答 `\x1b[{row};{col}R` を 80ms 以内に受信して
/// cursor 位置を返す。応答が来ない / パースできない端末では `None`。
/// `passthrough_read_raw` から呼ばれた `show_minibuffer` 専用 (stdin が raw モードで
/// `passthrough_read_raw` 側で握られているが、ここでは別途 fd 0 を `ManuallyDrop` で
/// 借りて非ブロッキングで読み取る)。応答前にユーザがキーを打った場合、その文字は
/// 応答パース用バッファに混入して捨てられる (実用上 80ms 以内にユーザが打つことは稀)。
#[cfg(unix)]
fn query_cursor_position_dsr(stdout: &mut io::Stdout) -> Option<(u16, u16)> {
    use std::os::unix::io::{AsRawFd, FromRawFd};
    let _ = write!(stdout, "\x1b[6n");
    let _ = stdout.flush();

    let mut stdin = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(0) });
    let fd = stdin.as_raw_fd();
    let mut buf: Vec<u8> = Vec::with_capacity(16);
    let start = std::time::Instant::now();
    let timeout = Duration::from_millis(80);

    while start.elapsed() < timeout {
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pollfd, 1, 10) };
        if ready <= 0 {
            continue;
        }
        let mut byte = [0u8; 1];
        match stdin.read(&mut byte) {
            Ok(1) => {
                buf.push(byte[0]);
                if byte[0] == b'R' {
                    return parse_dsr_response(&buf);
                }
                if buf.len() > 32 {
                    return None;
                }
            }
            _ => return None,
        }
    }
    None
}

/// aishプロンプト（ミニバッファ）を現在の状態で再描画する。
/// 入力長に応じて縦方向に拡張し、DECSTBMを動的に調整する。
/// 戻り値: 新しくミニバッファが占有する行数。
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
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
    total_scrolled: &mut u16,
) {
    let total_cols = term_cols as usize;
    let avail_first = total_cols.saturating_sub(label_width).max(1);
    let indent_width = label_width;
    let avail_cont = total_cols.saturating_sub(indent_width).max(1);

    let (vlines, cvline, cvcol) = compute_visual_layout(chars, cursor_pos, avail_first, avail_cont);
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

    // DECSTBM を更新（シュリンク時は不要行を消去、グロー時は bash 出力を scroll 退避）
    if new_rows_used != *rows_used {
        if new_rows_used < *rows_used {
            let clear_from = term_rows - *rows_used + 1;
            let clear_to = term_rows - new_rows_used;
            for r in clear_from..=clear_to {
                let _ = write!(stdout, "\x1b[{r};1H\x1b[2K");
            }
        } else if *rows_used > 0 {
            // grow: 現 DECSTBM の bottom に cursor を置いて \n を delta 個出し、
            // 現 scroll 領域内で bash 出力を上に退避してから minibuffer の伸長を許す。
            // was_at_bottom に関わらず常に scroll する: 画面上半分始まりでも
            // minibuffer は常に最下行起点で伸ばすため、scroll しないと minibuffer
            // 直上の行 (= bash 履歴) を上書きしてしまう。scroll で履歴を上に
            // 退避させて保護する。scroll 量は total_scrolled に積算して終了時の
            // cursor 復元に使う。stdout 専用、PTY には送らない。
            let delta = new_rows_used - *rows_used;
            let current_bottom = term_rows.saturating_sub(*rows_used).max(1);
            let _ = write!(stdout, "\x1b[{current_bottom};1H");
            for _ in 0..delta {
                #[allow(clippy::write_with_newline)]
                let _ = write!(stdout, "\n");
            }
            *total_scrolled = total_scrolled.saturating_add(delta);
        }
        let scroll_bottom = term_rows.saturating_sub(new_rows_used).max(1);
        let _ = write!(stdout, "\x1b[1;{scroll_bottom}r");
        *rows_used = new_rows_used;
    }

    let start_row = term_rows - new_rows_used + 1;

    for disp in 0..visible_count {
        let vi = scroll_top + disp;
        let row = start_row + disp as u16;
        let (s, e, is_first_line) = vlines[vi];
        let _ = write!(stdout, "\x1b[{row};1H\x1b[0m\x1b[2K");
        if is_first_line {
            let _ = write!(stdout, "{label}");
        } else {
            // 継続行はラベル幅ぶん空白でインデント
            for _ in 0..indent_width {
                let _ = stdout.write_all(b" ");
            }
        }
        let _ = write!(stdout, "{input_bg}");
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
    let _ = write!(stdout, "\x1b[{cursor_row};{cursor_col}H");
    let _ = stdout.flush();
}

/// aishプロンプト用のマルチラインエディタ。
/// 矢印キー/Home/End/Delete/BSによる編集、履歴ナビゲーション、
/// Alt+Enter / Shift+Enter (CSI u) による改行挿入をサポートする。
/// 入力長に応じて縦方向に拡張し、最大 max_rows 行まで表示する。
/// 戻り値は (入力テキスト, 最終的に占有した行数)。
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn read_minibuffer_line(
    stdout: &mut io::Stdout,
    term_rows: u16,
    term_cols: u16,
    max_rows: u16,
    label: &str,
    label_width: usize,
    input_bg: &str,
    total_scrolled: &mut u16,
) -> (Option<String>, u16) {
    // 入力の framing は crate::input に集約済み。ここは Tok を編集操作に対応づける薄い層。
    let mut src = Fd0Source::new();

    let mut chars: Vec<char> = Vec::new();
    let mut cursor_pos: usize = 0;
    let mut rows_used: u16 = 1;

    // 履歴ナビゲーション
    let hist_len = prompt_history().lock().map_or(0, |h| h.len());
    let mut hist_idx: usize = hist_len;
    let mut saved_input: Vec<char> = Vec::new();

    // bracketed paste 状態。PasteStart..PasteEnd の間は改行を送信ではなく挿入に回す。
    // prev_was_cr は CRLF (\r\n) を 1 つの \n に正規化するための直前 CR 追跡。
    let mut pasting = false;
    let mut prev_was_cr = false;

    redraw_minibuffer(
        stdout,
        term_rows,
        term_cols,
        max_rows,
        label,
        label_width,
        input_bg,
        &chars,
        cursor_pos,
        &mut rows_used,
        total_scrolled,
    );

    loop {
        let ev = input::next_event(&mut src);

        // bracketed paste 中: 改行は送信せずバッファに挿入 (複数行入力)。ペースト本文中の
        // 制御シーケンスで誤って cancel/submit しないよう、PasteEnd / Enter / Char 以外は無視。
        if pasting {
            match ev.tok {
                Tok::PasteEnd => {
                    pasting = false;
                    prev_was_cr = false;
                }
                Tok::Enter => {
                    // CRLF (\r\n) は 1 つの \n に正規化。\r 直後の \n はスキップする。
                    if ev.raw == [b'\n'] && prev_was_cr {
                        prev_was_cr = false;
                    } else {
                        chars.insert(cursor_pos, '\n');
                        cursor_pos += 1;
                        prev_was_cr = ev.raw == [b'\r'];
                    }
                }
                Tok::Char(c) => {
                    chars.insert(cursor_pos, c);
                    cursor_pos += 1;
                    prev_was_cr = false;
                }
                _ => {
                    prev_was_cr = false;
                }
            }
            redraw_minibuffer(
                stdout,
                term_rows,
                term_cols,
                max_rows,
                label,
                label_width,
                input_bg,
                &chars,
                cursor_pos,
                &mut rows_used,
                total_scrolled,
            );
            continue;
        }

        match ev.tok {
            // EOF: 入力があれば確定、無ければキャンセル
            Tok::Eof => {
                let text = if chars.is_empty() {
                    None
                } else {
                    Some(chars.iter().collect())
                };
                return (text, rows_used);
            }
            // Enter: 確定 ("exit" はキャンセル扱い)
            Tok::Enter => {
                let s: String = chars.iter().collect();
                if s.trim() == "exit" {
                    return (None, rows_used);
                }
                return (Some(s), rows_used);
            }
            // bracketed paste 開始: 以降 PasteEnd まで改行は送信せず挿入扱い (複数行入力)
            Tok::PasteStart => pasting = true,
            // Alt+Enter / 修飾 Enter (CSI u): 改行を挿入
            Tok::AltEnter | Tok::ModEnter => {
                chars.insert(cursor_pos, '\n');
                cursor_pos += 1;
            }
            Tok::Backspace => {
                if cursor_pos > 0 {
                    cursor_pos -= 1;
                    chars.remove(cursor_pos);
                }
            }
            // Ctrl+/ (0x1f) / Ctrl+C (0x03) / 単独 ESC: キャンセル
            Tok::Ctrl(0x1f) | Tok::Ctrl(0x03) | Tok::Esc => return (None, rows_used),
            // Ctrl+D (0x04): 空ならキャンセル、そうでなければ前方削除
            Tok::Ctrl(0x04) => {
                if chars.is_empty() {
                    return (None, rows_used);
                }
                if cursor_pos < chars.len() {
                    chars.remove(cursor_pos);
                }
            }
            Tok::Ctrl(0x01) => cursor_pos = 0, // Ctrl+A: 行頭
            Tok::Ctrl(0x05) => cursor_pos = chars.len(), // Ctrl+E: 行末
            Tok::Ctrl(0x02) => cursor_pos = cursor_pos.saturating_sub(1), // Ctrl+B: 左
            Tok::Ctrl(0x06) => {
                // Ctrl+F: 右
                if cursor_pos < chars.len() {
                    cursor_pos += 1;
                }
            }
            Tok::Ctrl(0x15) => {
                // Ctrl+U: 行頭まで削除
                chars.drain(..cursor_pos);
                cursor_pos = 0;
            }
            Tok::Ctrl(0x0b) => chars.truncate(cursor_pos), // Ctrl+K: 行末まで削除
            Tok::Ctrl(0x17) => {
                // Ctrl+W: 直前の単語を削除
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
            Tok::Left => cursor_pos = cursor_pos.saturating_sub(1),
            Tok::Right => {
                if cursor_pos < chars.len() {
                    cursor_pos += 1;
                }
            }
            Tok::Home => cursor_pos = 0,
            Tok::End => cursor_pos = chars.len(),
            Tok::Delete => {
                if cursor_pos < chars.len() {
                    chars.remove(cursor_pos);
                }
            }
            // 履歴ナビゲーション (Up = 過去へ / Down = 新しい方へ)
            Tok::Up | Tok::Down => {
                let is_up = matches!(ev.tok, Tok::Up);
                if let Ok(history) = prompt_history().lock() {
                    let new_idx = if is_up {
                        hist_idx.saturating_sub(1)
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
            // 通常文字: カーソル位置に挿入
            Tok::Char(c) => {
                chars.insert(cursor_pos, c);
                cursor_pos += 1;
            }
            // その他 (未知のエスケープ・フォーカス・Tab 等の制御文字・生バイト) は無視
            _ => {}
        }

        redraw_minibuffer(
            stdout,
            term_rows,
            term_cols,
            max_rows,
            label,
            label_width,
            input_bg,
            &chars,
            cursor_pos,
            &mut rows_used,
            total_scrolled,
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
) {
    MINIBUFFER_ACTIVE.store(true, Ordering::Relaxed);
    let (rows, cols) = terminal_size();
    let label_width = visible_width(aish_label);
    // 最大ミニバッファ行数: 端末高さの半分、かつ1以上
    let max_rows = (rows / 2).max(1);

    // DSR (\x1b[6n) で現在の cursor 位置を取得して、画面下端かどうかを動的判定する。
    // 応答が来ない端末では was_at_bottom = false の安全側 fallback (= 入口 \n を
    // 出さない / 終了時 cursor を rows 行目に置く)。
    let (saved_row, saved_col) = query_cursor_position_dsr(stdout).unwrap_or((rows, 1));
    let was_at_bottom = saved_row >= rows;

    // 画面下端のときだけ scroll 退避で空き行を確保。stdout 専用 LF: PTY には送らない。
    // 画面上半分のときは何もしない (上端を削らない)。
    // total_scrolled で minibuffer 表示中に積算した scroll 量を追跡し、終了時の
    // cursor 復元に使う (入口 scroll + grow scroll の合計)。
    let mut total_scrolled: u16 = 0;
    if was_at_bottom {
        #[allow(clippy::write_with_newline)]
        let _ = write!(stdout, "\n");
        let _ = stdout.flush();
        total_scrolled = 1;
    }

    let (result, rows_used) = read_minibuffer_line(
        stdout,
        rows,
        cols,
        max_rows,
        aish_label,
        label_width,
        input_bg,
        &mut total_scrolled,
    );

    // DECSTBM をフルリセット (1..rows)、ミニバッファが使用した追加行をクリア
    let _ = write!(stdout, "\x1b[0m\x1b[r");
    if rows_used > 1 {
        let clear_from = rows - rows_used + 1;
        let clear_to = rows;
        for r in clear_from..=clear_to {
            let _ = write!(stdout, "\x1b[{r};1H\x1b[2K");
        }
    } else if rows_used == 1 {
        // 1 行ミニバッファでも最終行に入力跡が残っているのでクリア
        let _ = write!(stdout, "\x1b[{rows};1H\x1b[2K");
    }
    // cursor を絶対座標で復元。total_scrolled は入口 scroll + grow scroll の合計で、
    // bash 入力欄がその行数ぶん上に動いているので保存位置から引く。
    // 画面下端起点 / 上半分起点いずれも同じロジックで扱える。
    let restored_row = saved_row.saturating_sub(total_scrolled).max(1);
    let _ = write!(stdout, "\x1b[{restored_row};{saved_col}H");
    let _ = stdout.flush();
    MINIBUFFER_ACTIVE.store(false, Ordering::Relaxed);

    match result {
        Some(text) if text.trim().is_empty() => {
            // 空 Enter → 何もしない。打ちかけがあれば画面に残す (ユーザが手で消す)。
        }
        Some(text) => {
            // 履歴に追加（重複は追加しない）
            if let Ok(mut history) = prompt_history().lock() {
                if history.last() != Some(&text) {
                    history.push(text.clone());
                }
            }
            // スクロールエリアにプロンプト内容をエコー表示
            // 複数行入力は各論理行の先頭に [aish] ラベルを付ける
            let _ = writeln!(stdout);
            for (i, line) in text.split('\n').enumerate() {
                if i > 0 {
                    let _ = writeln!(stdout);
                }
                let _ = write!(stdout, "{aish_label}{line}\x1b[K\x1b[0m");
            }
            let _ = writeln!(stdout);
            let _ = stdout.flush();
            // bash の打ちかけ入力消去 (Ctrl+A + Ctrl+K) はここでは送らない。
            // ユーザが AI 提案コマンドの実行を承認した直前 (main.rs 側) で初回 1 回
            // だけ送る。これにより「AI に質問はしたが提案を拒否」したケースで
            // bash の打ちかけが温存される。
            let _ = tx.send(InputEvent::AiPrompt(text));
        }
        None => {
            // キャンセル (ESC/Ctrl+C/Ctrl+//exit) → 1 行改行を出して bash プロンプト跡が
            // 同じ行に表示されるのを避ける ([aish] と bash プロンプトの混在防止)。
            // 打ちかけは画面と bash readline に残る。Ctrl+/ を誤って押した後でも
            // shell 入力中のテキストが失われない。改行で cursor は bash 入力欄の
            // 1 行下に移動するが、bash readline 側の cursor 認識はそのままなので、
            // 次の入力 byte は bash 側で受理され echoback で正しい位置 (新しい行) に
            // 描画される。最下行の場合は ONLCR の \n が scroll を引き起こし、bash
            // プロンプト 1 行ぶんが上に退避するだけで内容は失われない。
            let _ = writeln!(stdout);
            let _ = stdout.flush();
        }
    }
}

/// パススルーモードのrawキー入力処理。
/// Ctrl+/ でaishプロンプトを開き、それ以外は **元バイト列をそのまま** PTYに直送する。
/// 入力の framing (ESC/CSI/SS3/UTF-8) は crate::input に集約済み。透明性の根幹として
/// `InEvent.raw` を無加工で転送し、`Char` の再エンコードはしない (invalid UTF-8 /
/// Alt+非ASCII / paste / マウスシーケンスで壊れるため)。フォーカスイベントのみ破棄する。
#[cfg(unix)]
fn passthrough_read_raw(tx: &Sender<InputEvent>, input_bg: &str, aish_label: &str) {
    let mut src = Fd0Source::new();
    let mut stdout = io::stdout();

    loop {
        let ev = input::next_event(&mut src);
        match ev.tok {
            Tok::Eof => return,
            // Ctrl+/ → aishプロンプトを開く
            Tok::Ctrl(0x1f) => {
                show_minibuffer(&mut stdout, tx, input_bg, aish_label);
                return;
            }
            // フォーカスイベント (ESC[I / ESC[O) は破棄 (従来どおり TUI に流さない)
            Tok::FocusIn | Tok::FocusOut => {}
            // それ以外は元バイト列をそのまま PTY へ (Ctrl+C / Enter / 矢印 / 文字すべて raw)
            _ => {
                let _ = tx.send(InputEvent::PtyData(ev.raw));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visualize_control_line_cr() {
        assert_eq!(visualize_control_line("a\rb"), "a^Mb");
    }

    #[test]
    fn visualize_control_line_esc_sequence() {
        // `\x1b[2K` (行クリア) は `^[[2K` として丸見えになり、画面偽装に使えない。
        assert_eq!(visualize_control_line("\x1b[2K"), "^[[2K");
    }

    #[test]
    fn visualize_control_line_nul_tab_del() {
        assert_eq!(visualize_control_line("\0\t\x7f"), "^@^I^?");
    }

    #[test]
    fn visualize_control_line_keeps_printable() {
        assert_eq!(visualize_control_line("ls -la /tmp"), "ls -la /tmp");
        // マルチバイト印字文字は素通し。
        assert_eq!(visualize_control_line("日本語"), "日本語");
    }

    #[test]
    fn command_has_control_chars_clean() {
        assert!(!command_has_control_chars("ls -la"));
        assert!(!command_has_control_chars("echo 'hello world'"));
    }

    #[test]
    fn command_has_control_chars_detects_smuggling() {
        // CR で 2 コマンドに分裂させる古典的偽装。
        assert!(command_has_control_chars("git status\rrm -rf ~"));
        // ESC で行を消して危険部分を隠す偽装。
        assert!(command_has_control_chars("git status\r\x1b[2Krm -rf ~"));
        // TAB / 改行 / NUL も拒否対象。
        assert!(command_has_control_chars("a\tb"));
        assert!(command_has_control_chars("a\nb"));
        assert!(command_has_control_chars("a\0b"));
        assert!(command_has_control_chars("echo \x1b[0m"));
    }

    #[test]
    fn parse_dsr_response_typical() {
        assert_eq!(parse_dsr_response(b"\x1b[24;10R"), Some((24, 10)));
    }

    #[test]
    fn parse_dsr_response_single_digit() {
        assert_eq!(parse_dsr_response(b"\x1b[1;1R"), Some((1, 1)));
    }

    #[test]
    fn parse_dsr_response_large_terminal() {
        assert_eq!(parse_dsr_response(b"\x1b[200;500R"), Some((200, 500)));
    }

    #[test]
    fn parse_dsr_response_with_leading_garbage() {
        // ユーザがキーを打って混入したバイトが前に付くケース。`[` までは捨てて
        // パースしたいが、現実装は最初の `[` を起点にするのでこのケースは通る。
        assert_eq!(parse_dsr_response(b"x\x1b[24;10R"), Some((24, 10)));
    }

    #[test]
    fn parse_dsr_response_missing_terminator() {
        assert_eq!(parse_dsr_response(b"\x1b[24;10"), None);
    }

    #[test]
    fn parse_dsr_response_missing_bracket() {
        assert_eq!(parse_dsr_response(b"24;10R"), None);
    }

    #[test]
    fn parse_dsr_response_non_numeric() {
        assert_eq!(parse_dsr_response(b"\x1b[abc;def R"), None);
    }

    #[test]
    fn parse_dsr_response_empty() {
        assert_eq!(parse_dsr_response(b""), None);
    }

    #[test]
    fn match_confirm_ascii() {
        assert_eq!(match_confirm_char('y'), Some(ConfirmChoice::Yes));
        assert_eq!(match_confirm_char('Y'), Some(ConfirmChoice::Yes));
        assert_eq!(match_confirm_char('n'), Some(ConfirmChoice::No));
        assert_eq!(match_confirm_char('N'), Some(ConfirmChoice::No));
        assert_eq!(match_confirm_char('a'), Some(ConfirmChoice::All));
        assert_eq!(match_confirm_char('A'), Some(ConfirmChoice::All));
    }

    #[test]
    fn match_confirm_enter_and_space_default_yes() {
        assert_eq!(match_confirm_char('\n'), Some(ConfirmChoice::Yes));
        assert_eq!(match_confirm_char('\r'), Some(ConfirmChoice::Yes));
        assert_eq!(match_confirm_char(' '), Some(ConfirmChoice::Yes));
    }

    #[test]
    fn match_confirm_fullwidth_lowercase() {
        // IME 全角小文字 (英数モードや半角→全角変換時の自然な結果)
        assert_eq!(match_confirm_char('ｙ'), Some(ConfirmChoice::Yes));
        assert_eq!(match_confirm_char('ｎ'), Some(ConfirmChoice::No));
        assert_eq!(match_confirm_char('ａ'), Some(ConfirmChoice::All));
    }

    #[test]
    fn match_confirm_fullwidth_uppercase() {
        assert_eq!(match_confirm_char('Ｙ'), Some(ConfirmChoice::Yes));
        assert_eq!(match_confirm_char('Ｎ'), Some(ConfirmChoice::No));
        assert_eq!(match_confirm_char('Ａ'), Some(ConfirmChoice::All));
    }

    #[test]
    fn match_confirm_hiragana_natural_ime() {
        // ひらがなモードで "a" → あ, "n" を確定 → ん となる自然な IME 出力
        assert_eq!(match_confirm_char('あ'), Some(ConfirmChoice::All));
        assert_eq!(match_confirm_char('ん'), Some(ConfirmChoice::No));
    }

    #[test]
    fn match_confirm_unknown_returns_none() {
        assert_eq!(match_confirm_char('x'), None);
        assert_eq!(match_confirm_char('1'), None);
        assert_eq!(match_confirm_char('あ'), Some(ConfirmChoice::All)); // sanity
        assert_eq!(match_confirm_char('い'), None);
        assert_eq!(match_confirm_char('や'), None); // "ya" は受け付けない
        assert_eq!(match_confirm_char('な'), None); // "na" も受け付けない
    }
}
