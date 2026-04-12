# aish (AI + SSH)

**AI-assisted SSH shell** — Ask Claude Code for help right inside your SSH session.

[English](#english) | [日本語](#日本語)

---

## English

`aish` is a drop-in replacement for `ssh`. Work on the remote server as usual, and when you need help, press `Ctrl+/` to ask Claude Code. The AI can see what's on your screen, so you don't have to copy-paste errors or logs. If it suggests a command, you confirm before it runs.

### Prerequisites

- [Claude Code](https://claude.ai/install.sh) (`curl -fsSL https://claude.ai/install.sh | bash`)

### Install

```bash
sudo curl -fsSL -o /usr/bin/aish https://github.com/tryandhappy/aish/releases/latest/download/aish-$(uname -m)-unknown-linux-musl
sudo chmod 755 /usr/bin/aish
```

### Update

```bash
aish --update
```

### Usage

```bash
aish                    # Local shell
aish user@example.com   # SSH (same arguments as ssh)
```

| Input | Action |
|-------|--------|
| `Ctrl+/` | Ask the AI about the current screen |
| Everything else | Runs as a normal shell command |
| `exit` | Quit |

### Features

- Works like a normal shell (tab completion, vim, arrow keys, etc.)
- The AI sees your screen automatically — no copy-paste needed
- Suggested commands require your confirmation before running
- Conversations keep context across multiple questions

---

## 日本語

`aish` は `ssh` の代わりに使えるCLIです。リモートサーバをいつも通り操作しながら、困ったときは `Ctrl+/` で Claude Code に質問できます。AIは画面の内容を見ているので、エラーやログを貼り付ける必要はありません。コマンドを提案された場合は確認してから実行されます。

### 前提条件

- [Claude Code](https://claude.ai/install.sh) (`curl -fsSL https://claude.ai/install.sh | bash`)

### インストール

```bash
sudo curl -fsSL -o /usr/bin/aish https://github.com/tryandhappy/aish/releases/latest/download/aish-$(uname -m)-unknown-linux-musl
sudo chmod 755 /usr/bin/aish
```

### アップデート

```bash
aish --update
```

### 使い方

```bash
aish                    # ローカルシェル
aish user@example.com   # SSH接続 (sshと同じ引数)
```

| 入力 | 動作 |
|------|------|
| `Ctrl+/` | 画面の内容をAIに分析してもらう |
| それ以外 | 通常のシェルコマンドとして実行 |
| `exit` | 終了 |
