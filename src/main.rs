mod ai;
mod config;
mod conversation;
mod input;
mod input_gate;
mod mode;
mod prompt_sniffer;
mod pty_drain;
mod pty_handler;
mod ring_buffer;
mod ui;
mod update;
mod vetted_command;

use mode::Mode;
use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// 子プロセス死亡検出後、logout メッセージ等の残り出力が pty_rx に届くのを待つ時間。
/// (conversation.rs の完了待ちと main loop の終了 drain で共用)
const FINAL_DRAIN_WAIT: Duration = Duration::from_millis(50);

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
            _ => s.push_str(&format!("\\x{b:02x}")),
        }
    }
    if data.len() > max {
        s.push_str(&format!(" ... (+{} more bytes)", data.len() - max));
    }
    s
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
    ring_buffer: &mut ring_buffer::RingBuffer,
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
             /ai     <NAME>           switch AI backend (built-in: claude|codex|gemini|qwen|cursor|copilot, or any [[ai.providers]] name)"
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
            // current AI の session/history はリセット、ring_buffer の cursor は全 backend を末尾へ。
            // (他 backend の CLI セッション本体は本 phase では触らない — 完全リセットは将来課題)
            ai_session.clear_history();
            ring_buffer.mark_sent_all();
            Some("history cleared".to_string())
        }
        "ai" => {
            let Some(v) = value else {
                return Some(
                    "/ai requires a backend name (built-in: claude|codex|gemini|qwen|cursor|copilot, or any [[ai.providers]].name)".to_string(),
                );
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
        // 未知の /xxx は slash command として扱わず、そのまま AI プロンプトに流す。
        // 例: `/root/test.txt` のようなファイルパスや、`/foo bar` のような自然文を
        // AI に質問できるようにするため (タイポでも AI 側がフォローしやすい)。
        _ => None,
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
                return Err(format!("aish is already running here (PID {pid}).").into());
            }
        }
    }

    ui::save_terminal_settings();

    #[cfg(unix)]
    unsafe {
        libc::signal(
            libc::SIGWINCH,
            sigwinch_handler as *const () as libc::sighandler_t,
        );
    }

    let mut config = config::Config::load(args.config_path.as_deref())?;

    // `[[ai.providers]]` の registry を leak ベースで初期化。これ以降
    // `BackendKind::parse("generic:<name>")` が解決できるようになる。
    // 一度きりの呼び出し (OnceLock) なので nested aish 起動でも安全。
    ai::BackendKind::init_generics(&config.ai.providers);

    // モデル名の決定: --model > [ai].model > 既存 extra_args の -m
    // CLI 指定があれば config.ai.model を上書きし、各 backend の new() で extra_args に注入される。
    if let Some(m) = args.ai_model.as_deref() {
        config.ai.model = m.to_string();
    }

    // effort の決定: --effort > [ai].effort
    // claude / codex / copilot のみ native 対応、gemini / qwen / cursor は CLI 側に該当
    // フラグが無いので無視される。generic は recipe.effort_flag 次第。
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
                "Please install Claude Code.\ncurl -fsSL https://claude.ai/install.sh | bash".into()
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
        format!(
            "{} {}",
            config.display.shell_prefix_label,
            args.ssh_args.join(" ")
        )
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
        kind,
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
    // recv() の Err(切断) で break する loop。while let に置換せず元の構造を保つ。
    #[allow(clippy::while_let_loop)]
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
                ui::InputRequest::ReadConfirmKey => {
                    // Y/n/a 1 キー即確定。None は Ctrl+C / Ctrl+D / ESC = 全キャンセル。
                    let event = match ui::read_confirm_key() {
                        Some(choice) => ui::InputEvent::Confirm(choice),
                        None => ui::InputEvent::ReadLineCancelled,
                    };
                    if input_tx.send(event).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // 入力スレッド再開の 3 状態 (idle / pending / 静音タイマ) は InputGate に集約。
    // ばらの変数で持ち回ると再設定漏れで入力ハングを再発させるため。
    let mut gate = input_gate::InputGate::new(prompt_tx.clone());

    // メインループ
    'main_loop: loop {
        // 端末リサイズ検出
        if ui::check_and_clear_sigwinch() {
            let (new_rows, new_cols) = ui::terminal_size();
            let _ = pty.resize(new_rows, new_cols);
        }

        // PTY出力をチェック
        if pty_drain::drain_pty(
            &pty_rx,
            &mut ring_buffer,
            &mut io::stdout(),
            pty_drain::DrainOpts {
                display: pty_drain::DrainDisplay::UnlessMinibuffer,
                flush_each_chunk: true,
                ..Default::default()
            },
        )? {
            gate.note_pty_output();
        }

        // PTY出力が落ち着いたら入力スレッドを起動
        gate.maybe_request_passthrough();

        // PTYプロセスの終了チェック。
        // EOF 検出 (alive_rx) だけだと、子プロセスが exit しても master read が
        // EOF を返さないケース (リモート再起動による ssh 切断等) で
        // pty_reader スレッドが read() でブロックしたままになり詰まる。
        // child.try_wait() による能動検出も併用する。
        if alive_rx.try_recv().is_ok() || !pty.is_alive() {
            // 残りのPTY出力（logoutメッセージ等）を表示してから終了する
            thread::sleep(FINAL_DRAIN_WAIT);
            pty_drain::drain_pty(
                &pty_rx,
                &mut ring_buffer,
                &mut io::stdout(),
                pty_drain::DrainOpts {
                    display: pty_drain::DrainDisplay::UnlessMinibuffer,
                    ..Default::default()
                },
            )?;
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
                // 入力スレッドが idle に戻った。PTY 出力が落ち着いてから入力を再開する
                // (再 arm は guard の Drop が行う)。
                let _rearm = gate.rearm_on_drop();
                continue;
            }
            Ok(ui::InputEvent::AiPrompt(prompt)) => {
                // この arm はどの経路で終わっても入力スレッドが idle に戻る。
                // 再 arm は出口ごとの手書きではなく guard の Drop に任せる
                // (continue / break / `?` の全経路で発火)。
                let _rearm = gate.rearm_on_drop();
                if prompt.is_empty() {
                    continue;
                }
                // slash command (/help, /effort, /model, /clear, /ai) はローカルで処理し AI には送らない。
                if let Some(msg) = try_handle_slash_command(
                    &prompt,
                    &mut ai_session,
                    &config,
                    &mut kind,
                    &mut ring_buffer,
                ) {
                    ui::print_slash_result(&msg);
                    // shell プロンプトをリフレッシュする。打ちかけ消去 + 改行の不可分な
                    // 組み合わせは PtyHandler::refresh_prompt に固定されている (信頼の根幹)。
                    let _ = pty.refresh_prompt();
                    continue;
                }
                // AI 対話 1 セッション (打ちかけ消去 → send → Y/n/a → 実行 →
                // follow-up → 終了 refresh) は conversation.rs に閉じている。
                let end = conversation::AiConversation {
                    pty: &mut pty,
                    pty_rx: &pty_rx,
                    ring_buffer: &mut ring_buffer,
                    ai_session: ai_session.as_mut(),
                    kind,
                    display: &config.display,
                    prompt_tx: &prompt_tx,
                    input_rx: &input_rx,
                }
                .run(&prompt)?;
                if matches!(end, conversation::ConversationEnd::PtyDied) {
                    break 'main_loop;
                }
            }
            Ok(ui::InputEvent::Line(line)) => {
                let _rearm = gate.rearm_on_drop();
                match ui::parse_input(&line) {
                    ui::UserInput::Exit => {
                        pty.write(b"exit\n")?;
                    }
                    ui::UserInput::ShellCommand(cmd) => {
                        pty.write(format!("{cmd}\n").as_bytes())?;
                    }
                }
            }
            Ok(ui::InputEvent::ReadLineCancelled) => {
                // メインループでは ReadLine を発行していない (AI 確認時のみ) ので
                // ここに来るのは想定外。安全側で無視する。
                continue;
            }
            Ok(ui::InputEvent::Confirm(_)) => {
                // メインループでは ReadConfirmKey を発行していない (AI 確認時のみ) ので
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
    // aish 自身は DECSTBM を設定しない (minibuffer の伸長も全画面 scroll 方式で
    // region 不使用。ui::show_minibuffer 終了時に防御的な \x1b[r を送るのみ) ので、
    // 通常の終了経路ではここでリセットしなくても DECSTBM は default のはず。

    // aish 終了メッセージ。raw モードのまま出すので CRLF を明示する。
    // bash の "exit" echo の直後に、この行が画面に追加される形になる。
    // header_color が空文字なら色なしのプレーン表示。
    let _ = write!(
        io::stdout(),
        "{}aish session ended.\x1b[0m\r\n",
        config.display.header_color
    );
    let _ = io::stdout().flush();

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
    println!(
        "\
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
    https://github.com/tryandhappy/aish"
    );

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
                        eprintln!("\nResume this {} session with:\n  {cmd}", info.backend_name);
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
