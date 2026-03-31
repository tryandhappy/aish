# aish

SSH + AI アシスタント CLI ツール

SSH セッションに AI（Claude, ChatGPT, Gemini）を常駐させ、コマンド操作の監視・助言・代行を行います。

## インストール

```bash
# Rust がインストール済みであること
cargo build --release
cp target/release/aish ~/.local/bin/
```

## セットアップ

### API キーの設定

環境変数で設定（推奨）:

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
export OPENAI_API_KEY="sk-..."
export GOOGLE_API_KEY="AIza..."
```

または設定ファイル `~/.aish/config.toml` を作成:

```bash
cp config.toml.example ~/.aish/config.toml
# エディタで API キーを記入
```

少なくとも 1 つの AI の API キーが必要です。

## 使い方

```bash
# SSH 接続 + AI アシスタント起動（デフォルト: Claude）
aish user@hostname

# ChatGPT を初期 AI にして起動
aish user@hostname --ai chatgpt
```

### シェル内コマンド

| コマンド | 説明 |
|----------|------|
| `!<command>` | SSH 上でコマンドを直接実行 |
| `<テキスト>` | AI にプロンプトとして送信 |
| `/claude` | メイン AI を Claude に切替 |
| `/chatgpt` | メイン AI を ChatGPT に切替 |
| `/gemini` | メイン AI を Gemini に切替 |
| `/?` | 現在の AI に直近の出力を解説させる |
| `/? <ai>` | 指定した AI に解説させる |
| `/? all` | すべての AI に解説させる |
| `/help` | コマンド一覧を表示 |
| `/quit` | 終了 |

### 使用例

```
aish(Claude)> !df -h
Filesystem      Size  Used Avail Use% Mounted on
/dev/sda1        50G   32G   18G  64% /

aish(Claude)> ディスク容量が逼迫しているけど、大きいファイルはどこ?
[Claude] du コマンドで大きなディレクトリを特定できます。
  提案コマンド 1: du -sh /* | sort -rh | head -10
  コマンド 'du -sh /* | sort -rh | head -10' を実行しますか? [y/N] y
```

## 対応 AI

| AI | モデル | API キー環境変数 |
|----|--------|-----------------|
| Claude | claude-sonnet-4-20250514 | `ANTHROPIC_API_KEY` |
| ChatGPT | gpt-4o | `OPENAI_API_KEY` |
| Gemini | gemini-2.0-flash | `GOOGLE_API_KEY` |

## ドキュメント

- [仕様書](docs/SPEC.md)
- [設計書](docs/DESIGN.md)

## ライセンス

[LICENSE](LICENSE) を参照。
