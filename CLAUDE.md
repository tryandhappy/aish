# aish

# 概要
CLI SSH + AI (Claude Code)

# 目的
Claude Codeをサーバにインストールするのは現実的ではないため、クライアント側に用意して、サーバを調査・操作したい。

# 仕様

## 基本
- 文字コード:UTF-8 (4バイト文字を含む)
- 開発言語:Rust
- OS:Windows, MacOS, Linux(Ubuntu)
- aishコマンドはCLI

## 起動と設定
- 起動時にclaudeコマンドの存在を確認する。未インストールの場合は以下を表示して終了:
  Please install Claude Code.
  curl -fsSL https://claude.ai/install.sh | bash
- 設定ファイルはTOML形式。デフォルトは~/.aish/config.toml。--aish-configオプションで別のパスを指定可能。
- aishコマンドのオプションは、--aish-* で始まるものとし、それ以外はすべてSSHコマンドのオプションにそのまま渡す。

## モード
- モードは3つ
  - ローカルモード: オプション(接続先)がない場合。SSHの代わりにローカルシェルを実行する。
  - リモートモード: SSH接続中。
  - リモート終了モード: SSHが終了した状態。AIプロンプト入力かexitのみ使える。ローカルの読み書き実行は行わない。
- ローカルモード、リモート終了モードでexitと入力したらaishを終了。
- リモートモードでexitと入力した場合はsshコマンドにそのままexitを送信する。

## SSH
- ssh (OpenSSH)コマンドのサブプロセスをpty経由で起動する。
  - WindowsはConPTY
  - LinuxはPOSIX pty (openpty)
- SSH認証:SSH鍵認証, パスワード認証
- タイムアウトは設定しない。sshコマンドが続く限り実行し続ける。
- sshコマンドが終了したら、そのメッセージを表示してリモート終了モードに遷移する。
- SSH接続失敗時はそのエラーメッセージをそのまま表示する。

## リングバッファ
- sshの入出力をメモリ上のリングバッファに保存する。バッファサイズは1M。
- リングバッファにはANSIエスケープシーケンスを削除して保存する。
- AIに送信したリングバッファは削除する。差分だけAIに送信。
- AIの入力サイズを超える場合は、先頭を切る。

## ユーザ入力
- プリフィックスが@aiか?の場合は、AIのプロンプトとする。@aiと?の機能差は無い。
- それ以外の入力はSSHコマンド(またはローカルシェルコマンド)としてそのまま送信する。

## AI連携 (Claude Code CLI)
- AIから直接コマンドは実行させない。必ずaish経由で実行する。
- aishコマンドが動いている間は、同一のAIセッションを使用し続ける。
- 初回の実行:
  ```
  claude -p --output-format json \
    --disallowedTools "Bash,Edit,Write,Read" \
    --append-system-prompt "コマンドを提案してください。直接実行しないでください。" \
    --json-schema <スキーマ> \
    "プロンプト"
  ```
- 初回のレスポンスからsession_idを取得して保持する。
- 2回目以降の実行:
  ```
  claude -p --resume <session-id> \
    --output-format json \
    --json-schema <スキーマ> \
    "プロンプト"
  ```
- AIのレスポンスはJSON Schemaで構造化する。スキーマ:
  ```json
  {
    "type": "object",
    "properties": {
      "message": { "type": "string", "description": "ユーザへの説明" },
      "commands": {
        "type": "array",
        "items": { "type": "string" },
        "description": "実行を提案するコマンドのリスト(空配列も可)"
      }
    },
    "required": ["message", "commands"]
  }
  ```
- AIに送信するときは、SSHの内容を```terminalブロックで囲んで区別する。
- AIが提案したcommands配列をaish側でユーザ確認後、SSH(またはローカルシェル)に送信する。
- コマンド実行結果は次のプロンプトで```terminalブロックとしてAIに送信する。
- AIの初期システムプロンプトは設定ファイルで変更可能。デフォルト:「あなたはLinuxサーバ管理の専門家です。SSHセッションの内容を把握しています。」

## ユーザ確認
- ユーザ確認は(Y/n)とし、改行のみの場合はYとする。ESCはn。
- ファイルの読み込み、書き込み、コマンド実行をするときはユーザの確認を行う。
- 複数の読み込み、書き込み、コマンド実行はまとめてユーザ確認を行う。
- 追加の確認が必要なときはその都度、ユーザ確認を行う。

## 表示
- AIの応答はANSIエスケープで背景色をつける。 \e[46m{AI応答}\e[0m とする。
