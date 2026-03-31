# aish 仕様書

## 概要

aish は SSH + AI を統合した CLI ツール。  
SSH セッションに AI（Claude, ChatGPT, Gemini）を常駐させ、コマンド操作の監視・助言・代行を行う。

## 基本情報

| 項目 | 内容 |
|------|------|
| 名称 | aish (AI + SSH) |
| 開発言語 | Rust |
| 文字コード | UTF-8 (UTF8MB4) |
| 対応 OS | Windows, macOS, Linux (Ubuntu) |
| 対応 AI | Claude, ChatGPT, Gemini |

## 起動方法

```bash
aish <user@host> [--ai claude|chatgpt|gemini]
```

- `user@host`: SSH 接続先（必須）
- `--ai`: 初期 AI プロバイダ（デフォルト: claude）

## コマンド一覧

| コマンド | 説明 |
|----------|------|
| `!<command>` | SSH 上でコマンドを直接実行する |
| `<テキスト>` | AI にプロンプトとして送信する |
| `/claude` | メイン AI を Claude に切り替える |
| `/chatgpt` | メイン AI を ChatGPT に切り替える |
| `/gemini` | メイン AI を Gemini に切り替える |
| `/?` | 現在のメイン AI に直近の出力を解説させる |
| `/? <ai>` | 指定した AI に解説させる（claude, chatgpt, gemini） |
| `/? all` | すべての AI に解説させる |
| `ssh user@host` | SSH 接続（起動時に接続済みの場合は案内表示） |
| `/help` | コマンド一覧を表示する |
| `/quit` `/exit` | aish を終了する |

## 動作仕様

### AI 監視

- AI は SSH セッションのコンソール出力を常時監視する
- コマンドとその結果を AI が読み取り、いつでも助言できる状態を保つ
- 直近のターミナル出力は会話コンテキストとして AI に送信される

### AI 切替

- `/chatgpt`, `/claude`, `/gemini` で即座にメイン AI を切り替える
- 切替時に該当 AI の API キーが未設定の場合はエラーを表示する

### コマンド実行

- `!` で始まる入力は SSH セッション上でそのまま実行される
- AI がコマンドを提案する場合、`[COMMAND: <コマンド>]` 形式で返答する
- **サーバ上でのコマンド実行・ファイル読み取りは必ずユーザに確認する**

### 解説機能

- `/?` で現在のメイン AI が直近のターミナル出力を解説する
- `/? chatgpt` のように AI 名を指定すると、その AI が解説する
- `/? all` ですべての設定済み AI が順に解説する

## 設定

### 環境変数（優先）

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
export OPENAI_API_KEY="sk-..."
export GOOGLE_API_KEY="AIza..."
```

### 設定ファイル（~/.aish/config.toml）

```toml
anthropic_api_key = "sk-ant-..."
openai_api_key = "sk-..."
google_api_key = "AIza..."
default_ai = "claude"

[ssh]
identity_file = "~/.ssh/id_ed25519"
extra_args = ["-p", "2222"]
```

環境変数が設定されている場合は、設定ファイルの値より優先される。
