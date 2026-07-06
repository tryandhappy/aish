//! Windows の platform 実装 (Console API + VT モード)。
//!
//! 方針:
//! - stdin: `ENABLE_VIRTUAL_TERMINAL_INPUT` + LINE/ECHO/PROCESSED/QUICK_EDIT off。
//!   これで矢印キー等が Unix と同じ ANSI エスケープシーケンスとして届き、
//!   `input.rs` の byte→Tok framing (golden test 済み) をそのまま共有できる。
//!   PROCESSED off により Ctrl+C は生の 0x03 になり、Unix と同一経路で扱える。
//! - stdout: `ENABLE_VIRTUAL_TERMINAL_PROCESSING` (ANSI 描画)。
//!   **`DISABLE_NEWLINE_AUTO_RETURN` は設定しない** — aish は `writeln!` で `\n` だけ
//!   書く箇所があり、コンソール側の `\n`→CRLF 変換に依存する (Unix の OPOST 不可触と同じ)。
//! - 入力は `ReadConsoleInputW` ベースのポンプ 1 本に集約。`WaitForSingleObject` は
//!   マウス/フォーカス/key-up でもシグナル状態になるため、record を読んでフィルタし、
//!   KEY_EVENT の文字を UTF-8 化して byte キューへ積む。WINDOW_BUFFER_SIZE_EVENT は
//!   リサイズ通知 (`record_resize`) に変換する。
//!
//! サポート対象: Windows Terminal / Windows 10 1809+ の conhost (VT 対応ビルド)。
//! stdin がコンソールでない場合 (パイプ / mintty) は `console_ok()` が起動を拒否する。

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Console::{
    GetConsoleCP, GetConsoleMode, GetConsoleOutputCP, GetConsoleScreenBufferInfo,
    GetNumberOfConsoleInputEvents, GetStdHandle, ReadConsoleInputW, SetConsoleCP, SetConsoleMode,
    SetConsoleOutputCP, CONSOLE_SCREEN_BUFFER_INFO, ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS,
    ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_QUICK_EDIT_MODE,
    ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, ENABLE_WINDOW_INPUT,
    INPUT_RECORD, KEY_EVENT, LEFT_CTRL_PRESSED, RIGHT_CTRL_PRESSED, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE, WINDOW_BUFFER_SIZE_EVENT,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::System::Time::GetTimeZoneInformation;

/// UTF-8 コードページ。
const CP_UTF8: u32 = 65001;
/// GetExitCodeProcess の「実行中」値 (STATUS_PENDING)。
const STILL_ACTIVE: u32 = 259;
/// `/` キーの仮想キーコード (US/JIS 配列)。Ctrl+/ の uChar は端末/経路により
/// 0x1f・0・0x2f と揺れるため、uChar 値に依らず Ctrl+VK_OEM_2 を 0x1f に正規化する用。
const VK_OEM_2: u16 = 0xBF;

fn stdin_handle() -> HANDLE {
    unsafe { GetStdHandle(STD_INPUT_HANDLE) }
}

fn stdout_handle() -> HANDLE {
    unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }
}

/// 起動時のコンソールモード / コードページ (復元用)。
struct OrigConsole {
    stdin_mode: u32,
    stdout_mode: u32,
    input_cp: u32,
    output_cp: u32,
}

static ORIG_CONSOLE: OnceLock<OrigConsole> = OnceLock::new();

/// stdin/stdout がコンソールで、VT モード設定が可能かを確認する。
/// `save_terminal_settings` より前 (run() 冒頭) に 1 度呼ぶ。
pub fn console_ok() -> Result<(), String> {
    let mut mode: u32 = 0;
    if unsafe { GetConsoleMode(stdin_handle(), &mut mode) } == 0 {
        return Err(
            "stdin is not a console. aish requires Windows Terminal or a VT-capable conhost \
             (pipes / mintty are not supported)"
                .to_string(),
        );
    }
    if unsafe { GetConsoleMode(stdout_handle(), &mut mode) } == 0 {
        return Err("stdout is not a console".to_string());
    }
    Ok(())
}

/// 起動時にコンソールモードを保存し、VT raw 相当に設定する。main開始直後に呼ぶこと。
pub fn save_terminal_settings() {
    unsafe {
        let hin = stdin_handle();
        let hout = stdout_handle();
        let mut in_mode: u32 = 0;
        let mut out_mode: u32 = 0;
        if GetConsoleMode(hin, &mut in_mode) == 0 || GetConsoleMode(hout, &mut out_mode) == 0 {
            return; // console_ok() が先に拒否している想定
        }
        let _ = ORIG_CONSOLE.set(OrigConsole {
            stdin_mode: in_mode,
            stdout_mode: out_mode,
            input_cp: GetConsoleCP(),
            output_cp: GetConsoleOutputCP(),
        });

        // raw 相当: 行編集/エコー/シグナル処理を切り、VT 入力 + リサイズイベントを有効化。
        // ENABLE_PROCESSED_INPUT off → Ctrl+C が生 0x03 (Unix の ISIG off と同義)。
        // ENABLE_QUICK_EDIT_MODE off にはENABLE_EXTENDED_FLAGS が必要。
        let raw_in = (in_mode
            & !(ENABLE_LINE_INPUT
                | ENABLE_ECHO_INPUT
                | ENABLE_PROCESSED_INPUT
                | ENABLE_QUICK_EDIT_MODE))
            | ENABLE_VIRTUAL_TERMINAL_INPUT
            | ENABLE_WINDOW_INPUT
            | ENABLE_EXTENDED_FLAGS;
        SetConsoleMode(hin, raw_in);

        // DISABLE_NEWLINE_AUTO_RETURN は付けない (モジュール冒頭コメント参照)。
        SetConsoleMode(hout, out_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);

        SetConsoleCP(CP_UTF8);
        SetConsoleOutputCP(CP_UTF8);
    }
}

/// コンソールモードを起動時の状態に復元する。終了時に呼ぶこと。
pub fn restore_terminal_settings() {
    if let Some(orig) = ORIG_CONSOLE.get() {
        unsafe {
            SetConsoleMode(stdin_handle(), orig.stdin_mode);
            SetConsoleMode(stdout_handle(), orig.stdout_mode);
            SetConsoleCP(orig.input_cp);
            SetConsoleOutputCP(orig.output_cp);
        }
    }
}

pub fn terminal_size() -> (u16, u16) {
    unsafe {
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(stdout_handle(), &mut info) != 0 {
            let rows = (info.srWindow.Bottom - info.srWindow.Top + 1).max(0) as u16;
            let cols = (info.srWindow.Right - info.srWindow.Left + 1).max(0) as u16;
            if rows > 0 {
                return (rows, cols);
            }
        }
    }
    (24, 80)
}

/// 入力ポンプの状態。KEY_EVENT を UTF-8 化した未消費バイトを保持する。
struct Pump {
    queue: VecDeque<u8>,
    /// UTF-16 サロゲートペアの前半待ち。
    pending_high_surrogate: Option<u16>,
}

fn pump() -> &'static Mutex<Pump> {
    static PUMP: OnceLock<Mutex<Pump>> = OnceLock::new();
    PUMP.get_or_init(|| {
        Mutex::new(Pump {
            queue: VecDeque::new(),
            pending_high_surrogate: None,
        })
    })
}

/// 溜まっているコンソール入力イベントを全て読み、KEY_EVENT を UTF-8 バイトへ変換して
/// キューに積む。イベントが無ければ何もしない (ReadConsoleInputW でブロックしない)。
fn pump_available_events() {
    let hin = stdin_handle();
    let mut p = pump().lock().unwrap();
    loop {
        let mut avail: u32 = 0;
        if unsafe { GetNumberOfConsoleInputEvents(hin, &mut avail) } == 0 || avail == 0 {
            break;
        }
        let mut records: [INPUT_RECORD; 64] = unsafe { std::mem::zeroed() };
        let want = (avail as usize).min(records.len()) as u32;
        let mut read: u32 = 0;
        if unsafe { ReadConsoleInputW(hin, records.as_mut_ptr(), want, &mut read) } == 0 {
            break;
        }
        for rec in records.iter().take(read as usize) {
            match rec.EventType as u32 {
                KEY_EVENT => {
                    let key = unsafe { rec.Event.KeyEvent };
                    if key.bKeyDown == 0 {
                        continue; // key-up は破棄
                    }
                    let unit = unsafe { key.uChar.UnicodeChar };
                    let repeat = key.wRepeatCount.max(1) as usize;
                    // Ctrl+/ = aish のエントリキー。経路によって uChar が
                    //   0x1f (native conhost) / 0 (一部 VT 端末) /
                    //   0x2f (RDP/Remmina 等のレイアウト変換) と揺れるため、uChar 値に
                    //   依らず Ctrl+VK_OEM_2 を 0x1f へ正規化する (エントリキーの生命線)。
                    // Shift は見ない → Ctrl+Shift+/ (= Ctrl+?) も同じく拾う。
                    let ctrl =
                        key.dwControlKeyState & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0;
                    if ctrl && key.wVirtualKeyCode == VK_OEM_2 {
                        for _ in 0..repeat {
                            p.queue.push_back(0x1f);
                        }
                        continue;
                    }
                    if unit == 0 {
                        continue; // 文字を持たないキー (Shift 等) は破棄
                    }
                    // UTF-16 → UTF-8 (サロゲートペア対応)
                    let push_char = |c: char, p: &mut Pump| {
                        let mut buf = [0u8; 4];
                        let s = c.encode_utf8(&mut buf);
                        for _ in 0..repeat {
                            p.queue.extend(s.as_bytes());
                        }
                    };
                    if (0xD800..=0xDBFF).contains(&unit) {
                        p.pending_high_surrogate = Some(unit);
                    } else if (0xDC00..=0xDFFF).contains(&unit) {
                        if let Some(high) = p.pending_high_surrogate.take() {
                            let cp = 0x10000
                                + (((high as u32) - 0xD800) << 10)
                                + ((unit as u32) - 0xDC00);
                            if let Some(c) = char::from_u32(cp) {
                                push_char(c, &mut p);
                            }
                        }
                    } else {
                        p.pending_high_surrogate = None;
                        if let Some(c) = char::from_u32(unit as u32) {
                            push_char(c, &mut p);
                        }
                    }
                }
                WINDOW_BUFFER_SIZE_EVENT => super::record_resize(),
                _ => {} // MOUSE / FOCUS / MENU は破棄
            }
        }
    }
}

/// poll(fd0) 相当: timeout_ms 以内に 1 byte 読めれば返す。負値はブロッキング。
/// `input::ConsoleSource` と `query_cursor_position_dsr` が使う。
pub fn read_stdin_byte(timeout_ms: i32) -> Option<u8> {
    let deadline =
        (timeout_ms >= 0).then(|| Instant::now() + Duration::from_millis(timeout_ms as u64));
    loop {
        pump_available_events();
        if let Some(b) = pump().lock().unwrap().queue.pop_front() {
            return Some(b);
        }
        // 50ms 刻みで待つ (WaitForSingleObject はマウス等でも立つため、
        // シグナル → pump → キュー空なら再 Wait のループが必須)。
        let slice_ms = match deadline {
            Some(d) => {
                let rem = d.saturating_duration_since(Instant::now());
                if rem.is_zero() {
                    return None;
                }
                (rem.as_millis() as u32).clamp(1, 50)
            }
            None => 50,
        };
        unsafe {
            let r = WaitForSingleObject(stdin_handle(), slice_ms);
            if r != WAIT_OBJECT_0 {
                continue; // timeout / 異常 → deadline 判定へ
            }
        }
    }
}

/// stdin から利用可能なバイトをノンブロッキングで取得する (Unix 版と同セマンティクス)。
pub fn drain_stdin_nonblocking() -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(b) = read_stdin_byte(0) {
        out.push(b);
    }
    out
}

/// stdinからCtrl+C (0x03) が入力されているかノンブロッキングでチェック。
/// Ctrl+C 以外の入力は破棄する (Unix 版と同セマンティクス)。
pub fn check_stdin_cancel() -> bool {
    let mut found = false;
    while let Some(b) = read_stdin_byte(0) {
        if b == 0x03 {
            found = true;
        }
    }
    found
}

/// `\x1b[6n` (DSR) を端末に送り、応答 `\x1b[{row};{col}R` を 80ms 以内に受信して
/// cursor 位置を返す (Unix 版と同セマンティクス。応答は VT 入力として
/// KEY_EVENT 経由で届く)。
pub fn query_cursor_position_dsr(stdout: &mut io::Stdout) -> Option<(u16, u16)> {
    let _ = write!(stdout, "\x1b[6n");
    let _ = stdout.flush();

    let mut buf: Vec<u8> = Vec::with_capacity(16);
    let start = Instant::now();
    let timeout = Duration::from_millis(80);

    while start.elapsed() < timeout {
        let Some(byte) = read_stdin_byte(10) else {
            continue;
        };
        buf.push(byte);
        if byte == b'R' {
            return super::parse_dsr_response(&buf);
        }
        if buf.len() > 32 {
            return None;
        }
    }
    None
}

/// 前回 `check_and_clear_resize` が観測した端末サイズ (差分ポーリングの保険用)。
static LAST_SIZE: Mutex<(u16, u16)> = Mutex::new((0, 0));

/// リサイズ検出を有効化する。Windows はシグナルが無いので、主経路は入力ポンプの
/// WINDOW_BUFFER_SIZE_EVENT。ここでは差分ポーリングの基準サイズを記録するだけ。
pub fn install_resize_watch() {
    *LAST_SIZE.lock().unwrap() = terminal_size();
}

/// リサイズが発生していたか取得しクリアする。イベント経路に加えて、
/// 「前回とサイズが違う」も OR する (イベント取りこぼしの保険)。
pub fn check_and_clear_resize() -> bool {
    let flagged = super::take_resize_flag();
    let now = terminal_size();
    let mut last = LAST_SIZE.lock().unwrap();
    let changed = *last != (0, 0) && *last != now;
    *last = now;
    flagged || changed
}

/// PID の生存確認 (nested aish 検出用)。
pub fn pid_alive(pid: u32) -> bool {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut code) != 0;
        CloseHandle(handle);
        ok && code == STILL_ACTIVE
    }
}

/// epoch 秒に対するローカル TZ オフセット (秒)。
/// Windows には localtime_r の gmtoff 相当が無いため「現在の」TZ オフセットを返す
/// (ログのタイムスタンプ用途なので epoch は常にほぼ現在時刻)。
pub fn local_utc_offset_secs(_epoch: i64) -> i64 {
    unsafe {
        let mut tzi = std::mem::zeroed();
        let id = GetTimeZoneInformation(&mut tzi);
        // Bias は「UTC = local + Bias (分)」。id に応じた追加 Bias を加算する。
        let extra = match id {
            1 => tzi.StandardBias, // TIME_ZONE_ID_STANDARD
            2 => tzi.DaylightBias, // TIME_ZONE_ID_DAYLIGHT
            _ => 0,
        };
        -((tzi.Bias + extra) as i64) * 60
    }
}
