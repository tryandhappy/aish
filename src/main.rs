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
mod term;
mod ui;
mod update;
mod vetted_command;

use mode::Mode;
use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::sync::OnceLock;
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
    Update(update::UpdateChannel),
    Version,
    Help,
    /// `--list-providers`: 利用可能な backend (native + 組み込み + config) を一覧表示。
    /// `--config` を尊重するため path を持ち回る。
    ListProviders(Option<String>),
}

/// native backend 名を `a | b | c` 形式で返す。help / エラー文の重複を防ぐ。
fn native_backend_names() -> String {
    ai::BackendKind::all_native()
        .iter()
        .map(|k| k.as_str())
        .collect::<Vec<_>>()
        .join(" | ")
}

/// meta コマンド (`--list-providers` 等) 用に args から `--config <path>` を拾う。
fn find_config_path(args: &[String]) -> Option<String> {
    for (i, a) in args.iter().enumerate() {
        if a == "--config" {
            return args.get(i + 1).cloned();
        }
        if let Some(rest) = a.strip_prefix("--config=") {
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// `--update` に続く `--stable` / `--prerelease` から更新チャネルを決める。
/// 既定は `Stable` (フラグ無しの `aish --update` は安定版)。両方指定時は後勝ち。
fn parse_update_channel(args: &[String]) -> update::UpdateChannel {
    let mut channel = update::UpdateChannel::Stable;
    for arg in args {
        match arg.as_str() {
            "--stable" => channel = update::UpdateChannel::Stable,
            "--prerelease" => channel = update::UpdateChannel::Prerelease,
            _ => {}
        }
    }
    channel
}

fn parse_args() -> CliAction {
    let args: Vec<String> = std::env::args().skip(1).collect();
    parse_args_from(&args)
}

/// `parse_args` の本体 (テスト用に引数を注入できるよう分離)。
fn parse_args_from(args: &[String]) -> CliAction {
    for arg in args {
        match arg.as_str() {
            "--update" => return CliAction::Update(parse_update_channel(args)),
            "--version" | "-V" => return CliAction::Version,
            "--help" => return CliAction::Help,
            "--list-providers" => return CliAction::ListProviders(find_config_path(args)),
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
            match take_value(args, i, "--config") {
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
            match take_value(args, i, "--ai") {
                Some((v, adv)) => {
                    ai_backend = Some(v.to_string());
                    i += adv;
                    continue;
                }
                None => {
                    eprintln!(
                        "Error: --ai requires a value ({}, or a config/[[ai.providers]] name; see `aish --list-providers`)",
                        native_backend_names()
                    );
                    std::process::exit(1);
                }
            }
        }
        // --model <name> / --model=<name>
        if arg == "--model" || arg.starts_with("--model=") {
            match take_value(args, i, "--model") {
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
            match take_value(args, i, "--effort") {
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

/// 環境変数 AISH_DEBUG_PTY が設定されていれば true。起動時に 1 度だけ env を見てキャッシュ。
fn debug_pty_enabled() -> bool {
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| std::env::var_os("AISH_DEBUG_PTY").is_some())
}

/// AISH_DEBUG_PTY=1 のとき、drain した PTY(子シェル/ConPTY) 出力チャンクを escape して
/// **stderr** に出す (`AISH_DEBUG_KEYS` と同じく既定無効・stderr 出力なので Windows でも
/// `aish 2> pty.log` で取れる)。Windows の描画ズレ調査で ConPTY のカーソル位置指定
/// シーケンス (`\e[行;列H` 等) を実測する用途。TUI 表示 (stdout) とは別系統なので
/// リダイレクトすれば画面を汚さない。
pub(crate) fn debug_pty(data: &[u8]) {
    if debug_pty_enabled() {
        eprintln!(
            "[aish-pty {} bytes] {}",
            data.len(),
            debug_bytes(data, 4096)
        );
    }
}

/// `/model` `/effort` の共通処理。
/// - `value == Some("-")` / `Some("clear")` → クリア。
/// - `value == Some(other)` → その値を set (検証せずヒントのみ)。
/// - `value == None` → 候補一覧を解決し、空なら hint メッセージ、非空なら対話ピッカー。
///   候補末尾に `(default)` を足し、選べばクリア(CLI 既定に任せる)。取消時は変更しない。
///
/// `get_available` は None 経路でだけ評価される (取得コマンドはピッカーを開く時だけ実行)。
fn run_option_picker(
    which: &str,
    value: Option<&str>,
    session: &mut Box<dyn ai::AiBackend>,
    get_current: impl Fn(&dyn ai::AiBackend) -> Option<String>,
    get_available: impl Fn(&dyn ai::AiBackend) -> Vec<String>,
    set: impl Fn(&mut dyn ai::AiBackend, Option<&str>),
) -> String {
    match value {
        Some("-") | Some("clear") => {
            set(&mut **session, None);
            format!("{which}: (cleared)")
        }
        Some(v) => {
            set(&mut **session, Some(v));
            format!("{which}: {v}")
        }
        None => {
            let available = get_available(&**session);
            if available.is_empty() {
                return format!(
                    "{which}: 候補が未設定です ([ai.<backend>].{which}s / {which}s_command で設定するか /{which} <値> で直接指定)"
                );
            }
            let cur_idx = get_current(&**session)
                .as_deref()
                .and_then(|c| available.iter().position(|x| x == c));
            // 末尾に "(default)" 疑似エントリを足す (選べばクリア = CLI 既定に任せる)。
            let clear_idx = available.len();
            let mut items = available.clone();
            items.push("(default)".to_string());
            match ui::show_picker(&format!("/{which}"), &items, cur_idx) {
                Some(i) if i == clear_idx => {
                    set(&mut **session, None);
                    format!("{which}: (cleared)")
                }
                Some(i) => {
                    set(&mut **session, Some(&available[i]));
                    format!("{which}: {}", available[i])
                }
                None => format!("{which}: (unchanged)"),
            }
        }
    }
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
             /effort [<level>]        set reasoning effort (no value = pick from list, `-`/`clear` = clear)\n\
             /model  [<name>]         set model (no value = pick from list, `-`/`clear` = clear)\n\
             /clear                   clear conversation history / session\n\
             /ai     <NAME>           switch AI backend (built-in: claude|codex|gemini|qwen|cursor|copilot, or any [[ai.providers]] name)"
                .to_string(),
        ),
        "effort" => Some(run_option_picker(
            "effort",
            value,
            ai_session,
            |s| s.effort(),
            |s| s.available_efforts(),
            |s, v| s.set_effort(v),
        )),
        "model" => Some(run_option_picker(
            "model",
            value,
            ai_session,
            |s| s.model(),
            |s| s.available_models(),
            |s, v| s.set_model(v),
        )),
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

/// stderr が端末で、かつ `NO_COLOR` が未設定なら true（エラー出力を着色してよいか）。
/// `install_guide` の名前太字/URL 淡色と、`Error:` プレフィックスの赤字に共通で使う。
fn stderr_use_color() -> bool {
    use std::io::IsTerminal;
    std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
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
        if let Ok(pid) = pid_str.parse::<u32>() {
            if term::pid_alive(pid) {
                return Err(format!("aish is already running here (PID {pid}).").into());
            }
        }
    }

    term::console_ok()?;

    ui::save_terminal_settings();

    term::install_resize_watch();

    let mut config = config::Config::load(args.config_path.as_deref())?;

    // `[[ai.providers]]` の registry を leak ベースで初期化。これ以降
    // `BackendKind::parse("generic:<name>")` が解決できるようになる。
    // 一度きりの呼び出し (OnceLock) なので nested aish 起動でも安全。
    ai::BackendKind::init_generics(&config.ai.resolved_providers);

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
        // 選択した backend (既定 = claude、または --ai/config 指定) が未インストールなら、
        // 実際に使える AI CLI を自動検出してフォールバックする (探索順は auto_detect_order:
        // Claude Code → Codex → Gemini → 人気順 → generic)。見つかればそれに切り替え、
        // 起動バナー (print_startup_banner) にも切替後の kind が反映される。
        match ai::auto_detect_backend() {
            Some(found) => {
                // OPOST 温存 (Unix) / newline auto-return (Windows) 前提で println! の \n は
                // CRLF になる (banner と同じ)。raw モードでも桁ズレしない。
                println!(
                    "  \x1b[38;5;245mAI backend `{}` not found — using `{}`.\x1b[0m",
                    kind.as_str(),
                    found.as_str()
                );
                let _ = std::io::Write::flush(&mut std::io::stdout());
                kind = found;
            }
            // 1 つも見つからなければ、対応 AI CLI ごとの導入案内 (名前 + 公式 URL) を表示する。
            // 直接 exit すると main の restore_terminal_settings / cleanup_terminal_indicator が
            // 走らず端末が raw モードで残るので、必ず Err で抜けて main 側のクリーンアップを通す。
            // 他のエラーと同じく `Error:` プレフィックス付きで出力される。
            // 名前太字 / URL 淡色の着色は stderr が端末 (かつ NO_COLOR 未設定) のときだけ。
            None => {
                return Err(ai::install_guide(stderr_use_color()).into());
            }
        }
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

    // 起動バナー: 1 度だけ画面上部に表示する (status bar は廃止)。
    // backend ごとに色を変える 2 行 ASCII アート + バージョン・モデル・effort・キーヒント。
    // model / effort 未指定時はその欄を省略する。
    // **PTY spawn より前に描画する**: Windows の ConPTY は spawn 時点のカーソル位置を基準に
    // 子シェルの描画をアンカーするため、spawn 後にバナーを出すと子の初回プロンプト(絶対位置
    // 指定)がバナーを上書きする。先に描いてカーソルをバナー下へ進めてから spawn する。
    let banner_model = ai_session.model();
    let banner_effort = ai_session.effort();
    ui::print_startup_banner(
        kind,
        banner_model.as_deref(),
        banner_effort.as_deref(),
        env!("CARGO_PKG_VERSION"),
    );

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
            Ok(ui::InputEvent::MinibufferCancelled) => {
                // minibuffer キャンセル時もシェルプロンプトを再表示する
                // (AI 対話終了 / slash command と同じ refresh_prompt 経路)。
                // refresh_prompt は打ちかけ消去 (Ctrl+A+Ctrl+K) → 改行なので、
                // 未承認のコマンドを勝手に submit しない (信頼の根幹)。新プロンプト
                // 出力は次の main loop 先頭の drain_pty が表示する。
                // キャンセル直後に PassthroughEnded も届くが、両方で再 arm するのは
                // 送信経路 (AiPrompt + PassthroughEnded) と同じ既存パターン。
                let _rearm = gate.rearm_on_drop();
                let _ = pty.refresh_prompt();
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
    let natives = native_backend_names();
    println!(
        "\
aish v{version} — CLI SSH + AI

USAGE:
    aish [AISH_OPTIONS] [SSH_ARGS...]   Remote: ssh user@host etc. (引数はそのまま ssh に渡す)
    aish [AISH_OPTIONS]                 Local:  $SHELL を起動
    aish --version | --help
    aish --update [--stable | --prerelease]
    aish --list-providers              利用可能な AI バックエンド一覧を表示

AISH OPTIONS:
    --config <PATH>        設定ファイルパス (既定: ~/.aish/config.toml)
    --ai <KIND>            AI バックエンド: {natives} (既定: claude)
                           + 組み込みデフォルト recipe / [[ai.providers]] の名前も指定可
                           ([ai].backend より優先。一覧は `aish --list-providers`)
    --model <NAME>         使用モデル名 (例: sonnet, gpt-5, gemini-2.5-pro)
                           [ai].model および extra_args の -m 指定より優先される
    --effort <LEVEL>       reasoning effort (low | medium | high など)
                           claude / antigravity: --effort、codex: -c model_reasoning_effort= に変換
                           gemini / qwen / grok は CLI 非対応のため無視される

OTHER OPTIONS:
    --version, -V          バージョン表示
    --update [--stable|--prerelease]
                           GitHub Releases から自己更新 (例: sudo aish --update)
                           既定 --stable: prerelease を除いた安定版の最新
                           --prerelease : prerelease を含む最新版 (先端)
    --list-providers       利用可能な AI バックエンド (native + 組み込み + config) を一覧表示
    --help                 このヘルプを表示

KEYS (起動後):
    Ctrl+/                 aish プロンプトを開く (AI に質問)
    Y / Enter              提案コマンドを実行
    n                      この提案をスキップ
    a                      残りの提案をすべて自動承認
    Ctrl+C                 提案キャンセル / コマンド中断

SLASH COMMANDS (aish プロンプトに入力):
    /help                  利用可能な slash command を表示
    /effort [LEVEL]        reasoning effort を変更 (引数なし=候補ピッカー、`-`/`clear`=クリア)
    /model  [NAME]         モデルを変更 (引数なし=候補ピッカー、`-`/`clear`=クリア)
    /clear                 会話履歴 / セッションをクリア
    /ai     <KIND>         AI バックエンドを切り替え ({natives}, または config の provider 名)

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

/// `--list-providers`: native + 組み込みデフォルト + config の generic backend を、
/// 出所タグ付きで一覧表示する。
fn print_providers(config_path: Option<&str>) -> Result<(), String> {
    let config = config::Config::load(config_path)?;
    let builtin_names: std::collections::HashSet<String> = config::builtin_providers()
        .into_iter()
        .map(|r| r.name)
        .collect();
    let user_names: std::collections::HashSet<&str> = config
        .ai
        .providers
        .iter()
        .map(|o| o.name.as_str())
        .collect();

    println!("Available AI backends (use with `--ai <NAME>` or `/ai <NAME>`):");
    println!("  default backend: {}", config.ai.backend);
    println!();
    println!("Native (always available):");
    for kind in ai::BackendKind::all_native() {
        println!("  {}", kind.as_str());
    }
    println!();
    if config.ai.resolved_providers.is_empty() {
        println!("Generic providers (built-in defaults + [[ai.providers]]): (none)");
    } else {
        println!("Generic providers (built-in defaults + [[ai.providers]] overrides):");
        for r in &config.ai.resolved_providers {
            let source = match (
                builtin_names.contains(&r.name),
                user_names.contains(r.name.as_str()),
            ) {
                (true, true) => "built-in, overridden",
                (true, false) => "built-in",
                (false, _) => "config",
            };
            let args = if r.args.is_empty() {
                String::new()
            } else {
                format!(" args={:?}", r.args)
            };
            println!(
                "  {:<16} [{}]  binary={} parse={}{}",
                r.name, source, r.binary, r.parse, args
            );
        }
    }
    Ok(())
}

fn main() {
    match parse_args() {
        CliAction::Help => {
            print_help();
        }
        CliAction::ListProviders(config_path) => {
            if let Err(e) = print_providers(config_path.as_deref()) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        CliAction::Version => {
            println!("aish {}", env!("CARGO_PKG_VERSION"));
        }
        CliAction::Update(channel) => {
            if let Err(e) = update::run_update(channel) {
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
                    // `Error:` は端末なら赤字 (install_guide の名前太字/URL 淡色と統一の判定)。
                    if stderr_use_color() {
                        eprintln!("\x1b[31mError:\x1b[0m {e}");
                    } else {
                        eprintln!("Error: {e}");
                    }
                    std::process::exit(1);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_args_run_splits_flags_and_ssh_args() {
        // --ai/--model/--effort/--config を吸収し、残りが ssh 引数になる。
        let args = to_args(&[
            "--ai",
            "codex",
            "--model=gpt-5",
            "--effort",
            "high",
            "-p",
            "2222",
            "user@host",
        ]);
        let CliAction::Run(a) = parse_args_from(&args) else {
            panic!("expected Run");
        };
        assert_eq!(a.ai_backend.as_deref(), Some("codex"));
        assert_eq!(a.ai_model.as_deref(), Some("gpt-5"));
        assert_eq!(a.ai_effort.as_deref(), Some("high"));
        assert_eq!(a.ssh_args, vec!["-p", "2222", "user@host"]);
    }

    #[test]
    fn parse_args_equals_and_space_forms() {
        let args = to_args(&["--config=/tmp/c.toml"]);
        let CliAction::Run(a) = parse_args_from(&args) else {
            panic!("expected Run");
        };
        assert_eq!(a.config_path.as_deref(), Some("/tmp/c.toml"));

        let args = to_args(&["--config", "/tmp/c2.toml"]);
        let CliAction::Run(a) = parse_args_from(&args) else {
            panic!("expected Run");
        };
        assert_eq!(a.config_path.as_deref(), Some("/tmp/c2.toml"));
    }

    #[test]
    fn parse_args_top_level_actions_win() {
        assert!(matches!(
            parse_args_from(&to_args(&["--version"])),
            CliAction::Version
        ));
        assert!(matches!(
            parse_args_from(&to_args(&["--help"])),
            CliAction::Help
        ));
        assert!(matches!(
            parse_args_from(&to_args(&["--list-providers"])),
            CliAction::ListProviders(None)
        ));
        // どこにあっても優先される (ssh 引数と混在時)。
        assert!(matches!(
            parse_args_from(&to_args(&["user@host", "--version"])),
            CliAction::Version
        ));
    }

    #[test]
    fn parse_args_empty_is_local_shell_run() {
        let CliAction::Run(a) = parse_args_from(&[]) else {
            panic!("expected Run");
        };
        assert!(a.ssh_args.is_empty());
        assert!(a.ai_backend.is_none());
    }

    #[test]
    fn update_channel_defaults_to_stable() {
        let args = to_args(&["--update"]);
        assert_eq!(parse_update_channel(&args), update::UpdateChannel::Stable);
    }

    #[test]
    fn update_channel_explicit_stable() {
        let args = to_args(&["--update", "--stable"]);
        assert_eq!(parse_update_channel(&args), update::UpdateChannel::Stable);
    }

    #[test]
    fn update_channel_prerelease() {
        let args = to_args(&["--update", "--prerelease"]);
        assert_eq!(
            parse_update_channel(&args),
            update::UpdateChannel::Prerelease
        );
    }

    #[test]
    fn update_channel_last_wins() {
        let args = to_args(&["--update", "--prerelease", "--stable"]);
        assert_eq!(parse_update_channel(&args), update::UpdateChannel::Stable);
        let args = to_args(&["--update", "--stable", "--prerelease"]);
        assert_eq!(
            parse_update_channel(&args),
            update::UpdateChannel::Prerelease
        );
    }
}
