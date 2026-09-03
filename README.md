# aish (AI + SSH)

**English** | [日本語](README.ja.md)

**AI-assisted SSH shell** — Ask Claude Code for help right inside your SSH session.

- `aish` is AI + SSH/Terminal.
- Query the AI from your terminal with `Ctrl+/`.
- Work through tasks while consulting the AI.
- The AI sees your screen, so there's no need to paste errors or logs.
- Every command is confirmed before it runs, so you can relax.
- It uses the AI agent on the client side — nothing to install on the server.

![aish screenshot](docs/images/sample1.png)

## Demo Videos

**SSH mode**
https://github.com/tryandhappy/aish/raw/main/docs/movies/sample-ssh1.mp4

**Local mode**
https://github.com/tryandhappy/aish/raw/main/docs/movies/sample-local1.mp4



## Requirements

#### Supported OS

- Linux (tested on Ubuntu 24.04)
- Windows WSL2 (tested on Ubuntu 24.04)
- Windows 10 1809+ native **(Beta)** (Windows Terminal recommended) — prebuilt binaries provided (x86_64 / aarch64 `.exe` and `.zip`); `--update` self-update is not supported (run the installer/download manually)
- macOS (Intel / Apple Silicon) — not thoroughly tested

#### Required Commands

- An AI CLI — one of the [Supported AI CLIs](#supported-ai-clis) below (e.g. [Claude Code](https://code.claude.com/docs/en/overview), [ChatGPT Codex](https://openai.com/codex/), [Gemini CLI](https://cloud.google.com/blog/topics/developers-practitioners/introducing-gemini-cli/), [Qwen Code](https://qwen.ai/qwencode))
- OpenSSH (for remote SSH)
- bash or zsh (for the local shell)
- curl (for aish --update, and for the REST backends)



## Supported AI CLIs

Pick one with `--ai <name>` or set `backend` in `config.toml`. If the selected CLI is missing, aish falls back to the first installed one it finds.

**Native backends** (built in):

- `claude` — Claude Code (API, Pro, Max, Team, Enterprise); Free can't use Claude Code (**default**)
- `codex` — OpenAI ChatGPT Codex — not thoroughly tested
- `gemini` — Google Gemini CLI — not thoroughly tested
- `antigravity` (`agy`) — Google Antigravity CLI, successor to the Gemini CLI
- `qwen` — Qwen Code — not thoroughly tested
- `cursor` — Cursor Agent (`cursor-agent`), forced to `--mode plan`
- `copilot` — GitHub Copilot CLI (shell/write denied, plan mode)
- `grok` — xAI Grok CLI (`grok`) — run `which -a grok` to confirm the official CLI
- `cloudflare` — Cloudflare Workers AI over REST (auth via env: `CLOUDFLARE_ACCOUNT_ID` / `CLOUDFLARE_API_TOKEN`)
- `nvidia` — NVIDIA NIM over REST (auth via env: `NVIDIA_API_KEY`)

**Built-in recipes** (generic backends shipped with read-only safety baked in):

- `kimi` — MoonshotAI Kimi CLI (`--plan`)
- `opencode` — OpenCode (read-only agent injected via config)

You can add your own CLI as a `[[ai.providers]]` recipe in `config.toml`; run `aish --list-providers` to see all resolved backends. Safety flags are **not** auto-applied to user-defined providers — the recipe author is responsible for them.

Whichever backend you use, aish never grants it shell/write access on its own: the AI only *proposes* commands, and nothing runs until you approve it on screen.


## Installation

### Linux

```bash
sudo curl -fsSL -o /usr/local/bin/aish "https://github.com/tryandhappy/aish/releases/latest/download/aish-$(uname -m)-unknown-linux-musl"

sudo chmod 755 /usr/local/bin/aish
```

### macOS (Intel / Apple Silicon)

```bash
sudo mkdir -p /usr/local/bin

ARCH=$(uname -m); case "$ARCH" in arm64|aarch64) ARCH=aarch64;; x86_64|amd64) ARCH=x86_64;; esac

sudo curl -fsSL -o /usr/local/bin/aish "https://github.com/tryandhappy/aish/releases/latest/download/aish-$ARCH-apple-darwin"

sudo chmod 755 /usr/local/bin/aish
```

### Windows (Beta) (x86_64 / ARM64)

The installer detects your architecture, downloads `aish.exe`, verifies its SHA-256, installs it to `%LOCALAPPDATA%\Programs\aish`, and adds it to your user `PATH`.

**PowerShell:**

```powershell
irm https://raw.githubusercontent.com/tryandhappy/aish/main/install.ps1 | iex
```

Add `-Stable` to install the latest stable release instead of the newest (prerelease-inclusive) one:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/tryandhappy/aish/main/install.ps1))) -Stable
```

**cmd.exe** (downloads, runs, then removes the installer):

```cmd
curl -fsSL -o install.cmd https://raw.githubusercontent.com/tryandhappy/aish/main/install.cmd && install.cmd && del install.cmd
```

Open a new terminal so the updated `PATH` takes effect, then run `aish`. The binary is unsigned, so Windows SmartScreen may warn on first run (choose **More info → Run anyway**). `aish --update` is not supported on Windows — re-run the installer to upgrade.


## Update

```bash
sudo aish --update
```


### Build & Install for Development (overwrite /usr/local/bin/aish with a dev build)

```bash
cargo build --release && sudo install -m 755 target/release/aish /usr/local/bin/aish
```


## Usage

Log in to your AI agent, then run the `aish` command. After that, it's SSH/Terminal as usual.
Query the AI with Ctrl + /

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

### Antigravity (`agy`, successor to the Gemini CLI)
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


### AI Setup (config.toml)

```toml
# ~/.aish/config.toml — set backend = one of the following

# Claude Code (default)
[ai]
backend = "claude"

# Codex
[ai]
backend = "codex"

# Gemini
[ai]
backend = "gemini"

# Antigravity CLI (`agy`, successor to the Gemini CLI)
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

# xAI Grok CLI (`grok`)  — run `which -a grok` to confirm you have the official CLI
[ai]
backend = "grok"

# Kimi
[ai]
backend = "kimi"

# OpenCode
[ai]
backend = "opencode"

# Cloudflare Workers AI  (auth via env: CLOUDFLARE_ACCOUNT_ID / CLOUDFLARE_API_TOKEN)
[ai]
backend = "cloudflare"

# NVIDIA NIM  (auth via env: NVIDIA_API_KEY)
[ai]
backend = "nvidia"
```


## Slash Commands

Type these at the start of the `Ctrl+/` prompt:

| Command | What it does |
|---|---|
| `/help` | List available commands |
| `/model [name]` | Set the AI model. No argument opens a picker; `-` or `clear` clears it |
| `/effort [level]` | Set the reasoning effort (claude / codex / copilot / antigravity). Same picker / clear rules |
| `/ai <name>` | Switch AI backend (e.g. `/ai codex`) |
| `/clear` | Reset the current backend's conversation/session |

## Key Operations

**Passthrough (normal terminal):**

- `Ctrl+/` — open the aish prompt to ask the AI. Everything else goes straight to the shell.

**aish prompt (minibuffer):**

- `Enter` submit · `Alt+Enter` insert a newline (Shift+Enter is not supported) · `ESC` / `Ctrl+C` cancel
- `↑` / `↓` recall previous prompts — history persists across restarts in `~/.aish/history` (see the `[history]` config)
- Emacs-style editing: `Ctrl+A`/`Ctrl+E` line start/end, `Ctrl+B`/`Ctrl+F` left/right, `Ctrl+U`/`Ctrl+K` delete to start/end, `Ctrl+W` delete word

**Command confirmation** (`Exec? <cmd> [y/n/e/A/q]`):

- `y` / `Enter` / `Space` — run this command
- `n` / `ESC` — skip this command
- `e` — **edit** the command, then confirm again before it runs
- `A` — run this and auto-approve the rest
- `q` — cancel the rest
- `Ctrl+C` / `Ctrl+D` — cancel the rest without asking the AI to follow up

When several commands are queued, `Enter` defaults to **All**; on the last/only command it defaults to **Yes**.

## Make aish Your Default Interactive Shell

Extremely convenient.
Just `exit` when you don't need aish.

### bash (Linux / WSL2 Ubuntu, etc.)

Add the following to the **end** of `~/.bashrc`.

```bash
if [[ $- == *i* && -z "$AISH_PID" ]]; then
    # Exit returns to bash.
    PROMPT_COMMAND='unset PROMPT_COMMAND; command -v aish >/dev/null && aish'
    # Exit closes the terminal.
    #PROMPT_COMMAND='unset PROMPT_COMMAND; command -v aish >/dev/null && exec aish'
fi
```

### zsh (default on macOS)

Add the following to the **end** of `~/.zprofile`. `.zprofile` is read only by login shells, and the child zsh that aish launches is not a login shell, so it won't recurse (the `AISH_PID` guard is a safety net).

```zsh
if [[ -o interactive && -z "$AISH_PID" ]]; then
    # Exit returns to zsh.
    command -v aish >/dev/null && aish
    # Exit closes the terminal.
    #command -v aish >/dev/null && exec aish
fi
```

## Known Limitations

aish keeps a strict rule — *what you approve on screen is what runs on the server* — and never writes to the server on its own (no shell-integration hooks, no history rewriting). That design has a few consequences:

- **Command completion is detected passively** (by watching for the shell prompt to return). Exit codes are not captured, so the AI infers success/failure from the output. Long-running/streaming commands (`tail -f`) aren't auto-detected — exit them with `Ctrl+C`.
- **A custom shell prompt theme** may be missed on its first use, then learned.
- **Shift+Enter is not supported** for inserting newlines — use `Alt+Enter`.
- **zsh vi mode** (`bindkey -v`) can leave a stray `^A^K` on cancel (never runs an unapproved command).
- **IME preedit (unconfirmed) text** can't be read.
- **Windows native is Beta**: `--update` is unsupported (re-run the installer); a terminal resize or `cls` can overwrite aish's on-screen output.

## Community

We welcome bug reports, feedback, and questions on Discord or X.
Feel free to reach out — your stories are a valuable source of ideas.
(Sorry if replies are slow.)

###### Discord

https://discord.gg/nj3xz6RBQC

###### X

https://x.com/tryandhappy
