# aish (AI + SSH)

**AI-assisted SSH shell** — Ask Claude Code for help right inside your SSH session.

- `aish` は AI + SSH です。 `ssh` の代わりに使えるCLIです。
- SSHしながら、`Ctrl+/` で Claude Code に指示できます。
- AIは画面の内容を見ているので、エラーやログを貼り付ける必要はありません。
- コマンド実行時は必ず確認が入るので安心です。
- サーバにClaude Code CLIをインストールする必要がありません。

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

#### 必要なコマンド (v0.1.13)

- [Claude Code CLI](https://code.claude.com/docs/ja/overview) 
- OpenSSH (リモートSSH)
- bash (ローカルシェル)
- curl (--update)



## 対応AI

- Claude Code CLI (API, Pro?, Max?, Team?, Enterprise?) ※Freeは未対応

将来は他のAIにも対応予定です。例えばCodex。
対応してほしいAIがあれば一番下のコミュニティからお気軽にご連絡を。



## Claude Code CLI のライセンスについて

2026年4月4日にAnthropicは、Claude サブスクリプションプラン (Pro, Max, Team, Enterprise) に対し、サードパティ製自動ツールでの利用を禁止しました。
これは主に、OpenClaw、OpenCode、Cline、Roo Code等による高付加が問題になったためです。
`aish`は人間がプロンプトを入力するため、自動ではないと思っておりますが、心配な方はClaude APIプランをご検討ください。



## インストール

```bash
sudo curl -fsSL -o /usr/bin/aish https://github.com/tryandhappy/aish/releases/latest/download/aish-$(uname -m)-unknown-linux-musl
sudo chmod 755 /usr/bin/aish
```



## アップデート

```bash
sudo aish --update
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

ターミナルを開いた瞬間から aish に入りたい場合は、`~/.bashrc` の **末尾** に以下のいずれかを追加してください。挙動の好みで方式 A / B を選びます。

### 方式 A: ターミナル = aish

aish 内で `exit` するとターミナルウィンドウもそのまま閉じます。「ターミナルを開く == aish を開く」という感覚にしたい場合はこちら。

```bash
if [[ $- == *i* && -z "$AISH_PID" ]]; then
    PROMPT_COMMAND='unset PROMPT_COMMAND; command -v aish >/dev/null && exec aish'
fi
```

`exec` で bash プロセスを aish に置き換えるため、aish 終了 = bash 終了 = ターミナル終了になります。

### 方式 B: exit すると bash に戻る

aish 内で `exit` すると元の bash プロンプトに戻ります。bash で何か作業した後にもう一度 aish に入りたいときは `aish` と打ち直せます。端末を閉じるときはその bash でもう一度 `exit` します。

```bash
if [[ $- == *i* && -z "$AISH_PID" ]]; then
    PROMPT_COMMAND='unset PROMPT_COMMAND; command -v aish >/dev/null && aish'
fi
```

方式 A との違いは `exec` を付けないことだけです。aish は bash の子プロセスとして起動し、終了すると bash が制御を取り戻します。`unset PROMPT_COMMAND` が先頭にあるので、aish 終了後に再度自動起動することはありません。

### 動作のしくみ

`PROMPT_COMMAND` は bash が最初のプロンプトを描く直前に発火するフックです。`.bashrc` と `.profile` がすべて完走した状態で aish を起動するため、`~/.local/bin/claude` などのパスが通った状態で確実に aish に入ります。

#### 各要素の意味

- `$- == *i*` — 対話シェルのときだけ動作。`bash -c "..."`（git, make, vim `:!` 等の非対話呼び出し）には影響しません。
- `-z "$AISH_PID"` — aish 配下の子シェルでは再起動しません（aish は子シェルに `AISH_PID` を渡し、自身の二重起動も拒否します）。
- `command -v aish` — aish が PATH に無いときは起動せず通常の bash に戻ります（誤って端末が開かなくなる事故を防ぐ保険）。
- `unset PROMPT_COMMAND` — 1 回だけ発火するように自身を外します。

#### 他ツールとの順序

`starship`、`oh-my-bash`、`conda init`、`direnv` 等は独自に `PROMPT_COMMAND` を書き換えるため、上記スニペットは必ず **`.bashrc` の最終行**（これらの初期化より後）に置いてください。

#### chsh による設定は非推奨

`chsh` で aish 自体をログインシェルに設定するのは **非推奨** です。aish は内部で `$SHELL` を起動するラッパであり、`-c` オプションも持たない（未知の引数は ssh に転送される）ため、非対話呼び出しが破綻します。



## コミュニティ

バグ報告・ご意見・ご相談はDiscordまたはXで受け付けています。
お気軽にご相談ください。皆様の話がアイディアの元になり大変貴重です。
(返事が遅くなったらごめんなさい。)

###### Discord

https://discord.gg/nj3xz6RBQC

###### X

https://x.com/tryandhappy
