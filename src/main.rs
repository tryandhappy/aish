mod ai;
mod config;
mod marker;
mod mode;
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
    // ReadLine用ラベル（行末クリアなし — 入力エリアに背景色を漏らさない）
    let aish_label_readline = format!(
        "{}{}\x1b[0m ",
        config.display.prompt_color,
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
        }

        // PTY出力が落ち着いたらステータスバーとスクロール領域を復元
        if status_bar_needs_refresh && last_pty_output.elapsed() > Duration::from_millis(50) {
            let (rows, _cols) = ui::terminal_size();
            ui::resize_status_bar(rows);
            status_bar_needs_refresh = false;
        }

        // PTY出力が落ち着いたら入力スレッドを起動
        if pending_input && input_idle && last_pty_output.elapsed() > Duration::from_millis(50) {
            let request = if mode.accepts_shell_command() {
                ui::InputRequest::Passthrough(String::new())
            } else {
                ui::InputRequest::ReadLine(aish_label_readline.clone())
            };
            let _ = prompt_tx.send(request);
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
                if mode.accepts_shell_command() {
                    let _ = pty.write(&data);
                }
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
            Ok(ui::InputEvent::CtrlCExit) => break,
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

                            if !mode.accepts_shell_command() {
                                ui::print_ai_message(
                                    "(Commands cannot be executed in current mode)",
                                    &config.display,
                                );
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
                                        Ok(ui::InputEvent::CtrlCExit) => break false,
                                        Err(_) => break false,
                                    }
                                };

                                if !confirmed {
                                    continue;
                                }

                                any_executed = true;

                                // マーカーラッパで完了を厳密検出する。
                                // ヒアドキュメント・末尾 & ・未閉じクォート等の場合は
                                // 素のコマンドを送信し 500ms 無音ヒューリスティックにフォールバック。
                                let (cmd_bytes, mut scanner) = match marker::wrap_command(cmd) {
                                    Some((wrapped, id)) => {
                                        (wrapped, Some(marker::MarkerScanner::new(&id)))
                                    }
                                    None => (format!("{cmd}\n"), None),
                                };
                                pty.write(cmd_bytes.as_bytes())?;

                                // コマンド実行完了待ち。
                                // - PTY 出力をドレイン（マーカースキャナ通過後、画面・リングバッファへ）
                                // - stdin → PTY 転送（パスワード入力・Ctrl+C 中断・対話応答）
                                // - SIGWINCH 検知（リサイズ追従）
                                // - 完了判定: マーカー検出 / フォールバックは 500ms 無音
                                let quiet_threshold = Duration::from_millis(500);
                                let mut last_pty_activity = Instant::now();
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
                                        let to_emit = if let Some(s) = scanner.as_mut() {
                                            s.feed(&data)
                                        } else {
                                            data
                                        };
                                        if !to_emit.is_empty() {
                                            io::stdout().write_all(&to_emit)?;
                                            io::stdout().flush()?;
                                            ring_buffer.append(&to_emit);
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
                                    if let Some(s) = scanner.as_ref() {
                                        if s.marker_found() {
                                            break;
                                        }
                                    } else if last_pty_activity.elapsed() >= quiet_threshold {
                                        break;
                                    }
                                    thread::sleep(Duration::from_millis(20));
                                }

                                // 実行サマリを記録（exit code 取れれば付ける）
                                let summary = match scanner
                                    .as_ref()
                                    .and_then(|s| s.exit_code())
                                {
                                    Some(rc) => format!("`{cmd}` (exit {rc})"),
                                    None => format!("`{cmd}`"),
                                };
                                executed_summary.push(summary);
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
                if mode.accepts_shell_command() {
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
