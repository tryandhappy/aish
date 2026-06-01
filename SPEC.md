# aish 仕様書

CLI SSH + AI (Claude Code) ツール。クライアント側のClaude Codeから、ローカルシェルまたはSSH接続先サーバを調査・操作するための対話型UI。

---

## 0. 用語（各部の名称）

- **パススルーモード**: 通常のシェル操作状態。キー入力はPTYにそのまま転送される。
- **aishプロンプト**（ミニバッファ）: `Ctrl+/` で表示される `[aish]` 入力欄。ターミナル最下行（ステータスバー行）に表示され、AIへの質問を入力する。ESC / Ctrl+C / Ctrl+/ でキャンセル。
- **ステータスバー**: 最下行に常時表示される `aish v{version} | Ctrl+/ for AI` 行。DECSTBMスクロール領域外に固定表示。
- **スピナー**: AI応答待ち中にステータスバー行で回転するアニメーション（`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` + `Thinking...`）。
- **確認プロンプト**: AIが提案したコマンドの実行可否を問う `Execute? (Y/n)` 表示。
- **ReadLineモード**: AI対話中の確認プロンプト応答など、ライン編集付きで入力を受け付ける状態。

---

## 1. アーキテクチャ（ファイル構成）

| ファイル | 役割 |
|---|---|
| `main.rs` | メインループ。PTY読み取りスレッド、ユーザ入力スレッド、イベントループの3構成 |
| `ui.rs` | ターミナル制御。rawモード（セッション全体で維持）、ライン編集、パススルー、ANSI色、ステータスバー、ミニバッファ |
| `ai.rs` | Claude Code CLI連携。JSON Schema構造化レスポンス、セッション維持 (`--resume`)、ログ出力 |
| `config.rs` | TOML設定ロード |
| `pty_handler.rs` | portable-pty によるSSH / ローカルシェル起動。実端末サイズで起動し SIGWINCH で追従 |
| `update.rs` | セルフアップデート (`--update`)。GitHub Releases APIから最新バイナリをダウンロード |
| `ring_buffer.rs` | 1MBリングバッファ。ANSIエスケープ除去、差分送信 (`mark_sent` / `get_unsent`) |
| `mode.rs` | `Local` / `Remote` の2モード定義 |

---

## 2. 動作モード

| モード | 起動条件 | 挙動 |
|---|---|---|
| **Local** | SSH引数なし (`aish`) | `$SHELL`（未定義なら`/bin/bash`）をPTYで起動 |
| **Remote** | SSH引数あり (`aish user@host` 等) | `ssh` をPTYで起動。引数はそのままsshに渡す |

両モードとも `accepts_shell_command()` は true。終了は `exit` コマンド、または PTY プロセス終了。

---

## 3. コマンドラインオプション

| オプション | 意味 |
|---|---|
| `--version` / `-V` | バージョン表示して終了 |
| `--update` | GitHub Releases から最新バイナリをダウンロードして自己更新 |
| `--help` | ヘルプを表示して終了 |
| `--config <path>` | 設定ファイルのパスを指定（デフォルト `~/.aish/config.toml`）|
| `--ai <name>` | AIバックエンドを選択（built-in: `claude`/`codex`/`gemini`/`qwen`/`cursor`/`copilot`、または `[[ai.providers]]` の任意 `name`、既定 `claude`）。設定ファイル `[ai].backend` を上書きする。built-in 名は予約語で `[[ai.providers]]` 側に同名は登録不可 |
| `--model <name>` | 使用モデル名を指定（例: `sonnet`, `gpt-5`, `gemini-2.5-pro`）。`[ai].model` および各バックエンドの `extra_args` の `-m` より優先 |
| `--effort <level>` | reasoning effort レベル（例: `low`/`medium`/`high`）。claude → `--effort`、codex → `-c model_reasoning_effort=`、copilot → `--effort` に変換。gemini/qwen/cursor は CLI 非対応のため無視 |
| それ以外 | SSH引数としてそのまま `ssh` に渡す |

---

## 4. UI要素

### 4.1 ステータスバー
- ターミナル最下行に常時表示される1行。
- DECSTBM (`\x1b[1;{rows-1}r`) でスクロール領域を最下行を除く範囲に制限し、`\x1b[{rows};1H` に `aish v{version} | Ctrl+/ for AI` をラベル色で描画。
- PTY出力が50ms落ち着いたタイミングで再描画 (`resize_status_bar`)。シェル側のカーソルを壊さないよう `\x1b7`/`\x1b8`（DECSC/DECRC）で囲む。
- SIGWINCHでも同様に再設定する。
- 終了時には `\x1b[r`（スクロール領域解除）とステータスバー行クリア (`\x1b[2K`) を実行。

### 4.2 aishプロンプト（ミニバッファ）
- `Ctrl+/` (0x1F) で開く、ターミナル最下行のステータスバー行を置き換える入力欄。
- 表示: `[aish] ` ラベル（色付き）+ 入力テキスト。
- 入力中は `MINIBUFFER_ACTIVE` フラグが立ち、PTY出力の画面描画を抑制（リングバッファ記録は継続）。
- 確定時:
  - スクロール領域に `[aish] {text}` を **各論理行の先頭にラベルを付けて** エコー表示。
  - 履歴に追加（直前と同一なら追加しない）。
  - `InputEvent::AiPrompt(text)` をメインループへ送信。
- キャンセル経路: 単独ESC、Ctrl+C (0x03)、Ctrl+/ (0x1F)、入力が `exit` のままEnter。
- 空Enterは無操作（ステータスバーを復元するだけ）。
- 開く直前にシェル側コマンドを入力中だった場合（`at_line_start == false`）、キャンセル/確定時に `0x03` (Ctrl+C) をPTYに送り、部分入力を破棄。

### 4.3 マルチライン入力
- 入力長に応じてミニバッファが **縦方向に拡張** する。最大 `term_rows / 2` 行まで。
- `compute_visual_layout` が論理行と折り返しを計算:
  - 第1論理行の先頭はラベル（幅 `label_width`）分を差し引いた幅で折り返し。
  - 継続行（ソフトラップ / `\n` 後の新しい論理行）は `label_width` 分の空白インデントまたはラベルを付ける。
- DECSTBMを `rows_used` に応じて動的に `\x1b[1;{rows - rows_used}r` に調整。縮小時は不要になった行を `\x1b[2K` でクリア。
- 総可視行数が `max_rows` を超える場合、カーソル行が見える位置までスクロール (`scroll_top`)。

### 4.4 起動時の表示確保
- `setup_status_bar` 内で DECSTBMを設定する前に `\n` を1回出力し、ステータスバー1行分を確保。
- ターミナルタイトル: `\x1b]2;[aish] {ssh_args}\x07`（Localモードでは `[aish]`のみ）。終了時に空タイトルで復元。
- 通常動作中は PTY出力にaish独自の文字列を一切挿入しない（パススルーに徹する）。

### 4.5 スピナー
- AI応答待ち中にステータスバー行で点滅。
- フレーム: `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`、80msごとに更新。
- 表示: `{thinking_color}{frame} {thinking_message}\x1b[0m`（既定は `Thinking...`）。
- `\x1b7`/`\x1b8` でカーソルを保存・復元し、シェル入力欄を壊さない。
- `stop()` または Drop 時にステータスバーを再描画。

### 4.6 確認プロンプト
- AIが提案した `commands` を番号付きで全件表示（プラン提示）。
- 続けて各コマンドごとに `Execute [i/N]: {cmd} (Y/n) ` を `confirm_color` で表示し、ReadLineで個別に応答を受ける。
- 空Enter / `y` / `yes` （大小文字無視）を承認とみなす。それ以外は拒否してそのコマンドはスキップ、次のコマンドの確認へ進む。

---

## 5. キー入力

### 5.1 パススルーモード（通常シェル操作時）
| キー | 動作 |
|---|---|
| `Ctrl+/` (0x1F) | aishプロンプトを開く |
| それ以外 | PTYへ直送（Enter, Ctrl+C, Tab補完, Ctrl+L, Ctrl+R, 矢印キー, ESCシーケンス等すべて） |
| フォーカスイベント `\x1b[I` / `\x1b[O` | 破棄（PTYへ送らない） |
| UTF-8マルチバイト | 先頭バイトから長さ判定して全バイト読み取りPTYへ送信 |

Enter / Ctrl+C / 文字入力などいずれの場合も `passthrough_read_raw` は抜けず、`Ctrl+/` を受けるか PTY EOF までループを継続する。

### 5.2 aishプロンプト（ミニバッファ）
| キー | 動作 |
|---|---|
| `Enter` (`\r` / `\n`) | 確定。`exit` のみの入力ならキャンセル扱い |
| `Alt+Enter` (`\x1b\r` / `\x1b\n`) | 改行挿入 |
| `Shift+Enter` (CSI u `\x1b[13;Nu`、N=修飾ビット) | 改行挿入（ターミナル依存で届かないことあり） |
| `ESC` 単独 | キャンセル |
| `Ctrl+C` (0x03) | キャンセル |
| `Ctrl+/` (0x1F) | キャンセル |
| `Ctrl+D` (0x04) | 空ならキャンセル、そうでなければカーソル位置の文字を削除 |
| `BS` / `DEL` (0x08 / 0x7F) | カーソル左の文字を削除 |
| `Ctrl+A` (0x01) / `Home` | 行頭（全論理行の先頭）へ |
| `Ctrl+E` (0x05) / `End` | 行末（全論理行の末尾）へ |
| `Ctrl+B` (0x02) / `←` | カーソルを1文字左へ |
| `Ctrl+F` (0x06) / `→` | カーソルを1文字右へ |
| `Ctrl+U` (0x15) | カーソルより左をすべて削除 |
| `Ctrl+K` (0x0B) | カーソルより右をすべて削除 |
| `Ctrl+W` (0x17) | カーソル直前の単語（空白区切り）を削除 |
| `↑` / `↓` | プロンプト履歴ナビゲーション（新規入力は退避される） |
| `Delete` (`\x1b[3~`) | カーソル位置の文字を削除 |

### 5.3 ReadLineモード（確認プロンプト応答時）
- パススルーモードと同じrawモードで動作するが、矢印↑↓は履歴ナビゲーション、それ以外の編集キーは aishプロンプトと同等。
- `exit` 入力でaishを終了、それ以外は `UserInput::ShellCommand` としてPTYに送信。

### 5.4 Slash command（aishプロンプト内）

aishプロンプトで先頭が `/` の入力は AI に送らずローカルで処理する。各 backend の `AiBackend` trait のメソッドを呼び、結果を dim grey で表示。

| コマンド | 動作 |
|---|---|
| `/help` | 利用可能な slash command 一覧を表示 |
| `/effort [LEVEL]` | reasoning effort を runtime で変更（次回 send 以降に反映）。引数省略でクリア。gemini/qwen/cursor は CLI フラグが無いので保存のみで実リクエストに反映されない。claude/codex/copilot は native 反映 |
| `/model [NAME]` | モデルを runtime で変更（既存 session_id / history は維持）。引数省略でクリア |
| `/clear` | 会話履歴 / セッションをクリア。claude / codex / cursor / copilot / generic (native resume 設定時) は session_id を None に、gemini / qwen / generic (native resume 未設定時) は内部 history Vec を空にする |
| `/ai <NAME>` | AI バックエンドを切り替え（built-in `claude`/`codex`/`gemini`/`qwen`/`cursor`/`copilot` または `[[ai.providers]]` の `name`）。新しい backend を `create_backend` で構築し、現セッションは破棄される |
| `/<unknown>` | slash command として認識せず、入力をそのまま AI プロンプトに送る (例: `/root/test.txt` のようなファイルパス、`/foo bar` のような自然文も AI に届く) |

スラッシュコマンドはそれぞれ AI CLI 自身の対話モードで提供される `/<cmd>` とは独立に **aish 側で実装**されている（aish は CLI を非対話モードで起動するため、CLI 側の slash command は届かない）。

### 5.4 シグナル
| シグナル | ハンドラ | 動作 |
|---|---|---|
| `SIGWINCH` | `sigwinch_handler` | `SIGWINCH_RECEIVED` をセット |

メインループ側で非同期に消費する。SIGINT は独自に処理せず、OS デフォルトに委ねる（rawモードでは ISIG 無効のためキーボード Ctrl+C は SIGINT を発行しない）。

---

## 6. AI連携

aish は trait `AiBackend` を介して以下に対応する:

- 個別実装の **native backend 6 種**: **Claude Code / OpenAI Codex CLI / Google Gemini CLI / Alibaba Qwen Code / Cursor Agent CLI / GitHub Copilot CLI** (`src/ai/<name>.rs` に bespoke 実装)
- 設定駆動の **Generic CLI backend**: `[[ai.providers]]` に登録した任意の CLI を `--ai <NAME>` (registered `name` をそのまま) で使う。Rust コード変更なしで provider 追加可能 (`src/ai/generic.rs` の単一 driver が recipe を読んで動的に振る舞いを決定)。`name` は built-in 予約語と衝突不可

各バックエンドは JSON で `{message, commands[]}` 相当を返し、aish は提案ベースで動作する。CLI 非依存の原則 (透明性・サーバ無書き込み) は trait 実装側で守る責任を負う。

選択優先順位: `--ai` (CLI) > `[ai].backend` (設定) > `claude` (既定)。

### 6.0 バックエンド能力差

| 機能 | Claude | Codex | Gemini | Qwen | Cursor | Copilot |
|---|---|---|---|---|---|---|
| 実行ファイル | `claude` | `codex` | `gemini` | `qwen` | `cursor-agent` | `copilot` |
| 非対話モード | `claude -p` | `codex exec -` | `gemini` (stdin) | `qwen` (stdin) | `cursor-agent -p --trust` (stdin) | `copilot` (stdin、`-p` 不可) |
| プロンプト渡し | stdin | stdin | stdin | stdin | stdin | stdin |
| JSON 出力強制 | `--json-schema` | なし | なし | なし | `--output-format json` (外側ラッパのみ) | `--output-format json` (**JSONL** 形式) |
| 危険ツール無効化 | `--disallowedTools` | `-s read-only` + `--disable` 12 種 | system prompt のみ | system prompt のみ | `--mode plan` + `--sandbox <mode>` + system prompt | `--allow-all-tools --deny-tool=shell --deny-tool=write --no-ask-user --mode plan` (deny は allow に優先) |
| セッション再開 | `--resume <sid>` (JSON `session_id` 捕獲) | `exec resume <UUID>` (rollout ファイル名から UUID 捕獲) | best-effort (`--resume latest`) | best-effort (`--continue`) | `--resume <sid>` (JSON `session_id` 捕獲、claude と同形) | `--resume <sid>` (JSONL `result.sessionId` 捕獲) |
| reasoning effort | `--effort` (native) | `-c model_reasoning_effort=<level>` | なし | なし | なし | `--effort` (native, `none/low/medium/high/xhigh/max`) |
| 実装ファイル | `src/ai/claude.rs` | `src/ai/codex.rs` | `src/ai/gemini.rs` | `src/ai/qwen.rs` | `src/ai/cursor.rs` | `src/ai/copilot.rs` |

JSON Schema 強制が無いバックエンドは system prompt で `{"message":..., "commands":[...]}` 単独出力を強く指示し、`extract_json` で抽出する。失敗時は出力全体を `message` として `commands: []` でフォールバック。

**Claude / Codex / Cursor / Copilot** は CLI 側 session に履歴を委ねる。
- Claude: 初回 send で取得した `session_id` を保持し、2 回目以降 `--resume <sid>` で連結。
- Codex: 初回 send 後 `~/.codex/sessions/YYYY/MM/DD/rollout-...-<UUID>.jsonl` から UUID を捕獲し、
  2 回目以降 `codex exec resume <UUID>` で連結。`--ephemeral` は付けない。
- Cursor: 初回 send で取得した `session_id` (応答 JSON の `session_id` フィールド) を保持し、
  2 回目以降 `--resume <sid>` で連結。`--append-system-prompt` 相当が無いので system prompt は
  初回プロンプト先頭に焼き込む (resume 後は cursor-agent 側が記憶しているので再送しない)。
- Copilot: 出力 JSONL を行単位で走査し、最後の `assistant.message` 行の `data.content` を応答テキスト、
  `result` 行の `sessionId` を session UUID として捕獲。2 回目以降 `--resume <sid>` で連結。
  system prompt は cursor と同様、初回プロンプト先頭に焼き込む。

**Gemini / Qwen** は session resume 機構を非対話モードで安定して使えないため、各 backend 内部で
直近 8 ターン分の (user_prompt, ai_message) を履歴として保持し、毎回プロンプトに含めて再送する。
ターミナル差分 (ring buffer) と合わせることで、`mark_sent` 後でも multi-step ワークフローが文脈を保つ。

#### 安全性の差

- **Claude**: `--disallowedTools "Bash,Edit,Write,Read"` をフラグレベルでツール拒否。最も強力。
- **Codex**: `codex exec` は本来エージェント (内部でツールを呼ぶ) なので、aish の確認 UI を迂回しないよう
  ツール系 feature をすべて `--disable` で落として LLM のみに退化させる
  (`shell_tool` / `unified_exec` / `browser_use` / `computer_use` / `multi_agent` / `image_generation` /
  `tool_search` / `tool_suggest` / `plugins` / `apps` / `skill_mcp_dependency_install` /
  `tool_call_mcp_elicitation`)。さらに defense-in-depth として `-s read-only` sandbox を併用。
  この設定で codex は提案 JSON だけを返す純粋な LLM として動作する。
- **Copilot**: claude と同等の堅さ。`--allow-all-tools` (非対話モード必須) と
  `--deny-tool=shell` / `--deny-tool=write` (deny は allow に優先) で shell 実行とファイル書き込みを
  完全拒否。`--no-ask-user` で ask_user tool も無効化。さらに `--mode plan` (default) で
  read-only / planning モードに固定する四段構え。これで copilot は「LLM のみ」に退化する。
- **Gemini / Qwen**: フラグレベルの制約は無く、system prompt の「ツール禁止」指示のみ。
- **Cursor**: 個別ツール無効化フラグは無いが、`--mode plan` (read-only / planning モード、no edits) を
  既定で付与する。これは aish の「提案のみ、実行は aish 側で確認」セマンティクスと方針が一致する
  安全プリミティブ。さらに defense-in-depth として `[ai.cursor].sandbox` で `--sandbox enabled` を
  渡せる (OS レベルサンドボックス)。`--trust` は headless モードで必須のため毎回自動付与する。
  ツール抑制最終層として system prompt の「ツール禁止」指示も載せる (gemini / qwen と同じ best-effort)。
- 最大限の安全性が必要な場合は `--ai claude` か `--ai copilot` を使うこと。

### 6.1 起動
- aish起動時に選択されたバックエンドのバイナリ (`claude`/`codex`/`gemini`/`qwen`/`cursor-agent`) を `--version` で確認し、失敗なら「Please install ...」を表示して終了。Claude の場合のみインストールコマンドも併せて表示。実行ファイル名は `BackendKind::binary()` が返す (cursor のみ `cursor-agent`、他は `as_str()` と同じ)。

### 6.2 初回リクエスト
```
claude -p \
  --append-system-prompt "{system_prompt} コマンドを提案してください。直接実行しないでください。1度のレスポンスで提案するコマンドは1つだけにしてください。複数のステップが必要な場合は、実行結果を確認してから次のコマンドを提案してください。&&や||による条件付き実行は1つのコマンドとして維持してください。" \
  --output-format json \
  --disallowedTools "Bash,Edit,Write,Read" \
  --json-schema <AI_RESPONSE_SCHEMA> \
  "<prompt>"
```

### 6.3 2回目以降
```
claude -p --resume <session_id> \
  --output-format json \
  --disallowedTools "Bash,Edit,Write,Read" \
  --json-schema <AI_RESPONSE_SCHEMA> \
  "<prompt>"
```

`session_id` はClaude CLIのJSON出力 `session_id` フィールドから取得、以降保持。
`--disallowedTools` は安全制約として毎回明示。`--append-system-prompt` は append 動作のため初回のみ付与する（resume では既存セッションのシステムプロンプトを再利用）。

### 6.4 JSON Schema
```json
{
  "type": "object",
  "properties": {
    "message": { "type": "string" },
    "commands": { "type": "array", "items": { "type": "string" } }
  },
  "required": ["message", "commands"]
}
```

### 6.5 プロンプト組み立て
```
```terminal
{リングバッファのマーク以降の内容（ANSI除去済み）}
```

{ユーザ入力プロンプト}
```

リングバッファが空なら `terminal` フェンスは付けずプロンプトのみ送る。

### 6.6 コマンド実行ループ
1. AIレスポンスの `message` を `ai_color` で表示。
2. `commands` が空なら対話終了。
3. `commands` を番号付きで全件表示（プラン提示）。通常はシステムプロンプトの制約（6.2 参照）により1件のみ返るが、AIが複数件返した場合も以降の処理で1件ずつ確認する。
4. **各コマンドを1つずつ** `Execute [i/N]: {cmd} (Y/n)` で確認し、承認されたものは **そのまま** `<cmd>\n` として PTY に送信する。コマンドを変形・ラップしない（**透明性が信頼の根幹**: ユーザが画面で承認した文字列 = サーバで実行される文字列）。
5. **完了待ちループ**は約 20ms 周期で以下を並行処理する:
   - PTY 出力ドレイン（画面表示 + リングバッファ追記 + `PromptSniffer.feed()`）。
   - `stdin → PTY` 転送（ノンブロッキング poll で fd 0 を直読）。実行中コマンドへのキー入力（パスワード入力・対話プロンプト応答）と Ctrl+C による中断（PTY 経由でシェルが SIGINT を発行）が可能。
   - SIGWINCH 検知（端末リサイズ追従）。
   - 完了判定: **`PromptSniffer.matches_prompt()` が真 + 200ms 静音** で完了。
6. すべて拒否された場合（1つも実行されなかった場合）は対話終了。
7. 少なくとも1つ実行した場合、followup プロンプトに各コマンドの実行サマリ（`` `cmd` ``）を含めて AI へ送信し、出力本体は `terminal` フェンスでリングバッファから渡す。
8. 2へ戻る（空提案でループ終了）。
9. ループ終了後、PTYに `\n` を送信してシェルプロンプトを再描画。

`PromptSniffer` は PTY 出力末尾を ANSI 除去後 256 バイト保持し、最後の行が `[終端文字][空白]+` で終わるかを構造的に判定する。既定の終端文字は `$ # > % ➜ ❯ »`（`:` は ssh password prompt 等で誤検出するため除外）。検出成功時に `record_match()` が呼ばれ、観察された終端文字を学習セットに加える。**多段 SSH** や `sudo bash` で PS1 が動的に変わっても、終端構造が共通なので追従できる。

**サーバ側に対して何の書き込みも注入もしない**（`PROMPT_COMMAND` の改変、history 抑制、shell 統合シーケンスの自動セットアップ等は一切行わない）。完了判定は純粋に PTY 出力の passive 観察のみで行う。代償として exit code は取得できないが、AI は出力テキストから成否を推測する。

#### 既知の制約
- カスタムテーマ (oh-my-zsh robbyrussell の `➜  ~ ` 等、末尾が既定セット外の文字) は最初の検出で見逃す。`record_match` で学習させるか、設定で終端文字を追加する想定（後者は今後）。
- `tail -f` 等の連続出力コマンドはプロンプトに戻らないので完了判定されない。ユーザ Ctrl+C で抜ける運用。
- 出力途中に偽プロンプト風文字列が出ても、200ms 静音条件で大半救済される。

### 6.7 キャンセル
- AIプロセス実行中、stdinをノンブロッキングpollして `0x03` 検知で `child.kill()`。エラー `"Cancelled"` として扱い、`^C` を表示して対話終了。

### 6.8 セッション再開コマンド表示
- aish 終了時、`AiBackend::resume_command()` が `Some(cmd)` を返す場合 stderr に `Resume this <kind> session with:\n  <cmd>` を出力する。
  - claude: `claude --resume <UUID>` (session_id が捕獲できた場合)
  - codex:  `codex resume <UUID>` (rollout ファイルから UUID が捕獲できた場合)
  - gemini: `gemini --resume latest` (best-effort、1 ターン以上会話があれば)
  - qwen:   `qwen --continue` (best-effort、1 ターン以上会話があれば)
  - cursor: `cursor-agent --resume <UUID>` (応答 JSON の `session_id` を捕獲できた場合)
  - copilot: `copilot --resume <UUID>` (JSONL `result.sessionId` を捕獲できた場合)
  - generic: `<binary> <resume_flag> <sid>` (recipe.resume_flag が設定済み + session_id 捕獲済みのとき)
- gemini / qwen は非対話モードでの session 永続化が CLI 仕様として保証されていないため、表示はしてもコマンド実行で aish の会話が読み戻せないことがある。
- cursor / copilot は `--resume` が非対話モードでも安定動作することを実機で確認済み (copilot は cache token も効く)。

### 6.9 JSON抽出
- Claude CLIの出力にJSON前後のテキストが混じる可能性に対応し、`extract_json` で最外の `{...}` をバランス解析で抽出。
- `structured_output` があればそれを、なければ `result` をレスポンスボディとして採用。
- `result` が文字列の場合も JSON としてパース試行、失敗したら `message: <そのまま>, commands: []` にフォールバック。

### 6.10 ログ
- `[log]` セクションで `enabled = true` 時、`claude {args}` / レスポンス本文 / `[stderr] ...` を `path`（既定 `~/.aish/logs/claude-code.log`）に追記。
- 各エントリは `=== YYYY-MM-DD HH:MM:SS ===` のタイムスタンプヘッダ付き。ローカルTZ（`libc::localtime_r`）で計算。

---

## 7. リングバッファ

- 固定1MB、書き込み位置 / 未送信位置（`sent_pos`）を保持。
- `append(data)`: `strip_ansi_escapes::strip` でANSI除去してから格納。
- `get_unsent()`: `sent_pos` 以降の内容を `String::from_utf8_lossy` で返す。
- `mark_sent()`: AIレスポンス取得成功時に呼び、次回のコンテキストに含めない。
- バッファ満杯時、未送信長がcapacityを超えるなら `sent_pos = 0` にリセット（古いデータも含めて最新1MB分を送る）。

---

## 8. スレッド構成

### 8.1 PTY読み取りスレッド
- `pty_reader.read(&mut buf[0u8; 4096])` をループ。受信データを `pty_tx` 経由でメインへ送信。
- EOF / エラー時に `alive_tx.send(())` を送信。

### 8.2 入力スレッド
- `prompt_rx` から `InputRequest::Passthrough` / `ReadLine` を受け取り、対応する読み取りを実行。
- `Passthrough`: `passthrough_read` → `InputEvent::PassthroughEnded` で完了通知。
- `ReadLine`: `read_line` の結果を `InputEvent::Line` で送信。

### 8.3 メインループ
- 約1ms ポーリングで以下を順に処理:
  1. SIGWINCH検知→PTYリサイズ＆ステータスバー再描画
  2. PTY出力ドレイン（`minibuffer_active()` ならstdout描画を抑制）
  3. PTY出力50ms落ち着いたらステータスバー再描画
  4. 入力スレッドがidleかつ同50ms条件でリクエスト送信
  5. PTYプロセス終了検知
  6. 入力イベント処理

---

## 9. 入力イベント管理

| フラグ / 状態 | 役割 |
|---|---|
| `pending_input` | 入力リクエストを次の安定点で送るべきか |
| `input_idle` | 入力スレッドが `prompt_rx.recv()` で待機中か（キュー重複防止） |
| `MINIBUFFER_ACTIVE` | ミニバッファ表示中（PTY出力の画面描画を抑制） |
| `SIGWINCH_RECEIVED` | 端末リサイズ要求 |
| `TERM_ROWS` | 現在の端末高さキャッシュ（ステータスバー・スピナー用） |

### 備考
- AI対話が終了してパススルーへ戻る直前、確認プロンプトのReadLineで `input_idle` が false になっているため、メインループ側で **明示的に `input_idle = true` に戻す**。これを忘れると入力リクエストが再送されずにハングする。
- `Ctrl+/` 受信時は `InputEvent::PassthroughEnded` がメインループへ届き、`input_idle = true` に戻して次の入力リクエスト（AiPrompt用のミニバッファ呼び出し）を発行可能にする。

---

## 10. ターミナル制御

### 10.1 termios
- `save_terminal_settings` で起動時の `termios` を保存し、同時にrawモード (`ICANON | ECHO | ISIG` を解除、`VMIN=1, VTIME=0`) に設定。
- rawモードは **セッション全体で維持**。個別の `read_line` / `passthrough` では再設定しない。
- `restore_terminal_settings` で終了時に元の状態に戻す。

### 10.2 ANSIエスケープ
- DECSTBM `\x1b[{top};{bottom}r`: スクロール領域。ステータスバー常時表示とミニバッファ拡張に使用。
- DECSC/DECRC `\x1b7` / `\x1b8`: カーソル位置の保存・復元。シェル側の入力位置を保全。
- CUP `\x1b[{row};{col}H`: カーソル位置指定。
- EL `\x1b[K` / `\x1b[2K`: 行末までクリア / 行全体クリア。
- SGR `\x1b[0m` + ユーザ設定色（前景・背景、256色・TrueColor対応）。

### 10.3 可視幅計算
- `visible_width(s)`: `strip_ansi_escapes::strip` でANSIを除いた上で `UnicodeWidthChar::width` を合算。全角=2、半角=1、制御文字=0。
- ミニバッファのラベル幅算出、折り返し計算、BS時の消去幅計算に使用。

---

## 11. 設定ファイル (`~/.aish/config.toml`)

TOML形式。未指定フィールドはデフォルト値。

### 11.1 トップレベル
| キー | 型 | 既定値 | 説明 |
|---|---|---|---|
| `system_prompt` | string | `"あなたはLinuxサーバ管理の専門家です。SSHセッションの内容を把握しています。"` | AIのシステムプロンプト |
| `language` | string | `"Japanese"` | 空文字以外なら `Respond in {language}.` をシステムプロンプトに付加 |

### 11.2 `[display]`
| キー | 既定値 | 用途 |
|---|---|---|
| `shell_prefix_label` | `[aish]` | ターミナルタイトル先頭 |
| `header_color` | `\x1b[38;5;208m` | ステータスバー色 |
| `prompt_label` | `[aish]` | aishプロンプトラベル |
| `prompt_color` | `\x1b[38;5;208;48;2;50;35;20m` | aishプロンプトの前景＋背景色 |
| `thinking_message` | `Thinking...` | スピナーメッセージ |
| `thinking_color` | `\x1b[38;5;208m` | スピナー色 |
| `ai_color` | `\x1b[38;5;216m` | AIレスポンス色 |
| `input_color` | `""` | ミニバッファ入力テキストの背景色 |
| `confirm_color` | `\x1b[38;5;228;48;5;239m` | `Execute? (Y/n)` の色 |

### 11.3 `[log]`
| キー | 既定値 | 説明 |
|---|---|---|
| `enabled` | `false` | ログ出力有効化 |
| `path` | `~/.aish/logs/claude-code.log` | ログファイルパス（`~/` はホーム展開） |

### 11.4 `[ai]`
| キー | 既定値 | 説明 |
|---|---|---|
| `backend` | `"claude"` | 使用する AI CLI (`claude`/`codex`/`gemini`/`qwen`/`cursor`/`copilot`)。`--ai` で上書き可能 |
| `model` | `""` | モデル名（例: `sonnet`, `gpt-5`）。空ならバックエンド CLI 既定。`--model` で上書き可能 |
| `effort` | `""` | reasoning effort レベル。claude → `--effort`、codex → `-c model_reasoning_effort=`、copilot → `--effort` (native) に変換。gemini/qwen/cursor は無視。`--effort` で上書き可能 |
| `system_prompt` | `""` | 空ならトップレベル `system_prompt` にフォールバック |
| `language` | `""` | 空ならトップレベル `language` にフォールバック |

#### `[ai.claude]`
| キー | 既定値 | 説明 |
|---|---|---|
| `disallowed_tools` | `"Bash,Edit,Write,Read"` | claude CLI に渡す `--disallowedTools` の値 |
| `extra_args` | `[]` | claude CLI に追加で渡す引数（先頭からの位置はビルトイン引数の後ろ） |

#### `[ai.codex]` / `[ai.gemini]` / `[ai.qwen]`
| キー | 既定値 | 説明 |
|---|---|---|
| `extra_args` | `[]` | 各 CLI への追加引数 (例: `["-m", "gpt-5.5"]`)。aish ビルトイン引数の後ろに追記される |

#### `[ai.cursor]`
| キー | 既定値 | 説明 |
|---|---|---|
| `extra_args` | `[]` | `cursor-agent` への追加引数。aish ビルトイン引数 (`-p --output-format json --trust`、`--mode <m>`、`--sandbox <s>`、`--resume <sid>`) の後ろに追記される |
| `mode` | `"plan"` | `--mode <value>` に渡す値 (`"plan"` / `"ask"` / `""`)。`"plan"` は read-only / propose-only の cursor-agent モードで aish の用途に合致する安全側既定。`""` で `--mode` を付けない (= 通常モード、危険) |
| `sandbox` | `""` | `--sandbox <value>` に渡す値 (`"enabled"` / `"disabled"`)。空 / 未指定なら `--sandbox` を付けない (cursor-agent 既定に従う)。defense-in-depth |

`--trust` は cursor-agent headless モードで必須のため、aish が常に自動付与する (config からは指定不可)。
未指定だと `Workspace Trust Required` で実行が拒否される。

Free プランの cursor-agent では Named models が使えず `auto` のみ指定可能なので、無料アカウントでは
`[ai].model = "auto"` または起動時 `--model auto` を指定する。

#### `[ai.copilot]`
| キー | 既定値 | 説明 |
|---|---|---|
| `extra_args` | `[]` | `copilot` への追加引数。aish ビルトイン引数の後ろに追記される (例: `["--disable-builtin-mcps"]`) |
| `mode` | `"plan"` | `--mode <value>` に渡す値 (`"plan"` / `"interactive"` / `"autopilot"` / `""`)。`"plan"` は read-only / propose-only モードで aish の用途に合致する安全側既定 |

以下は信頼の根幹に直結するため aish が常に自動付与する (config からは指定不可):

- `--output-format json` (JSONL 出力で session_id / assistant text を確実に取り出す)
- `--allow-all-tools` (非対話モードで必須)
- `--deny-tool=shell` / `--deny-tool=write` (shell 実行とファイル書き込みを完全拒否; deny は --allow-all-tools に優先)
- `--no-ask-user` (会話は aish が仕切る前提で copilot 側からの user 質問を抑止)

認証は `gh auth login` か `copilot login`、もしくは `COPILOT_GITHUB_TOKEN` / `GH_TOKEN` /
`GITHUB_TOKEN` env のいずれか。所属組織の Copilot ポリシーで CLI 利用が許可されている必要がある
(`Access denied by policy settings` が出る場合は組織側の許可が必要)。

#### `[[ai.providers]]` (Generic CLI backend のレシピ配列)

`src/ai/generic.rs::GenericCliBackend` が読む config 駆動レシピ。`--ai generic:<NAME>` /
`/ai generic:<NAME>` でアクティブ化する。配列なので複数同時に登録可能 (上限 256 個)。

| キー | 既定値 | 説明 |
|---|---|---|
| `name` | (必須) | provider 一意識別子 (`/ai generic:<NAME>` の `<NAME>` 部分) |
| `binary` | (必須) | 実行ファイル名 (PATH 検索) または絶対パス |
| `args` | `[]` | 固定引数。aish が動的引数 (resume/model/effort/prompt-as-flag) を後ろに追加する |
| `prompt_delivery` | `"stdin"` | `"stdin"` / `"arg"` (positional 末尾) / `"flag"` (`prompt_flag` の値として渡す) |
| `prompt_flag` | `""` | `prompt_delivery="flag"` のとき必須 (例 `"-p"`) |
| `parse` | `"lossy"` | `"lossy"` / `"extract_json"` / `"jsonl"` |
| `jsonl_content_path` | `""` | `parse="jsonl"` のとき `"type:dot.path"` 形式で最終応答テキストのパスを指定 |
| `jsonl_session_path` | `""` | `parse="jsonl"` のとき同形式で session_id 用 |
| `session_id_path` | `""` | `parse="extract_json"` のとき抽出 JSON 内の session_id フィールド名 (top-level key) |
| `resume_flag` | `""` | session_id 捕獲時の resume 引数 (例 `"--resume"`)。空 + session_id_path 空なら native resume なし |
| `model_flag` | `""` | model 引数名 (例 `"--model"` / `"-m"`)。空なら model 渡しなし |
| `effort_flag` | `""` | reasoning effort 引数名。空なら保存のみで実リクエストに反映しない |
| `color` | `208` | 256-color (`/ai/<name>` ラベル・banner 色) |
| `system_prompt_inline` | `true` | `true`: 初回プロンプト先頭に焼き込む。`false`: 毎回プロンプトを history + system + context で再構築 |
| `history_turns` | `8` | native resume 無効時に内部保持する (user, ai) ターン数 |

起動時に `AiConfig::validate_providers()` で以下を検証:
- 配列長 <= 256
- `name` 一意
- `name` が built-in 予約語 (claude/codex/gemini/qwen/cursor/copilot) と衝突しないこと
- `parse` / `prompt_delivery` 値の妥当性
- `prompt_delivery="flag"` のとき `prompt_flag` が非空

不正があれば config 読み込みエラーで起動拒否する (`Invalid [[ai.providers]] in <path>: ...`)。
予約語は `BackendKind::all_native()` から導出するので、native backend を増やしたら自動的に予約語にも加わる。

**安全性**: aish 側からは native backend (claude / copilot 等) と違って `--deny-tool` 相当の
強制フラグは付けない。利用者が `args` に `--mode plan` / `--sandbox` 等を明示的に含める想定。
**信頼できる CLI のみ登録すること**。aish の確認 UI (提案コマンドの Y/n) は generic backend でも
同じく機能するが、CLI 側が独自にツール実行を始める可能性は config 著者の責任で抑える。

**メモリ**: `[[ai.providers]]` 各エントリは起動時に `Box::leak` され `&'static str` 化される。
プロセス全期間で生存するため、reload 機構はサポートしていない (aish 再起動で反映)。
ordinal は `6 + index` (native の後ろに連番) で、ring_buffer の sent_marks HashMap キーに使う。

トップレベル `system_prompt` / `language` は後方互換のため残す。`[ai]` セクションが省略されたり
そのフィールドが空文字なら、トップレベルの値が `[ai]` 側にコピーされる。

---

## 12. セルフアップデート (`--update`)

1. `std::env::consts::ARCH` で対応ターゲットを決定（`x86_64-unknown-linux-musl` / `aarch64-unknown-linux-musl`）。他は拒否。**`detect_target()` は OS を見ず ARCH のみで分岐し、常に linux-musl 名を返す**。そのため**macOS では `--update` は未対応**（release.yml は `*-apple-darwin` バイナリも配布するが、self-update は Linux musl 名でアセットを探しに行くので機能しない）。macOS で self-update を有効化するなら `detect_target()` に `std::env::consts::OS` 分岐を足し、darwin アセット名を返すよう拡張すること。
2. `curl` で `https://api.github.com/repos/tryandhappy/aish/releases/latest` を叩いて `tag_name` を取得。
3. 現バージョンと一致したら `"Already up to date."` で終了。
4. `aish-{target}` を一時ファイルへダウンロード。
5. **SHA256 チェックサム検証**:
   - 同じリリースから `aish-{target}.sha256` を取得（`sha256sum` 形式: `<64-hex>  <filename>`）。
   - ローカルで `sha256sum` コマンドにより一時ファイルのハッシュを計算。
   - 一致しない場合は一時ファイルを削除してエラー終了（インストールは行わない）。
   - リリース側で `.sha256` が公開されていない場合もエラー終了（fail-closed）。
6. `chmod 0755` → 現在の実行ファイルパスへ `rename`（クロスFS時は `copy` + 一時削除）。
7. 成功時 `"Updated to v{latest}"` 表示。

CIワークフロー（`.github/workflows/release.yml`）側で `aish-{target}.sha256` を生成し、リリースアセットとして公開する。release.yml の build matrix は Linux musl 2種（cross + deb/rpm）に加え macOS darwin 2種（`x86_64-apple-darwin` / `aarch64-apple-darwin`、cargo ビルド・tar.gz と生バイナリのみ）を作る。macOS ランナーには `sha256sum` が無いため checksum は `shasum -a 256` で生成するが、出力形式は `<64-hex>  <filename>`（空白2つ区切り）で `sha256sum` と同一なので self-update のパーサと互換。

---

## 13. エラー時の挙動

| 状況 | 挙動 |
|---|---|
| claude 未インストール | 起動時エラー表示＋`exit 1` |
| 設定ファイルパースエラー（デフォルトパス） | 警告を出して `Config::default()` で続行 |
| 設定ファイル読み込みエラー（デフォルトパス） | 同上 |
| 設定ファイルパース／読み込みエラー（`--config` 明示） | エラー終了（`exit 1`） |
| `--update` SHA256 検証失敗 | 一時ファイルを削除してエラー終了 |
| `--update` `.sha256` 取得失敗 | fail-closed でエラー終了 |
| claude 実行失敗 (非ゼロ終了) | `AI error: ...` 表示してループ継続 |
| claude 出力が空 | `claude returned empty output` でエラー |
| claude 出力にJSONなし | `No JSON found in claude output: ...` |
| AIキャンセル (Ctrl+C中) | `^C` 表示後、対話ループ終了。aishは継続 |
| PTY終了 | 残り PTY 出力（logout メッセージ等）を表示してから aish 終了 |

---

## 14. 既知の制約

- **Shift+Enterによる改行**: kitty keyboard protocol (`\x1b[>1u`) を有効化しないと届かない。有効化するとEnter/Esc/BSなど他のキーも別形式になり、既存ハンドラと不整合が起きる。ターミナル横断で安定動作しないため**非対応**。改行は `Alt+Enter` を使う。
- **Windows**: `pty_handler` は portable-pty で対応しているが、`save_terminal_settings` 等のUI部はUnix限定。Windowsビルドは `read_line_cooked` フォールバックのみ。
- **リングバッファのUTF-8境界**: `String::from_utf8_lossy` でマルチバイトが切れていたら置換文字になる。
