# aish (AI + SSH)

**AI-assisted SSH shell** — Ask Claude Code for help right inside your SSH session.

- `aish` は AI + SSH です。 `ssh` の代わりに使えるCLIです。
- SSHしながら、`Ctrl+/` で AIに指示できます。
- AIは画面の内容を見ているので、エラーやログを貼り付ける必要はありません。
- コマンド実行時は必ず確認が入るので安心です。
- サーバにAI Agentをインストールする必要がありません。

![aish screenshot](docs/images/sample1.png)

## デモ動画

**SSHモード**
https://github.com/tryandhappy/aish/raw/main/docs/movies/sample-ssh1.mp4

**ローカルモード**
https://github.com/tryandhappy/aish/raw/main/docs/movies/sample-local1.mp4



## 前提条件

#### 対応OS

- Linux (テストしているのは Ubuntu 24.04)
- Windows WSL2 (テストしているのは Ubuntu 24.04)

#### 必要なコマンド

- AI CLI (いずれか)
  - [Claude Code CLI](https://code.claude.com/docs/ja/overview)
  - [OpenAI ChatGPT Codex](https://openai.com/ja-JP/codex/)
  - [Google Gemini CLI](https://cloud.google.com/blog/ja/topics/developers-practitioners/introducing-gemini-cli/)
  - [Qwen Code](https://qwen.ai/qwencode)
- OpenSSH (リモートSSH)
- bash (ローカルシェル)
- curl (aish --update)



## 対応AI CLI

- Claude Code CLI (API, Pro?, Max?, Team?, Enterprise?) ※FreeはClaude Codeが使えないので未対応
- OpenAI ChatGPT Codex
- Google Gemini
- Qwen Code (未テスト)


## インストール

```bash
sudo curl -fsSL -o /usr/bin/aish https://github.com/tryandhappy/aish/releases/latest/download/aish-$(uname -m)-unknown-linux-musl
sudo chmod 755 /usr/bin/aish
```

## アップデート

```bash
sudo aish --update
```


### ソースからビルド＆インストール (開発版を /usr/bin/aish に上書き)

```bash
cargo build --release && sudo install -m 755 target/release/aish /usr/bin/aish
```


## 使い方

```bash
claude login

aish                    # ローカルシェル
aish user@example.com   # SSH接続 (sshと同じ引数)
```

| 入力 | 動作 |
|------|------|
| `Ctrl+/` | AIプロンプト入力 |
| `exit` | 終了 |



## 対話シェルを常に aish にする

`~/.bashrc` の **末尾** に以下を追加。

```bash
if [[ $- == *i* && -z "$AISH_PID" ]]; then
    # Exit returns to bash.
    PROMPT_COMMAND='unset PROMPT_COMMAND; command -v aish >/dev/null && aish'
    # Exit closes the terminal.
    #PROMPT_COMMAND='unset PROMPT_COMMAND; command -v aish >/dev/null && exec aish'
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

