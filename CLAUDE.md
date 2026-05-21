# aish

CLI SSH + AI (Claude Code)。ローカルシェル または SSH接続先サーバを、クライアント側のClaude Codeから調査・操作する対話型ツール。

## 開発環境
- 言語: Rust
- ビルド: `export PATH="$HOME/.cargo/bin:$PATH" && cargo build`
- 対応OS: Linux (Ubuntu), macOS, Windows（UI部はUnix限定、Windowsは `read_line_cooked` フォールバック）

## 仕様

詳細な仕様（アーキテクチャ、動作モード、UI要素、キー入力、AI連携、リングバッファ、スレッド構成、設定ファイル、エラー挙動、既知の制約など）は **[SPEC.md](./SPEC.md)** を参照。

## 信頼の根幹

aish は SSH でサーバを管理する道具なので、**ユーザが画面で承認したコマンド = サーバで実行されるコマンド** を保つこと、**サーバ側に勝手な書き込みをしないこと** が大原則。

具体的に避けるべき行為:
- AI 提案コマンドをラップして別の文字列に変形する（マーカーラッパ等）
- `PROMPT_COMMAND` / `precmd` / `set +o history` で shell 環境を黙って書き換える
- `HISTCONTROL=ignorespace` 依存等の「履歴に残さない工夫」
- 任意の shell 統合シーケンスの自動セットアップ

完了判定や exit code 取得が必要でも、**passive 検出**（PTY 出力を観察するだけ）の範囲で実現する。それで取れない情報は諦める。

## 実装上の注意

コードから直ちに読み取れない、間違えやすいポイント：

- **rawモードはセッション全体で維持**する（`save_terminal_settings` で設定）。`read_line` / `passthrough` 個別での再設定・復元は不要。
- **stdin の termios は `c_lflag` の `ICANON|ECHO|ISIG|IEXTEN` に加えて `c_iflag` の raw 化フラグ群 (`IGNBRK|BRKINT|PARMRK|ISTRIP|INLCR|IGNCR|ICRNL|IXON`) も落とす**。ICRNL を残すと、ユーザの Enter (`\r`) が端末 driver の段階で `\n` に変換され PTY に届く。`prompt_toolkit` 系の選択ピッカー (`Application` / `questionary` 等) は `Keys.ControlM` (= `\r`) のみを「選択確定」にバインドしているため、CR→NL 変換がかかると Enter が無反応になる (`aws configure sso` のアカウント選択画面で再現)。`c_oflag` (OPOST) は触らない: `show_minibuffer` 等で `writeln!(stdout)` が `\n` のみを書く箇所があり、端末側の NL→CRLF 変換に依存している。
- **AI対話終了後は `input_idle = true` を明示的に設定**すること。確認プロンプトの ReadLine で false になったまま戻ると入力リクエストが再送されずハングする。
- **aishプロンプト表示中は PTY出力の画面描画を抑制**（`MINIBUFFER_ACTIVE` フラグ）。ただしリングバッファへの記録は継続する。
- **通常動作中は PTY出力に aish 独自の文字列を一切挿入しない**（パススルーに徹する）。ステータスバーは DECSTBM の外に描画する。
- **Shift+Enter による改行は非対応**。ターミナル間で CSI u / legacy の扱いが揃わないため。改行は `Alt+Enter` のみサポート。
- **bash の打ちかけ入力消去 (Ctrl+A + Ctrl+K = `0x01 0x0b`) は AI 提案コマンドの「最初の実行」直前 1 回だけ PTY に送る** (`src/main.rs` の AI 確認ループで `!any_executed` のときに送出)。show_minibuffer 終了時 (= AI 質問送信時) には送らないので、AI に質問はしたが提案コマンドを全部拒否したケースでは bash の打ちかけが温存される。Ctrl+C (`0x03`) ではなく Ctrl+A+Ctrl+K を使うのは SIGINT を発火させずに行消去だけしたいため (vim/top 等の子プロセスを意図せず kill しない)。bash プロンプト直後 (打ちかけなし) で送っても no-op なので害なし。後続の AI 提案コマンドは前のコマンドが完了して bash プロンプトに戻った後送られるので追加で送らない。bash readline 以外 (vim 子プロセス等) に届くと `^A^K` がリテラル文字として流れる副作用は残るが、SIGINT 直撃よりは穏当な失敗モード。
- **AI 提案コマンドの完了判定は `PromptSniffer` による passive 検出**。コマンドはユーザ承認文字列をそのまま PTY に送り、PTY 出力末尾がプロンプト形 (`[$#>%➜❯»][空白]+`) になり 200ms 静音したら完了。
- **TUI アプリ (vim / less / top 等) 終了後は aish 側から何も出さない**。bash 単体と同じ「alt screen から戻った状態」に任せる。元はステータスバー復旧のため `Ctrl+L` を撃っていたが、ステータスバー廃止 (commit 7d13700) で動機が消えたため一律全廃した経緯がある。再導入する場合は vim insert モード中の誤発火 (バッファに `^L` がリテラル混入) を避けるため、PTY 出力の passive 検出だけで一意に「終了」と判定できる根拠を用意すること。`\x1b[2J`・DECSTBM (`\x1b[..r`)・alt screen 突入 (`\x1b[?1049h` 等) は TUI 動作中にも出るので終了判定には使えない。
- **`show_minibuffer` は入口で DSR (`\x1b[6n`) を投げて cursor row を取得し、`row == rows` (画面下端) のときだけ `\n` を出して scroll 退避する**。画面上半分のときは何もしない (上端を削らない)。終了時は `\x1b7` / `\x1b8` の DECSC/DECRC ではなく、取得した `(row, col)` を `\x1b[{row - scrolled};{col}H` で **絶対座標指定して cursor を復元する** — `was_at_bottom` のときは scroll で bash 入力欄が `rows_used` 行ぶん上に動いているため `scrolled = rows_used`、画面上半分ケースでは `scrolled = 0`。`redraw_minibuffer` の grow scroll も `was_at_bottom` 条件付きにして画面下端起点のときだけ発火させる (画面上半分始まりで伸長時に余分な scroll を起こさないため)。DSR 応答は `\x1b[{row};{col}R` 形式を 80ms timeout で stdin から読む (`query_cursor_position_dsr`); fd 0 は `passthrough_read_raw` が raw モードで握っているが `ManuallyDrop` で借りて `libc::poll` 非ブロッキング読み取りする。応答前にユーザがキーを打った場合は混入バイトを捨てる単純実装 (実害稀)。応答なし端末では `was_at_bottom = false` fallback で安全側 (= 入口 `\n` 不発火、cursor は絶対座標復元なのでズレない)。すべて stdout 専用で PTY には送らないので bash の実行状態には影響しない。alt screen (vim 等) 中に Ctrl+/ で minibuffer を出すと表示が崩れるが、`Ctrl+L` で TUI 側 redraw 可能なため許容仕様。
- **パススルーで PTY に転送する ESC シーケンスは完全な形まで読み切ってからまとめて送る**。CSI (`ESC [ ... <0x40-0x7E>`) だけでなく SS3 (`ESC O <1 byte>`) も同様。途中で分割して 2 回の write になると、受信側 (vim 等) は ESC タイムアウトで別キーと解釈する (例: `ESC O` + `H` → `ESC` + `O` (open line above) と誤解)。Home/End や F1〜F4、アプリケーションカーソルモードの矢印キーが該当。
- **ring_buffer の未送信 cursor は backend ごとに独立**。`sent_marks: [u64; BackendKind::COUNT]` で保持し、`get_unsent_for(kind)` / `mark_sent_for(kind)` を経由する。`/ai` 切替時に新 AI が「これまでの会話の続き」を catch-up できるようにするための仕組み。**新規 backend を追加するときは `BackendKind::ordinal()` / `all()` / `COUNT` を必ず更新する**。実行ファイル名が enum 名 (`as_str()`) と異なる backend (例: `cursor` → `cursor-agent`) を追加する場合は `BackendKind::binary()` 側に分岐を追加すること。`factory::check_installed` と spawn は `binary()` を経由するが、設定 / slash command / 表示は `as_str()` (短縮名) を使う。
- **cursor backend は `--trust` を常時付与する**。cursor-agent の headless モード (`-p`) は `--trust` 無しだと `Workspace Trust Required` で実行を拒否し、stdin は読まずプロンプト表示だけ出して非ゼロ終了する。`AiError::EmptyOutput` / `NonZeroExit` 経由でユーザに見えるが原因が分かりにくいので、config からは指定不可で `src/ai/cursor.rs` の固定引数に埋め込んでいる。`--yolo` / `-f` (Run Everything) は意図しない実行を許してしまうので絶対に付けない。安全側の `--trust` 単体で十分。
- **cursor backend のツール抑制は `--mode plan` + system prompt の二段構え**。cursor-agent には codex の `--disable shell_tool ...` に相当する個別ツール無効化フラグが無いので、`[ai.cursor].mode = "plan"` (default) で read-only / propose-only モードに固定するのが主防御。OS レベル defense-in-depth として `[ai.cursor].sandbox = "enabled"` を併用可。**`mode = ""` (通常モード) は明示的にユーザが選んだときだけ。aish 側の確認 UI を迂回するリスクがあるので推奨しない**。
- **cursor-agent の Free プランでは Named models が使えず `auto` のみ**。`--model sonnet-4` 等を指定すると `Named models unavailable Free plans can only use Auto.` で `EmptyOutput` エラーになる。paid プランなら sonnet-4 等を指定可能。config では `[ai].model = "auto"` を案内している。
- **copilot backend は `-p` フラグを付けない**。`copilot -p <text>` フラグは positional / stdin と排他で、aish のように stdin で prompt を流すと `error: too many arguments. Expected 0 arguments but got 1.` で死ぬ。copilot CLI は stdin を自動検出するので `-p` なしで `run_cli_capture_stdout` 経由の stdin 渡しで動く。これは他の `-p` 必須 backend (claude, cursor) と挙動が逆になるので、`src/ai/copilot.rs` を編集するときは注意。
- **copilot backend のツール抑制は四段構え**: `--allow-all-tools` (非対話必須) + `--deny-tool=shell` + `--deny-tool=write` + `--no-ask-user` + `--mode plan` (default)。deny は allow に優先するので、`--allow-all-tools` を付けても shell 実行と書き込みは完全拒否される。これで copilot は claude / codex 同等の「LLM のみ」状態に退化する。これらは config からは指定不可で固定引数に埋め込んでいる (信頼の根幹に直結するため)。`--yolo` / `--allow-all` (Run Everything 系) は絶対に付けない。
- **copilot の `--output-format json` は JSONL (1 行 1 オブジェクト)**。他の backend のような単一の外側 JSON ではない。`src/ai/copilot.rs::parse_jsonl_envelope` で行ごとに走査し、`type == "assistant.message"` 行の `data.content` (最終応答テキスト) と `type == "result"` 行の `sessionId` (session UUID) を取り出す。ephemeral な delta / status 行 (`session.*`, `assistant.message_delta`, `assistant.reasoning`, `assistant.turn_*` 等) は無視する。複数 `assistant.message` がある場合は最後のものを採用 (一応 multi-turn 想定だが現状は 1 turn のみ)。
- **copilot は組織ポリシーで CLI 利用が拒否されることがある**。`Error: Access denied by policy settings (Request ID: ...)` が出たら個人の Copilot 設定 (https://github.com/settings/copilot) または所属組織の admin に確認が必要。aish 側では `AiError::NonZeroExit { stderr }` 経由でユーザに見えるので、エラー本文の `policy` 文字列でユーザが原因に気付ける。
- **AI 応答受信後は ring_buffer に注釈を append してから `mark_sent_for(current_kind)`** の順序を守る。注釈 (`[aish→<kind>]> ...` / `[ai/<kind>]> ...` / `[ai/<kind> suggests] ...`) は **current AI は再受信せず、他 backend は次回 catch-up で受信する**。逆順にすると current AI が自分の発話をループで受信してしまう。
- **`/clear` は ring_buffer.mark_sent_all() で全 backend の cursor を末尾に進める**が、AI CLI 内部の session/history は **current backend のみ** リセットする (他 backend の instance は保持していないため)。「全 AI の会話を仕切り直す」セマンティクスを守りつつ、副作用は最小限にする妥協案。
- **ring_buffer の `[link]` ライクな PTY 文字列と注釈ラベル (`[aish→...]` / `[ai/...]`) は衝突しうる**。AI は文脈で区別する想定。誤検出が頻発したらフォーマットを XML 風 (`<aish to="claude">`) 等に変更する余地あり。

## 開発フロー

ユーザから明示的に依頼された作業を行うときは、以下のフローに従う。これは CLAUDE.md「信頼の根幹」を守るための運用ルールであり、デフォルトの「commit は明示要求があるときだけ」を上書きする。

- **ユーザのプロンプトが疑問文で終わっている場合、それは質問であって作業依頼ではない**。勝手にソースコードを修正してはいけない。まず質問に答え、必要であれば修正案を提示してユーザの指示を待つ。
- **ソースコードを修正する前に、仕様や要件に疑問点があれば全て必ずユーザに確認する**。曖昧な前提のまま作業を進めない。判断に迷ったら手を動かす前に聞く。
- **コードに新しい仕様 / 落とし穴 / 不変条件を導入する変更を行ったら、同じ作業フローの中で CLAUDE.md「実装上の注意」(仕様が膨らんだ場合は SPEC.md) への追記を必ず行う**。対象は、コードから直ちには読み取れず後から見て間違える可能性のある挙動。typo 修正・cosmetic refactor・既存仕様の範囲内の小さな修正は対象外。判断に迷ったら追記する側に倒す。
- **依頼された作業が完了したら、コード変更と CLAUDE.md / SPEC.md の追記を 1 つの commit にまとめて自動作成してよい**。ユーザに「commit しますか？」と都度確認しなくてよい。コミットメッセージは既存スタイル (`Feat:` / `Fix:` / `Refactor:` / `Docs:` / `Chore:` 等の日本語プレフィックス + 短い説明、必要なら body) に従う。
- **push は別途ユーザに確認する**。リモートに公開する操作は引き続き確認が必要。
- **タグ付け / git tag タグをつける。git push --tagsでGitHub Actionが動き出し、リリースされるが、git pushは自動では行わない。

## 設定ファイル
- `~/.aish/config.toml` (TOML)。`--config <path>` で変更可能。
- `config.toml.example` にサンプルあり。
