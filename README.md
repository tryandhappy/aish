# aish

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
| `? <question>` | Ask the AI |
| `Ctrl+/` | Let the AI analyze the current screen |
| Anything else | Runs as a normal shell command |
| `exit` | Quit |

### Features

- Works like a normal shell (tab completion, vim, arrow keys, etc.)
- The AI sees your screen automatically — no copy-paste needed
- Suggested commands always need your confirmation before running
- Conversations keep their context across multiple questions

### License

- This software is provided free of charge.
- Redistribution of this software is prohibited.
- Commercial use is permitted.
- Reverse engineering, including decompilation and disassembly, is prohibited.
- This software is provided "AS IS", without warranty of any kind.

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
| `? <質問>` | AIに質問する |
| `Ctrl+/` | 画面の内容をAIに分析してもらう |
| それ以外 | 普通のシェルコマンドとして実行 |
| `exit` | 終了 |

### 機能

- 普通のシェルと同じように使える (タブ補完、vim、矢印キーなど)
- AIが画面を自動で把握する — コピペ不要
- 提案されたコマンドは必ず確認してから実行
- 会話の文脈が維持されるので、続けて質問できる

### ライセンス

- 本ソフトウェアは無料で使用できます。
- 本ソフトウェアの再配布は禁止されています。
- 商用利用は許可されています。
- リバースエンジニアリング（逆コンパイル、逆アセンブル等）は禁止されています。
- 本ソフトウェアは現状有姿（AS IS）で提供され、いかなる保証もありません。
