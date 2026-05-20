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
- **aish プロンプトで AI に質問を送信した直後** (非空 Enter 確定時) に限り、bash の打ちかけ入力を Ctrl+A + Ctrl+K (`0x01 0x0b`) で消去してから AI 提案コマンドが PTY に流れるようにする。Ctrl+C (`0x03`) ではなく Ctrl+A+Ctrl+K を使うのは、SIGINT を発火させずに行消去だけしたいため (vim/top 等の子プロセスを意図せず kill しない)。空 Enter / ESC キャンセルの場合は **何も送らない** ので、打ちかけは画面に残ったままユーザの手元に保たれる。`cancel_shell` フラグは `!at_line_start` (= aish 側のキー入力ヒストリで PTY に文字を送ったあと Enter していない状態) で立つ。bash readline 以外 (vim 子プロセス等) に Ctrl+A+Ctrl+K が届くと `^A^K` がリテラル文字として流れる副作用は残るが、SIGINT 直撃よりは穏当な失敗モードとする。
- **AI 提案コマンドの完了判定は `PromptSniffer` による passive 検出**。コマンドはユーザ承認文字列をそのまま PTY に送り、PTY 出力末尾がプロンプト形 (`[$#>%➜❯»][空白]+`) になり 200ms 静音したら完了。
- **TUI アプリ (vim / less / top 等) 終了後は aish 側から何も出さない**。bash 単体と同じ「alt screen から戻った状態」に任せる。元はステータスバー復旧のため `Ctrl+L` を撃っていたが、ステータスバー廃止 (commit 7d13700) で動機が消えたため一律全廃した経緯がある。再導入する場合は vim insert モード中の誤発火 (バッファに `^L` がリテラル混入) を避けるため、PTY 出力の passive 検出だけで一意に「終了」と判定できる根拠を用意すること。`\x1b[2J`・DECSTBM (`\x1b[..r`)・alt screen 突入 (`\x1b[?1049h` 等) は TUI 動作中にも出るので終了判定には使えない。
- **`show_minibuffer` 入口と `redraw_minibuffer` の伸長 (grow) 時には stdout に `\n` を出して空き行を scroll 退避で確保する**。入口の `\n` は `\x1b7` の直後に出し、cursor が画面最終行なら scroll で bash 出力を退避・それ以外なら cursor 下降のみで上端を保護する。伸長時は `*rows_used > 0` のときだけ、現 DECSTBM bottom (`term_rows - *rows_used`) に cursor を置いてから `\n` を delta 個出し、現 scroll 領域内でだけ scroll させる。初回 0→1 の grow は入口 `\n` と重複するので `*rows_used > 0` ガードで除外している (これを外すと画面上半分しか使っていないケースで強制 scroll が発生して上端が削れる)。すべて stdout 専用で PTY には一切送らない (bash の入力バッファや実行状態に影響しない)。alt screen (vim 等) 中に Ctrl+/ で minibuffer を出すと表示が崩れるが、`Ctrl+L` で TUI 側 redraw 可能なため許容仕様としている。
- **パススルーで PTY に転送する ESC シーケンスは完全な形まで読み切ってからまとめて送る**。CSI (`ESC [ ... <0x40-0x7E>`) だけでなく SS3 (`ESC O <1 byte>`) も同様。途中で分割して 2 回の write になると、受信側 (vim 等) は ESC タイムアウトで別キーと解釈する (例: `ESC O` + `H` → `ESC` + `O` (open line above) と誤解)。Home/End や F1〜F4、アプリケーションカーソルモードの矢印キーが該当。
- **ring_buffer の未送信 cursor は backend ごとに独立**。`sent_marks: [u64; BackendKind::COUNT]` で保持し、`get_unsent_for(kind)` / `mark_sent_for(kind)` を経由する。`/ai` 切替時に新 AI が「これまでの会話の続き」を catch-up できるようにするための仕組み。**新規 backend を追加するときは `BackendKind::ordinal()` / `all()` / `COUNT` を必ず更新する**。
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
- **タグ付け / リリースは `release` slash command 経由のみ**。auto-commit の対象外。

## 設定ファイル
- `~/.aish/config.toml` (TOML)。`--config <path>` で変更可能。
- `config.toml.example` にサンプルあり。
