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
- Windows 10 1809+ native (Windows Terminal recommended) — prebuilt binaries provided (x86_64 / aarch64 `.exe` and `.zip`); `--update` self-update is not supported (run the installer/download manually)
- macOS (Intel / Apple Silicon) — not thoroughly tested

#### Required Commands

- AI CLI (one of)
  - [Claude Code](https://code.claude.com/docs/en/overview)
  - [ChatGPT Codex](https://openai.com/codex/)
  - [Gemini CLI](https://cloud.google.com/blog/topics/developers-practitioners/introducing-gemini-cli/)
  - [Qwen Code](https://qwen.ai/qwencode)
- OpenSSH (for remote SSH)
- bash or zsh (for the local shell)
- curl (for aish --update)



## Supported AI CLIs

- Claude Code (API, Pro, Max, Team, Enterprise) — Free is not supported since it can't use Claude Code (default)
- OpenAI ChatGPT Codex — not thoroughly tested
- Google Gemini — not thoroughly tested
- Qwen Code — not thoroughly tested


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

### Windows (x86_64 / ARM64)

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

## Community

We welcome bug reports, feedback, and questions on Discord or X.
Feel free to reach out — your stories are a valuable source of ideas.
(Sorry if replies are slow.)

###### Discord

https://discord.gg/nj3xz6RBQC

###### X

https://x.com/tryandhappy
