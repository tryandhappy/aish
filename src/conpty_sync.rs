//! Windows ConPTY の「二重画面モデル」と aish 直接描画の再同期 (SPEC.md § 15.8)。
//!
//! Windows では子シェルの出力は一旦 ConPTY 内部のスクリーンバッファに描画され、
//! ConPTY がそれを絶対座標付きの再描画コマンド (CUP `\x1b[r;cH` / ECH `\x1b[nX` 等)
//! として合成し直したものが aish に届く。一方、aish が自前で stdout に描く行
//! (AI 応答・`[aish]` エコー・Exec? 確認行・minibuffer のスクロール) は ConPTY の
//! 関知外なので、実画面だけがスクロールして ConPTY の内部モデルと行ズレする。
//! その状態で PTY 出力の表示を再開すると、PSReadLine のコマンドエコーや打ちかけ
//! 消去がモデル基準の絶対座標 (= ずれた行) に描かれ、aish の出力を上書きする
//! (2026-08 実測: 通常出力・プロンプト再表示は相対 `\r\n` だが、コマンドエコーの
//! 再描画は `\x1b[6;26H...`、ESC の行消去は `\x1b[24;26H\x1b[13X` の絶対座標)。
//!
//! 対策 = 再同期 (resync): 「PTY と実画面が同期している瞬間」の cursor 位置を
//! anchor として記録しておき、aish の直接描画から PTY 表示へ戻る境目で、
//! aish が描いた行を anchor 行より上へ全画面 LF スクロールで退避してから cursor を
//! anchor へ絶対復帰する。以降 ConPTY の絶対座標と実画面が行単位で一致するので、
//! パススルーは従来どおり無加工でよい。挿入するのは空行スクロールと cursor 移動
//! だけで、PTY への入力注入もコマンド変形もしない (信頼の根幹は不変)。
//!
//! Unix では子シェル (bash readline) が相対シーケンスで redisplay するためこの
//! 問題自体が起きない。`crate::term::cursor_position()` が Unix では常に None を
//! 返すので、本モジュールの resync / capture は Unix では自然に no-op になる。

use std::io::{self, Write};
use std::sync::Mutex;

/// ConPTY の内部モデルと実画面が同期していた瞬間の cursor 位置 (row, col)。
/// 記録タイミング: minibuffer 入口の DSR (入力スレッド) / AI 提案コマンド完了時
/// (main スレッド)。読み書きが別スレッドなので Mutex。
static ANCHOR: Mutex<Option<(u16, u16)>> = Mutex::new(None);

/// anchor を明示座標で記録する (minibuffer 入口の DSR 結果を流用する用)。
pub fn set_anchor(row: u16, col: u16) {
    *ANCHOR.lock().unwrap() = Some((row, col));
}

/// 現在の実 cursor 位置を anchor として記録する。「PTY 出力を無加工表示し終えた
/// 直後」(= 実 cursor == ConPTY cursor の瞬間) にだけ呼ぶこと。
/// Unix では cursor_position() が None なので anchor はクリアされ、no-op と等価。
pub fn capture_anchor() {
    *ANCHOR.lock().unwrap() = crate::term::cursor_position();
}

/// anchor を無効化する。端末リサイズ後は ConPTY が全面再描画するため行の対応が
/// 変わっており、古い anchor で resync すると誤った量をスクロールする。
pub fn clear_anchor() {
    *ANCHOR.lock().unwrap() = None;
}

/// aish 直接描画 → PTY 表示再開の境目で、実画面を ConPTY モデルに合わせ直す。
///
/// aish が描いた行 (anchor 行以降に伸びた分) を全画面 LF スクロールで anchor 行より
/// 上へ退避し、cursor を anchor へ絶対復帰する。全画面スクロールなので退避された行は
/// 通常のシェル出力と同じく scrollback に残る (minibuffer の伸長と同じ方式)。
///
/// 戻り値 = 実画面をスクロールしたか。true のとき、表示上のシェルプロンプト行も
/// 上へ動いているため、呼び出し側は次のコマンド送信前に `refresh_prompt` で
/// 新しいプロンプトを描かせること (エコーが「プロンプトの無い行」に浮くのを防ぐ)。
/// anchor 未記録 / Unix / cursor 取得不能 / 移動なしのときは何もせず false。
pub fn resync() -> io::Result<bool> {
    let anchor = *ANCHOR.lock().unwrap();
    let Some((anchor_row, anchor_col)) = anchor else {
        return Ok(false);
    };
    let Some((cur_row, cur_col)) = crate::term::cursor_position() else {
        return Ok(false);
    };
    if (cur_row, cur_col) == (anchor_row, anchor_col) {
        return Ok(false); // 何も描いていない (自動承認の連続実行等) → no-op
    }
    let scroll = scroll_rows_needed(anchor_row, cur_row, cur_col);
    let mut out = io::stdout();
    if scroll > 0 {
        let (rows, _) = crate::term::terminal_size();
        // cursor を実画面最下行に置いた LF の全画面スクロール (DECSTBM は使わない)。
        write!(out, "\x1b[{rows};1H")?;
        for _ in 0..scroll {
            out.write_all(b"\n")?;
        }
    }
    write!(out, "\x1b[{anchor_row};{anchor_col}H")?;
    out.flush()?;
    Ok(scroll > 0)
}

/// aish が描いた最終行を anchor 行より上に退避するのに必要なスクロール行数 (純関数)。
/// cursor が行頭 (col == 1) なら直前の行までが描画済み、行中なら cursor 行自体も
/// 描画済みとして数える。
fn scroll_rows_needed(anchor_row: u16, cur_row: u16, cur_col: u16) -> u16 {
    let text_end = if cur_col > 1 {
        cur_row
    } else {
        cur_row.saturating_sub(1)
    };
    if text_end >= anchor_row {
        text_end - anchor_row + 1
    } else {
        0
    }
}

/// PTY チャンクから「端末状態を変えるが何も描画しないシーケンス」だけを通す。
/// 起動バースト (ConPTY の全画面クリア + 初回プロンプト描画) を画面に出さずに
/// 吸収する際、win32-input-mode 要求 (`\x1b[?9001h`) やフォーカス通知
/// (`\x1b[?1004h`)、ウィンドウタイトル OSC まで捨てると入力経路が変わってしまう
/// ため、それらだけ実端末へ転送する用途 (main の Windows ローカル起動処理)。
///
/// 通すもの: DEC private mode set/reset (`\x1b[?...h` / `\x1b[?...l`)、OSC
/// (`\x1b]...BEL` / `\x1b]...ST`)。それ以外 (テキスト・CUP・ED・SGR 等) は捨てる。
/// チャンク境界で分断された未完シーケンスは捨てる (起動バーストは実測上
/// シーケンス単位で届くため実害なし)。
pub fn filter_terminal_state(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            Some(b'[') => {
                // CSI: 終端バイト (0x40-0x7e) まで走査
                let mut j = i + 2;
                while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                    j += 1;
                }
                if j >= bytes.len() {
                    break; // 未完 (分断) は捨てる
                }
                if bytes.get(i + 2) == Some(&b'?') && (bytes[j] == b'h' || bytes[j] == b'l') {
                    out.extend_from_slice(&bytes[i..=j]);
                }
                i = j + 1;
            }
            Some(b']') => {
                // OSC: BEL (0x07) または ST (ESC \) 終端まで
                let mut j = i + 2;
                let mut end = None;
                while j < bytes.len() {
                    if bytes[j] == 0x07 {
                        end = Some(j);
                        break;
                    }
                    if bytes[j] == 0x1b && bytes.get(j + 1) == Some(&b'\\') {
                        end = Some(j + 1);
                        break;
                    }
                    j += 1;
                }
                match end {
                    Some(e) => {
                        out.extend_from_slice(&bytes[i..=e]);
                        i = e + 1;
                    }
                    None => break, // 未完終端は捨てる
                }
            }
            Some(_) => i += 2, // その他の 2 byte ESC シーケンスは捨てる
            None => break,
        }
    }
    out
}

/// 起動バーストが「全画面クリア + プロンプト 1 行だけ」のクリーンな形かを判定する。
/// クリーンなら main の Windows ローカル起動処理がバナー再配置 + 空 Enter 注入で
/// バナーを温存できる。profile 出力等で複数行のテキストが混ざる環境では false を
/// 返し、呼び出し側は従来動作 (バースト全体をそのまま表示) にフォールバックする。
pub fn startup_burst_is_clean(bytes: &[u8]) -> bool {
    // ConPTY の初期全画面クリアが含まれること (これが無ければ従来動作で問題ない)
    if !bytes.windows(4).any(|w| w == b"\x1b[2J") {
        return false;
    }
    let stripped = strip_ansi_escapes::strip(bytes);
    let text = String::from_utf8_lossy(&stripped);
    let text = text.trim_matches(['\u{7}', ' ']);
    // プロンプト 1 行だけ = 非空 かつ 改行を含まない
    !text.is_empty() && !text.contains('\n') && !text.contains('\r')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_amount_no_output_since_anchor() {
        // cursor が anchor と同行・行頭 (col 1) → 描画済みは anchor より上 → 0
        assert_eq!(scroll_rows_needed(24, 24, 1), 0);
        // anchor より上にいる (minibuffer キャンセルで上に復元された等) → 0
        assert_eq!(scroll_rows_needed(24, 20, 5), 0);
    }

    #[test]
    fn scroll_amount_counts_partial_line() {
        // anchor 行上で col > 1 = anchor 行自体に描いた → 1 行退避
        assert_eq!(scroll_rows_needed(24, 24, 10), 1);
    }

    #[test]
    fn scroll_amount_multi_line_output() {
        // anchor=20 で cursor (24,1): 描画済みは 23 行目まで → 23-20+1 = 4
        assert_eq!(scroll_rows_needed(20, 24, 1), 4);
        // 行中 cursor (24,5): 24 行目まで → 5
        assert_eq!(scroll_rows_needed(20, 24, 5), 5);
    }

    #[test]
    fn scroll_amount_top_of_screen() {
        assert_eq!(scroll_rows_needed(1, 8, 1), 7);
    }

    #[test]
    fn filter_keeps_private_modes_and_osc_only() {
        // 実測した ConPTY 初期バースト (2026-08、SPEC § 15.8):
        // モード設定 + 全画面クリア + プロンプト + タイトル OSC + カーソル表示
        let burst = b"\x1b[?9001h\x1b[?1004h\x1b[?25l\x1b[2J\x1b[m\x1b[HPS C:\\>\x1b[1C\x1b]0;title\x07\x1b[?25h";
        let kept = filter_terminal_state(burst);
        assert_eq!(
            kept,
            b"\x1b[?9001h\x1b[?1004h\x1b[?25l\x1b]0;title\x07\x1b[?25h"
        );
    }

    #[test]
    fn filter_drops_text_and_absolute_moves() {
        assert_eq!(filter_terminal_state(b"plain text\r\n"), b"");
        assert_eq!(filter_terminal_state(b"\x1b[6;26HGet-Date\x1b[93m"), b"");
    }

    #[test]
    fn filter_keeps_st_terminated_osc() {
        assert_eq!(
            filter_terminal_state(b"\x1b]0;t\x1b\\rest"),
            b"\x1b]0;t\x1b\\"
        );
    }

    #[test]
    fn filter_drops_truncated_sequences() {
        // チャンク境界で切れた CSI / OSC は捨てる (通さない)
        assert_eq!(filter_terminal_state(b"\x1b[?9001"), b"");
        assert_eq!(filter_terminal_state(b"\x1b]0;title-without-end"), b"");
        assert_eq!(filter_terminal_state(b"\x1b"), b"");
    }

    #[test]
    fn clean_burst_is_detected() {
        // powershell -NoLogo の実測バースト形 (プロンプト 1 行のみ)
        let burst = b"\x1b[?9001h\x1b[?1004h\x1b[?25l\x1b[2J\x1b[m\x1b[HPS C:\\Users\\u>\x1b[1C\x1b]0;C:\\WINDOWS\\...\x07\x1b[?25h";
        assert!(startup_burst_is_clean(burst));
    }

    #[test]
    fn burst_with_profile_output_is_not_clean() {
        // profile が何か出力した (複数行) → フォールバック側
        let burst = b"\x1b[?25l\x1b[2J\x1b[m\x1b[Hloading profile...\r\nPS C:\\>\x1b[?25h";
        assert!(!startup_burst_is_clean(burst));
    }

    #[test]
    fn burst_without_clear_screen_is_not_clean() {
        // 全画面クリアが無い形 (将来 ConPTY が挙動を変えた場合) は従来動作へ
        assert!(!startup_burst_is_clean(b"PS C:\\>"));
    }

    #[test]
    fn anchor_roundtrip_and_clear() {
        set_anchor(10, 20);
        assert_eq!(*ANCHOR.lock().unwrap(), Some((10, 20)));
        clear_anchor();
        assert_eq!(*ANCHOR.lock().unwrap(), None);
    }
}
