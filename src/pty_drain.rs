use crate::prompt_sniffer::PromptSniffer;
use crate::ring_buffer::RingBuffer;
use crate::ui;
use std::io;
use std::sync::mpsc;

/// drain 中の stdout への転送方針。
#[derive(Default, Clone, Copy, PartialEq)]
pub enum DrainDisplay {
    /// stdout へ転送しない (ring_buffer 記録のみ)。AI 対話前の消去 redisplay 吸収用。
    #[default]
    Hidden,
    /// minibuffer 表示中以外は転送。メインループの通常 tick / PTY 終了時 drain 用。
    UnlessMinibuffer,
    /// 常に転送。AI コマンド実行待ち等、minibuffer が出ない文脈用。
    Always,
}

/// `drain_pty` の動作オプション。bool 位置引数の取り違えを防ぐため名前付きで渡す。
#[derive(Default)]
pub struct DrainOpts<'a> {
    pub display: DrainDisplay,
    /// チャンクごとに flush する。false の呼び出し側は従来どおり drain 後に
    /// 自分で `stdout().flush()` すること (ここでは flush しない)。
    pub flush_each_chunk: bool,
    /// 最初のチャンク先頭の連続 \r\n を「表示からだけ」除去する。記録は完全な
    /// data のまま。AI 対話終了後のプロンプト refresh で先頭改行を畳む用。
    pub skip_leading_newline: bool,
    /// 吸い出した生チャンクをそのまま追記する捕獲バッファ (表示有無と無関係)。
    /// Windows ローカル起動の初期バースト解析 (`conpty_sync::startup_burst_is_clean`)
    /// と、クリーンでなかった場合のフォールバック再表示に使う。
    pub capture: Option<&'a mut Vec<u8>>,
    /// コマンド完了の passive 検出へ流す sniffer。
    pub sniffer: Option<&'a mut PromptSniffer>,
    /// AISH_DEBUG 用の通算チャンク数。最初の 3 チャンクを debug_log する。
    pub debug_chunk_count: Option<&'a mut usize>,
}

/// pty_rx に現在溜まっているチャンクを全部吸い出す。戻り値は 1 チャンク以上処理したか。
///
/// 不変条件: 表示の有無・先頭改行の除去に関わらず、吸い出した data は必ず
/// 「trim 前の完全な形」で ring_buffer に記録される (「PTY 出力は全て ring_buffer に
/// 入る」。append 内部の ANSI strip は従来どおり)。チャンク内の処理順は
/// debug → 表示 → flush → 記録 → sniffer で固定。表示 write / flush の失敗 (`?`) は
/// そのチャンクを記録せずに伝播する (旧コード互換)。
pub fn drain_pty(
    rx: &mpsc::Receiver<Vec<u8>>,
    ring_buffer: &mut RingBuffer,
    out: &mut dyn io::Write,
    opts: DrainOpts<'_>,
) -> io::Result<bool> {
    let DrainOpts {
        display,
        flush_each_chunk,
        skip_leading_newline,
        mut capture,
        mut sniffer,
        mut debug_chunk_count,
    } = opts;
    let mut got_any = false;
    let mut first_chunk = true;
    while let Ok(data) = rx.try_recv() {
        got_any = true;
        // AISH_DEBUG_PTY=1 のとき、ConPTY/子シェルが出す生チャンクを stderr にダンプ
        // (Windows 描画ズレ調査用。カーソル位置指定シーケンスを実測する)。既定無効。
        crate::debug_pty(&data);
        // win32-input-mode の set/reset を検出して合成キーのエンコード方式を切り替える
        // (kill_line / refresh_prompt の生 ESC が握りつぶされる問題 — § 15.13)。
        // 全 PTY 出力はこの関数を通る (choke point) ためここで一元的に観測する。
        crate::pty_handler::note_pty_output(&data);
        if let Some(count) = debug_chunk_count.as_deref_mut() {
            *count += 1;
            if *count <= 3 {
                crate::debug_log(&format!(
                    "pty chunk #{} ({} bytes): {}",
                    count,
                    data.len(),
                    crate::debug_bytes(&data, 200)
                ));
            }
        }
        let show = match display {
            DrainDisplay::Hidden => false,
            DrainDisplay::UnlessMinibuffer => !ui::minibuffer_active(),
            DrainDisplay::Always => true,
        };
        let visible: &[u8] = if skip_leading_newline && first_chunk {
            strip_leading_crlf(&data)
        } else {
            &data
        };
        first_chunk = false;
        if show && !visible.is_empty() {
            out.write_all(visible)?;
            if flush_each_chunk {
                out.flush()?;
            }
        }
        if let Some(cap) = capture.as_deref_mut() {
            cap.extend_from_slice(&data);
        }
        ring_buffer.append(&data);
        if let Some(s) = sniffer.as_deref_mut() {
            s.feed(&data);
        }
    }
    Ok(got_any)
}

/// 先頭の連続する \r / \n を取り除いた表示用スライスを返す (記録用 data には触らない)。
fn strip_leading_crlf(data: &[u8]) -> &[u8] {
    let start = data
        .iter()
        .position(|&b| b != b'\r' && b != b'\n')
        .unwrap_or(data.len());
    &data[start..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::BackendKind;

    fn channel_with(chunks: &[&[u8]]) -> mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = mpsc::channel();
        for c in chunks {
            tx.send(c.to_vec()).unwrap();
        }
        rx
    }

    fn recorded(rb: &RingBuffer) -> String {
        rb.get_unsent_for(BackendKind::Claude)
    }

    #[test]
    fn strip_leading_crlf_golden() {
        assert_eq!(strip_leading_crlf(b"\r\n\r\nfoo"), b"foo");
        assert_eq!(strip_leading_crlf(b"\r\n"), b"");
        assert_eq!(strip_leading_crlf(b"foo\r\n"), b"foo\r\n");
        assert_eq!(strip_leading_crlf(b""), b"");
    }

    #[test]
    fn hidden_records_but_does_not_display() {
        let rx = channel_with(&[b"abc", b"def"]);
        let mut rb = RingBuffer::new();
        let mut out: Vec<u8> = Vec::new();
        let got = drain_pty(&rx, &mut rb, &mut out, DrainOpts::default()).unwrap();
        assert!(got);
        assert!(out.is_empty());
        assert_eq!(recorded(&rb), "abcdef");
    }

    #[test]
    fn empty_channel_returns_false() {
        let rx = channel_with(&[]);
        let mut rb = RingBuffer::new();
        let mut out: Vec<u8> = Vec::new();
        let got = drain_pty(&rx, &mut rb, &mut out, DrainOpts::default()).unwrap();
        assert!(!got);
    }

    #[test]
    fn skip_leading_newline_trims_display_only_on_first_chunk() {
        let rx = channel_with(&[b"\r\n\r\nprompt$ ", b"\nrest"]);
        let mut rb = RingBuffer::new();
        let mut out: Vec<u8> = Vec::new();
        drain_pty(
            &rx,
            &mut rb,
            &mut out,
            DrainOpts {
                display: DrainDisplay::Always,
                flush_each_chunk: true,
                skip_leading_newline: true,
                ..Default::default()
            },
        )
        .unwrap();
        // 表示は初回チャンクのみ trim、第 2 チャンクはそのまま
        assert_eq!(out, b"prompt$ \nrest");
        // 記録は trim 前の data (先頭改行が残る)。\r が消えるのは RingBuffer::append
        // 内部の ANSI strip の既存仕様で、skip_leading_newline の除去とは無関係。
        assert_eq!(recorded(&rb), "\n\nprompt$ \nrest");
    }

    #[test]
    fn capture_accumulates_raw_chunks_even_when_hidden() {
        let rx = channel_with(&[b"\x1b[2J\x1b[H", b"PS C:\\> "]);
        let mut rb = RingBuffer::new();
        let mut out: Vec<u8> = Vec::new();
        let mut cap: Vec<u8> = Vec::new();
        drain_pty(
            &rx,
            &mut rb,
            &mut out,
            DrainOpts {
                capture: Some(&mut cap),
                ..Default::default() // display: Hidden
            },
        )
        .unwrap();
        assert!(out.is_empty());
        assert_eq!(cap, b"\x1b[2J\x1b[HPS C:\\> ");
    }

    #[test]
    fn sniffer_sees_all_chunks() {
        let rx = channel_with(&[b"user@host:~", b"$ "]);
        let mut rb = RingBuffer::new();
        let mut out: Vec<u8> = Vec::new();
        let mut sniffer = PromptSniffer::new();
        drain_pty(
            &rx,
            &mut rb,
            &mut out,
            DrainOpts {
                display: DrainDisplay::Always,
                sniffer: Some(&mut sniffer),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(sniffer.matches_prompt());
    }

    #[test]
    fn debug_chunk_count_accumulates_across_calls() {
        let mut count = 0usize;
        let mut rb = RingBuffer::new();
        let mut out: Vec<u8> = Vec::new();
        let rx = channel_with(&[b"a", b"b"]);
        drain_pty(
            &rx,
            &mut rb,
            &mut out,
            DrainOpts {
                debug_chunk_count: Some(&mut count),
                ..Default::default()
            },
        )
        .unwrap();
        let rx = channel_with(&[b"c"]);
        drain_pty(
            &rx,
            &mut rb,
            &mut out,
            DrainOpts {
                debug_chunk_count: Some(&mut count),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn write_failure_skips_recording_of_failed_chunk() {
        struct FailWriter;
        impl io::Write for FailWriter {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("boom"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let rx = channel_with(&[b"abc"]);
        let mut rb = RingBuffer::new();
        let res = drain_pty(
            &rx,
            &mut rb,
            &mut FailWriter,
            DrainOpts {
                display: DrainDisplay::Always,
                ..Default::default()
            },
        );
        // 表示 write の失敗はそのチャンクを記録せず伝播する (旧コードと同順序)
        assert!(res.is_err());
        assert_eq!(recorded(&rb), "");
    }
}
