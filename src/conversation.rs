//! AI 対話 1 セッション (Ctrl+/ プロンプト送信 〜 対話終了後のシェルプロンプト復帰) の
//! 制御フロー。打ちかけ消去 → send → 表示 → Y/n/a 確認 → 実行 → follow-up → 終了
//! refresh までを `AiConversation::run` に閉じ込め、main loop はイベント分配だけを行う。

use crate::ai;
use crate::config::DisplayConfig;
use crate::debug_log;
use crate::prompt_sniffer;
use crate::pty_drain;
use crate::pty_handler::PtyHandler;
use crate::ring_buffer::RingBuffer;
use crate::ui;
use crate::vetted_command::VettedCommand;
use std::io::{self, Write};
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
/// Windows では別用途でも流用: kill_line (単独 ESC) 送信直後にコマンド文字列を送ると
/// PowerShell の VT 入力デコーダが `ESC` + コマンド先頭の `[` 等を CSI シーケンスとして
/// 誤消費し先頭文字が欠落する (§ 15.13)。ESC が単独キーとして確定する猶予として使う。
const KILL_LINE_REDISPLAY_WAIT: Duration = Duration::from_millis(150);
/// refresh_prompt (打ちかけ消去 + 改行) 送信後、シェルプロンプトの再表示出力を待つ時間。
const PROMPT_REFRESH_WAIT: Duration = Duration::from_millis(200);

/// `AiConversation::run` の終わり方。
pub enum ConversationEnd {
    /// 対話が正常に終了し、シェルプロンプトを復帰させた。
    Finished,
    /// コマンド実行待ち中に子プロセスが死んだ (残り出力は drain 済み)。
    /// 呼び出し側は main loop を抜けること。
    PtyDied,
}

/// `confirm_and_execute` の結果。
enum ExecReport {
    /// 確認ループを最後まで (または中止で) 抜けた。
    Done {
        /// 実行したコマンドの要約 (`` `cmd` `` 形式)。空 = 1 つも実行していない。
        executed: Vec<String>,
        /// 抜けた理由 (follow-up するか / 文面の分岐用)。
        outcome: ExecOutcome,
    },
    /// コマンド実行待ち中に子プロセスが死んだ。
    PtyDied,
}

/// 確認ループを抜けた理由。follow-up の有無と文面を決める。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum ExecOutcome {
    /// 全コマンドを処理し終えた (実行 / スキップ含む)。
    Completed,
    /// q: 残りを中止。実行済みがあれば AI に follow-up する。
    Quit,
    /// Ctrl+C / Ctrl+D: 残りを中止し、実行有無に関わらず AI に問い合わせない。
    Abort,
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
    /// q/Q: このコマンドを含む残りを中止。実行済みがあれば AI に follow-up する。
    QuitRest,
    /// Ctrl+C / Ctrl+D: このコマンドを含む残り全部を中止し、AI に問い合わせない。
    /// (ESC はここではなく Skip = n と同じ「1 回分スキップ」に変更済み)
    AbortNoAi,
}

/// 確認ループの承認モード。`RunRest` 確定後は `All` になり以降の確認を省略する。
/// 旧実装の `auto_approve_remaining: bool` の置き換えで、`CancelRest` が即 break する
/// 制御フローと合わせて「自動承認中かつキャンセル済み」の矛盾状態を表現不能にする。
#[derive(PartialEq, Eq, Clone, Copy)]
enum Approval {
    AskEach,
    All,
}

/// `wait_for_command_completion` の結果。
enum CommandWait {
    /// 出力末尾がプロンプト形に戻り静音した (= コマンド完了とみなす)。
    PromptReturned,
    /// 実行中に Ctrl+C (0x03) が押された。PTY へ転送済みで、コマンドは中断され
    /// プロンプトに復帰した。残りコマンドは実行せず中止する。
    Interrupted,
    /// 子プロセス (ssh / shell) が死んだ。残り出力は drain 済み。
    PtyDied,
}

/// AI 対話 1 セッションが触る資源の束。main loop から借用して `run()` に渡す。
/// フィールドは全て借用なので、対話終了後は main loop が引き続き同じ資源を使う。
pub struct AiConversation<'a> {
    pub pty: &'a mut PtyHandler,
    pub pty_rx: &'a mpsc::Receiver<Vec<u8>>,
    pub ring_buffer: &'a mut RingBuffer,
    pub ai_session: &'a mut dyn ai::AiBackend,
    pub kind: ai::BackendKind,
    pub display: &'a DisplayConfig,
    pub prompt_tx: &'a mpsc::Sender<ui::InputRequest>,
    pub input_rx: &'a mpsc::Receiver<ui::InputEvent>,
}

impl AiConversation<'_> {
    /// AI 対話 1 セッション全体: 打ちかけ消去 → send → 表示 → Y/n/a → 実行 →
    /// follow-up send → 対話終了後のシェルプロンプト復帰まで。
    ///
    /// `PtyDied` を返す経路では終了 refresh (refresh_prompt + drain) を行わない
    /// (子プロセスが居ないので意味がなく、旧実装の `break 'main_loop` と同じ挙動)。
    pub fn run(
        mut self,
        initial_prompt: &str,
    ) -> Result<ConversationEnd, Box<dyn std::error::Error>> {
        discard_stale_readline_input(self.pty, self.pty_rx, self.ring_buffer)?;
        let context = self.ring_buffer.get_unsent_for(self.kind);
        let mut ai_result = self.send_with_spinner(&context, initial_prompt);

        // 次ターン以降の send で各 backend に渡す user_prompt 元テキスト。
        // 初回はユーザ入力、follow-up は aish が生成した要約文。
        let mut last_prompt_for_annotation = initial_prompt.to_string();

        // AIとの対話ループ: コマンド実行→結果をAIに送信→分析→繰り返し
        loop {
            match ai_result {
                Ok(response) => {
                    // 注釈 append → mark_sent_for の順序不変条件ごと
                    // record_ai_exchange にカプセル化されている。
                    self.ring_buffer.record_ai_exchange(
                        self.kind,
                        &last_prompt_for_annotation,
                        &response.message,
                        &response.commands,
                    );

                    ui::print_ai_message(&response.message, self.kind, self.display);

                    // コマンド提案がない場合は対話終了
                    if response.commands.is_empty() {
                        break;
                    }

                    ui::print_ai_commands(&response.commands, self.display);

                    let (executed, outcome) = match self.confirm_and_execute(&response.commands)? {
                        ExecReport::PtyDied => return Ok(ConversationEnd::PtyDied),
                        ExecReport::Done { executed, outcome } => (executed, outcome),
                    };

                    if !should_follow_up(&outcome, &executed, response.command_result_followup) {
                        break;
                    }

                    // 実行結果をAIに送信して分析を継続
                    let follow_up_context = self.ring_buffer.get_unsent_for(self.kind);
                    println!();
                    let follow_up_text = build_follow_up_text(&outcome, &executed);
                    last_prompt_for_annotation = follow_up_text.clone();
                    ai_result = self.send_with_spinner(&follow_up_context, &follow_up_text);
                }
                Err(e) => {
                    if matches!(e, ai::AiError::Cancelled) {
                        eprintln!("^C");
                    } else {
                        eprintln!("{}", format_ai_error(self.kind, &e));
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
        self.pty.refresh_prompt()?;
        thread::sleep(PROMPT_REFRESH_WAIT);
        // 先頭の改行を表示からだけ除去してプロンプトだけ見せる (記録は完全)
        pty_drain::drain_pty(
            self.pty_rx,
            self.ring_buffer,
            &mut io::stdout(),
            pty_drain::DrainOpts {
                display: pty_drain::DrainDisplay::Always,
                flush_each_chunk: true,
                skip_leading_newline: true,
                ..Default::default()
            },
        )?;
        Ok(ConversationEnd::Finished)
    }

    /// スピナーを出しながら AI に 1 ターン送る。start / stop の対をここに閉じ、
    /// send 経路ごとの止め忘れを防ぐ。
    fn send_with_spinner(
        &mut self,
        terminal_context: &str,
        user_prompt: &str,
    ) -> Result<ai::AiResponse, ai::AiError> {
        let sp_model = self.ai_session.model();
        let sp_effort = self.ai_session.effort();
        let spinner = ui::Spinner::start(
            self.display,
            self.kind,
            sp_model.as_deref(),
            sp_effort.as_deref(),
        );
        let result = self.ai_session.send(&ai::AiRequest {
            terminal_context,
            user_prompt,
        });
        spinner.stop();
        result
    }

    /// 提案コマンドを 1 つずつ Y/n/a 確認して実行する。
    fn confirm_and_execute(
        &mut self,
        commands: &[String],
    ) -> Result<ExecReport, Box<dyn std::error::Error>> {
        let total = commands.len();
        let mut executed: Vec<String> = Vec::new();
        let mut approval = Approval::AskEach;
        for (i, cmd) in commands.iter().enumerate() {
            // AI 提案コマンドの制御文字検証 (偽装・密輸の拒否理由は
            // vetted_command.rs のドキュメント参照)。Approval 分岐より
            // 前に置くことで Approval::All (= [a]) のまとめ承認経路も
            // 必ずこのガードを通る。以降この iteration では検証済みの
            // VettedCommand だけを扱い、表示 (confirm prompt) と送信
            // (send_approved_command) が同一の検証済み値になる。
            let cmd = match VettedCommand::vet(cmd) {
                Ok(vetted) => vetted,
                Err(raw) => {
                    ui::print_rejected_command(raw, self.display);
                    continue;
                }
            };
            let decision = match approval {
                Approval::All => ConfirmDecision::Run,
                Approval::AskEach => {
                    ui::print_single_confirm_prompt(&cmd, i + 1, total, self.display);
                    let _ = self.prompt_tx.send(ui::InputRequest::ReadConfirmKey);
                    wait_confirm_decision(self.input_rx)
                }
            };
            match decision {
                ConfirmDecision::Skip => continue,
                ConfirmDecision::QuitRest => {
                    return Ok(ExecReport::Done {
                        executed,
                        outcome: ExecOutcome::Quit,
                    });
                }
                ConfirmDecision::AbortNoAi => {
                    return Ok(ExecReport::Done {
                        executed,
                        outcome: ExecOutcome::Abort,
                    });
                }
                ConfirmDecision::RunRest => approval = Approval::All,
                ConfirmDecision::Run => {}
            }

            // 最初に実行する AI 提案コマンドの直前で、bash の打ちかけ
            // 入力を消去する。後続コマンドは前のコマンドが完了して bash
            // プロンプトに戻った状態で送られるので、追加不要。
            // (executed への push はコマンド完了後なので、
            //  「空 = まだ何も実行していない」が成立する)
            if executed.is_empty() {
                self.pty.kill_line()?;
                // Windows: kill_line は単独 ESC (0x1b) 送信。直後に間を置かず
                // コマンド文字列を送ると、コマンドが `[` 始まりの場合
                // (`[System.Environment]` 等) PowerShell の VT 入力デコーダが
                // `ESC` + `[` + 次バイトを 1 個の CSI シーケンスとして消費し、
                // 先頭 1-2 文字が欠落したまま実行される (ユーザ承認内容と実際に
                // 実行される文字列が食い違う trust 上の問題)。ESC が単独キーとして
                // 確定する猶予を与えるため送信を遅らせる。Unix は kill_line が
                // Ctrl+A+Ctrl+K でこの衝突が起きないため待つ必要はないが、
                // 分岐を増やさず両OSで同じ待ちを入れる (実害はコマンド実行が
                // 150ms 遅れるだけ)。
                thread::sleep(KILL_LINE_REDISPLAY_WAIT);
            }

            // ユーザが承認したコマンドをそのまま PTY に送る。ラップしない。
            self.pty.send_approved_command(&cmd)?;
            debug_log(&format!("=== exec start: {cmd}"));

            // コマンド実行完了待ち（passive 検出）。子プロセス死亡時は
            // 待っても意味がないので対話ごと抜ける (main loop も終了する)。
            match wait_for_command_completion(self.pty, self.pty_rx, self.ring_buffer)? {
                CommandWait::PtyDied => return Ok(ExecReport::PtyDied),
                CommandWait::Interrupted => {
                    // 実行中コマンドへ Ctrl+C は転送済み (中断済み)。このコマンド
                    // 自体は「実行した」として記録し、残りは送らず中止する。
                    // 確認画面での Ctrl+C と同じく follow-up はしない。
                    executed.push(format!("`{cmd}`"));
                    return Ok(ExecReport::Done {
                        executed,
                        outcome: ExecOutcome::Abort,
                    });
                }
                CommandWait::PromptReturned => {}
            }

            executed.push(format!("`{cmd}`"));
        }
        Ok(ExecReport::Done {
            executed,
            outcome: ExecOutcome::Completed,
        })
    }
}

fn format_ai_error(kind: ai::BackendKind, error: &ai::AiError) -> String {
    format!(
        "[{}] {}\nPlease check your login or usage limit.",
        kind.as_str(),
        error
    )
}

/// コマンド実行後に結果を AI へ自動問い合わせ (follow-up) するかの判定 (純関数、テスト対象)。
/// - `Abort` (Ctrl+C/Ctrl+D): 実行有無に関わらず常に no (AI に問わない)。
/// - `executed` が空 (全スキップ / q 即中止): no。
/// - AI が `command_result_followup: false` を宣言: no (`Quit` でも no で一貫)。
///   実行結果は ring_buffer の未送信 cursor に残り、次のユーザ質問時に送られる。
fn should_follow_up(outcome: &ExecOutcome, executed: &[String], followup: bool) -> bool {
    if matches!(outcome, ExecOutcome::Abort) {
        return false;
    }
    if executed.is_empty() {
        return false;
    }
    followup
}

/// follow-up の user_prompt 文言を組み立てる (純関数、テスト対象)。
/// `Quit` は「実行された分だけで分析」、`Completed` は「分析 + 追加提案」を指示する。
fn build_follow_up_text(outcome: &ExecOutcome, executed: &[String]) -> String {
    if matches!(outcome, ExecOutcome::Quit) {
        format!(
            "ユーザが残りのコマンドの実行を中止しました。実行されたコマンド: {}。出力は terminal フェンスに含まれます。実行された分だけで分析してください。",
            executed.join(", ")
        )
    } else {
        format!(
            "実行したコマンド: {}。出力は terminal フェンスに含まれます。分析してください。追加の操作が必要であれば提案してください。",
            executed.join(", ")
        )
    }
}

/// Y/n/a 確認の入力イベントを 1 決定に解決するまで待つ。
/// 確認待ち中に届く無関係イベント (PtyData / PassthroughEnded / AiPrompt) は
/// 無視して読み直す。channel 切断 (入力スレッド消滅 = 退出間際) は Skip 扱い (旧挙動互換)。
fn wait_confirm_decision(input_rx: &mpsc::Receiver<ui::InputEvent>) -> ConfirmDecision {
    loop {
        match input_rx.recv() {
            Ok(ui::InputEvent::Confirm(choice)) => match choice {
                ui::ConfirmChoice::Yes => return ConfirmDecision::Run,
                ui::ConfirmChoice::No => return ConfirmDecision::Skip,
                ui::ConfirmChoice::All => return ConfirmDecision::RunRest,
                ui::ConfirmChoice::Quit => return ConfirmDecision::QuitRest,
            },
            // Ctrl+C / Ctrl+D: 残りすべてを中止し、AI に問い合わせない
            // (ESC は ConfirmChoice::No 経由で Skip = 1 回スキップに変更済み)
            Ok(ui::InputEvent::ReadLineCancelled) => return ConfirmDecision::AbortNoAi,
            Ok(ui::InputEvent::PtyData(_))
            | Ok(ui::InputEvent::PassthroughEnded)
            | Ok(ui::InputEvent::MinibufferCancelled)
            | Ok(ui::InputEvent::AiPrompt(_)) => continue,
            Err(_) => return ConfirmDecision::Skip,
        }
    }
}

/// AI 提案コマンド送信後の実行完了待ち (passive 検出)。
/// - PTY 出力をドレインして画面 / リングバッファ / sniffer へ
/// - stdin → PTY 転送（パスワード入力・対話応答・Ctrl+C 中断）
/// - SIGWINCH 検知（リサイズ追従）
/// - 完了判定: PTY 出力末尾がプロンプト形 + `PROMPT_QUIET_THRESHOLD` 静音
///
/// stdin に Ctrl+C (0x03) が含まれていたら、バイトはそのまま PTY へ転送して実行中
/// コマンドを中断させつつ中断フラグを立て、プロンプト復帰時に `Interrupted` を返す
/// (呼び出し側が残りコマンドを中止する)。Ctrl+D (0x04) は対話プログラムでの EOF を
/// 壊さないよう転送のみで中止扱いにはしない。
///
/// 子プロセス死亡時 (sudo reboot 等で SSH が切れた後はプロンプトに戻らないため、
/// sniffer ベースの完了判定だと永遠にハングする) は残り出力をドレインして
/// `PtyDied` を返す。
fn wait_for_command_completion(
    pty: &mut PtyHandler,
    pty_rx: &mpsc::Receiver<Vec<u8>>,
    ring_buffer: &mut RingBuffer,
) -> Result<CommandWait, Box<dyn std::error::Error>> {
    let mut sniffer = prompt_sniffer::PromptSniffer::new();
    let mut last_pty_activity = Instant::now();
    let mut chunk_count = 0usize;
    let mut interrupted = false;
    loop {
        if !pty.is_alive() {
            thread::sleep(crate::FINAL_DRAIN_WAIT);
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
            // Ctrl+C はそのまま実行中コマンドへ転送して中断させる。残りコマンドの
            // 中止判定はプロンプト復帰時に行う (この場で抜けると ^C + プロンプトの
            // 出力を取りこぼし、画面とリングバッファがずれる)。Ctrl+D (0x04) は
            // 対話プログラムの EOF として正当なので中止扱いにはしない。
            if stdin_bytes.contains(&0x03) {
                interrupted = true;
            }
            pty.write(&stdin_bytes)?;
        }
        if last_pty_activity.elapsed() >= PROMPT_QUIET_THRESHOLD && sniffer.matches_prompt() {
            sniffer.record_match();
            debug_log(&format!(
                "exec end: chunks={chunk_count} interrupted={interrupted}"
            ));
            return Ok(if interrupted {
                CommandWait::Interrupted
            } else {
                CommandWait::PromptReturned
            });
        }
        thread::sleep(EXEC_POLL_INTERVAL);
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
    pty: &mut PtyHandler,
    pty_rx: &mpsc::Receiver<Vec<u8>>,
    ring_buffer: &mut RingBuffer,
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
    fn confirm_quit_is_quit_rest() {
        let rx = rx_with(vec![ui::InputEvent::Confirm(ui::ConfirmChoice::Quit)]);
        assert_eq!(wait_confirm_decision(&rx), ConfirmDecision::QuitRest);
    }

    #[test]
    fn cancel_is_abort_no_ai() {
        // Ctrl+C / Ctrl+D 由来の ReadLineCancelled は AbortNoAi (AI 問い合わせなし)。
        let rx = rx_with(vec![ui::InputEvent::ReadLineCancelled]);
        assert_eq!(wait_confirm_decision(&rx), ConfirmDecision::AbortNoAi);
    }

    #[test]
    fn unrelated_events_are_ignored_until_decision() {
        let rx = rx_with(vec![
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

    #[test]
    fn follow_up_matrix() {
        // {Completed, Quit, Abort} × {followup} × {executed 空/非空} の確定仕様を固定。
        let some = vec!["ls".to_string()];
        let none: Vec<String> = vec![];
        // Abort は常に no (実行済みがあっても AI に問わない)。
        assert!(!should_follow_up(&ExecOutcome::Abort, &some, true));
        assert!(!should_follow_up(&ExecOutcome::Abort, &none, true));
        assert!(!should_follow_up(&ExecOutcome::Abort, &some, false));
        // executed 空は常に no。
        assert!(!should_follow_up(&ExecOutcome::Completed, &none, true));
        assert!(!should_follow_up(&ExecOutcome::Quit, &none, true));
        // followup=false は Quit でも no (一切 follow-up なしで一貫)。
        assert!(!should_follow_up(&ExecOutcome::Completed, &some, false));
        assert!(!should_follow_up(&ExecOutcome::Quit, &some, false));
        // 従来動作: 実行済みあり + followup=true のみ yes。
        assert!(should_follow_up(&ExecOutcome::Completed, &some, true));
        assert!(should_follow_up(&ExecOutcome::Quit, &some, true));
    }

    #[test]
    fn follow_up_text_distinguishes_quit_from_completed() {
        let executed = vec!["ls".to_string(), "df -h".to_string()];
        let quit = build_follow_up_text(&ExecOutcome::Quit, &executed);
        assert!(quit.contains("中止しました"));
        assert!(quit.contains("ls, df -h"));
        assert!(quit.contains("実行された分だけで分析"));
        let done = build_follow_up_text(&ExecOutcome::Completed, &executed);
        assert!(done.contains("実行したコマンド: ls, df -h"));
        assert!(done.contains("追加の操作が必要であれば提案"));
    }

    #[test]
    fn ai_error_message_names_backend_and_hints_login_or_limit() {
        let error = ai::AiError::NonZeroExit {
            stderr: "rate limit exceeded".to_string(),
        };
        assert_eq!(
            format_ai_error(ai::BackendKind::Claude, &error),
            "[claude] AI CLI failed: rate limit exceeded\nPlease check your login or usage limit."
        );
    }
}
