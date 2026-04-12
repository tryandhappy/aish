# aish (AI + SSH)

**AI-assisted SSH shell** — SSHセッションの中からそのまま Claude Code に相談できるCLI。

**AI-assisted SSH shell** — Ask Claude Code for help right inside your SSH session.

[English](#english) | [日本語](#日本語)

---

## English

`aish` is a drop-in replacement for `ssh`. Work on the remote server as usual, and when you need help, just type `?` to ask Claude Code. The AI can see what's on your screen, so you don't have to copy-paste errors or logs. If it suggests a command, you confirm before it runs.

Claude Code must be installed first.

### Usage

```bash
# Local shell
aish

# SSH (same arguments as the ssh command)
aish user@example.com
```

Once you see the `[aish]` prompt:

| Input | What happens |
|-------|--------------|
| `Ctrl+/` | Let the AI analyze the current screen |
| Anything else | Runs as a normal shell command |
| `exit` | Quit |

### Features

- Works like a normal shell (tab completion, vim, arrow keys, etc.)
- The AI sees your screen automatically — no copy-paste needed
- Suggested commands always need your confirmation before running
- Conversations keep their context across multiple questions

---

## 日本語

`aish` は `ssh` の代わりに使えるCLIです。リモートサーバをいつも通り操作しながら、困ったときは `?` と入力するだけで Claude Code に質問できます。AIは画面に表示されている内容を見ているので、エラーメッセージやログを貼り付け直す必要はありません。コマンドを提案された場合は、確認してから実行されます。

前提として Claude Code がインストールされている必要があります。

### 使い方

```bash
# ローカルシェル
aish

# SSH接続 (sshコマンドと同じ引数)
aish user@example.com
```

`[aish]` プロンプトが出たら:

| 入力 | 動作 |
|------|------|
| `Ctrl+/` | 画面の内容をAIに分析してもらう |
| それ以外 | 普通のシェルコマンドとして実行 |
| `exit` | 終了 |


