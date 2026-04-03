mod ai;
mod config;
mod mode;
mod pty_handler;
mod ring_buffer;
mod ui;

use mode::Mode;
use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

struct AishArgs {
    config_path: Option<String>,
    ssh_args: Vec<String>,
}

fn parse_args() -> AishArgs {
    let args: Vec<String> = std::env::args().skip(1).collect();
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

    AishArgs {
        config_path,
        ssh_args,
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    ui::save_terminal_settings();

    if !ai::check_claude_installed() {
        eprintln!("Please install Claude Code.");
        eprintln!("curl -fsSL https://claude.ai/install.sh | bash");
        std::process::exit(1);
    }

    let args = parse_args();
    let config = config::Config::load(args.config_path.as_deref());

    let mut mode = if args.ssh_args.is_empty() {
        Mode::Local
    } else {
        Mode::Remote
    };

    let mut pty = if mode == Mode::Local {
        pty_handler::PtyHandler::spawn_local_shell()?
    } else {
        pty_handler::PtyHandler::spawn_ssh(&args.ssh_args)?
    };

    let mut ring_buffer = ring_buffer::RingBuffer::new();
    let mut ai_session = ai::AiSession::new(&config.system_prompt);

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

    // ユーザ入力を読み取るスレッド
    let (input_tx, input_rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        loop {
            match ui::read_line() {
                Some(line) => {
                    if input_tx.send(line).is_err() {
                        break;
                    }
                }
                None => break,
            }
        }
    });

    // メインループ
    loop {
        // PTY出力をチェック
        while let Ok(data) = pty_rx.try_recv() {
            io::stdout().write_all(&data)?;
            io::stdout().flush()?;
            ring_buffer.append(&data);
        }

        // PTYプロセスの終了チェック
        if alive_rx.try_recv().is_ok() {
            match mode {
                Mode::Remote => {
                    eprintln!("\nSSH session ended.");
                    mode = Mode::RemoteEnded;
                }
                Mode::Local => {
                    break;
                }
                Mode::RemoteEnded => {}
            }
        }

        // ユーザ入力をチェック
        match input_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(line) => {
                match ui::parse_input(&line) {
                    ui::UserInput::Exit => match mode {
                        Mode::Local | Mode::RemoteEnded => break,
                        Mode::Remote => {
                            pty.write(b"exit\n")?;
                        }
                    },
                    ui::UserInput::AiAnalyze | ui::UserInput::AiPrompt(_) => {
                        let initial_prompt = match ui::parse_input(&line) {
                            ui::UserInput::AiAnalyze => {
                                "表示されている内容を調べて、気になる点や問題点があれば解説してください。".to_string()
                            }
                            ui::UserInput::AiPrompt(p) => {
                                if p.is_empty() { continue; }
                                p
                            }
                            _ => unreachable!(),
                        };

                        let context = ring_buffer.get_unsent();
                        ui::print_ai_thinking();

                        let mut ai_result = ai_session.send(&context, &initial_prompt);

                        // AIとの対話ループ: コマンド実行→結果をAIに送信→分析→繰り返し
                        loop {
                            match ai_result {
                                Ok(response) => {
                                    ring_buffer.mark_sent();
                                    ui::print_ai_message(&response.message);

                                    // コマンド提案がない場合は対話終了
                                    if response.commands.is_empty() {
                                        break;
                                    }

                                    if !mode.accepts_shell_command() {
                                        ui::print_ai_message(
                                            "(Commands cannot be executed in current mode)",
                                        );
                                        break;
                                    }

                                    ui::print_confirm_prompt(&response.commands);
                                    let confirmed = match input_rx.recv() {
                                        Ok(line) => ui::parse_confirm(&line),
                                        Err(_) => false,
                                    };

                                    if !confirmed {
                                        break;
                                    }

                                    // コマンド実行
                                    for cmd in &response.commands {
                                        pty.write(format!("{}\n", cmd).as_bytes())?;
                                        thread::sleep(Duration::from_millis(500));
                                        while let Ok(data) = pty_rx.try_recv() {
                                            io::stdout().write_all(&data)?;
                                            io::stdout().flush()?;
                                            ring_buffer.append(&data);
                                        }
                                    }

                                    // 出力が落ち着くまで待機
                                    loop {
                                        thread::sleep(Duration::from_millis(500));
                                        let mut got_data = false;
                                        while let Ok(data) = pty_rx.try_recv() {
                                            io::stdout().write_all(&data)?;
                                            io::stdout().flush()?;
                                            ring_buffer.append(&data);
                                            got_data = true;
                                        }
                                        if !got_data {
                                            break;
                                        }
                                    }

                                    // 実行結果をAIに送信して分析を継続
                                    let follow_up_context = ring_buffer.get_unsent();
                                    print!("\n");
                                    ui::print_ai_thinking();
                                    ai_result = ai_session.send(
                                        &follow_up_context,
                                        "コマンドの実行結果です。分析してください。追加の操作が必要であれば提案してください。",
                                    );
                                }
                                Err(e) => {
                                    eprintln!("AI error: {}", e);
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
                    }
                    ui::UserInput::ShellCommand(cmd) => {
                        if mode.accepts_shell_command() {
                            pty.write(format!("{}\n", cmd).as_bytes())?;
                        } else {
                            eprintln!("Cannot execute commands in {} mode.", mode);
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(())
}

fn main() {
    let result = run();
    ui::restore_terminal_settings();
    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
