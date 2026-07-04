//! Platform 層: 端末の低レベル操作を OS 別実装に隔離する。
//!
//! ここに置くのは raw mode 設定/復元・poll 付き stdin 読み・端末サイズ・DSR・
//! リサイズ検出・Ctrl+C 検出・PID 生存確認・ローカル TZ オフセットのみ。
//! UI ロジック (描画・framing・キー判定) は ui.rs / input.rs に置き、
//! プラットフォーム依存操作は必ずこのモジュール経由で行う。
//!
//! Unix 実装 (unix.rs) は ui.rs / main.rs / ai/common.rs からの**純移動**
//! (termios フラグ・poll timeout・DSR 80ms/10ms・EINTR 処理はロジック不変)。
//! Windows 実装 (windows.rs) は Console API + VT モード。

use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// 端末リサイズ通知フラグ。
/// unix: SIGWINCH handler が立てる / windows: 入力ポンプの WINDOW_BUFFER_SIZE_EVENT が立てる。
static RESIZE_RECEIVED: AtomicBool = AtomicBool::new(false);

/// リサイズ発生を記録する (シグナルハンドラ / 入力ポンプから呼ばれる)。
pub fn record_resize() {
    RESIZE_RECEIVED.store(true, Ordering::Relaxed);
}

/// リサイズフラグを取得してクリアする (platform 実装の `check_and_clear_resize` が使う)。
fn take_resize_flag() -> bool {
    RESIZE_RECEIVED.swap(false, Ordering::Relaxed)
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
