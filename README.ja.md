# aish (AI + SSH)

[English](README.md) | **日本語**

**AI 連携 SSH シェル** — SSH セッションの中から、そのまま Claude Code に相談できます。

- `aish` は AI + SSH/Terminal です。
- ターミナルから `Ctrl+/` で AIに問い合わせできます。
- AIと相談しながら作業することができます。
- AIに画面の内容送るので、エラーやログを貼り付ける必要はありません。
- コマンド実行時は必ず確認が入るので安心です。
- クライアントのAI Agentを使用するので、サーバにインストールする必要がありません。

![aish screenshot](docs/images/sample1.png)

## デモ動画

**SSHモード**
https://github.com/tryandhappy/aish/raw/main/docs/movies/sample-ssh1.mp4

**ローカルモード**
https://github.com/tryandhappy/aish/raw/main/docs/movies/sample-local1.mp4



## 前提条件

#### 対応OS

- Linux (Testing on Ubuntu 24.04)
- Windows WSL2 (Testing on Ubuntu 24.04)
- Windows 10 1809+ ネイティブ **(ベータ版)** (Windows Terminal 推奨) ※プレビルドバイナリ配布あり (x86_64 / aarch64 の `.exe`・`.zip`)。`--update` 自己更新は非対応 (手動でダウンロード)
- macOS (Intel・Apple Silicon) ※テスト不十分

#### 必要なコマンド

- AI CLI — 下記 [対応AI CLI](#対応ai-cli) のいずれか（例: [Claude Code](https://code.claude.com/docs/ja/overview), [ChatGPT Codex](https://openai.com/ja-JP/codex/), [Gemini CLI](https://cloud.google.com/blog/ja/topics/developers-practitioners/introducing-gemini-cli/), [Qwen Code](https://qwen.ai/qwencode)）
- OpenSSH (リモートSSH)
- bash または zsh (ローカルシェル)
- curl (aish --update、および REST backend)



## 対応AI CLI

`--ai <名前>` で選ぶか、`config.toml` の `backend` に設定します。選んだ CLI が未インストールなら、実在する最初の CLI に自動フォールバックします。

**ネイティブ backend**（組み込み）:

- `claude` — Claude Code (API, Pro, Max, Team, Enterprise)。Free は Claude Code が使えないので未対応（**既定**）
- `codex` — OpenAI ChatGPT Codex ※テスト不十分
- `gemini` — Google Gemini CLI ※テスト不十分
- `antigravity` (`agy`) — Google Antigravity CLI、Gemini CLI の後継
- `qwen` — Qwen Code ※テスト不十分
- `cursor` — Cursor Agent (`cursor-agent`)、`--mode plan` 固定
- `copilot` — GitHub Copilot CLI（shell/write を deny、plan モード）
- `grok` — xAI Grok CLI (`grok`) ※`which -a grok` で公式 CLI か確認
- `cloudflare` — Cloudflare Workers AI を REST 経由（認証は環境変数: `CLOUDFLARE_ACCOUNT_ID` / `CLOUDFLARE_API_TOKEN`）
- `nvidia` — NVIDIA NIM を REST 経由（認証は環境変数: `NVIDIA_API_KEY`）

**組み込み recipe**（read-only の安全設定を焼き込んで同梱した generic backend）:

- `kimi` — MoonshotAI Kimi CLI（`--plan`）
- `opencode` — OpenCode（config 注入で read-only agent を強制）

`config.toml` の `[[ai.providers]]` recipe で独自 CLI を追加できます。`aish --list-providers` で解決済みの全 backend を確認できます。ユーザ定義 provider には安全フラグを**自動付与しません**（recipe 著者の責任）。

どの backend でも、aish が勝手に shell/write 権限を与えることはありません。AI はコマンドを*提案*するだけで、画面で承認するまで何も実行されません。


## インストール

### Linux

```bash
sudo curl -fsSL -o /usr/local/bin/aish "https://github.com/tryandhappy/aish/releases/latest/download/aish-$(uname -m)-unknown-linux-musl"

sudo chmod 755 /usr/local/bin/aish
```

### macOS (Intel・Apple Silicon)

```bash
sudo mkdir -p /usr/local/bin

ARCH=$(uname -m); case "$ARCH" in arm64|aarch64) ARCH=aarch64;; x86_64|amd64) ARCH=x86_64;; esac

sudo curl -fsSL -o /usr/local/bin/aish "https://github.com/tryandhappy/aish/releases/latest/download/aish-$ARCH-apple-darwin"

sudo chmod 755 /usr/local/bin/aish
```

### Windows (ベータ版) (x86_64 / ARM64)

インストーラがアーキテクチャを判定し `aish.exe` を取得、SHA-256 を検証して `%LOCALAPPDATA%\Programs\aish` に配置、ユーザ `PATH` に追加します。

**PowerShell:**

```powershell
irm https://raw.githubusercontent.com/tryandhappy/aish/main/install.ps1 | iex
```

`-Stable` を付けると prerelease を含む最新ではなく安定版を入れます:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/tryandhappy/aish/main/install.ps1))) -Stable
```

**cmd.exe**(ダウンロード→実行→インストーラ削除):

```cmd
curl -fsSL -o install.cmd https://raw.githubusercontent.com/tryandhappy/aish/main/install.cmd && install.cmd && del install.cmd
```

`PATH` を反映させるためターミナルを開き直してから `aish` を実行してください。バイナリは未署名のため初回実行時に Windows SmartScreen の警告が出ることがあります (**詳細情報 → 実行**)。Windows では `aish --update` 非対応のため、更新はインストーラを再実行してください。


## アップデート

```bash
sudo aish --update
```


### 開発用 ビルド＆インストール (開発版を /usr/local/bin/aish に上書き)

```bash
cargo build --release && sudo install -m 755 target/release/aish /usr/local/bin/aish
```


## 使い方

AI Agentにログインして、aishコマンドを実行。あとはいつもどおりSSH/Terminal。
AIへの問い合わせは Crtl + /

### Claude Code (Default)
```bash
claude login

aish
```

### Codex
```bash
codex login

aish --ai codex
```

### Gemini
``` bash
gemini login

aish --ai gemini
```

### Antigravity (`agy`, Gemini CLI の後継)
``` bash
# install: curl -fsSL https://antigravity.google/cli/install.sh | bash

aish --ai antigravity
aish --ai antigravity --model gemini-3-pro --effort high
```

### Qwen
```bash
qwen

aish --ai qwen
```


### Sample Option
```
# Claude Code
aish --ai claude
aish --ai claude --model opus
aish --ai claude --model opus --effort xhigh

# Claude Code Usage
aish --ai claude \
  --model sonnet|opus|haiku|best|claude-opus-4-8|sonnet[1m]|opus[1m] \
  --effort low|medium|high|xhigh|max|ultracode

# Codex
aish --ai codex --model gpt-5.6-sol
aish --ai codex --model gpt-5.6-sol --effort xhigh

## Codex Usage
aish --ai \
  codex --model gpt-5.6-sol|gpt-5.6-terra|gpt-5.5 \
  --effort low|medium|high|xhigh
```


### AI別 設定例 (config.toml)

```toml
# ~/.aish/config.toml — backend に以下のいずれかを設定

# Claude Code (既定)
[ai]
backend = "claude"

# Codex
[ai]
backend = "codex"

# Gemini
[ai]
backend = "gemini"

# Antigravity CLI (`agy`, Gemini CLI の後継)
[ai]
backend = "antigravity"

# Qwen
[ai]
backend = "qwen"

# Cursor
[ai]
backend = "cursor"

# GitHub Copilot
[ai]
backend = "copilot"

# xAI Grok CLI (`grok`)  — `which -a grok` で公式 CLI か確認
[ai]
backend = "grok"

# Kimi
[ai]
backend = "kimi"

# OpenCode
[ai]
backend = "opencode"

# Cloudflare Workers AI  (認証は環境変数: CLOUDFLARE_ACCOUNT_ID / CLOUDFLARE_API_TOKEN)
[ai]
backend = "cloudflare"

# NVIDIA NIM  (認証は環境変数: NVIDIA_API_KEY)
[ai]
backend = "nvidia"
```


## スラッシュコマンド

`Ctrl+/` プロンプトの先頭で入力します:

| コマンド | 動作 |
|---|---|
| `/help` | 使えるコマンド一覧 |
| `/model [名前]` | AI モデルを設定。引数なしでピッカー、`-` / `clear` でクリア |
| `/effort [レベル]` | 推論エフォートを設定（claude / codex / copilot / antigravity）。ピッカー / クリアは同様 |
| `/ai <名前>` | AI backend を切替（例: `/ai codex`） |
| `/clear` | 現在の backend の会話 / セッションをリセット |

## キー操作

**パススルー（通常のターミナル）:**

- `Ctrl+/` — aish プロンプトを開いて AI に問い合わせ。それ以外はそのままシェルへ。

**aish プロンプト（ミニバッファ）:**

- `Enter` 送信 · `Alt+Enter` 改行挿入（Shift+Enter は非対応）· `ESC` / `Ctrl+C` キャンセル
- `↑` / `↓` で過去のプロンプトを呼び出し — 履歴は `~/.aish/history` に永続化され再起動後も残る（`[history]` 設定）
- Emacs 風編集: `Ctrl+A`/`Ctrl+E` 行頭/行末、`Ctrl+B`/`Ctrl+F` 左/右、`Ctrl+U`/`Ctrl+K` 行頭/行末まで削除、`Ctrl+W` 単語削除

**コマンド確認**（`Exec? <cmd> [y/n/e/A/q]`）:

- `y` / `Enter` / `Space` — このコマンドを実行
- `n` / `ESC` — このコマンドをスキップ
- `e` — コマンドを**編集**し、実行前にもう一度確認
- `A` — これを実行し、残りは自動承認
- `q` — 残りを中止
- `Ctrl+C` / `Ctrl+D` — 残りを中止（AI に follow-up しない）

複数コマンドがある場合、`Enter` の既定は **All**、最後 / 単一コマンドでは **Yes**。

## 対話シェルを常に aish にする

極めて便利です。
aishいらないときはexitしてください。

### bash (Linux / WSL2 Ubuntu等)

`~/.bashrc` の **末尾** に以下を追加。

```bash
if [[ $- == *i* && -z "$AISH_PID" ]]; then
    # Exit returns to bash.
    PROMPT_COMMAND='unset PROMPT_COMMAND; command -v aish >/dev/null && aish'
    # Exit closes the terminal.
    #PROMPT_COMMAND='unset PROMPT_COMMAND; command -v aish >/dev/null && exec aish'
fi
```

### zsh (macOS のデフォルト)

`~/.zprofile` の **末尾** に以下を追加。`.zprofile` はログインシェルでだけ読まれ、aish が起動する子 zsh はログインシェルではないので再帰しません (`AISH_PID` ガードは保険)。

```zsh
if [[ -o interactive && -z "$AISH_PID" ]]; then
    # Exit returns to zsh.
    command -v aish >/dev/null && aish
    # Exit closes the terminal.
    #command -v aish >/dev/null && exec aish
fi
```

## 既知の制約

aish は「**画面で承認した物 = サーバで実行される物**」という原則を厳守し、サーバ側に勝手な書き込みをしません（shell 統合フックや履歴書き換えをしない）。この設計の代償として、いくつか制約があります:

- **完了判定は passive**（シェルプロンプトが戻るのを観察）。exit code は取得せず、AI は出力から成否を推測します。`tail -f` 等の連続出力は自動検出できないので `Ctrl+C` で抜けてください。
- **カスタムプロンプトテーマ**は初回だけ見逃すことがあります（以降は学習）。
- **改行挿入の Shift+Enter は非対応**です（`Alt+Enter` を使用）。
- **zsh の vi モード**（`bindkey -v`）ではキャンセル時に `^A^K` が残ることがあります（未承認コマンドの実行には至りません）。
- **IME の未確定文字（preedit）**は取得できません。
- **Windows ネイティブはベータ版**: `--update` 非対応（インストーラを再実行）。リサイズや `cls` で画面上の aish 出力が上書きされることがあります。

## コミュニティ

バグ報告・ご意見・ご相談はDiscordまたはXで受け付けています。
お気軽にご相談ください。皆様の話がアイディアの元になり大変貴重です。
(返事が遅くなったらごめんなさい。)

###### Discord

https://discord.gg/nj3xz6RBQC

###### X

https://x.com/tryandhappy
