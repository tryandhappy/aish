# aish

CLI SSH + AI (Claude Code)。ローカルシェル / SSH 接続先サーバを、クライアント側 Claude Code から調査・操作する対話型ツール。

## 開発環境
- 言語: Rust。ビルド: `export PATH="$HOME/.cargo/bin:$PATH" && cargo build`
- 対応 OS: Linux (Ubuntu) / macOS / Windows（UI 部は Unix 限定、Windows は `read_line_cooked` フォールバック）
- CI (`.github/workflows/ci.yml`): 全 push で `cargo fmt --all -- --check` / `cargo clippy --all-targets -- -D warnings` (ubuntu) / `cargo test` (ubuntu + macOS)。**push 前に 3 つともローカルで通す**。`release.yml`（タグ push でのリリース）とは独立。
  - **テストは `cargo test`、`--lib` を付けない**: bin-only crate（`src/lib.rs` なし）なので `cargo test --lib` は 0 件。
  - **clippy `-D warnings` は構造的 lint を一部 `#[allow(clippy::...)]` で抑制**（`utf8_char_len` の `if_same_then_else`、minibuffer/echo の `write_with_newline`、`compute_visual_layout` の `needless_range_loop`、minibuffer 関数群の `too_many_arguments`、入力スレッドの `while_let_loop`）。trust-critical / 意図的コード温存のためで、**安易に外して writeln! 化やリファクタしない**。

## 仕様

アーキテクチャ・動作モード・UI・キー入力・AI 連携・リングバッファ・スレッド構成・設定ファイル・エラー挙動・既知の制約・**実装ノート/落とし穴**などの詳細は **[SPEC.md](./SPEC.md)** を参照。

## 信頼の根幹

SSH でサーバを管理する道具なので、**ユーザが画面で承認したコマンド = サーバで実行されるコマンド** を保ち、**サーバ側に勝手な書き込みをしない**ことが大原則。

避けるべき行為:
- AI 提案コマンドをラップして別文字列に変形（マーカーラッパ等）
- `PROMPT_COMMAND` / `precmd` / `set +o history` で shell 環境を黙って書き換え
- `HISTCONTROL=ignorespace` 依存等の「履歴に残さない工夫」
- 任意の shell 統合シーケンスの自動セットアップ

完了判定や exit code 取得が必要でも **passive 検出**（PTY 出力を観察するだけ）の範囲で実現し、取れない情報は諦める。

## 実装上の注意

コードから直ちに読み取れず間違えやすいルール。**各ルールの背景・理由・過去バグ経緯・エッジケースは [SPEC.md](./SPEC.md) § 15「実装ノート（落とし穴）」を参照**（CLAUDE.md=ルール、SPEC.md § 15=詳細）。trust-critical な「〜しないこと」は必ず守る（消えているのは長い説明だけで根拠は § 15 にある）。

端末入力 / termios（§ 15.1）:
- **raw モードはセッション全体で維持**（`save_terminal_settings`）。`passthrough` / `read_confirm_key` 個別での再設定・復元はしない。
- **低レベル入力の framing は `src/input.rs` に集約**。fd 0 直読み（`ManuallyDrop::from_raw_fd(0)`）を増やさない。passthrough は `ev.raw` をそのまま PTY へ送り、**`Tok::Char` を再エンコードして送らない**。byte→Tok は golden test で固定。
- **termios は `c_lflag`(ICANON|ECHO|ISIG|IEXTEN) + `c_iflag` raw 化群(IGNBRK..ICRNL|IXON) を落とす。`c_oflag`(OPOST) は触らない**。
- **パススルーの ESC/CSI/SS3 は完全な形まで読み切ってからまとめて送る。追加 byte 読みは poll(50ms) 付き**（blocking だと partial sequence で全入力がハングする）。

確認プロンプト Y/n/a/q（§ 15.2）:
- **1 キー即確定**（`read_confirm_key`）。Enter/Space=実行、`n`=1 回スキップ、`a`=残り自動承認、`q`=残り中止(follow-up あり)、Ctrl+C/Ctrl+D=残り中止(AI に問わない)、**ESC=`n`(1 回スキップ)**。未知キーは無視して再読み取り。**ESC を Ctrl+C 系 abort に戻さない**。
- **Enter は `b < 0x20` 判定より先に `Tok::Enter` に分類**（過去 2 回再発した「Enter が効かない」回帰。golden test が防ぐ）。
- **`echo_confirm` は `match` を持たず `write!("{c}\x1b[0m\n")` だけ。大小区別のため分岐を足さない**。キャンセル(Ctrl+C/Ctrl+D)時は抜ける前に stdout へ `\n` を 1 つ出す。

AI コマンド実行（§ 15.3, 15.7）:
- **実行中の Ctrl+C(0x03) は実行中コマンドへ転送して中断 + 残りコマンドを中止**（`ExecOutcome::Abort`、follow-up なし、両承認モードで一様）。**Ctrl+D(0x04) は対象外**（転送のみ）。
- **制御文字ガードは `VettedCommand` 型**。表示・送信が `&VettedCommand` のみ受理し「**承認した物 = 実行する物**」を型で保証（撤去・迂回は型エラー）。
- **AI 由来 `message`/`commands` は描画前に制御文字を caret 可視化**（`visualize_control_line`）。**生 `println!` に戻さない**（見た目 ≠ 送るバイトの偽装防止）。
- **完了判定は `PromptSniffer` の passive 検出**（プロンプト形 + 200ms 静音）。

minibuffer / 打ちかけ（§ 15.4, 15.5）:
- **aishプロンプト表示中は PTY 描画を抑制**（`MINIBUFFER_ACTIVE`。リングバッファ記録は継続）。
- **`show_minibuffer` の scroll は cursor を実画面最下行に置いた全画面 LF scroll。DECSTBM region は使わない**（region scroll は scrollback を破棄する）。cursor 復元は絶対座標。**shrink branch の行クリアは撤去しない**。
- **minibuffer キャンセルは `InputEvent::MinibufferCancelled` → main loop の `refresh_prompt` でプロンプト再表示**（打ちかけは消えるが未承認 submit はしない）。`show_minibuffer` 側は stdout に `\n` を出さない。
- **minibuffer は bracketed paste マーカーを honor。aish 自身は `ESC[?2004h` を出さない**（端末状態を変えない原則）。
- **`0x01,0x0b`(Ctrl+A+Ctrl+K) リテラルは `pty_handler.rs` 以外に書かない**（`kill_line`/`refresh_prompt` にカプセル化）。プロンプトリフレッシュ改行は必ず `refresh_prompt()` を使い、素の `pty.write(b"\n")` を書かない（打ちかけの未承認 submit 防止 = 信頼の根幹）。
- **AI 対話直前（`run` 冒頭）に打ちかけを消去してから drain**（折り返し打ちかけ + n キャンセルで `Exec?` 行が上書きされるのを防ぐ。撤去時は pyte で再検証）。

`/model` `/effort` ピッカー（§ 15.12）:
- **`ui::show_picker` は confirm と同じく main スレッドが fd0 直読みの同期ブロッキング関数**（`AiPrompt` arm 内 = 入力スレッド parked 前提。`InputEvent`/`InputRequest` 経由にしない）。**termios 再設定はしない**（raw はセッション維持）。stdout 専用で PTY に書かない。
- **領域は先に `\n`×N で確保→原点へ戻り、以降は最終行末で止まる相対描画**（末尾改行を出さず予約超え scroll を防ぐ。DECSTBM 不使用）。終了時に `\x1b[<L-1>A\r\x1b[0J` で消去し原点へ（後続の `print_slash_result` が原点から出す）。**ナビは純関数 `picker_step` に分離し golden test**（↑↓ クランプ / Enter=Select / Esc・Ctrl+C・Ctrl+D=Cancel）。
- **候補解決は backend ごと `available_models`/`available_efforts`（trait）= static list > 取得コマンド > 組み込み既定**（`common::resolve_option_list`）。取得コマンドは**ピッカーを開く時だけローカル実行**（起動時に走らせない。サーバ書き込み・承認フローと無関係）。effort 既定は claude/codex/copilot のみ同梱、model 既定は無し。
- **`/model` `/effort` は常に `Some(...)` を返す**（None だと通常 AI プロンプト扱い）。引数なし=ピッカー、`-`/`clear`=クリア、その他=検証せず set（ヒントのみ）。

drain / 入力スレッド（§ 15.6）:
- **PTY 吸い出しは全て `pty_drain::drain_pty` 経由**（手書き try_recv ループを再導入しない。全 data を ring_buffer に記録する不変条件を一元保証）。**通常動作中は PTY 出力に独自文字列を挿入しない**。
- **入力スレッド再開は `InputGate` + `rearm_on_drop()` RAII guard**（idle に戻る arm の入口で取得。手書き arm 呼び出しを増やさない）。

その他 UI（§ 15.8）:
- **TUI(vim/less/top) 終了後は aish から何も出さない**。**Shift+Enter 非対応**（改行は `Alt+Enter`）。**IME の未確定文字(preedit) は取得不能**。

ring_buffer / backend（§ 15.9）:
- **未送信 cursor は backend ごと独立**（`get_unsent_for`/`mark_sent_for`、sent_marks は `HashMap`）。**新規 native backend は `all_native()` に追加**（enum 名 ≠ 実行ファイル名なら `binary()` に分岐）。
- **`BackendKind::parse` は native → generic registry の 2 段。`main::run` は Config::load → `init_generics` → parse の順序を守る**。
- **AI 応答の注釈記録は `record_ai_exchange` 経由**（append → `mark_sent_for(current)` の順序不変条件。ばらに戻さない）。`/clear` は `mark_sent_all()`（AI CLI 内部 session は current のみリセット）。

AI backends（§ 15.10）:
- **Claude: 毎ターン守らせるルールは system prompt でなく `--json-schema` の description に書く**（system prompt は初回のみ）。独立コマンドは `;` 連結せず `commands` 配列に分割（ただし `&&`/`||`・制御構文内の `;` は維持）。
- **Claude `--disallowedTools` は `MANDATORY_DENY`(Bash/Edit/Write) を常に union し args 末尾で push**（extra_args で剥がせないように。**前方へ戻したり union を外したりしない**）。
- **cursor は `--trust` 常時 + `--mode plan`、copilot は `-p` 無し + 四段 deny。`--yolo`/Run Everything 系は絶対に付けない**（固定埋め込み、config 不可）。
- **Generic backend（ユーザの新規 `[[ai.providers]]`）は安全フラグを自動付与しない**（recipe 著者が `--mode plan` 等を明示。信頼できない CLI は確認 UI を迂回し得る）。
- **組み込みデフォルト recipe は `config::builtin_providers()` 1 関数に集約し、aish が著者として安全フラグを `args` に焼き込んで同梱する**（generic 原則の例外）。**read-only/plan 相当を強制できない CLI は同梱しない**（実機で要検証）。ユーザ上書きは `[[ai.providers]]` の同名エントリで**フィールド単位マージ**（書いたフィールドだけ。`args` は丸ごと置換）。registry へ渡すのは `resolved_providers`（生 `providers` でない）。

セルフアップデート（§ 15.11）:
- **`aish --update` は `--stable`(既定=`/releases/latest`) / `--prerelease`(`/releases[0]`) の 2 チャネル。この向きを逆にしない**。prerelease は Cargo.toml の `version` にも識別子(`0.9.0-rc.1`)を含める。

## 開発フロー

ユーザから明示依頼された作業を行うときのフロー（「信頼の根幹」を守る運用ルールであり、デフォルトの「commit は明示要求時のみ」を上書きする）:

- **プロンプトが疑問文で終わるなら質問であって作業依頼ではない**。勝手にソースを修正せず、まず答え、必要なら修正案を提示して指示を待つ。
- **ソース修正前に、仕様や要件の疑問点は全て必ずユーザに確認する**。曖昧な前提で進めない。迷ったら手を動かす前に聞く。
- **新しい仕様 / 落とし穴 / 不変条件を導入したら、同じ作業フロー内でドキュメント追記する**: CLAUDE.md「実装上の注意」に **1 行ルール**、背景・理由・過去バグ経緯・エッジケースは SPEC.md § 15 に（住み分けを保つ）。対象はコードから読み取れず後で間違えうる挙動。typo・cosmetic refactor・既存仕様内の小修正は対象外。迷ったら追記する側に倒す。
- **作業完了時、コード変更 + CLAUDE.md / SPEC.md 追記を 1 commit にまとめて自動作成してよい**（都度「commit しますか？」と聞かなくてよい）。**メッセージは自動で短く作る**（`Feat:`/`Fix:`/`Refactor:`/`Docs:`/`Chore:` 等の日本語プレフィックス + 1 行の短い説明。body は原則付けない）。
- **PR は一切作らない**（GitHub の PR 作成手順・`gh pr create` 等を提案しない）。
- **push は別途ユーザに確認する**。
- **タグ付け**: `git tag` でタグを付け、`git push --tags` で GitHub Action が動きリリースされる。`git push` は自動で行わない。

## 設定ファイル
- `~/.aish/config.toml` (TOML)。`--config <path>` で変更可能。サンプルは `config.toml.example`。
