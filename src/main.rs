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
                    ui::UserInput::AiPrompt(prompt) => {
                        if prompt.is_empty() {
                            continue;
                        }
                        let context = ring_buffer.get_unsent();
                        ui::print_ai_message("Thinking...");

                        match ai_session.send(&context, &prompt) {
                            Ok(response) => {
                                ring_buffer.mark_sent();
                                ui::print_ai_message(&response.message);

                                if !response.commands.is_empty()
                                    && mode.accepts_shell_command()
                                    && ui::confirm_execution(&response.commands)
                                {
                                    for cmd in &response.commands {
                                        pty.write(format!("{}\n", cmd).as_bytes())?;
                                        // コマンド間に少し待機
                                        thread::sleep(Duration::from_millis(500));
                                        // 出力を読み取る
                                        while let Ok(data) = pty_rx.try_recv() {
                                            io::stdout().write_all(&data)?;
                                            io::stdout().flush()?;
                                            ring_buffer.append(&data);
                                        }
                                    }
                                } else if !response.commands.is_empty()
                                    && !mode.accepts_shell_command()
                                {
                                    ui::print_ai_message(
                                        "(Commands cannot be executed in current mode)",
                                    );
                                }
                            }
                            Err(e) => {
                                eprintln!("AI error: {}", e);
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
    if let Err(e) = run() {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
