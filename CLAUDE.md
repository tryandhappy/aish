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
- **AI対話終了後は `input_idle = true` を明示的に設定**すること。確認プロンプトの ReadLine で false になったまま戻ると入力リクエストが再送されずハングする。
- **aishプロンプト表示中は PTY出力の画面描画を抑制**（`MINIBUFFER_ACTIVE` フラグ）。ただしリングバッファへの記録は継続する。
- **通常動作中は PTY出力に aish 独自の文字列を一切挿入しない**（パススルーに徹する）。ステータスバーは DECSTBM の外に描画する。
- **Shift+Enter による改行は非対応**。ターミナル間で CSI u / legacy の扱いが揃わないため。改行は `Alt+Enter` のみサポート。
- **入力中コマンドがある状態で aishプロンプトをキャンセル / 確定した場合**、PTY に `Ctrl+C` (0x03) を送って部分入力を破棄する。
- **AI 提案コマンドの完了判定は `PromptSniffer` による passive 検出**。コマンドはユーザ承認文字列をそのまま PTY に送り、PTY 出力末尾がプロンプト形 (`[$#>%➜❯»][空白]+`) になり 200ms 静音したら完了。

## 設定ファイル
- `~/.aish/config.toml` (TOML)。`--aish-config <path>` で変更可能。
- `config.toml.example` にサンプルあり。
