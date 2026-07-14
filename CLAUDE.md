# aish

CLI SSH + AI (Claude Code)。ローカルシェル / SSH 接続先サーバを、クライアント側 Claude Code から調査・操作する対話型ツール。

## 開発環境
- Rust。ビルド: `export PATH="$HOME/.cargo/bin:$PATH" && cargo build`
- 対応 OS: Linux (Ubuntu) / macOS / Windows 10 1809+ native（Windows Terminal 推奨。**Windows 実機検証済み (2026-07)** — チェックリストは SPEC.md § 15.13）
- CI (`ci.yml`): 全 push で `cargo fmt --all -- --check` / `cargo clippy --all-targets -- -D warnings` (ubuntu) / `cargo test` (ubuntu + macOS)。**push 前に 3 つともローカルで通す**（`release.yml`=タグ push リリースとは独立）。
  - **テストは `cargo test`、`--lib` を付けない**（bin-only crate なので 0 件になる）。
  - **clippy の構造的 lint は一部 `#[allow(clippy::...)]` で意図的に抑制**（`utf8_char_len` の `if_same_then_else`、minibuffer/echo の `write_with_newline`、`compute_visual_layout` の `needless_range_loop`、minibuffer 関数群の `too_many_arguments`、入力スレッドの `while_let_loop`）。trust-critical / 意図的コード温存のため、**安易に外して writeln! 化やリファクタしない**。

## 仕様

アーキテクチャ・UI・キー入力・AI 連携・設定・エラー挙動・既知の制約・**実装ノート/落とし穴**の詳細は **[SPEC.md](./SPEC.md)** を参照。

## 信頼の根幹

SSH でサーバを管理する道具なので、**ユーザが画面で承認したコマンド = サーバで実行されるコマンド**を保ち、**サーバ側に勝手な書き込みをしない**ことが大原則。

避けるべき行為: AI 提案コマンドのラップ・変形（マーカーラッパ等）/ `PROMPT_COMMAND`・`precmd`・`set +o history` 等での黙った shell 書き換え / `HISTCONTROL=ignorespace` 依存等の履歴抑制 / shell 統合シーケンスの自動セットアップ。

完了判定や exit code 取得が必要でも **passive 検出**（PTY 出力を観察するだけ）の範囲で実現し、取れない情報は諦める。

## 実装上の注意

コードから直ちに読み取れず間違えやすいルール。**背景・理由・過去バグ経緯・エッジケースは [SPEC.md](./SPEC.md) § 15 を参照**（CLAUDE.md=ルール、SPEC.md § 15=詳細）。trust-critical な「〜しない」は根拠が § 15 にあるので必ず守る。

端末入力 / termios（§ 15.1）:
- **raw モードはセッション全体で維持**（`save_terminal_settings`）。個別関数での再設定・復元をしない。
- **低レベル入力の framing は `src/input.rs` に集約**。fd 0 直読みを増やさない。passthrough は `ev.raw` をそのまま PTY へ送り、**`Tok::Char` を再エンコードしない**。byte→Tok は golden test で固定。
- **termios は `c_lflag`(ICANON|ECHO|ISIG|IEXTEN) + `c_iflag` raw 化群を落とす。`c_oflag`(OPOST) は触らない**。
- **パススルーの ESC/CSI/SS3 は完全な形まで読み切ってからまとめて送る。追加 byte 読みは poll(50ms) 付き**（blocking にしない）。

確認プロンプト Y/n/a/q（§ 15.2）:
- **1 キー即確定**（`read_confirm_key`）。Enter/Space=実行、`n`=1 回スキップ、`a`=残り自動承認、`q`=残り中止(follow-up あり)、Ctrl+C/Ctrl+D=残り中止(AI に問わない)、**ESC=`n`**。未知キーは無視して再読み取り。**ESC を Ctrl+C 系 abort に戻さない**。
- **Enter は `b < 0x20` 判定より先に `Tok::Enter` に分類**（golden test で固定）。
- **`echo_confirm` は `match` を持たず `write!("{c}\x1b[0m\n")` だけ。大小区別の分岐を足さない**。キャンセル(Ctrl+C/Ctrl+D)時は抜ける前に stdout へ `\n` を 1 つ出す。

AI コマンド実行（§ 15.3, 15.7）:
- **実行中の Ctrl+C(0x03) は実行中コマンドへ転送して中断 + 残りコマンド中止**（`ExecOutcome::Abort`、follow-up なし、両承認モードで一様）。**Ctrl+D(0x04) は対象外**（転送のみ）。
- **AI 応答の `command_result_followup: false` は実行後の AI 自動問い合わせを抑制**（`q` でも抑制、欠落時 true = 従来動作）。判定基準は Claude schema description と `build_system_prompt` の**両方に同一文言**で記述（片方だけ直さない）。
- **制御文字ガードは `VettedCommand` 型**。表示・送信が `&VettedCommand` のみ受理し「**承認した物 = 実行する物**」を型で保証（撤去・迂回は型エラー）。
- **AI 由来 `message`/`commands` は描画前に制御文字を caret 可視化**（`visualize_control_line`）。**生 `println!` に戻さない**。
- **完了判定は `PromptSniffer` の passive 検出**（プロンプト形 + 200ms 静音）。

minibuffer / 打ちかけ（§ 15.4, 15.5）:
- **aishプロンプト表示中は PTY 描画を抑制**（`MINIBUFFER_ACTIVE`。リングバッファ記録は継続）。
- **`show_minibuffer` の scroll は cursor を実画面最下行に置いた全画面 LF scroll。DECSTBM region は使わない**。cursor 復元は絶対座標。**shrink branch の行クリアは撤去しない**。
- **minibuffer キャンセルは `InputEvent::MinibufferCancelled` → main loop の `refresh_prompt`**（打ちかけは消えるが未承認 submit はしない）。`show_minibuffer` 側は stdout に `\n` を出さない。
- **minibuffer は bracketed paste マーカーを honor。aish 自身は `ESC[?2004h` を出さない**（端末状態を変えない原則）。
- **`0x01,0x0b`(Ctrl+A+Ctrl+K) リテラルは `pty_handler.rs` 以外に書かない**（`kill_line`/`refresh_prompt` にカプセル化）。プロンプトリフレッシュ改行は必ず `refresh_prompt()`、素の `pty.write(b"\n")` を書かない（未承認 submit 防止 = 信頼の根幹）。
- **AI 対話直前（`run` 冒頭）に打ちかけを消去してから drain**（撤去時は pyte で再検証）。

`/model` `/effort` ピッカー（§ 15.12）:
- **`ui::show_picker` は confirm と同じく main スレッドが fd0 直読みの同期ブロッキング関数**（`InputEvent`/`InputRequest` 経由にしない）。**termios 再設定はしない**。stdout 専用で PTY に書かない。
- **領域は先に `\n`×N で確保→原点へ戻る相対描画（末尾改行を出さない）。DECSTBM 不使用**。終了時 `\x1b[<L-1>A\r\x1b[0J` で消去。**ナビは純関数 `picker_step` に分離し golden test**。
- **候補解決は backend ごと `available_models`/`available_efforts` = static list > 取得コマンド > 組み込み既定**（`common::resolve_option_list`）。取得コマンドは**ピッカーを開く時だけローカル実行**。effort 既定は claude/codex/copilot のみ、model 既定は全 native backend（generic は recipe 由来のみ）。
- **`/model` `/effort` は常に `Some(...)` を返す**。引数なし=ピッカー、`-`/`clear`=クリア、その他=検証せず set。

platform 層 / Windows（§ 15.13）:
- **低レベル端末操作（raw mode・poll 付き 1byte 読み・端末サイズ・DSR・リサイズ・Ctrl+C 検出・PID 生存確認）は `src/term/` に集約**。ui.rs 等で libc / Console API を直接叩かない。**term/unix.rs は ui.rs からの純移動でロジック変更禁止**（§ 15.1 の termios ルール準拠）。
- **Windows 入力は `ReadConsoleInputW` ポンプ 1 本**（term/windows.rs。ReadFile/ReadConsoleW 併用禁止）。**Ctrl+/ は uChar 値に依らず（0x1f/0/0x2f）Ctrl+VK_OEM_2 を 0x1f に正規化**（native/一部 VT 端末/RDP・Remmina のレイアウト変換すべてに対応 = エントリキーの生命線。`if unit==0` ガードより前で判定し、Shift は見ない）。**stdout の `DISABLE_NEWLINE_AUTO_RETURN` は設定しない**（Unix の OPOST 不可触と同義）。
- **Windows の kill_line/refresh_prompt は ESC(0x1b)**（PSReadLine に 0x01,0x0b は効かず未承認 submit リスク）。既定シェルは powershell.exe。cmd の末尾空白なしプロンプトは sniffer の `is_cmd_style_prompt` 特例（**学習させない**）。
- **Windows の自己更新（`--update`）は Unsupported を維持**。ただし**エラーで落とさず、README と同じ手動再インストール用 PowerShell コマンドを表示して `Ok(())` で終える**（`run_update` 冒頭で `cfg!(windows)` 分岐 → `windows_update_hint(channel)`。tag 行は `--stable`/`--prerelease` に追従）。Windows バイナリ（x86_64/aarch64 の生 exe + zip）は release に同梱済み（`release.yml` の matrix）。

drain / 入力スレッド（§ 15.6）:
- **PTY 吸い出しは全て `pty_drain::drain_pty` 経由**（手書き try_recv ループを再導入しない）。**通常動作中は PTY 出力に独自文字列を挿入しない**。
- **入力スレッド再開は `InputGate` + `rearm_on_drop()` RAII guard**（idle に戻る arm の入口で取得。手書き arm 呼び出しを増やさない）。

その他 UI（§ 15.8）:
- **TUI(vim/less/top) 終了後は aish から何も出さない**。**Shift+Enter 非対応**（改行は `Alt+Enter`）。**IME の未確定文字(preedit) は取得不能**。

ring_buffer / backend（§ 15.9）:
- **未送信 cursor は backend ごと独立**（`get_unsent_for`/`mark_sent_for`、sent_marks は `HashMap`）。**新規 native backend は `all_native()` に追加**（enum 名 ≠ 実行ファイル名なら `binary()` に分岐）。
- **`BackendKind::parse` は native → generic registry の 2 段。`main::run` は Config::load → `init_generics` → parse の順序を守る**。
- **選択 backend が未インストールなら `ai::auto_detect_backend()` で実在する AI CLI にフォールバック**（探索順 = `auto_detect_order()`: Claude→Codex→Gemini→copilot→cursor→qwen の固定人気順 → generic registry 順。**REST backend の Cloudflare/Nvidia は binary=curl で誤検出するため除外**）。順序は純関数で golden test 固定。切替時は `println!` で 1 行通知（raw モードでも OPOST/newline-auto-return 前提で桁ズレしない）。1 つも無ければ **`ai::install_guide()`（各 AI 名 + 公式 URL の一覧、`auto_detect_order` と同じ 6 種を人気順）を `MissingAiBackend` に載せて Err**。**main は `MissingAiBackend` を downcast し `Error:` プレフィックスを付けずに整形出力**（他の Err は従来どおり `Error:` 付き）。URL 一覧は `install_guide()` が唯一の定義。
- **AI 応答の注釈記録は `record_ai_exchange` 経由**（append → `mark_sent_for(current)` の順序不変条件）。`/clear` は `mark_sent_all()`（AI CLI 内部 session は current のみリセット）。

AI backends（§ 15.10）:
- **Claude: 毎ターン守らせるルールは system prompt でなく `--json-schema` の description に書く**（system prompt は初回のみ）。独立コマンドは `;` 連結せず `commands` 配列に分割（`&&`/`||`・制御構文内の `;` は維持）。
- **Claude `--disallowedTools` は `MANDATORY_DENY`(Bash/Edit/Write) を常に union し args 末尾で push**（**前方へ戻したり union を外したりしない**）。
- **cursor は `--trust` 常時 + `--mode plan`、copilot は `-p` 無し + 四段 deny。`--yolo`/Run Everything 系は絶対に付けない**（固定埋め込み、config 不可）。
- **Generic backend（ユーザの新規 `[[ai.providers]]`）は安全フラグを自動付与しない**（recipe 著者が明示する）。
- **組み込みデフォルト recipe は `config::builtin_providers()` 1 関数に集約し、安全フラグを `args` に焼き込んで同梱**（generic 原則の例外）。**read-only/plan 相当を強制できない CLI は同梱しない**。ユーザ上書きは同名エントリの**フィールド単位マージ**（`args`/`env` は丸ごと置換）。registry へ渡すのは `resolved_providers`。
- **OpenCode（`opencode`）は generic recipe として同梱**。read-only は `OPENCODE_CONFIG_CONTENT` env 注入（recipe の `env` フィールド → `run_cli_capture_stdout_env`）+ deny 付き専用 agent `aish`（task/todowrite も無効化）で強制。**`--auto` は絶対に付けない / `--agent plan` に変えない**（plan agent は ask ベースで headless ハング。経緯は SPEC.md § 15.10）。
- **Cloudflare Workers AI は native backend `cloudflare`**（`src/ai/cloudflare_workers_ai.rs`）。REST を `curl` 経由で叩き HTTP クレートを足さない。`binary()`="curl"。認証は環境変数のみ（config 不可）。**呼び出し名 `cloudflare`、設定セクション/ファイル名は `cloudflare-workers-ai` 系**。curl は `-f` を付けず `success` を見る。session 無し（内部 history）。
- **NVIDIA NIM は native backend `nvidia`**（`src/ai/nvidia_nim.rs`、cloudflare と同方式の curl REST）。認証は環境変数 `NVIDIA_API_KEY` のみ（config 不可）。**呼び出し名 `nvidia`、設定セクションは `nvidia-nim`**。成功判定は `choices` の有無（NIM のエラーは JSON とは限らない: 存在しない model は素のテキスト "404 page not found"）。
- **xAI Grok Build (`grok`) は read-only 強制を実機検証できなかったため同梱 recipe にしない**（`config.toml.example` にコメントアウト例のみ、経緯は SPEC.md § 15.10）。`grok` はコミュニティ製 `@vibe-kit/grok-cli`（npm）ともバイナリ名が衝突しうるので、ユーザが登録する際は `which -a grok` 確認を促すこと。

セルフアップデート（§ 15.11）:
- **`aish --update` は `--stable`(既定=`/releases/latest`) / `--prerelease`(`/releases[0]`) の 2 チャネル。この向きを逆にしない**。prerelease は Cargo.toml の `version` にも識別子(`0.9.0-rc.1`)を含める。
- **更新バイナリの tmp はインストール先と同一ディレクトリに置き `rename()` で原子置換。`/tmp` 経由 + copy fallback にしない**。

## 開発フロー

明示依頼された作業のフロー（「信頼の根幹」を守る運用ルール。デフォルトの「commit は明示要求時のみ」を上書き）:

- **プロンプトが疑問文で終わるなら質問であって作業依頼ではない**。勝手に修正せず、答えて必要なら修正案を提示し指示を待つ。
- **ソース修正前に、仕様・要件の疑問点は全て必ずユーザに確認**。曖昧な前提で進めない。
- **新しい仕様 / 落とし穴 / 不変条件を導入したら同じ作業内でドキュメント追記**: CLAUDE.md に **1 行ルール**、背景・理由・経緯は SPEC.md § 15（住み分けを保つ）。typo・cosmetic refactor・既存仕様内の小修正は対象外。迷ったら追記する側に倒す。
- **完了時、コード変更 + CLAUDE.md / SPEC.md 追記を 1 commit に自動作成してよい**（都度確認不要）。**メッセージは短く自動生成**（`Feat:`/`Fix:`/`Refactor:`/`Docs:`/`Chore:` + 1 行。body は原則なし）。
- **PR は一切作らない**（`gh pr create` 等を提案しない）。
- **push は別途ユーザに確認する**。
- **タグ付け**: `git tag` + `git push --tags` で GitHub Action がリリース。`git push` は自動で行わない。
- **リリースの既定チャネルは prerelease**。ユーザが「stable / 安定版」と明確に指示しない限り、新バージョンは prerelease（ハイフン付き識別子 or `gh release edit --prerelease --latest=false`）にする。安定版は明示要求時のみ（詳細は `/release` スキル § 1）。

## 設定ファイル
- `~/.aish/config.toml` (TOML)。`--config <path>` で変更可能。サンプルは `config.toml.example`。
