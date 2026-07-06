//! Unix (Linux/macOS) の platform 実装。
//! ui.rs / main.rs / ai/common.rs からの**純移動** — termios のフラグ集合・
//! poll timeout・DSR の 80ms/10ms・EINTR 処理などロジックは一切変更しない。
//! 変更する場合は必ず SPEC.md § 15.1 の背景を確認すること。

use std::io::{self, Read, Write};
use std::sync::OnceLock;
use std::time::Duration;

static ORIG_TERMIOS: OnceLock<libc::termios> = OnceLock::new();

/// 起動時のコンソール適合チェック。Unix では termios 取得失敗でも従来どおり
/// 継続する (既存挙動維持) ため常に Ok。Windows 実装は VT 非対応環境を拒否する。
pub fn console_ok() -> Result<(), String> {
    Ok(())
}

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

/// 起動時にtermiosを保存し、rawモードに設定する。main開始直後に呼ぶこと。
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

/// termiosを起動時の状態に復元する。終了時に呼ぶこと。
pub fn restore_terminal_settings() {
    use std::os::unix::io::AsRawFd;
    if let Some(orig) = ORIG_TERMIOS.get() {
        let fd = io::stdin().as_raw_fd();
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, orig) };
    }
}

pub fn terminal_size() -> (u16, u16) {
    use std::os::unix::io::AsRawFd;
    let fd = io::stdout().as_raw_fd();
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_row > 0 {
        return (ws.ws_row, ws.ws_col);
    }
    (24, 80)
}

/// stdin から利用可能なバイトをノンブロッキングで取得する。
/// AI 提案コマンドの完了待ち中に、ユーザのキー入力 / Ctrl+C / パスワード入力等を
/// PTY に転送するために使う。`BufReader` をバイパスして fd 0 を直接読む。
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

/// stdinからCtrl+C (0x03) が入力されているかノンブロッキングでチェック。
/// `std::io::stdin().read()` は lock + 内部バッファ経由なので、入力スレッド側との
/// 競合や 1 byte 取り損ねで Ctrl+C を見逃すことがあった。`libc::read` で生 fd から
/// 直接 1 byte ずつ読むことで、単発の Ctrl+C でも確実に検出する。
pub fn check_stdin_cancel() -> bool {
    let fd = libc::STDIN_FILENO;
    let mut found = false;
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
        let mut buf = [0u8; 1];
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 1) };
        match n {
            1 if buf[0] == 0x03 => found = true,
            1 => {} // Ctrl+C 以外の入力は破棄 (キャンセル中のキー入力をシェルへ渡さない)
            _ => break,
        }
    }
    found
}

/// `\x1b[6n` (DSR) を端末に送り、応答 `\x1b[{row};{col}R` を 80ms 以内に受信して
/// cursor 位置を返す。応答が来ない / パースできない端末では `None`。
/// `passthrough_read_raw` から呼ばれた `show_minibuffer` 専用 (stdin が raw モードで
/// `passthrough_read_raw` 側で握られているが、ここでは別途 fd 0 を `ManuallyDrop` で
/// 借りて非ブロッキングで読み取る)。応答前にユーザがキーを打った場合、その文字は
/// 応答パース用バッファに混入して捨てられる (実用上 80ms 以内にユーザが打つことは稀)。
pub fn query_cursor_position_dsr(stdout: &mut io::Stdout) -> Option<(u16, u16)> {
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
                    return super::parse_dsr_response(&buf);
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

extern "C" fn sigwinch_handler(_sig: libc::c_int) {
    super::record_resize();
}

/// 端末リサイズ検出を有効化する (SIGWINCH ハンドラ登録)。run() 冒頭で 1 度呼ぶ。
pub fn install_resize_watch() {
    unsafe {
        libc::signal(
            libc::SIGWINCH,
            sigwinch_handler as *const () as libc::sighandler_t,
        );
    }
}

/// リサイズが発生していたか取得しクリアする。
pub fn check_and_clear_resize() -> bool {
    super::take_resize_flag()
}

/// PID の生存確認 (nested aish 検出用)。`kill(pid, 0)` が成功すれば生存。
pub fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    unsafe { libc::kill(pid, 0) == 0 }
}

/// epoch 秒に対するローカル TZ オフセット (秒)。`timestamp_local` が使う。
pub fn local_utc_offset_secs(epoch: i64) -> i64 {
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = epoch as libc::time_t;
        libc::localtime_r(&t, &mut tm);
        tm.tm_gmtoff as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_alive_for_self_and_dead_for_bogus() {
        // 自プロセスは生存、u32::MAX (存在し得ない / i32 変換不可) は false。
        assert!(pid_alive(std::process::id()));
        assert!(
            !pid_alive(u32::MAX),
            "i32 範囲外は kill(-1) 事故防止で false"
        );
    }

    #[test]
    fn local_utc_offset_is_sane_and_deterministic() {
        let epoch = 1_783_000_000i64; // 2026-07 ごろ
        let off = local_utc_offset_secs(epoch);
        assert!((-14 * 3600..=14 * 3600).contains(&off), "off={off}");
        assert_eq!(off % 60, 0, "分単位のはず: {off}");
        assert_eq!(off, local_utc_offset_secs(epoch), "同一 epoch で決定的");
    }

    #[test]
    fn resize_flag_swap_semantics() {
        // record → true が 1 度だけ返り、以降 false (global atomic のためこの 1 テストに集約)。
        while check_and_clear_resize() {} // 他要因の残フラグを掃除
        super::super::record_resize();
        assert!(check_and_clear_resize());
        assert!(!check_and_clear_resize());
    }

    #[test]
    fn terminal_size_is_always_positive() {
        // tty あり: ioctl の実値 / tty なし (CI): (24, 80) フォールバック。どちらも正。
        let (rows, cols) = terminal_size();
        assert!(rows > 0);
        assert!(cols > 0);
    }

    #[test]
    fn console_ok_is_always_ok_on_unix() {
        assert!(console_ok().is_ok());
    }
}
