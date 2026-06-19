# aish

CLI SSH + AI (Claude Code)。ローカルシェル または SSH接続先サーバを、クライアント側のClaude Codeから調査・操作する対話型ツール。

## 開発環境
- 言語: Rust
- ビルド: `export PATH="$HOME/.cargo/bin:$PATH" && cargo build`
- 対応OS: Linux (Ubuntu), macOS, Windows（UI部はUnix限定、Windowsは `read_line_cooked` フォールバック）
- CI: `.github/workflows/ci.yml` が全 push で `cargo fmt --all -- --check` / `cargo clippy --all-targets -- -D warnings` (ubuntu) と `cargo test` (ubuntu + macOS) を回す。**push 前にこの 3 つをローカルで通すこと**。`release.yml` (タグ push でのリリース) とは独立。
  - **テスト実行は `cargo test`。`--lib` を付けない**: aish は bin-only crate (`src/lib.rs` なし) なので `cargo test --lib` はターゲット不在で 0 件になる。
  - **clippy `-D warnings` は構造的 lint を一部 `#[allow(clippy::...)]` で抑制している** (`utf8_char_len` の `if_same_then_else`、minibuffer/echo の `write_with_newline`、`compute_visual_layout` の `needless_range_loop`、minibuffer 関数群の `too_many_arguments`、入力スレッドの `while_let_loop`)。いずれも trust-critical / 意図的なコードを温存するためで、**安易に外して writeln! 化やリファクタをしない**。

## 仕様

詳細な仕様（アーキテクチャ、動作モード、UI要素、キー入力、AI連携、リングバッファ、スレッド構成、設定ファイル、エラー挙動、既知の制約、**実装ノート/落とし穴**など）は **[SPEC.md](./SPEC.md)** を参照。

## 信頼の根幹

aish は SSH でサーバを管理する道具なので、**ユーザが画面で承認したコマンド = サーバで実行されるコマンド** を保つこと、**サーバ側に勝手な書き込みをしないこと** が大原則。

具体的に避けるべき行為:
- AI 提案コマンドをラップして別の文字列に変形する（マーカーラッパ等）
- `PROMPT_COMMAND` / `precmd` / `set +o history` で shell 環境を黙って書き換える
- `HISTCONTROL=ignorespace` 依存等の「履歴に残さない工夫」
- 任意の shell 統合シーケンスの自動セットアップ

完了判定や exit code 取得が必要でも、**passive 検出**（PTY 出力を観察するだけ）の範囲で実現する。それで取れない情報は諦める。

## 実装上の注意

コードから直ちに読み取れず間違えやすいルール。**各ルールの背景・理由・過去バグ経緯・エッジケースは [SPEC.md](./SPEC.md) § 15「実装ノート（落とし穴）」を参照**。trust-critical な「〜しないこと」は必ず守る（消えているのは長い説明だけで、根拠は § 15 にある）。

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
- **Generic backend は安全フラグを自動付与しない**（recipe 著者が `--mode plan` 等を明示。信頼できない CLI は確認 UI を迂回し得る）。

セルフアップデート（§ 15.11）:
- **`aish --update` は `--stable`(既定=`/releases/latest`) / `--prerelease`(`/releases[0]`) の 2 チャネル。この向きを逆にしない**。prerelease は Cargo.toml の `version` にも識別子(`0.9.0-rc.1`)を含める。

## 開発フロー

ユーザから明示的に依頼された作業を行うときは、以下のフローに従う。これは CLAUDE.md「信頼の根幹」を守るための運用ルールであり、デフォルトの「commit は明示要求があるときだけ」を上書きする。

- **ユーザのプロンプトが疑問文で終わっている場合、それは質問であって作業依頼ではない**。勝手にソースコードを修正してはいけない。まず質問に答え、必要であれば修正案を提示してユーザの指示を待つ。
- **ソースコードを修正する前に、仕様や要件に疑問点があれば全て必ずユーザに確認する**。曖昧な前提のまま作業を進めない。判断に迷ったら手を動かす前に聞く。
- **コードに新しい仕様 / 落とし穴 / 不変条件を導入する変更を行ったら、同じ作業フローの中でドキュメントへの追記を必ず行う**。CLAUDE.md「実装上の注意」に **1 行ルール**を足し、その**背景・理由・過去バグ経緯・エッジケースは SPEC.md § 15「実装ノート」**に書く（CLAUDE.md=ルール、SPEC.md § 15=詳細、という住み分けを保つ）。対象は、コードから直ちには読み取れず後から見て間違える可能性のある挙動。typo 修正・cosmetic refactor・既存仕様の範囲内の小さな修正は対象外。判断に迷ったら追記する側に倒す。
- **依頼された作業が完了したら、コード変更と CLAUDE.md / SPEC.md の追記を 1 つの commit にまとめて自動作成してよい**。ユーザに「commit しますか？」と都度確認しなくてよい。コミットメッセージは既存スタイル (`Feat:` / `Fix:` / `Refactor:` / `Docs:` / `Chore:` 等の日本語プレフィックス + 短い説明、必要なら body) に従う。
- **push は別途ユーザに確認する**。リモートに公開する操作は引き続き確認が必要。
- **タグ付け / git tag タグをつける。git push --tagsでGitHub Actionが動き出し、リリースされるが、git pushは自動では行わない。

## 設定ファイル
- `~/.aish/config.toml` (TOML)。`--config <path>` で変更可能。
- `config.toml.example` にサンプルあり。
