# aish

CLI SSH + AI (Claude Code)。クライアント側のClaude Codeからサーバを調査・操作する。

## 各部の名称
- **パススルーモード**: 通常のシェル操作。キー入力はPTYにそのまま転送される
- **aishプロンプト**: `Ctrl+/` で表示される `[aish]` 入力欄。ターミナル最下行に表示され、AIへの質問を入力する。ESC/Ctrl+Cでキャンセル
- **起動バナー**: aish起動時に1回だけ表示されるバージョン情報行
- **スピナー**: AI応答待ち中に表示される回転アニメーション（`Thinking...`）
- **確認プロンプト**: AIが提案したコマンドの実行可否を問う `(Y/n)` 表示

## 開発
- 言語: Rust
- ビルド: `export PATH="$HOME/.cargo/bin:$PATH" && cargo build`
- OS: Windows, MacOS, Linux(Ubuntu)

## アーキテクチャ
- `main.rs` - メインループ。PTY読み取りスレッド、ユーザ入力スレッド、イベントループの3構成
- `ui.rs` - ターミナル制御。rawモード(セッション全体で維持)、行編集、パススルーモード（Ctrl+/でaishプロンプト）、ANSI色
- `ai.rs` - Claude Code CLI連携。JSON Schema構造化レスポンス、セッション維持(--resume)
- `config.rs` - TOML設定。表示色、システムプロンプト。`[[hosts]]`パターンマッチは未実装
- `pty_handler.rs` - portable-ptyによるSSH/ローカルシェル起動（実端末サイズで起動、SIGWINCH対応）
- `update.rs` - セルフアップデート（`--update`）。GitHub Releases APIから最新バイナリをダウンロード
- `ring_buffer.rs` - 1MBリングバッファ。ANSIエスケープ除去、差分送信
- `mode.rs` - Local / Remote / RemoteEnded の3モード

## モード
- Local: SSH引数なし。ローカルシェル実行
- Remote: SSH接続中
- RemoteEnded: SSH終了後。入力はすべてAIプロンプトとして処理。exitで終了

## ユーザ入力
- `Ctrl+/`: aishプロンプトを開いてAI呼び出し。ターミナル最下行に[aish]プロンプトを表示し、入力確定でAI呼び出し。ESC/Ctrl+Cでキャンセル。空Enterは無操作
- RemoteEndedモード: 通常入力はすべてAIプロンプトとして処理。exitで終了
- それ以外: PTYにパススルー（タブ補完等そのまま動作）
- Enter/Ctrl+C: パススルーモードではPTYに送信しループ継続（パススルーを抜けない）
- Ctrl+C連打: ReadLineモードではaish終了
- aishプロンプト中はPTY出力の画面表示を抑制（MINIBUFFER_ACTIVE フラグ）。リングバッファへの記録は継続
- aishプロンプト実行時は `[aish] プロンプト内容` をコンソールに描画してから処理
- 入力中コマンドがある時にaishプロンプトをキャンセルすると、PTYにCtrl+Cを送信してコマンドを破棄

## コマンドラインオプション
- `--version` / `-V`: バージョン表示
- `--update`: セルフアップデート（GitHub Releasesから最新版をダウンロード）
- `--aish-config <path>`: 設定ファイルのパス指定

## AI連携
- `claude -p --output-format json --json-schema <schema>` でコマンド提案を取得
- 初回: `--disallowedTools "Bash,Edit,Write,Read" --append-system-prompt <prompt>`
- 2回目以降: `--resume <session-id>`
- レスポンス: `{ "message": string, "commands": string[] }`
- コマンド実行前にユーザ確認 (Y/n)。実行結果はAIにフィードバック

## 表示
- ターミナルタイトル: `\x1b]2;[aish] host\x07` で設定。終了時に空文字で復元
- 起動バナー: `aish v{version} | Ctrl+/ for AI` をグレーで表示
- 通常時: PTY出力にaish表示を一切挿入しない（パススルーのみ）
- RemoteEndedモード: ReadLineプロンプトとして`[aish]`ラベルを表示
- Ctrl+Cヒント: ReadLineモードでCtrl+C時に`(Ctrl+C to exit)`を2秒間表示
- 色: 設定ファイルでプロンプト/Thinking/AI応答の前景・背景色を指定可能（色名 or 256色番号）

## 入力スレッド管理
- `pending_input`: 入力スレッド起動待ちフラグ。PTY出力が50ms落ち着いたら発火
- `input_idle`: 入力スレッドがprompt_rx.recv()で待機中か。キュー重複防止
- `PassthroughEnded`イベント: aishプロンプト（Ctrl+/）で発生。input_idle=trueに戻す
- rawモードはセッション全体で維持（save_terminal_settingsで設定）。passthrough/readlineの個別設定・復元は不要
- AI対話終了後は`input_idle=true`を明示的に設定すること（確認プロンプトのReadLineでfalseになるため）
- SIGWINCH: 端末リサイズ時にPTYサイズを追従

## 設定ファイル
- `~/.aish/config.toml` (TOML)。`--aish-config`で変更可能
- `config.toml.example` にサンプルあり

## メモリ
- [設計判断メモ](.claude/memory/design-decisions.md) — 色設定方針、各機能の設計判断など
