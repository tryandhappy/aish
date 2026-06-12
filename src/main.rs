mod ai;
mod config;
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
use std::time::{Duration, Instant};

/// AI 提案コマンドの完了判定に使う PTY 静音時間。出力末尾がプロンプト形でも、
/// この時間静まるまでは「出力途中にたまたまプロンプト風の行が出た」可能性を排除しない。
const PROMPT_QUIET_THRESHOLD: Duration = Duration::from_millis(200);
/// コマンド実行完了待ちループの poll 間隔 (CPU を占有しないための休止)。
const EXEC_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// kill_line (打ちかけ消去) 送信後、bash の消去 redisplay が pty_rx に到着するのを待つ時間。
/// SSH 越しでは取りこぼし得るが、その場合も次の main loop drain で追従するだけで実害はない。
const KILL_LINE_REDISPLAY_WAIT: Duration = Duration::from_millis(150);
/// 子プロセス死亡検出後、logout メッセージ等の残り出力が pty_rx に届くのを待つ時間。
const FINAL_DRAIN_WAIT: Duration = Duration::from_millis(50);
/// refresh_prompt (打ちかけ消去 + 改行) 送信後、シェルプロンプトの再表示出力を待つ時間。
const PROMPT_REFRESH_WAIT: Duration = Duration::from_millis(200);

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

/// AI 対話を始める直前に、bash readline に残っている打ちかけ (ユーザが Ctrl+/ の前に
/// Enter せず入力していた未確定文字列) を消去し、消去 redisplay を画面に流さず
/// ring_buffer にだけ吸収する。
///
/// 「カーソルが bash の readline モデルと同期しているこのタイミング」でしか正しく
/// 消せない。show_minibuffer が DSR でカーソルを bash プロンプト位置 (= 打ちかけ末尾)
/// に絶対座標復元した直後であり、まだ AI 出力を 1 文字も stdout に描いていない
/// (slash command は手前の分岐で continue 済み) ので、実カーソル == bash の readline
/// カーソル。この状態でだけ kill_line の消去 redisplay (折り返した打ちかけを畳むための
/// cursor-up 等) が打ちかけ自身の行に正しく当たり、暴走しない。
///
/// これを入れないと: 打ちかけが端末幅を超えて複数行に折り返している場合、対話終了後
/// リフレッシュ (refresh_prompt) で送る Ctrl+A に対し bash が `ESC[A` (cursor-up) を
/// 複数行ぶん吐き、その上移動が「aish が stdout に出した Exec? 行 (bash は与り知らない)」
/// の上で起き、シェルプロンプトが Exec? 行を上書きしてしまう (= ユーザ報告の不具合)。
/// 先に空にしておけば readline バッファは 1 行に収まり、末尾リフレッシュは cursor-up を
/// 一切吐かず、プロンプトは必ず新しい行に出る。撤去する場合は折り返し打ちかけ +
/// n キャンセルで Exec? 行が上書きされないことを pyte ドライバ等で必ず再検証すること。
///
/// 消去に伴う bash の redisplay バイトは pty_rx に届くので drain して取り除く (放置すると
/// 次の main loop drain で AI 出力の後に描画され画面が乱れる)。画面には転送せず捨てる:
/// 折り返した打ちかけは画面/scrollback に残るが (ユーザが打った内容の記録として無害)、
/// cursor 制御エスケープを今 stdout に流すと AI 出力の開始位置がずれるため。
/// ring_buffer には従来どおり追記して「PTY 出力は全て ring_buffer に入る」不変条件を保つ。
/// sleep(150ms) は消去 redisplay の到着待ち (SSH 越しでは取りこぼし得るが、その場合でも
/// 次 main loop drain で追従するだけで上書きは起きない)。
fn discard_stale_readline_input(
    pty: &mut pty_handler::PtyHandler,
    pty_rx: &mpsc::Receiver<Vec<u8>>,
    ring_buffer: &mut ring_buffer::RingBuffer,
) -> io::Result<()> {
    let _ = pty.kill_line();
    thread::sleep(KILL_LINE_REDISPLAY_WAIT);
    pty_drain::drain_pty(
        pty_rx,
        ring_buffer,
        &mut io::stdout(),
        pty_drain::DrainOpts::default(), // Hidden: 表示せず記録のみ
    )?;
    Ok(())
}

/// `wait_for_command_completion` の結果。
enum CommandWait {
    /// 出力末尾がプロンプト形に戻り静音した (= コマンド完了とみなす)。
    PromptReturned,
    /// 子プロセス (ssh / shell) が死んだ。残り出力は drain 済み。
    /// 呼び出し側は main loop ごと抜けること。
    PtyDied,
}

/// AI 提案コマンド送信後の実行完了待ち (passive 検出)。
/// - PTY 出力をドレインして画面 / リングバッファ / sniffer へ
/// - stdin → PTY 転送（パスワード入力・Ctrl+C 中断・対話応答）
/// - SIGWINCH 検知（リサイズ追従）
/// - 完了判定: PTY 出力末尾がプロンプト形 + `PROMPT_QUIET_THRESHOLD` 静音
///
/// 子プロセス死亡時 (sudo reboot 等で SSH が切れた後はプロンプトに戻らないため、
/// sniffer ベースの完了判定だと永遠にハングする) は残り出力をドレインして
/// `PtyDied` を返す。
fn wait_for_command_completion(
    pty: &mut pty_handler::PtyHandler,
    pty_rx: &mpsc::Receiver<Vec<u8>>,
    ring_buffer: &mut ring_buffer::RingBuffer,
) -> Result<CommandWait, Box<dyn std::error::Error>> {
    let mut sniffer = prompt_sniffer::PromptSniffer::new();
    let mut last_pty_activity = Instant::now();
    let mut chunk_count = 0usize;
    loop {
        if !pty.is_alive() {
            thread::sleep(FINAL_DRAIN_WAIT);
            pty_drain::drain_pty(
                pty_rx,
                ring_buffer,
                &mut io::stdout(),
                pty_drain::DrainOpts {
                    display: pty_drain::DrainDisplay::Always,
                    ..Default::default()
                },
            )?;
            io::stdout().flush().ok();
            return Ok(CommandWait::PtyDied);
        }
        if ui::check_and_clear_sigwinch() {
            let (new_rows, new_cols) = ui::terminal_size();
            let _ = pty.resize(new_rows, new_cols);
        }
        let got_pty = pty_drain::drain_pty(
            pty_rx,
            ring_buffer,
            &mut io::stdout(),
            pty_drain::DrainOpts {
                display: pty_drain::DrainDisplay::Always,
                flush_each_chunk: true,
                sniffer: Some(&mut sniffer),
                debug_chunk_count: Some(&mut chunk_count),
                ..Default::default()
            },
        )?;
        if got_pty {
            last_pty_activity = Instant::now();
        }
        let stdin_bytes = ui::drain_stdin_nonblocking();
        if !stdin_bytes.is_empty() {
            pty.write(&stdin_bytes)?;
        }
        if last_pty_activity.elapsed() >= PROMPT_QUIET_THRESHOLD && sniffer.matches_prompt() {
            sniffer.record_match();
            debug_log(&format!("exec end: chunks={chunk_count}"));
            return Ok(CommandWait::PromptReturned);
        }
        thread::sleep(EXEC_POLL_INTERVAL);
    }
}

/// AI 提案コマンドの Y/n/a 確認 1 回分の結果。
/// 旧実装の bool 2 つ (confirmed / user_cancelled) の組み合わせ表現を置き換え、
/// 「このコマンドをどうするか」と「残り全部をどうするか」を 1 つの値で運ぶ。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum ConfirmDecision {
    /// y/Y/Enter/Space: このコマンドを実行する。
    Run,
    /// n/N (または入力 channel 切断): このコマンドをスキップして次へ。
    Skip,
    /// a/A: このコマンドを実行し、以降の残りも自動承認する。
    RunRest,
    /// Ctrl+C / Ctrl+D / ESC: このコマンドを含む残り全部をキャンセル。
    CancelRest,
}

/// 確認ループの承認モード。`RunRest` 確定後は `All` になり以降の確認を省略する。
/// 旧実装の `auto_approve_remaining: bool` の置き換えで、`CancelRest` が即 break する
/// 制御フローと合わせて「自動承認中かつキャンセル済み」の矛盾状態を表現不能にする。
#[derive(PartialEq, Eq, Clone, Copy)]
enum Approval {
    AskEach,
    All,
}

/// Y/n/a 確認の入力イベントを 1 決定に解決するまで待つ。
/// 確認待ち中に届く無関係イベント (Line / PtyData / PassthroughEnded / AiPrompt) は
/// 無視して読み直す。channel 切断 (入力スレッド消滅 = 退出間際) は Skip 扱い (旧挙動互換)。
fn wait_confirm_decision(input_rx: &mpsc::Receiver<ui::InputEvent>) -> ConfirmDecision {
    loop {
        match input_rx.recv() {
            Ok(ui::InputEvent::Confirm(choice)) => match choice {
                ui::ConfirmChoice::Yes => return ConfirmDecision::Run,
                ui::ConfirmChoice::No => return ConfirmDecision::Skip,
                ui::ConfirmChoice::All => return ConfirmDecision::RunRest,
            },
            // Ctrl+C / Ctrl+D / ESC: 残りすべてをキャンセル
            Ok(ui::InputEvent::ReadLineCancelled) => return ConfirmDecision::CancelRest,
            Ok(ui::InputEvent::Line(_))
            | Ok(ui::InputEvent::PtyData(_))
            | Ok(ui::InputEvent::PassthroughEnded)
            | Ok(ui::InputEvent::AiPrompt(_)) => continue,
            Err(_) => return ConfirmDecision::Skip,
        }
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
                discard_stale_readline_input(&mut pty, &pty_rx, &mut ring_buffer)?;
                let context = ring_buffer.get_unsent_for(kind);
                let sp_model = ai_session.model();
                let sp_effort = ai_session.effort();
                let spinner = ui::Spinner::start(
                    &config.display,
                    kind,
                    sp_model.as_deref(),
                    sp_effort.as_deref(),
                );
                let mut ai_result = ai_session.send(&ai::AiRequest {
                    terminal_context: &context,
                    user_prompt: &prompt,
                });
                spinner.stop();

                // 次ターン以降の send で各 backend に渡す user_prompt 元テキスト。
                // 初回はユーザ入力、follow-up は aish が生成した要約文。
                let mut last_prompt_for_annotation = prompt.clone();

                // AIとの対話ループ: コマンド実行→結果をAIに送信→分析→繰り返し
                loop {
                    match ai_result {
                        Ok(response) => {
                            // 注釈 append → mark_sent_for の順序不変条件ごと
                            // record_ai_exchange にカプセル化されている。
                            ring_buffer.record_ai_exchange(
                                kind,
                                &last_prompt_for_annotation,
                                &response.message,
                                &response.commands,
                            );

                            ui::print_ai_message(&response.message, kind, &config.display);

                            // コマンド提案がない場合は対話終了
                            if response.commands.is_empty() {
                                break;
                            }

                            ui::print_ai_commands(&response.commands, &config.display);

                            // コマンドを1つずつ確認＋実行
                            let total = response.commands.len();
                            let mut executed_summary: Vec<String> = Vec::new();
                            let mut approval = Approval::AskEach;
                            // Ctrl+C で残り全部キャンセルされたか (follow-up 文面の分岐用)
                            let mut user_cancelled = false;
                            for (i, cmd) in response.commands.iter().enumerate() {
                                // AI 提案コマンドの制御文字検証 (偽装・密輸の拒否理由は
                                // vetted_command.rs のドキュメント参照)。Approval 分岐より
                                // 前に置くことで Approval::All (= [a]) のまとめ承認経路も
                                // 必ずこのガードを通る。以降この iteration では検証済みの
                                // VettedCommand だけを扱い、表示 (confirm prompt) と送信
                                // (send_approved_command) が同一の検証済み値になる。
                                let cmd = match vetted_command::VettedCommand::vet(cmd) {
                                    Ok(vetted) => vetted,
                                    Err(raw) => {
                                        ui::print_rejected_command(raw, &config.display);
                                        continue;
                                    }
                                };
                                let decision = match approval {
                                    Approval::All => ConfirmDecision::Run,
                                    Approval::AskEach => {
                                        ui::print_single_confirm_prompt(
                                            &cmd,
                                            i + 1,
                                            total,
                                            &config.display,
                                        );
                                        let _ = prompt_tx.send(ui::InputRequest::ReadConfirmKey);
                                        wait_confirm_decision(&input_rx)
                                    }
                                };
                                match decision {
                                    ConfirmDecision::Skip => continue,
                                    ConfirmDecision::CancelRest => {
                                        user_cancelled = true;
                                        break;
                                    }
                                    ConfirmDecision::RunRest => approval = Approval::All,
                                    ConfirmDecision::Run => {}
                                }

                                // 最初に実行する AI 提案コマンドの直前で、bash の打ちかけ
                                // 入力を消去する。後続コマンドは前のコマンドが完了して bash
                                // プロンプトに戻った状態で送られるので、追加不要。
                                // (executed_summary への push はコマンド完了後なので、
                                //  「空 = まだ何も実行していない」が成立する)
                                if executed_summary.is_empty() {
                                    pty.kill_line()?;
                                }

                                // ユーザが承認したコマンドをそのまま PTY に送る。ラップしない。
                                pty.send_approved_command(&cmd)?;
                                debug_log(&format!("=== exec start: {cmd}"));

                                // コマンド実行完了待ち（passive 検出）。子プロセス死亡時は
                                // 待っても意味がないのでメインループごと抜ける。
                                if matches!(
                                    wait_for_command_completion(
                                        &mut pty,
                                        &pty_rx,
                                        &mut ring_buffer
                                    )?,
                                    CommandWait::PtyDied
                                ) {
                                    break 'main_loop;
                                }

                                executed_summary.push(format!("`{cmd}`"));
                            }

                            if executed_summary.is_empty() {
                                break;
                            }

                            // 実行結果をAIに送信して分析を継続
                            let follow_up_context = ring_buffer.get_unsent_for(kind);
                            println!();
                            let sp_model = ai_session.model();
                            let sp_effort = ai_session.effort();
                            let spinner = ui::Spinner::start(
                                &config.display,
                                kind,
                                sp_model.as_deref(),
                                sp_effort.as_deref(),
                            );
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
                            last_prompt_for_annotation = follow_up_text.clone();
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

                // AI対話終了後、シェルのプロンプトを再表示させる。
                // 提案コマンドを1つも実行しなかった場合 (全拒否 / 提案なし) は
                // 初回実行直前の kill_line を通っていないため、ユーザが Ctrl+/ 前に
                // 打ちかけた未確定入力が bash readline に残る。refresh_prompt が改行前に
                // 消去し、打ちかけの勝手な実行を防ぐ (実行済み経路では行が空なので no-op)。
                pty.refresh_prompt()?;
                thread::sleep(PROMPT_REFRESH_WAIT);
                // 先頭の改行を表示からだけ除去してプロンプトだけ見せる (記録は完全)
                pty_drain::drain_pty(
                    &pty_rx,
                    &mut ring_buffer,
                    &mut io::stdout(),
                    pty_drain::DrainOpts {
                        display: pty_drain::DrainDisplay::Always,
                        flush_each_chunk: true,
                        skip_leading_newline: true,
                        ..Default::default()
                    },
                )?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// channel にイベント列を流し込んだ状態の Receiver を作る。
    fn rx_with(events: Vec<ui::InputEvent>) -> mpsc::Receiver<ui::InputEvent> {
        let (tx, rx) = mpsc::channel();
        for ev in events {
            tx.send(ev).unwrap();
        }
        rx
    }

    #[test]
    fn confirm_yes_is_run() {
        let rx = rx_with(vec![ui::InputEvent::Confirm(ui::ConfirmChoice::Yes)]);
        assert_eq!(wait_confirm_decision(&rx), ConfirmDecision::Run);
    }

    #[test]
    fn confirm_no_is_skip() {
        let rx = rx_with(vec![ui::InputEvent::Confirm(ui::ConfirmChoice::No)]);
        assert_eq!(wait_confirm_decision(&rx), ConfirmDecision::Skip);
    }

    #[test]
    fn confirm_all_is_run_rest() {
        let rx = rx_with(vec![ui::InputEvent::Confirm(ui::ConfirmChoice::All)]);
        assert_eq!(wait_confirm_decision(&rx), ConfirmDecision::RunRest);
    }

    #[test]
    fn cancel_is_cancel_rest() {
        let rx = rx_with(vec![ui::InputEvent::ReadLineCancelled]);
        assert_eq!(wait_confirm_decision(&rx), ConfirmDecision::CancelRest);
    }

    #[test]
    fn unrelated_events_are_ignored_until_decision() {
        let rx = rx_with(vec![
            ui::InputEvent::Line("ls".to_string()),
            ui::InputEvent::PtyData(vec![0x41]),
            ui::InputEvent::PassthroughEnded,
            ui::InputEvent::AiPrompt("hi".to_string()),
            ui::InputEvent::Confirm(ui::ConfirmChoice::Yes),
        ]);
        assert_eq!(wait_confirm_decision(&rx), ConfirmDecision::Run);
    }

    #[test]
    fn disconnected_channel_is_skip() {
        // 入力スレッド消滅 (退出間際) は Skip 扱い (旧実装の Err(_) => break false 互換)。
        let rx = rx_with(vec![]);
        assert_eq!(wait_confirm_decision(&rx), ConfirmDecision::Skip);
    }
}
