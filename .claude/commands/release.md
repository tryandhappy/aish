---
description: aish の version bump → commit → tag → push の対話的リリース手順
---

# Release Procedure

aish プロジェクトをリリースする。次の手順をこの順で進めること。各ステップで明示的にユーザの確認を取る場面では `AskUserQuestion` を使う。

## 0. 事前チェック

並列で実行:
- `git status` — 作業ツリーがクリーンであることを確認 (untracked / modified が残っていれば中止して報告)
- `git rev-parse --abbrev-ref HEAD` — 現ブランチが `main` であることを確認 (違えば中止して報告)
- `grep '^version' Cargo.toml` — 現在のバージョンを取得

## 1. 新バージョンの決定

現在のバージョンと、引数 `$ARGUMENTS` の有無で挙動を分ける:

- `$ARGUMENTS` が指定されていればそれを新バージョンとして採用 (`v` プレフィックス付きでも無しでも受ける。内部では `v` 無しの形式で扱う)
- 指定が無ければ `AskUserQuestion` でユーザに新バージョンを問い合わせる。デフォルト候補は patch bump (例: 0.4.5 → 0.4.6)、minor bump (0.4.5 → 0.5.0)、major bump (0.4.5 → 1.0.0)、および **prerelease** (例: 0.5.0-rc.1 / 0.5.0-beta.1 — 最新版チャネル向け先行リリース)

採用バージョンを `NEW=<x.y.z>` として以降使う。

### リリースチャネル (安定版 / 最新版)

タグに **SemVer のハイフン付き prerelease 識別子** (`-rc.N` / `-beta.N` / `-alpha.N` 等) が含まれるかで、`release.yml` が GitHub Release の `prerelease` フラグを自動判定する:

- **ハイフン無し** (`v0.9.0`) → `prerelease: false` → GitHub の `Latest` バッジが付く = **安定版**。`aish --update` (既定 `--stable`) が拾う。
- **ハイフン付き** (`v0.9.0-rc.1`) → `prerelease: true` → `Latest` バッジは付かない = **最新版チャネルのみ**。`aish --update --prerelease` だけが拾う。

prerelease をリリースする場合は **`NEW` に識別子をそのまま含める** (`NEW=0.9.0-rc.1`)。これにより `Cargo.toml` の `version` も `0.9.0-rc.1` になり、`update.rs` の `if latest == current` 比較 (タグから `v` を剥いだ値 vs `CARGO_PKG_VERSION`) が正しく一致する。Cargo.toml を数値だけ (`0.9.0`) にしてタグだけ `-rc.1` にすると、prerelease バイナリが「常に更新あり」と誤判定されるので避けること。

## 2. バージョン更新

`Cargo.toml` の `version = "..."` 行を `NEW` に書き換え (Edit ツール)。

`cargo build` を実行して `Cargo.lock` の `aish` エントリを `NEW` に同期。ビルドが警告無しで通ることも確認すること。失敗したら中止して報告。

## 3. コミットメッセージ生成

前回 release tag (`git describe --tags --abbrev=0`) から HEAD までのコミット履歴を取得し、リリース commit のメッセージを生成する:

```bash
git log --oneline <prev_tag>..HEAD
git diff --stat <prev_tag>..HEAD
```

このプロジェクトの commit 慣習に従ってメッセージを作成 (日本語、`Feat:` / `Fix:` / `Refactor:` 等のプレフィックス + 短い説明 + 必要なら body):

- **タイトル行**: 1 行目はリリースに含まれる主な変更点をまとめた概要 (~70 字以内)。複数の変更がある場合は `、` で区切って列挙
- **本文 (任意)**: 個別変更の箇条書き (各々が `git log --oneline` 1 行に対応する形)
- `Release: vX.Y.Z` という形式は **使わない**

例 (実際の git log を参照したもの):
```
Feat: --update 例追加、Ctrl+C 1 回でキャンセル、4 backend で session_id 保持と resume コマンド表示、aish 二重起動防止

- --help の --update 説明に `(例: sudo aish --update)` を追記。
- check_stdin_cancel を ... 修正。
- AiBackend trait に resume_command() を追加し、...
- ...
```

生成したメッセージを `AskUserQuestion` でユーザに確認 (Approve そのまま / 編集 / キャンセル)。

## 4. コミットとタグ

並列で実行:

```bash
git add Cargo.toml Cargo.lock
```

その後 (sequential、メッセージは step 3 で生成・承認されたもの):

```bash
git commit -m "<生成メッセージ>"
git tag -a "v$NEW" -m "Release v$NEW"
```

タグメッセージは短い `"Release vX.Y.Z"` で固定 (タグはバージョンを示せば十分。詳細は commit 側にある)。重要なリリースで一覧性が欲しい場合は希望に応じてハイライトを 3-6 行追記しても良い。

## 5. push 確認

`AskUserQuestion` で次を聞く:

1. push しますか? (origin に main + tag を送信)
2. ローカルバイナリも `cargo install --path . --force` で更新しますか?

両方とも独立に Yes/No 選択。

選択に応じて:
- push: `git push origin main --tags`
- install: `cargo install --path . --force` (release ビルド + `~/.cargo/bin/aish` に上書き)

## 6. 完了報告

最後に、何が行われたかを 3-5 行で要約 (新バージョン、commit hash、tag 名、push 有無、install 有無)。GitHub Release 作成までは行わない (任意作業として `gh release create v$NEW --notes-from-tag` を提案するに留める)。

---

## 注意事項

- バージョン番号は SemVer (`X.Y.Z` 数値のみ) を期待する。`v` プレフィックスは tag 側にだけ付与し、Cargo.toml 内では付けない
- 既に同名の tag が存在する場合は `git tag -l "v$NEW"` で事前検出して中止すること (force 上書きはしない)
- `cargo build` が失敗 / 警告ありで終わった場合は commit 前に必ず止まる
