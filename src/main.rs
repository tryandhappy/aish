mod ai;
mod config;
mod mode;
mod prompt_sniffer;
mod pty_handler;
mod ring_buffer;
mod ui;
mod update;

use mode::Mode;
use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

struct AishArgs {
    config_path: Option<String>,
    ssh_args: Vec<String>,
}

enum CliAction {
    Run(AishArgs),
    Update,
    Version,
}

fn parse_args() -> CliAction {
    let args: Vec<String> = std::env::args().skip(1).collect();

    for arg in &args {
        match arg.as_str() {
            "--update" => return CliAction::Update,
            "--version" | "-V" => return CliAction::Version,
            _ => {}
        }
    }

    let mut config_path = None;
    let mut ssh_args = Vec::new();
    let mut i = 0;

    while i < args.len() {
        if args[i] == "--aish-config" {
            if i + 1 < args.len() {
                config_path = Some(args[i + 1].clone());
                i += 2;
                continue;
            } else {
                eprintln!("Error: --aish-config requires a value");
                std::process::exit(1);
            }
        }
        if args[i].starts_with("--aish-") {
            eprintln!("Warning: Unknown aish option: {}", args[i]);
            i += 1;
            continue;
        }
        ssh_args.push(args[i].clone());
        i += 1;
    }

    CliAction::Run(AishArgs {
        config_path,
        ssh_args,
    })
}

#[cfg(unix)]
extern "C" fn sigwinch_handler(_sig: libc::c_int) {
    ui::record_sigwinch();
}

/// 環境変数 AISH_DEBUG=1 のときだけ /tmp/aish-debug.log にデバッグメモを書く。
/// 平時は no-op (ファイル open すらしない)。
fn debug_log(msg: &str) {
    if std::env::var("AISH_DEBUG").ok().as_deref() != Some("1") {
        return;
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/aish-debug.log")
    {
        let _ = writeln!(f, "[{}] {}", std::process::id(), msg);
    }
}

/// バイト列をデバッグ用にエスケープ表記で文字列化する（先頭 N バイト）。
fn debug_bytes(data: &[u8], max: usize) -> String {
    let n = data.len().min(max);
    let mut s = String::with_capacity(n * 4);
    for &b in &data[..n] {
        match b {
            0x1b => s.push_str("\\e"),
            0x0a => s.push_str("\\n"),
            0x0d => s.push_str("\\r"),
            0x09 => s.push_str("\\t"),
            0x07 => s.push_str("\\a"),
            0x08 => s.push_str("\\b"),
            0x0c => s.push_str("\\f"),
            0x20..=0x7e => s.push(b as char),
            _ => s.push_str(&format!("\\x{:02x}", b)),
        }
    }
    if data.len() > max {
        s.push_str(&format!(" ... (+{} more bytes)", data.len() - max));
    }
    s
}

/// PTY 出力に TUI コマンドが端末状態を変更した形跡があるかを検出する。
/// 検出すべき変化:
/// - alt screen 出入り (`\x1b[?1049h/l`、`\x1b[?1047h/l`、`\x1b[?47h/l`)
/// - DECSTBM (スクロール領域) 変更 (`\x1b[<n>;<m>r` または `\x1b[r`)
/// - 画面クリア (`\x1b[2J`、`\x1bc` (RIS))
/// いずれか検出されたら aish のレイアウトが壊れている可能性ありとみなす。
fn contains_tui_signature(data: &[u8]) -> bool {
    // alt screen
    if data.windows(8).any(|w| w == b"\x1b[?1049h" || w == b"\x1b[?1049l") {
        return true;
    }
    if data.windows(8).any(|w| w == b"\x1b[?1047h" || w == b"\x1b[?1047l") {
        return true;
    }
    if data.windows(6).any(|w| w == b"\x1b[?47h" || w == b"\x1b[?47l") {
        return true;
    }
    // 画面クリア
    if data.windows(4).any(|w| w == b"\x1b[2J" || w == b"\x1b[1J") {
        return true;
    }
    if data.windows(2).any(|w| w == b"\x1bc") {
        return true;
    }
    // DECSTBM 変更: \x1b[ followed by digits/semicolons followed by 'r'
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == 0x1b && data[i + 1] == b'[' {
            let mut j = i + 2;
            while j < data.len() && (data[j].is_ascii_digit() || data[j] == b';') {
                j += 1;
            }
            if j < data.len() && data[j] == b'r' {
                return true;
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    false
}

fn run(args: AishArgs) -> Result<(), Box<dyn std::error::Error>> {
    ui::save_terminal_settings();

    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGWINCH, sigwinch_handler as *const () as libc::sighandler_t);
    }

    if !ai::check_claude_installed() {
        eprintln!("Please install Claude Code.");
        eprintln!("curl -fsSL https://claude.ai/install.sh | bash");
        std::process::exit(1);
    }

    let config = config::Config::load(args.config_path.as_deref())?;

    let (term_rows, term_cols) = ui::terminal_size();
    let pty_rows = term_rows.saturating_sub(1).max(1);

    let mode = if args.ssh_args.is_empty() {
        Mode::Local
    } else {
        Mode::Remote
    };

    let mut pty = if mode == Mode::Local {
        pty_handler::PtyHandler::spawn_local_shell(pty_rows, term_cols)?
    } else {
        pty_handler::PtyHandler::spawn_ssh(&args.ssh_args, pty_rows, term_cols)?
    };

    let mut ring_buffer = ring_buffer::RingBuffer::new();
    let mut ai_session = ai::AiSession::new(&config.system_prompt, &config.language, &config.log);

    // PTY出力を読み取るスレッド
    let (pty_tx, pty_rx) = mpsc::channel::<Vec<u8>>();
    let (alive_tx, alive_rx) = mpsc::channel::<()>();

    let mut pty_reader = pty.take_reader();

    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) => {
                    let _ = alive_tx.send(());
                    break;
                }
                Ok(n) => {
                    let _ = pty_tx.send(buf[..n].to_vec());
                }
                Err(_) => {
                    let _ = alive_tx.send(());
                    break;
                }
            }
        }
    });

    // ターミナルタイトルにaishラベルを設定
    let title = if args.ssh_args.is_empty() {
        config.display.shell_prefix_label.clone()
    } else {
        format!("{} {}", config.display.shell_prefix_label, args.ssh_args.join(" "))
    };
    print!("\x1b]2;{title}\x07");
    io::stdout().flush().ok();

    // ステータスバー: 最下行に [aish] ラベルを常時表示
    let status_label = format!(
        "aish v{} | Ctrl+/ for AI",
        env!("CARGO_PKG_VERSION"),
    );
    let status_color = &config.display.header_color;
    ui::setup_status_bar(term_rows, &status_label, status_color);

    let aish_label = format!(
        "{}{}\x1b[0m ",
        ui::build_color_start(&config.display.prompt_color),
        config.display.prompt_label,
    );

    // ユーザ入力を読み取るスレッド（パススルーモード対応）
    let (prompt_tx, prompt_rx) = mpsc::channel::<ui::InputRequest>();
    let (input_tx, input_rx) = mpsc::channel::<ui::InputEvent>();
    let input_bg = config.display.input_color.clone();
    let input_aish_label = aish_label.clone();
    thread::spawn(move || {
        loop {
            let request = match prompt_rx.recv() {
                Ok(r) => r,
                Err(_) => break,
            };
            match request {
                ui::InputRequest::Passthrough(prompt) => {
                    if !prompt.is_empty() {
                        print!("{prompt}");
                        io::stdout().flush().ok();
                    }
                    ui::passthrough_read(&input_tx, &input_bg, &input_aish_label);
                }
                ui::InputRequest::ReadLine(prompt) => {
                    if !prompt.is_empty() {
                        print!("{prompt}");
                        io::stdout().flush().ok();
                    }
                    let line = ui::read_line().unwrap_or_else(|| "n".to_string());
                    if input_tx.send(ui::InputEvent::Line(line)).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut pending_input = true; // 入力スレッド起動待ち
    let mut input_idle = true;
    let mut last_pty_output = Instant::now();
    let mut status_bar_needs_refresh = false;
    // passthrough モードで TUI コマンド (top/vim/less 等) が走った形跡。
    // 検出されると次の status bar 再描画タイミングで重い復旧を実行する。
    let mut tui_recovery_pending = false;

    // メインループ
    loop {
        // 端末リサイズ検出
        if ui::check_and_clear_sigwinch() {
            let (new_rows, new_cols) = ui::terminal_size();
            let new_pty_rows = new_rows.saturating_sub(1).max(1);
            let _ = pty.resize(new_pty_rows, new_cols);
            ui::resize_status_bar(new_rows);
        }

        // PTY出力をチェック
        while let Ok(data) = pty_rx.try_recv() {
            if !ui::minibuffer_active() {
                io::stdout().write_all(&data)?;
                io::stdout().flush()?;
            }
            ring_buffer.append(&data);
            last_pty_output = Instant::now();
            status_bar_needs_refresh = true;
            if !tui_recovery_pending && contains_tui_signature(&data) {
                tui_recovery_pending = true;
                debug_log(&format!(
                    "[main loop] tui_signature detected: {}",
                    debug_bytes(&data, 200)
                ));
            }
        }

        // PTY出力が落ち着いたらステータスバーとスクロール領域を復元。
        // TUI コマンド (top 等) 終了後は shell に Ctrl+L を送って画面クリア +
        // プロンプト再描画を **shell 自身に** 任せる方式に切替。
        // aish 側で escape を組み立てるよりも端末固有のクセに強い。
        if status_bar_needs_refresh && last_pty_output.elapsed() > Duration::from_millis(50) {
            let (rows, _cols) = ui::terminal_size();
            if tui_recovery_pending {
                debug_log("[main loop] tui recovery: Ctrl+L to shell");
                io::stdout().write_all(b"\x1b[r")?;
                io::stdout().flush()?;
                pty.write(b"\x0c")?;
                thread::sleep(Duration::from_millis(200));
                let mut response = Vec::new();
                while let Ok(data) = pty_rx.try_recv() {
                    response.extend_from_slice(&data);
                    io::stdout().write_all(&data)?;
                    ring_buffer.append(&data);
                }
                io::stdout().flush()?;
                debug_log(&format!(
                    "[main loop] Ctrl+L response: {} bytes: {}",
                    response.len(),
                    debug_bytes(&response, 300)
                ));
                tui_recovery_pending = false;
            }
            ui::resize_status_bar(rows);
            status_bar_needs_refresh = false;
        }

        // PTY出力が落ち着いたら入力スレッドを起動
        if pending_input && input_idle && last_pty_output.elapsed() > Duration::from_millis(50) {
            let _ = prompt_tx.send(ui::InputRequest::Passthrough(String::new()));
            pending_input = false;
            input_idle = false;
        }

        // PTYプロセスの終了チェック
        if alive_rx.try_recv().is_ok() {
            // 残りのPTY出力（logoutメッセージ等）を表示してから終了する
            thread::sleep(Duration::from_millis(50));
            while let Ok(data) = pty_rx.try_recv() {
                if !ui::minibuffer_active() {
                    io::stdout().write_all(&data)?;
                }
                ring_buffer.append(&data);
            }
            io::stdout().flush().ok();
            break;
        }

        // ユーザ入力をチェック
        match input_rx.try_recv() {
            Ok(ui::InputEvent::PtyData(data)) => {
                let _ = pty.write(&data);
                continue;
            }
            Ok(ui::InputEvent::PassthroughEnded) => {
                // 入力スレッドがidle状態に戻った
                input_idle = true;
                // PTY出力が落ち着いてから[aish]プロンプトを再表示し入力を再開
                pending_input = true;
                last_pty_output = Instant::now();
                continue;
            }
            Ok(ui::InputEvent::AiPrompt(prompt)) => {
                input_idle = true;
                if prompt.is_empty() {
                    pending_input = true;
                    last_pty_output = Instant::now();
                    continue;
                }
                let context = ring_buffer.get_unsent();
                let spinner = ui::Spinner::start(&config.display);
                let mut ai_result = ai_session.send(&context, &prompt);
                spinner.stop();

                // AIとの対話ループ: コマンド実行→結果をAIに送信→分析→繰り返し
                loop {
                    match ai_result {
                        Ok(response) => {
                            ring_buffer.mark_sent();
                            ui::print_ai_message(&response.message, &config.display);

                            // コマンド提案がない場合は対話終了
                            if response.commands.is_empty() {
                                break;
                            }

                            ui::print_ai_commands(&response.commands, &config.display);

                            // コマンドを1つずつ確認＋実行
                            let total = response.commands.len();
                            let mut any_executed = false;
                            let mut executed_summary: Vec<String> = Vec::new();
                            for (i, cmd) in response.commands.iter().enumerate() {
                                ui::print_single_confirm_prompt(
                                    cmd,
                                    i + 1,
                                    total,
                                    &config.display,
                                );
                                let _ = prompt_tx
                                    .send(ui::InputRequest::ReadLine(String::new()));
                                let confirmed = loop {
                                    match input_rx.recv() {
                                        Ok(ui::InputEvent::Line(line)) => {
                                            break ui::parse_confirm(&line)
                                        }
                                        Ok(ui::InputEvent::PtyData(_))
                                        | Ok(ui::InputEvent::PassthroughEnded) => continue,
                                        Ok(ui::InputEvent::AiPrompt(_)) => continue,
                                        Err(_) => break false,
                                    }
                                };

                                if !confirmed {
                                    continue;
                                }

                                any_executed = true;

                                // ユーザが承認したコマンドをそのまま PTY に送る。ラップしない。
                                pty.write(format!("{cmd}\n").as_bytes())?;
                                debug_log(&format!("=== exec start: {cmd}"));

                                // コマンド実行完了待ち（passive 検出）。
                                // - PTY 出力をドレインして画面 / リングバッファ / sniffer へ
                                // - stdin → PTY 転送（パスワード入力・Ctrl+C 中断・対話応答）
                                // - SIGWINCH 検知（リサイズ追従）
                                // - 完了判定: PTY 出力末尾がプロンプト形 + 200ms 静音
                                // - alt screen 利用検知: top/vim 等が DECSTBM を破壊することへの備え
                                let quiet_threshold = Duration::from_millis(200);
                                let mut sniffer = prompt_sniffer::PromptSniffer::new();
                                let mut last_pty_activity = Instant::now();
                                let mut tui_detected = false;
                                let mut chunk_count = 0usize;
                                loop {
                                    if ui::check_and_clear_sigwinch() {
                                        let (new_rows, new_cols) = ui::terminal_size();
                                        let new_pty_rows =
                                            new_rows.saturating_sub(1).max(1);
                                        let _ = pty.resize(new_pty_rows, new_cols);
                                        ui::resize_status_bar(new_rows);
                                    }
                                    let mut got_pty = false;
                                    while let Ok(data) = pty_rx.try_recv() {
                                        chunk_count += 1;
                                        if chunk_count <= 3 {
                                            debug_log(&format!(
                                                "pty chunk #{} ({} bytes): {}",
                                                chunk_count,
                                                data.len(),
                                                debug_bytes(&data, 200)
                                            ));
                                        }
                                        io::stdout().write_all(&data)?;
                                        io::stdout().flush()?;
                                        ring_buffer.append(&data);
                                        sniffer.feed(&data);
                                        if !tui_detected && contains_tui_signature(&data) {
                                            tui_detected = true;
                                            debug_log("tui_detected = true");
                                        }
                                        got_pty = true;
                                    }
                                    if got_pty {
                                        last_pty_activity = Instant::now();
                                    }
                                    let stdin_bytes = ui::drain_stdin_nonblocking();
                                    if !stdin_bytes.is_empty() {
                                        pty.write(&stdin_bytes)?;
                                    }
                                    if last_pty_activity.elapsed() >= quiet_threshold
                                        && sniffer.matches_prompt()
                                    {
                                        sniffer.record_match();
                                        break;
                                    }
                                    thread::sleep(Duration::from_millis(20));
                                }

                                // 完了後 status bar を再描画する。TUI が origin mode を
                                // on にしたまま抜けた可能性があるなら一回限りの DECOM
                                // reset (\x1b[?6l) と \n 送信で復旧する。
                                let (rows, _cols) = ui::terminal_size();
                                debug_log(&format!(
                                    "exec end: tui_detected={}, chunks={}",
                                    tui_detected, chunk_count
                                ));
                                if tui_detected {
                                    debug_log("[wait loop] tui recovery: Ctrl+L to shell");
                                    io::stdout().write_all(b"\x1b[r")?;
                                    io::stdout().flush()?;
                                    pty.write(b"\x0c")?;
                                    thread::sleep(Duration::from_millis(200));
                                    while let Ok(data) = pty_rx.try_recv() {
                                        io::stdout().write_all(&data)?;
                                        ring_buffer.append(&data);
                                    }
                                    io::stdout().flush()?;
                                }
                                ui::resize_status_bar(rows);

                                executed_summary.push(format!("`{cmd}`"));
                            }

                            if !any_executed {
                                break;
                            }

                            // 実行結果をAIに送信して分析を継続
                            let follow_up_context = ring_buffer.get_unsent();
                            println!();
                            let spinner = ui::Spinner::start(&config.display);
                            let follow_up_text = format!(
                                "実行したコマンド: {}。出力は terminal フェンスに含まれます。分析してください。追加の操作が必要であれば提案してください。",
                                executed_summary.join(", ")
                            );
                            ai_result = ai_session.send(&follow_up_context, &follow_up_text);
                            spinner.stop();
                        }
                        Err(e) => {
                            if e.to_string() == ai::CANCELLED {
                                eprintln!("^C");
                            } else {
                                eprintln!("AI error: {e}");
                            }
                            break;
                        }
                    }
                }

                // AI対話終了後、シェルのプロンプトを再表示させる
                pty.write(b"\n")?;
                thread::sleep(Duration::from_millis(200));
                let mut first = true;
                while let Ok(data) = pty_rx.try_recv() {
                    let output = if first {
                        first = false;
                        // 先頭の改行を除去してプロンプトだけ表示
                        let trimmed = data.iter()
                            .position(|&b| b != b'\r' && b != b'\n')
                            .unwrap_or(data.len());
                        &data[trimmed..]
                    } else {
                        &data
                    };
                    if !output.is_empty() {
                        io::stdout().write_all(output)?;
                        io::stdout().flush()?;
                    }
                    ring_buffer.append(&data);
                }
                input_idle = true;
                pending_input = true;
                last_pty_output = Instant::now();
            }
            Ok(ui::InputEvent::Line(line)) => {
                input_idle = true;
                match ui::parse_input(&line) {
                    ui::UserInput::Exit => {
                        pty.write(b"exit\n")?;
                        pending_input = true;
                        last_pty_output = Instant::now();
                    }
                    ui::UserInput::ShellCommand(cmd) => {
                        pty.write(format!("{cmd}\n").as_bytes())?;
                        pending_input = true;
                        last_pty_output = Instant::now();
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(mpsc::TryRecvError::Disconnected) => break,
        }
    }

    // ステータスバー・スクロール領域をクリーンアップ
    let (final_rows, _) = ui::terminal_size();
    ui::cleanup_status_bar(final_rows);

    if let Some(sid) = ai_session.session_id() {
        eprintln!("\nResume this session with:\nclaude --resume {sid}");
    }

    Ok(())
}

fn main() {
    match parse_args() {
        CliAction::Version => {
            println!("aish {}", env!("CARGO_PKG_VERSION"));
        }
        CliAction::Update => {
            if let Err(e) = update::run_update() {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        CliAction::Run(args) => {
            let result = run(args);
            // ターミナルタイトルを復元
            print!("\x1b]2;\x07");
            io::stdout().flush().ok();
            ui::restore_terminal_settings();
            if let Err(e) = result {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
    }
}
