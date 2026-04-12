mod ai;
mod config;
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
extern "C" fn sigint_handler(_sig: libc::c_int) {
    ui::record_ctrl_c();
}

#[cfg(unix)]
extern "C" fn sigwinch_handler(_sig: libc::c_int) {
    ui::record_sigwinch();
}

/// ANSIエスケープシーケンスをbash PS1用エスケープに変換
/// 例: "\x1b[36m" → "\\[\\e[36m\\]"
fn ansi_to_ps1_escape(ansi: &str) -> String {
    if ansi.is_empty() {
        return String::new();
    }
    let inner = ansi.replace('\x1b', "\\e");
    format!("\\[{}\\]", inner)
}

fn run(args: AishArgs) -> Result<(), Box<dyn std::error::Error>> {
    ui::save_terminal_settings();

    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGINT, sigint_handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGWINCH, sigwinch_handler as *const () as libc::sighandler_t);
    }

    if !ai::check_claude_installed() {
        eprintln!("Please install Claude Code.");
        eprintln!("curl -fsSL https://claude.ai/install.sh | bash");
        std::process::exit(1);
    }

    let config = config::Config::load(args.config_path.as_deref());

    let (term_rows, term_cols) = ui::terminal_size();

    let mut mode = if args.ssh_args.is_empty() {
        Mode::Local
    } else {
        Mode::Remote
    };

    let mut pty = if mode == Mode::Local {
        pty_handler::PtyHandler::spawn_local_shell(term_rows, term_cols)?
    } else {
        pty_handler::PtyHandler::spawn_ssh(&args.ssh_args, term_rows, term_cols)?
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
    print!("\x1b]2;{}\x07", title);
    io::stdout().flush().ok();

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
                        print!("{}", prompt);
                        io::stdout().flush().ok();
                    }
                    ui::passthrough_read(&input_tx, &input_bg, &input_aish_label);
                }
                ui::InputRequest::ReadLine(prompt) => {
                    if !prompt.is_empty() {
                        print!("{}", prompt);
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
    let mut last_ctrl_c_count: u32 = 0;
    let mut last_ctrl_c_time = Instant::now();
    let mut ctrl_c_hint_until: Option<Instant> = None;
    let mut prompt_command_set = false;
    let mut prompt_setup_suppress = false;
    let mut prompt_setup_buf: Vec<u8> = Vec::new();
    // PROMPT_COMMANDセットアップ用コマンドを事前構築（設定ファイルの色・ラベルを使用）
    let ps1_color = ansi_to_ps1_escape(&config.display.shell_prefix_color);
    let ps1_reset = if ps1_color.is_empty() { "" } else { "\\[\\e[0m\\]" };
    let prompt_setup_cmd = format!(
        " __ap=\"$PS1\";__ac=\"$PROMPT_COMMAND\";PROMPT_COMMAND='PS1=\"{}{}{} $__ap\";eval \"$__ac\"';trap 'PS1=\"$__ap\";PROMPT_COMMAND=\"$__ac\"' EXIT\n",
        ps1_color, config.display.shell_prefix_label, ps1_reset
    );

    // メインループ
    loop {
        // 端末リサイズ検出
        if ui::check_and_clear_sigwinch() {
            let (new_rows, new_cols) = ui::terminal_size();
            let _ = pty.resize(new_rows, new_cols);
        }

        // Ctrl+C連打チェック
        let cc = ui::ctrl_c_count();
        if cc > last_ctrl_c_count {
            let now = Instant::now();
            if cc - last_ctrl_c_count >= 2
                || (last_ctrl_c_count > 0
                    && now.duration_since(last_ctrl_c_time) < Duration::from_secs(2))
            {
                eprintln!();
                break;
            }
            last_ctrl_c_count = cc;
            last_ctrl_c_time = now;
            ctrl_c_hint_until = Some(now + Duration::from_secs(2));
        }

        // Ctrl+Cヒントの期限切れチェック
        if let Some(deadline) = ctrl_c_hint_until {
            if Instant::now() >= deadline {
                ctrl_c_hint_until = None;
            }
        }

        // PTY出力をチェック
        while let Ok(data) = pty_rx.try_recv() {
            if prompt_setup_suppress {
                prompt_setup_buf.extend_from_slice(&data);
            } else if !ui::minibuffer_active() {
                io::stdout().write_all(&data)?;
                io::stdout().flush()?;
            }
            ring_buffer.append(&data);
            last_pty_output = Instant::now();
        }

        // PROMPT_COMMANDセットアップ: [aish]プレフィックスをPS1に追加
        if !prompt_command_set && mode.accepts_shell_command()
            && pending_input && input_idle
            && last_pty_output.elapsed() > Duration::from_millis(50)
        {
            prompt_setup_suppress = true;
            let _ = pty.write(prompt_setup_cmd.as_bytes());
            prompt_command_set = true;
            last_pty_output = Instant::now();
            continue;
        }

        // セットアップ出力の抑制終了→バナー表示＋新しいプロンプト表示
        if prompt_setup_suppress && last_pty_output.elapsed() > Duration::from_millis(50) {
            prompt_setup_suppress = false;
            // 起動バナー
            eprint!("\r\x1b[2K{}aish v{} | Ctrl+/ for AI\x1b[0m\r\n",
                config.display.header_color, env!("CARGO_PKG_VERSION"));
            // 抑制した出力の最後の行（新しいプロンプト）を表示
            if let Some(pos) = prompt_setup_buf.iter().rposition(|&b| b == b'\n') {
                let prompt_line = &prompt_setup_buf[pos + 1..];
                if !prompt_line.is_empty() {
                    io::stdout().write_all(prompt_line)?;
                    io::stdout().flush()?;
                }
            }
            prompt_setup_buf.clear();
            last_pty_output = Instant::now();
        }

        // PTY出力が落ち着いたら入力スレッドを起動
        if pending_input && input_idle && last_pty_output.elapsed() > Duration::from_millis(50) {
            let request = if mode.accepts_shell_command() {
                ui::InputRequest::Passthrough(String::new())
            } else {
                let hint = if ctrl_c_hint_until.is_some() {
                    "\x1b[33m(Ctrl+C to exit)\x1b[0m "
                } else {
                    ""
                };
                ui::InputRequest::ReadLine(format!("{}{}", aish_label_readline, hint))
            };
            let _ = prompt_tx.send(request);
            pending_input = false;
            input_idle = false;
        }

        // PTYプロセスの終了チェック
        if alive_rx.try_recv().is_ok() {
            match mode {
                Mode::Remote => {
                    eprintln!("\nSSH session ended.");
                    mode = Mode::RemoteEnded;
                    ui::request_passthrough_exit();
                    pending_input = true;
                    last_pty_output = Instant::now();
                }
                Mode::Local => {
                    break;
                }
                Mode::RemoteEnded => {}
            }
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

                            ui::print_confirm_prompt(&response.commands, &config.display);
                            let _ = prompt_tx.send(ui::InputRequest::ReadLine(String::new()));
                            let confirmed = loop {
                                match input_rx.recv() {
                                    Ok(ui::InputEvent::Line(line)) => break ui::parse_confirm(&line),
                                    Ok(ui::InputEvent::PtyData(_))
                                    | Ok(ui::InputEvent::PassthroughEnded) => continue,
                                    Ok(ui::InputEvent::AiPrompt(_)) => continue,
                                    Ok(ui::InputEvent::CtrlCExit) => break false,
                                    Err(_) => break false,
                                }
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
                            let spinner = ui::Spinner::start(&config.display);
                            ai_result = ai_session.send(
                                &follow_up_context,
                                "コマンドの実行結果です。分析してください。追加の操作が必要であれば提案してください。",
                            );
                            spinner.stop();
                        }
                        Err(e) => {
                            if e.to_string() == ai::CANCELLED {
                                eprintln!("^C");
                            } else {
                                eprintln!("AI error: {}", e);
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
                    ui::UserInput::Exit => match mode {
                        Mode::Local | Mode::RemoteEnded => break,
                        Mode::Remote => {
                            pty.write(b"exit\n")?;
                            pending_input = true;
                            last_pty_output = Instant::now();
                        }
                    },
                    ui::UserInput::ShellCommand(cmd) => {
                        if mode.accepts_shell_command() {
                            pty.write(format!("{}\n", cmd).as_bytes())?;
                            pending_input = true;
                            last_pty_output = Instant::now();
                        } else if mode == Mode::RemoteEnded {
                            // RemoteEndedモードでは通常入力をAIプロンプトとして扱う
                            let trimmed = cmd.trim();
                            if trimmed.is_empty() {
                                pending_input = true;
                                last_pty_output = Instant::now();
                            } else {
                                let context = ring_buffer.get_unsent();
                                let spinner = ui::Spinner::start(&config.display);
                                let ai_result = ai_session.send(&context, trimmed);
                                spinner.stop();

                                match ai_result {
                                    Ok(response) => {
                                        ring_buffer.mark_sent();
                                        ui::print_ai_message(&response.message, &config.display);
                                        if !response.commands.is_empty() {
                                            ui::print_ai_message(
                                                "(Commands cannot be executed in current mode)",
                                                &config.display,
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        if e.to_string() == ai::CANCELLED {
                                            eprintln!("^C");
                                        } else {
                                            eprintln!("AI error: {}", e);
                                        }
                                    }
                                }
                                input_idle = true;
                                pending_input = true;
                                last_pty_output = Instant::now();
                            }
                        }
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(mpsc::TryRecvError::Disconnected) => break,
        }
    }

    // PROMPT_COMMANDの復元
    if prompt_command_set {
        let _ = pty.write(b" PS1=\"$__ap\";PROMPT_COMMAND=\"$__ac\";unset __ap __ac;trap - EXIT\n");
        thread::sleep(Duration::from_millis(200));
        while let Ok(_) = pty_rx.try_recv() {}
    }

    if let Some(sid) = ai_session.session_id() {
        eprintln!("\nResume this session with:\nclaude --resume {}", sid);
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
                eprintln!("Error: {}", e);
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
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
    }
}
