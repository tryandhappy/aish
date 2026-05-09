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
    ai_backend: Option<String>,
    ai_model: Option<String>,
    ai_effort: Option<String>,
    ssh_args: Vec<String>,
}

enum CliAction {
    Run(AishArgs),
    Update,
    Version,
    Help,
}

fn parse_args() -> CliAction {
    let args: Vec<String> = std::env::args().skip(1).collect();

    for arg in &args {
        match arg.as_str() {
            "--update" => return CliAction::Update,
            "--version" | "-V" => return CliAction::Version,
            "--help" => return CliAction::Help,
            _ => {}
        }
    }

    let mut config_path = None;
    let mut ai_backend = None;
    let mut ai_model = None;
    let mut ai_effort = None;
    let mut ssh_args = Vec::new();
    let mut i = 0;

    /// `--name VAL` (空白区切り) と `--name=VAL` (= 区切り) の両形式から値を取り出す。
    /// 戻り値: (value, advance) — value は取り出した値、advance は i を進める量 (1 または 2)。
    /// 値が無い (引数末尾 / `--name=` で空文字) 場合 None。
    fn take_value<'a>(args: &'a [String], i: usize, name: &str) -> Option<(&'a str, usize)> {
        let arg = args[i].as_str();
        if arg == name {
            args.get(i + 1).map(|v| (v.as_str(), 2))
        } else if let Some(rest) = arg.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
            if rest.is_empty() {
                None
            } else {
                Some((rest, 1))
            }
        } else {
            None
        }
    }

    while i < args.len() {
        let arg = args[i].as_str();
        // --config <path> / --config=<path>
        if arg == "--config" || arg.starts_with("--config=") {
            match take_value(&args, i, "--config") {
                Some((v, adv)) => {
                    config_path = Some(v.to_string());
                    i += adv;
                    continue;
                }
                None => {
                    eprintln!("Error: --config requires a value");
                    std::process::exit(1);
                }
            }
        }
        // --ai <kind> / --ai=<kind>
        if arg == "--ai" || arg.starts_with("--ai=") {
            match take_value(&args, i, "--ai") {
                Some((v, adv)) => {
                    ai_backend = Some(v.to_string());
                    i += adv;
                    continue;
                }
                None => {
                    eprintln!("Error: --ai requires a value (claude|codex|gemini|qwen)");
                    std::process::exit(1);
                }
            }
        }
        // --model <name> / --model=<name>
        if arg == "--model" || arg.starts_with("--model=") {
            match take_value(&args, i, "--model") {
                Some((v, adv)) => {
                    ai_model = Some(v.to_string());
                    i += adv;
                    continue;
                }
                None => {
                    eprintln!("Error: --model requires a value");
                    std::process::exit(1);
                }
            }
        }
        // --effort <level> / --effort=<level>
        if arg == "--effort" || arg.starts_with("--effort=") {
            match take_value(&args, i, "--effort") {
                Some((v, adv)) => {
                    ai_effort = Some(v.to_string());
                    i += adv;
                    continue;
                }
                None => {
                    eprintln!("Error: --effort requires a value (low|medium|high など)");
                    std::process::exit(1);
                }
            }
        }
        ssh_args.push(args[i].clone());
        i += 1;
    }

    CliAction::Run(AishArgs {
        config_path,
        ai_backend,
        ai_model,
        ai_effort,
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

/// PTY 出力に TUI コマンドが終了した形跡があるかを検出する。
/// 「終了」のシグナルだけを拾うのが重要: 動作中に発生し得るシーケンス
/// (`\x1b[2J`, DECSTBM, alt screen 突入) を拾うと、TUI 動作中に
/// recovery (Ctrl+L) を撃ち、insert モード中のバッファに ^L が紛れ込む等の
/// 誤動作を引き起こす。
/// 検出対象:
/// - alt screen 抜け (`\x1b[?1049l`、`\x1b[?1047l`、`\x1b[?47l`)
/// - 端末フルリセット (`\x1bc`, RIS)
fn contains_tui_signature(data: &[u8]) -> bool {
    // alt screen 終了のみ検出する。
    // 「TUI が終わった」ことを確実に示すのは alt screen からの抜け (`?1049l` 等)。
    // \x1b[2J や DECSTBM (\x1b[..r) は vim 等が動作中にも送出するため、
    // これらを拾うと TUI 内で Ctrl+L を撃ち、insert モード中のバッファに ^L が
    // 紛れ込むなどの誤動作を引き起こす。
    if data.windows(8).any(|w| w == b"\x1b[?1049l") {
        return true;
    }
    if data.windows(8).any(|w| w == b"\x1b[?1047l") {
        return true;
    }
    if data.windows(6).any(|w| w == b"\x1b[?47l") {
        return true;
    }
    if data.windows(2).any(|w| w == b"\x1bc") {
        return true;
    }
    false
}

/// aish プロンプト入力が slash command か判定し、該当すれば処理する。
/// 戻り値:
///   `None` — slash command ではない (通常の AI プロンプトとして送信せよ)
///   `Some(message)` — 処理済み。`message` をユーザに表示し AI 送信はスキップ
fn try_handle_slash_command(
    input: &str,
    ai_session: &mut Box<dyn ai::AiBackend>,
    config: &config::Config,
    kind: &mut ai::BackendKind,
) -> Option<String> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let mut parts = trimmed[1..].splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let value = parts.next().map(str::trim).filter(|s| !s.is_empty());
    match cmd {
        "help" => Some(
            "available slash commands:\n\
             /help                    show this help\n\
             /effort [low|medium|high|...]   set reasoning effort (no value = clear)\n\
             /model  [<name>]         set model (no value = clear, fall back to config/default)\n\
             /clear                   clear conversation history / session\n\
             /ai     <claude|codex|gemini|qwen>   switch AI backend"
                .to_string(),
        ),
        "effort" => {
            ai_session.set_effort(value);
            let label = value.unwrap_or("(cleared)");
            Some(format!("effort: {label}"))
        }
        "model" => {
            ai_session.set_model(value);
            let label = value.unwrap_or("(cleared)");
            Some(format!("model: {label}"))
        }
        "clear" => {
            ai_session.clear_history();
            Some("history cleared".to_string())
        }
        "ai" => {
            let Some(v) = value else {
                return Some("/ai requires a backend (claude|codex|gemini|qwen)".to_string());
            };
            let new_kind = match ai::BackendKind::parse(v) {
                Ok(k) => k,
                Err(e) => return Some(format!("/ai: {e}")),
            };
            if new_kind == *kind {
                return Some(format!("ai: already using {}", new_kind.as_str()));
            }
            if !ai::check_installed(new_kind) {
                return Some(format!(
                    "ai: backend `{}` is not installed or not on PATH",
                    new_kind.as_str()
                ));
            }
            match ai::create_backend(new_kind, &config.ai, &config.log) {
                Ok(new_session) => {
                    *ai_session = new_session;
                    *kind = new_kind;
                    Some(format!("ai backend switched to {}", new_kind.as_str()))
                }
                Err(e) => Some(format!("ai: failed to switch: {e}")),
            }
        }
        other => Some(format!(
            "unknown slash command: /{other}  (try /help)"
        )),
    }
}

/// 終了時にユーザに表示する情報をまとめた構造体。
/// 端末を cooked モードに戻した後で表示するために `run()` の戻り値とする。
struct ExitInfo {
    /// 各 backend の resume コマンド例 (例: `claude --resume <UUID>`)。
    resume_command: Option<String>,
    /// その resume が紐づく backend 識別名 (claude / codex / ...)。
    backend_name: String,
}

fn run(args: AishArgs) -> Result<ExitInfo, Box<dyn std::error::Error>> {
    // 二重起動防止 (nested only): aish が起動した子シェルには AISH_PID=<aish_pid> を渡すため、
    // その子シェル (またはその子孫) から aish を再起動すると環境変数を継承してこのチェックに当たる。
    // PID が現在も生きていれば nested と判断して refuse。stale (kill -0 失敗) なら無視して続行。
    // 別ターミナルで起動した場合は環境変数を共有しないので、複数の aish を並行起動できる。
    if let Ok(pid_str) = std::env::var("AISH_PID") {
        if let Ok(pid) = pid_str.parse::<i32>() {
            #[cfg(unix)]
            let alive = unsafe { libc::kill(pid, 0) == 0 };
            #[cfg(not(unix))]
            let alive = false;
            if alive {
                return Err(format!(
                    "aish is already running in this shell (PID {pid}).\n\
                     Run `exit` to leave the parent aish first, or open a new terminal."
                )
                .into());
            }
        }
    }

    ui::save_terminal_settings();

    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGWINCH, sigwinch_handler as *const () as libc::sighandler_t);
    }

    let mut config = config::Config::load(args.config_path.as_deref())?;

    // モデル名の決定: --model > [ai].model > 既存 extra_args の -m
    // CLI 指定があれば config.ai.model を上書きし、各 backend の new() で extra_args に注入される。
    if let Some(m) = args.ai_model.as_deref() {
        config.ai.model = m.to_string();
    }

    // effort の決定: --effort > [ai].effort
    // claude / codex のみ対応、gemini / qwen は CLI 側に該当フラグが無いので無視される。
    if let Some(e) = args.ai_effort.as_deref() {
        config.ai.effort = e.to_string();
    }

    // バックエンド種別の決定: --ai > [ai].backend > "claude"
    // どちらの経路でも未知の値はエラーで弾く（CLI と config の挙動を揃える）。
    // `/ai` slash command で実行中に切り替えできるよう mut。
    let mut kind = match args.ai_backend.as_deref() {
        Some(s) => ai::BackendKind::parse(s).map_err(|e| format!("--ai: {e}"))?,
        None => ai::BackendKind::parse(&config.ai.backend)
            .map_err(|e| format!("[ai].backend in config: {e}"))?,
    };

    if !ai::check_installed(kind) {
        // 直接 exit すると main の restore_terminal_settings / cleanup_terminal_indicator が
        // 走らず端末が raw モードで残るので、必ず Err で抜けて main 側のクリーンアップを通す。
        return Err(match kind {
            ai::BackendKind::Claude => {
                "Please install Claude Code.\ncurl -fsSL https://claude.ai/install.sh | bash"
                    .into()
            }
            other => format!(
                "Backend `{}` is not installed or not on PATH.",
                other.as_str()
            )
            .into(),
        });
    }

    // バックエンド初期化は PTY 起動より先に行う。
    // 未実装バックエンド (codex/gemini/qwen) を選んだ場合、PTY や SSH を立ち上げる前に
    // ここで NotInstalled エラーを返してプロセスを終了させる。
    let mut ai_session: Box<dyn ai::AiBackend> = ai::create_backend(kind, &config.ai, &config.log)
        .map_err(|e| format!("Failed to initialize AI backend: {e}"))?;

    let (term_rows, term_cols) = ui::terminal_size();
    let pty_rows = term_rows;

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

    // ターミナル側に "aish 動作中" を示す OSC を送る:
    //   OSC 0/1/2: ウィンドウ/タブ/アイコンタイトル
    //   OSC 10/11/12: 前景/背景/カーソル色 (config で空文字なら送らない)
    // PTY コンテンツ領域には干渉しないので、fullscreen アプリ等と衝突しない。
    let title = if args.ssh_args.is_empty() {
        config.display.shell_prefix_label.clone()
    } else {
        format!("{} {}", config.display.shell_prefix_label, args.ssh_args.join(" "))
    };
    ui::setup_terminal_indicator(
        &title,
        &config.display.term_fg_color,
        &config.display.term_bg_color,
        &config.display.term_cursor_color,
    );

    // 起動バナー: 1 度だけ画面上部に表示する (status bar は廃止)
    // backend ごとに色を変える 2 行 ASCII アート + バージョン・モデル・effort・キーヒント。
    // model / effort 未指定時はその欄を省略する。
    let banner_model = ai_session.model();
    let banner_effort = ai_session.effort();
    ui::print_startup_banner(
        kind.as_str(),
        banner_model.as_deref(),
        banner_effort.as_deref(),
        env!("CARGO_PKG_VERSION"),
    );

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
                    // None は Ctrl+C / EOF。確認プロンプト側で「残り全部キャンセル」として扱う。
                    let event = match ui::read_line() {
                        Some(line) => ui::InputEvent::Line(line),
                        None => ui::InputEvent::ReadLineCancelled,
                    };
                    if input_tx.send(event).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut pending_input = true; // 入力スレッド起動待ち
    let mut input_idle = true;
    let mut last_pty_output = Instant::now();
    // passthrough モードで TUI コマンド (top/vim/less 等) が走った形跡。
    // 検出されると PTY 出力が落ち着いたタイミングで Ctrl+L 復旧を実行する。
    let mut tui_recovery_pending = false;

    // メインループ
    'main_loop: loop {
        // 端末リサイズ検出
        if ui::check_and_clear_sigwinch() {
            let (new_rows, new_cols) = ui::terminal_size();
            let _ = pty.resize(new_rows, new_cols);
        }

        // PTY出力をチェック
        while let Ok(data) = pty_rx.try_recv() {
            if !ui::minibuffer_active() {
                io::stdout().write_all(&data)?;
                io::stdout().flush()?;
            }
            ring_buffer.append(&data);
            last_pty_output = Instant::now();
            if !tui_recovery_pending && contains_tui_signature(&data) {
                tui_recovery_pending = true;
                debug_log(&format!(
                    "[main loop] tui_signature detected: {}",
                    debug_bytes(&data, 200)
                ));
            }
        }

        // PTY出力が落ち着いたら TUI コマンド (top 等) からの復帰処理を行う。
        // shell に Ctrl+L を送って画面クリア + プロンプト再描画を **shell 自身に** 任せる。
        // aish 側で escape を組み立てるよりも端末固有のクセに強い。
        if tui_recovery_pending && last_pty_output.elapsed() > Duration::from_millis(50) {
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

        // PTY出力が落ち着いたら入力スレッドを起動
        if pending_input && input_idle && last_pty_output.elapsed() > Duration::from_millis(50) {
            let _ = prompt_tx.send(ui::InputRequest::Passthrough(String::new()));
            pending_input = false;
            input_idle = false;
        }

        // PTYプロセスの終了チェック。
        // EOF 検出 (alive_rx) だけだと、子プロセスが exit しても master read が
        // EOF を返さないケース (リモート再起動による ssh 切断等) で
        // pty_reader スレッドが read() でブロックしたままになり詰まる。
        // child.try_wait() による能動検出も併用する。
        if alive_rx.try_recv().is_ok() || !pty.is_alive() {
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
                // slash command (/help, /effort, /model, /clear, /ai) はローカルで処理し AI には送らない。
                if let Some(msg) =
                    try_handle_slash_command(&prompt, &mut ai_session, &config, &mut kind)
                {
                    ui::print_slash_result(&msg);
                    // shell プロンプトをリフレッシュ。aish プロンプトを開いた時点で minibuffer 側が
                    // (in-progress 入力があれば) Ctrl+C で行を空にしているので、ここで `\n` を送ると
                    // 空コマンド扱いで新規プロンプトだけが描画される。
                    let _ = pty.write(b"\n");
                    pending_input = true;
                    last_pty_output = Instant::now();
                    continue;
                }
                let context = ring_buffer.get_unsent();
                let spinner = ui::Spinner::start(&config.display);
                let mut ai_result = ai_session.send(&ai::AiRequest {
                    terminal_context: &context,
                    user_prompt: &prompt,
                });
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
                            // ユーザが [a] (= all) を選んだ後は残りを自動承認する
                            let mut auto_approve_remaining = false;
                            // ユーザが Ctrl+C で残り全部キャンセルを選んだ
                            let mut user_cancelled = false;
                            for (i, cmd) in response.commands.iter().enumerate() {
                                let confirmed = if auto_approve_remaining {
                                    true
                                } else {
                                    ui::print_single_confirm_prompt(
                                        cmd,
                                        i + 1,
                                        total,
                                        &config.display,
                                    );
                                    let _ = prompt_tx
                                        .send(ui::InputRequest::ReadLine(String::new()));
                                    loop {
                                        match input_rx.recv() {
                                            Ok(ui::InputEvent::Line(line)) => {
                                                match ui::parse_confirm(&line) {
                                                    ui::ConfirmChoice::Yes => break true,
                                                    ui::ConfirmChoice::No => break false,
                                                    ui::ConfirmChoice::All => {
                                                        auto_approve_remaining = true;
                                                        break true;
                                                    }
                                                }
                                            }
                                            Ok(ui::InputEvent::ReadLineCancelled) => {
                                                // Ctrl+C: 残りすべてをキャンセル
                                                user_cancelled = true;
                                                break false;
                                            }
                                            Ok(ui::InputEvent::PtyData(_))
                                            | Ok(ui::InputEvent::PassthroughEnded) => continue,
                                            Ok(ui::InputEvent::AiPrompt(_)) => continue,
                                            Err(_) => break false,
                                        }
                                    }
                                };

                                if !confirmed {
                                    if user_cancelled {
                                        break;
                                    }
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
                                    // 子プロセス (ssh / shell) が死んだら待っても意味がない。
                                    // sudo reboot 等で SSH が切れた後はプロンプトに戻らないため、
                                    // sniffer ベースの完了判定だと永遠にハングする。
                                    // 残り出力をドレインしてメインループごと抜ける。
                                    if !pty.is_alive() {
                                        thread::sleep(Duration::from_millis(50));
                                        while let Ok(data) = pty_rx.try_recv() {
                                            io::stdout().write_all(&data)?;
                                            ring_buffer.append(&data);
                                        }
                                        io::stdout().flush().ok();
                                        break 'main_loop;
                                    }
                                    if ui::check_and_clear_sigwinch() {
                                        let (new_rows, new_cols) = ui::terminal_size();
                                        let _ = pty.resize(new_rows, new_cols);
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

                                // 完了後、TUI が DECSTBM や origin mode を残したまま抜けた
                                // 可能性があるなら shell に Ctrl+L を送って復旧する。
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

                                executed_summary.push(format!("`{cmd}`"));
                            }

                            if !any_executed {
                                break;
                            }

                            // 実行結果をAIに送信して分析を継続
                            let follow_up_context = ring_buffer.get_unsent();
                            println!();
                            let spinner = ui::Spinner::start(&config.display);
                            let follow_up_text = if user_cancelled {
                                format!(
                                    "ユーザが Ctrl+C で残りのコマンドをキャンセルしました。実行されたコマンド: {}。出力は terminal フェンスに含まれます。実行された分だけで分析してください。",
                                    executed_summary.join(", ")
                                )
                            } else {
                                format!(
                                    "実行したコマンド: {}。出力は terminal フェンスに含まれます。分析してください。追加の操作が必要であれば提案してください。",
                                    executed_summary.join(", ")
                                )
                            };
                            ai_result = ai_session.send(&ai::AiRequest {
                                terminal_context: &follow_up_context,
                                user_prompt: &follow_up_text,
                            });
                            spinner.stop();
                        }
                        Err(e) => {
                            if matches!(e, ai::AiError::Cancelled) {
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
            Ok(ui::InputEvent::ReadLineCancelled) => {
                // メインループでは ReadLine を発行していない (AI 確認時のみ) ので
                // ここに来るのは想定外。安全側で無視する。
                continue;
            }
            Err(mpsc::TryRecvError::Empty) => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(mpsc::TryRecvError::Disconnected) => break,
        }
    }

    // 終了時の DECSTBM リセットはここでは送らない。
    // \x1b[r は VT100 仕様上、引数有無にかかわらずカーソルを (1,1) に
    // 移動させる副作用があり、aish 終了直後に親シェル画面の先頭に
    // カーソルが飛んでしまう。
    // minibuffer 終了時 (ui::show_minibuffer) と TUI コマンド復旧時
    // (main loop / 確認ループ内) でそれぞれ \x1b[r を送っているので、
    // 通常の終了経路ではここでリセットしなくても DECSTBM は default のはず。

    // 終了時に表示する resume 情報は raw モードを抜けた後に main() 側で出す。
    // backend ごとに resume_command() trait 実装が形式を返す:
    //   claude → `claude --resume <UUID>`
    //   codex  → `codex resume <UUID>`
    //   gemini → `gemini --resume latest` (best-effort)
    //   qwen   → `qwen --continue`        (best-effort)
    Ok(ExitInfo {
        resume_command: ai_session.resume_command(),
        backend_name: kind.as_str().to_string(),
    })
}

fn print_help() {
    let version = env!("CARGO_PKG_VERSION");
    println!("\
aish v{version} — CLI SSH + AI

USAGE:
    aish [AISH_OPTIONS] [SSH_ARGS...]   Remote: ssh user@host etc. (引数はそのまま ssh に渡す)
    aish [AISH_OPTIONS]                 Local:  $SHELL を起動
    aish --version | --update | --help

AISH OPTIONS:
    --config <PATH>        設定ファイルパス (既定: ~/.aish/config.toml)
    --ai <KIND>            AI バックエンド: claude | codex | gemini | qwen (既定: claude)
                           [ai].backend より優先される
    --model <NAME>         使用モデル名 (例: sonnet, gpt-5, gemini-2.5-pro)
                           [ai].model および extra_args の -m 指定より優先される
    --effort <LEVEL>       reasoning effort (low | medium | high など)
                           claude: --effort、codex: -c model_reasoning_effort= に変換
                           gemini / qwen は CLI 非対応のため無視される

OTHER OPTIONS:
    --version, -V          バージョン表示
    --update               GitHub Releases から自己更新 (例: sudo aish --update)
    --help                 このヘルプを表示

KEYS (起動後):
    Ctrl+/                 aish プロンプトを開く (AI に質問)
    Y / Enter              提案コマンドを実行
    n                      この提案をスキップ
    a                      残りの提案をすべて自動承認
    Ctrl+C                 提案キャンセル / コマンド中断

SLASH COMMANDS (aish プロンプトに入力):
    /help                  利用可能な slash command を表示
    /effort [LEVEL]        reasoning effort を変更 (引数なしでクリア)
    /model  [NAME]         モデルを変更 (引数なしでクリア)
    /clear                 会話履歴 / セッションをクリア
    /ai     <KIND>         AI バックエンドを切り替え (claude|codex|gemini|qwen)

EXAMPLES:
    aish                                # ローカルシェルを Claude で
    aish user@host                      # SSH 接続を Claude で
    aish --ai codex                     # ローカルシェルを Codex で
    aish --ai gemini user@host          # SSH を Gemini で
    aish --model sonnet                 # Claude を sonnet モデルで起動
    aish --ai codex --model gpt-5
    aish --effort high                  # Claude を high reasoning で起動
    aish --ai codex --effort medium
    aish --config /path/to/config.toml --ai codex

CONFIG:
    ~/.aish/config.toml に [ai] backend = \"codex\" 等で既定を変更可能。
    詳細は config.toml.example および SPEC.md を参照。

REPOSITORY:
    https://github.com/tryandhappy/aish");

    // 末尾に ssh --help (= ssh 自身の usage) を追記。SSH_ARGS にどんな引数が
    // 渡せるかをそのまま見せる。ssh が PATH に無い・出力が空ならスキップ。
    println!();
    println!("SSH ARGUMENTS (`ssh --help`):");
    match std::process::Command::new("ssh").arg("--help").output() {
        Ok(out) => {
            let mut combined = out.stdout;
            combined.extend_from_slice(&out.stderr);
            let text = String::from_utf8_lossy(&combined);
            let trimmed = text.trim();
            if trimmed.is_empty() {
                println!("    (ssh が出力を返しませんでした)");
            } else {
                for line in trimmed.lines() {
                    println!("    {line}");
                }
            }
        }
        Err(_) => {
            println!("    (ssh コマンドが PATH に見つかりません)");
        }
    }
}

fn main() {
    match parse_args() {
        CliAction::Help => {
            print_help();
        }
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
            // OSC 0/1/2 (タイトル) と OSC 10/11/12 (色) をリセット
            ui::cleanup_terminal_indicator();
            ui::restore_terminal_settings();
            // ↑ ここで cooked モードに戻る。以降の println / eprintln は通常通り改行される。
            match result {
                Ok(info) => {
                    if let Some(cmd) = info.resume_command {
                        eprintln!(
                            "\nResume this {} session with:\n  {cmd}",
                            info.backend_name
                        );
                    }
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
}
