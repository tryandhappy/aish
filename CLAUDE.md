# aish

CLI SSH + AI (Claude Code)。クライアント側のClaude Codeからサーバを調査・操作する。

## 開発
- 言語: Rust
- ビルド: `export PATH="$HOME/.cargo/bin:$PATH" && cargo build`
- OS: Windows, MacOS, Linux(Ubuntu)

## アーキテクチャ
- `main.rs` - メインループ。PTY読み取りスレッド、ユーザ入力スレッド、イベントループの3構成
- `ui.rs` - ターミナル制御。rawモード(セッション全体で維持)、行編集、パススルーモード（@ai/?検出ステートマシン）、ANSI色
- `ai.rs` - Claude Code CLI連携。JSON Schema構造化レスポンス、セッション維持(--resume)
- `config.rs` - TOML設定。表示色、システムプロンプト。`[[hosts]]`パターンマッチは未実装
- `pty_handler.rs` - portable-ptyによるSSH/ローカルシェル起動(24x80固定)
- `ring_buffer.rs` - 1MBリングバッファ。ANSIエスケープ除去、差分送信
- `mode.rs` - Local / Remote / RemoteEnded の3モード

## モード
- Local: SSH引数なし。ローカルシェル実行
- Remote: SSH接続中
- RemoteEnded: SSH終了後。AI入力とexitのみ。通常コマンドは無視して[aish]プロンプト再表示

## ユーザ入力
- `@ai <prompt>` または `? <prompt>`: AI呼び出し
- `Ctrl+/`: AI自動分析
- それ以外: PTYにパススルー（タブ補完等そのまま動作）
- Ctrl+C: パススルーモードではPTYにそのまま送信（record_ctrl_cしない）。ReadLineモードでは連打でaish終了

## AI連携
- `claude -p --output-format json --json-schema <schema>` でコマンド提案を取得
- 初回: `--disallowedTools "Bash,Edit,Write,Read" --append-system-prompt <prompt>`
- 2回目以降: `--resume <session-id>`
- レスポンス: `{ "message": string, "commands": string[] }`
- コマンド実行前にユーザ確認 (Y/n)。実行結果はAIにフィードバック

## 表示
- ターミナルタイトル: `\x1b]2;[aish] host\x07` で設定。終了時に空文字で復元
- `[aish]` ラベル: メインループから直接描画。`last_line`追跡 + `looks_like_prompt`(`$#%>`末尾判定) + `aish_drawn`フラグで制御
  - `aish_drawn`はPTY出力の改行でfalseにリセット、描画後にtrue
  - 入力スレッドとは独立して動作（競合回避）
- Ctrl+Cヒント: ReadLineモードでCtrl+C時に`(Ctrl+C to exit)`を2秒間表示
- 色: 設定ファイルでプロンプト/Thinking/AI応答の前景・背景色を指定可能（色名 or 256色番号）

## 入力スレッド管理
- `pending_input`: 入力スレッド起動待ちフラグ。PTY出力が50ms落ち着いたら発火
- `input_idle`: 入力スレッドがprompt_rx.recv()で待機中か。キュー重複防止
- `PassthroughEnded`イベント: パススルーがEnter等で終了時に送信。input_idle=trueに戻す
- rawモードはセッション全体で維持（save_terminal_settingsで設定）。passthrough/readlineの個別設定・復元は不要
- AI対話終了後は`input_idle=true`を明示的に設定すること（確認プロンプトのReadLineでfalseになるため）

## 設定ファイル
- `~/.aish/config.toml` (TOML)。`--aish-config`で変更可能
- `config.toml.example` にサンプルあり
