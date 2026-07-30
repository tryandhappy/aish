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
- Windows 10 1809+ ネイティブ (Windows Terminal 推奨) ※プレビルドバイナリ配布あり (x86_64 / aarch64 の `.exe`・`.zip`)。`--update` 自己更新は非対応 (手動でダウンロード)
- macOS (Intel・Apple Silicon) ※テスト不十分

#### 必要なコマンド

- AI CLI (いずれか)
  - [Claude Code](https://code.claude.com/docs/ja/overview)
  - [ChatGPT Codex](https://openai.com/ja-JP/codex/)
  - [Gemini CLI](https://cloud.google.com/blog/ja/topics/developers-practitioners/introducing-gemini-cli/)
  - [Qwen Code](https://qwen.ai/qwencode)
- OpenSSH (リモートSSH)
- bash または zsh (ローカルシェル)
- curl (aish --update)



## 対応AI CLI

- Claude Code (API, Pro, Max, Team, Enterprise) ※FreeはClaude Codeが使えないので未対応 (デフォルト)
- OpenAI ChatGPT Codex ※テスト不十分
- Google Gemini ※テスト不十分
- Qwen Code ※テスト不十分


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

### Windows (x86_64 / ARM64)

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
aish --ai codex --model gpt-5.5
aish --ai codex --model gpt-5.5 --effort xhigh

## Codex Usage
aish --ai \
  codex --model gpt-5.5|gpt-5.4|gpt-g.4-mini \
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

# Qwen
[ai]
backend = "qwen"

# Cursor
[ai]
backend = "cursor"

# GitHub Copilot
[ai]
backend = "copilot"

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

## コミュニティ

バグ報告・ご意見・ご相談はDiscordまたはXで受け付けています。
お気軽にご相談ください。皆様の話がアイディアの元になり大変貴重です。
(返事が遅くなったらごめんなさい。)

###### Discord

https://discord.gg/nj3xz6RBQC

###### X

https://x.com/tryandhappy
