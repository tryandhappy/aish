# aish 仕様書

CLI SSH + AI (Claude Code) ツール。クライアント側のClaude Codeから、ローカルシェルまたはSSH接続先サーバを調査・操作するための対話型UI。

---

## 0. 用語（各部の名称）

- **パススルーモード**: 通常のシェル操作状態。キー入力はPTYにそのまま転送される。
- **aishプロンプト**（ミニバッファ）: `Ctrl+/` で表示される `[aish]` 入力欄。ターミナル最下行（ステータスバー行）に表示され、AIへの質問を入力する。ESC / Ctrl+C / Ctrl+/ でキャンセル。
- **ステータスバー**: 最下行に常時表示される `aish v{version} | Ctrl+/ for AI` 行。DECSTBMスクロール領域外に固定表示。
- **スピナー**: AI応答待ち中にステータスバー行で回転するアニメーション（`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` + `Thinking...`）。
- **確認プロンプト**: AIが提案したコマンドの実行可否を問う `Exec? {cmd} [Y/n/a/q]` 表示。
- **ReadLineモード**: AI対話中の確認プロンプト応答など、ライン編集付きで入力を受け付ける状態。

---

## 1. アーキテクチャ（ファイル構成）

| ファイル | 役割 |
|---|---|
| `main.rs` | メインループ。PTY読み取りスレッド、ユーザ入力スレッド、イベントループの3構成。slash command 処理 |
| `conversation.rs` | AI 対話 1 セッションの制御フロー (`AiConversation::run`)。打ちかけ消去 → send → Y/n/a 確認 (`ConfirmDecision` / `Approval`) → 実行 → 完了 passive 検出 → follow-up → 終了 refresh |
| `ui.rs` | ターミナル制御。rawモード（セッション全体で維持）、ライン編集、パススルー、ANSI色、ミニバッファ |
| `input.rs` | 低レベル端末入力の framing (唯一の場所)。`ByteSource` → `next_event` → `Tok` |
| `input_gate.rs` | パススルー入力スレッド再開の 3 状態管理 (`InputGate` + `rearm_on_drop` RAII guard) |
| `ai/` | AI backend 層。`AiBackend` trait + claude/codex/gemini/qwen/cursor/copilot/generic の各実装、factory、共通 prompt/spawn (`common.rs`) |
| `config.rs` | TOML設定ロード |
| `pty_handler.rs` | portable-pty によるSSH / ローカルシェル起動。実端末サイズで起動し SIGWINCH で追従。`kill_line` / `refresh_prompt` / `send_approved_command` |
| `pty_drain.rs` | PTY 出力吸い出しの一元化 (`drain_pty` + `DrainOpts`)。表示方針・先頭改行 trim・sniffer 連携 |
| `prompt_sniffer.rs` | シェルプロンプト復帰の passive 検出 (終端文字学習つき) |
| `vetted_command.rs` | AI 提案コマンドの制御文字検証 newtype (`VettedCommand`)。「承認した物 = 実行される物」の型保証 |
| `update.rs` | セルフアップデート (`--update`)。GitHub Releases APIから最新バイナリをダウンロード |
| `ring_buffer.rs` | 1MBリングバッファ。ANSIエスケープ除去、backend 別差分送信、AI 注釈記録 (`record_ai_exchange`) |
| `mode.rs` | `Local` / `Remote` の2モード定義 |

---

## 2. 動作モード

| モード | 起動条件 | 挙動 |
|---|---|---|
| **Local** | SSH引数なし (`aish`) | `$SHELL`（macOSは通常zsh、未定義なら`/bin/bash`）をPTYで起動 |
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
- 伸長時は cursor を実画面最下行に置いた LF の **全画面スクロール** で行を確保する（DECSTBM の scroll region は使わない。region が全画面でないスクロールは押し出された行を scrollback に保存せず破棄するのが xterm 系端末の標準挙動のため）。押し出された行は通常のシェル出力と同様 scrollback に退避される。
- 縮小時は不要になった行を `\x1b[2K` でクリア（スクロールは戻さない）。スクロール確保済み行数は高水位マーク `reserved_rows`（縮小でも減らさない）で追跡し、縮小→再伸長では高水位を超えるまで再スクロールしない。
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
- 続けて各コマンドごとに `Exec? {cmd} [Y/n/a/q] ` を `confirm_color` で表示し、1 キー即確定（`read_confirm_key`）で個別に応答を受ける。最後（または単一）のコマンドでは `[Y/n]`。
- キーの意味は § 6.6 step 4 を参照（`Y`/Enter/Space=実行, `n`/`ESC`=1 回スキップ, `a`=残り自動承認, `q`=残り中止+AI follow-up, `Ctrl+C`/`Ctrl+D`=残り中止+AI 問い合わせなし）。

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
4. **各コマンドを1つずつ** `Exec? {cmd} [Y/n/a/q]` で確認し、承認されたものは **そのまま** `<cmd>\n` として PTY に送信する。コマンドを変形・ラップしない（**透明性が信頼の根幹**: ユーザが画面で承認した文字列 = サーバで実行される文字列）。各キーの意味:
   - `Y`/Enter/Space: このコマンドを実行。
   - `n`/`ESC`: このコマンド 1 回分だけスキップして次へ（ESC は n と同じ）。
   - `a`: このコマンドを実行し、以降の残りも自動承認。
   - `q`: 残りを中止。1 つでも実行済みなら AI に follow-up、1 つも実行していなければ通常プロンプトに戻る。
   - `Ctrl+C`/`Ctrl+D`: 残りを中止し、**実行有無に関わらず AI に問い合わせない**。
   - 残コマンドが無い最後（または単一）のコマンドでは `a`/`q` を畳んで `[Y/n]` 表示。
5. **完了待ちループ**は約 20ms 周期で以下を並行処理する:
   - PTY 出力ドレイン（画面表示 + リングバッファ追記 + `PromptSniffer.feed()`）。
   - `stdin → PTY` 転送（ノンブロッキング poll で fd 0 を直読）。実行中コマンドへのキー入力（パスワード入力・対話プロンプト応答）と Ctrl+C による中断（PTY 経由でシェルが SIGINT を発行）が可能。
   - SIGWINCH 検知（端末リサイズ追従）。
   - 完了判定: **`PromptSniffer.matches_prompt()` が真 + 200ms 静音** で完了。
6. すべて拒否された場合（1つも実行されなかった場合）、または `Ctrl+C`/`Ctrl+D` で中止した場合は AI に問い合わせず対話終了。
7. 少なくとも1つ実行し、かつ `Ctrl+C`/`Ctrl+D` 以外で抜けた場合、followup プロンプトに各コマンドの実行サマリ（`` `cmd` ``）を含めて AI へ送信し、出力本体は `terminal` フェンスでリングバッファから渡す（`q` 中止時は「残りを中止した」旨の文面に切り替わる）。
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
- DECSTBM `\x1b[r`: ミニバッファ終了時の防御的フルリセットのみ（aish は scroll region を設定しない。region 内スクロールは scrollback に行が残らないため、ミニバッファ拡張は全画面スクロールで行う）。
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

1. `detect_target()` が `std::env::consts::{OS, ARCH}` の組で対応ターゲットを決定する（`target_for(os, arch)` 純関数に委譲）。マッピング: linux/x86_64 → `x86_64-unknown-linux-musl`、linux/aarch64 → `aarch64-unknown-linux-musl`、macos/x86_64 → `x86_64-apple-darwin`、macos/aarch64 → `aarch64-apple-darwin`。それ以外は `Unsupported platform: {os}/{arch}` で拒否。`OS`/`ARCH` はコンパイル時にビルドターゲットへ固定される定数なので、各バイナリは自分のプラットフォームを正しく自己申告する（Rust の ARCH は Apple Silicon でも `aarch64`）。
2. `curl` で `https://api.github.com/repos/tryandhappy/aish/releases/latest` を叩いて `tag_name` を取得。
3. 現バージョンと一致したら `"Already up to date."` で終了。
4. `aish-{target}` を一時ファイルへダウンロード。
5. **SHA256 チェックサム検証**:
   - 同じリリースから `aish-{target}.sha256` を取得（`sha256sum` 形式: `<64-hex>  <filename>`）。
   - ローカルで一時ファイルのハッシュを計算。**`sha256sum` を先に試し、spawn 失敗 (= macOS に `sha256sum` が無い) なら `shasum -a 256` にフォールバック**。出力形式は両者同一なので `parse_sha256_hash` で共通に扱える。
   - 一致しない場合は一時ファイルを削除してエラー終了（インストールは行わない）。
   - リリース側で `.sha256` が公開されていない場合もエラー終了（fail-closed）。
6. `chmod 0755` → 現在の実行ファイルパスへ `rename`（クロスFS時は `copy` + 一時削除）。書き込み先は `current_exe()` なのでインストール場所に依存せず、macOS でも実行中バイナリを置換できる（旧 inode は実行中プロセスが保持）。
7. 成功時 `"Updated to v{latest}"` 表示。

インストール先の規約: **手動インストール / self-update は `/usr/local/bin/aish`**（全 OS 共通。FHS でパッケージ管理外ソフトの正規の場所、かつ macOS の SIP で `/usr/bin` に書けない問題も回避。PATH 優先度も `/usr/bin` より高い）。一方 **deb/rpm パッケージの dest は `/usr/bin/aish`**（`Cargo.toml` の `[package.metadata.deb]`）で、こちらはパッケージマネージャ管理下なので FHS 上 `/usr/bin` が正しい。両者は意図的に置き場が異なる。

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
| AI CLI 実行失敗 (非ゼロ終了) | `[{ai}] AI CLI failed: ...` と `Please check your login or usage limit.` を表示してループ継続 |
| claude 出力が空 | `claude returned empty output` でエラー |
| claude 出力にJSONなし | `No JSON found in claude output: ...` |
| AIキャンセル (Ctrl+C中) | `^C` 表示後、対話ループ終了。aishは継続 |
| PTY終了 | 残り PTY 出力（logout メッセージ等）を表示してから aish 終了 |

---

## 14. 既知の制約

- **Shift+Enterによる改行**: kitty keyboard protocol (`\x1b[>1u`) を有効化しないと届かない。有効化するとEnter/Esc/BSなど他のキーも別形式になり、既存ハンドラと不整合が起きる。ターミナル横断で安定動作しないため**非対応**。改行は `Alt+Enter` を使う。
- **Windows**: `pty_handler` は portable-pty で対応しているが、`save_terminal_settings` 等のUI部はUnix限定。Windowsビルドは `read_line_cooked` フォールバックのみ。
- **リングバッファのUTF-8境界**: `String::from_utf8_lossy` でマルチバイトが切れていたら置換文字になる。
- **シェル互換性**: aish は **readline / emacs 互換の行編集を持つ対話シェル** を前提とする。bash と zsh の emacs モード (= macOS のデフォルト) が該当し、macOS では `$SHELL`（=zsh）がそのまま起動して追加対応なしで動作する。打ちかけ入力の消去に使う `Ctrl+A`+`Ctrl+K` (`0x01,0x0b`) は emacs 行編集に依存するため、**zsh を vi モード (`bindkey -v`) にしている場合のみ** 劣化し、折り返した打ちかけが綺麗に畳まれず `^A^K` がリテラルで残ることがある（vim 等の TUI 子プロセスへ届いたときと同じ穏当な失敗モードで、クラッシュや「承認していないコマンドの実行」には至らない）。プロンプト戻り検出は `%`（zsh 一般ユーザ）/ `#`（root）を終端集合に含む（§ 9 参照）ので zsh プロンプトでも機能する。

---

## 15. 実装ノート（落とし穴）

コードから直ちには読み取れず、後から見て間違えやすい実装上の注意。CLAUDE.md「実装上の注意」が
1 行ルールで参照する詳細（理由・過去バグ経緯・エッジケース）の本体。

### 15.1 端末入力 framing / termios

- **raw モードはセッション全体で維持**する（`save_terminal_settings` で設定）。`passthrough` / `read_confirm_key` 個別での再設定・復元は不要。
- **低レベル端末入力の framing は `src/input.rs` に集約（唯一の場所）**。`ByteSource` trait から 1 byte ずつ読み、`next_event` が UTF-8 組み立て / ESC・CSI・SS3 解析 / poll+timeout を行って `InEvent { raw, tok }` を返す。`read_confirm_key` / `passthrough_read_raw` / `read_minibuffer_line` の 3 つはこの `Tok` を消費するだけの薄い層。**新規コードで fd 0 を直接 (`ManuallyDrop::from_raw_fd(0)`) 読む実装を増やさないこと**（現状の例外は `drain_stdin_nonblocking` と `query_cursor_position_dsr` の 2 つだけ。passthrough が止まっている間にメイン/同スレッドが読むので競合しない）。**`raw`（元バイト列）が主役で `tok`（分類）は副**: passthrough は必ず `ev.raw` をそのまま PTY に送り、**`Tok::Char` を再エンコードして送ってはいけない**（invalid UTF-8 / Alt+非ASCII / paste / マウスシーケンスで壊れる）。focus event（`ESC[I`/`ESC[O`）は decoder では `Tok::FocusIn/FocusOut` として返し、捨てるかは消費側（passthrough だけが破棄）が決める。byte→Tok の対応は golden test 群で固定。変えたらテストも更新する。minibuffer の CSI 解析が partial sequence で blocking してハングし得た旧バグは framing の全 poll 化で解消。
- **stdin の termios は `c_lflag` の `ICANON|ECHO|ISIG|IEXTEN` に加えて `c_iflag` の raw 化フラグ群（`IGNBRK|BRKINT|PARMRK|ISTRIP|INLCR|IGNCR|ICRNL|IXON`）も落とす**。ICRNL を残すと、ユーザの Enter（`\r`）が端末 driver 段階で `\n` に変換され PTY に届く。`prompt_toolkit` 系の選択ピッカー（`Application` / `questionary` 等）は `Keys.ControlM`（= `\r`）のみを「選択確定」にバインドしているため、CR→NL 変換がかかると Enter が無反応になる（`aws configure sso` のアカウント選択画面で再現）。`c_oflag`（OPOST）は触らない: `show_minibuffer` 等で `writeln!(stdout)` が `\n` のみを書く箇所があり、端末側の NL→CRLF 変換に依存している。
- **パススルーで PTY に転送する ESC シーケンスは完全な形まで読み切ってからまとめて送る**。CSI（`ESC [ ... <0x40-0x7E>`）だけでなく SS3（`ESC O <1 byte>`）も同様。途中で分割して 2 回の write になると、受信側（vim 等）は ESC タイムアウトで別キーと解釈する（例: `ESC O` + `H` → `ESC` + `O`（open line above）と誤解）。Home/End や F1〜F4、アプリケーションカーソルモードの矢印キーが該当。
- **ESC sequence の追加 byte 読みはすべて poll(50ms) 付きにする**（`Fd0Source::read_byte` + `decode_csi`/`decode_ss3`/`decode_utf8`。継続 byte は `POLL_TIMEOUT_MS`、最初の 1 byte だけブロッキング）。CSI inner loop / SS3 tail を blocking read にすると、partial sequence（例: マウストラッキング有効中の whiptail でフォーカス切替時に断片送信）で stdin read が固まり、raw mode 下では `Ctrl+C` も単なる `0x03` バイトなので全キー入力が PTY に到達しなくなる（pi-hole installer + whiptail + Ghostty focus 切替で再現実績あり）。timeout したら溜めた seq_bytes は不完全なまま PTY に転送する（transparent proxy 原則）。fail-safe として CSI バッファに長さ上限（`MAX_SEQ_LEN = 64`）。`is_ok()` 判定は `Ok(0)`（EOF / fd 切断）でも通って未初期化 byte を push するため `Ok(1)` で厳密判定する。
- 「bash readline」は readline / emacs 互換の行編集を持つ対話シェルの意味で zsh の emacs モードも含む。詳細は § 14「シェル互換性」。

### 15.2 確認プロンプト Y/n/a/q（`read_confirm_key`）

- **1 キー即確定**（`src/ui.rs`）。Enter は不要。byte 読み・UTF-8 組み立て・ESC 解析は `crate::input` の `next_event` に集約され、`read_confirm_key` は `Tok` を解釈するだけ（`Tok::Char(c)` を `match_confirm_char` に通し、`Tok::Enter`/`Tok::Esc`/`Tok::Ctrl(0x03|0x04)` を個別処理）。
- 受理する文字は ASCII `y`/`Y`/`n`/`N`/`a`/`A`/`q`/`Q` + IME 全角 `ｙ`/`Ｙ`/`ｎ`/`Ｎ`/`ａ`/`Ａ`/`ｑ`/`Ｑ` + ひらがな `あ`（= "a" の自然 IME 確定）と `ん`（= "n" 確定）。Enter（`\n`/`\r`）と Space はデフォルト Yes。未知キーは**無視して再読み取り**（打ち間違いで意図せず No になる事故を避ける）。
- **Enter が制御文字フィルタに飲まれる順序トラップ（過去 2 回再発）は `input::next_event` 側で構造的に解消済み**: Enter（`0x0a`/`0x0d`）は `b < 0x20` 判定より先に `Tok::Enter` に分類され、golden test（`enter_is_not_swallowed_by_control_filter` 等）が回帰を防ぐ。
- **各キーの semantics**: `y`/Enter/Space=このコマンド実行、`n`=このコマンド 1 回スキップ、`a`=残り自動承認、`q`=残り中止（実行済みがあれば AI follow-up・無ければ通常プロンプト）、`Ctrl+C`/`Ctrl+D`=残り中止かつ **AI に問い合わせない**（`InputEvent::ReadLineCancelled` → `ConfirmDecision::AbortNoAi`）。**`ESC` 単独は `n` と同じ「1 回スキップ」**（`ConfirmChoice::No`）。旧実装の「ESC = 残り全部キャンセル」から変更したので ESC を Ctrl+C 系 abort arm に戻さないこと。q（`Quit`）と Ctrl+C/Ctrl+D（`ReadLineCancelled`）は **AI follow-up するか否かが唯一の違い**で、`ExecOutcome::{Quit,Abort}` として運ばれる（Abort は executed 非空でも follow-up せず `break`）。
- **キャンセル時（Ctrl+C / Ctrl+D）は抜ける前に必ず stdout へ `\n` を 1 つ出す**。出さないと、直後にメインループが送るプロンプトリフレッシュが、カーソルがまだ `Exec? … [Y/n/a/q] ` 行末にあるためその行を上書きして消す（ユーザ報告: キャンセルで最終行がプロンプトに上書きされる）。`n`/`y`/`a`/`q` と ESC は `echo_confirm` 末尾 `\n` でクリーン（ESC は char が無いので `'n'` を echo）。echo char を持たない Ctrl+C/Ctrl+D だけ abort arm で明示的に `\n` を出す（`Tok::Eof` は対象外）。
- **echo はマッチした入力 char をそのままの大小で 1 文字 + `\n` で手動描画**（raw mode は ECHO off）。「ユーザが押したキー = 画面に映る文字」を保つため（`y`→`y`、`Y`→`Y`、全角・ひらがなも押下通り）。Enter は char が無い byte 段で先取り処理し、デフォルト Yes の視覚表現として `'Y'` を固定 echo（ESC も skip 表現として `'n'` を固定 echo）。Space は UTF-8 デコード経由で ` ` がそのまま echo される。**`echo_confirm` は `match` を持たず `write!("{c}\x1b[0m\n")` だけ**。大小区別のためにこの関数へ分岐を足さないこと（足すと「押下が常に大文字化される」旧バグ復活と区別が付かない）。

### 15.3 AI 提案コマンド実行中の Ctrl+C 中断

- **確認後、PTY に送って完了待ちしている間に Ctrl+C（0x03）を押すと、実行中コマンドを中断し残りコマンドも中止する**。検知は `wait_for_command_completion`（`src/conversation.rs`）の stdin→PTY 転送部に閉じている: drain した stdin バイトに `0x03` が含まれていたら `interrupted` フラグを立て、**バイトはそのまま PTY へ転送**（実行中コマンドへ Ctrl+C を届け SIGINT で中断させるため。即 return すると `^C` + プロンプト復帰の出力を取りこぼし画面/ring_buffer がずれるので、転送だけして判定はプロンプト復帰時まで遅延）。復帰時に `interrupted` なら `CommandWait::Interrupted` を返し、`confirm_and_execute` が当該コマンドを `executed` に積んだ上で `ExecOutcome::Abort` で即 return（= 残りを送らない・確認画面 Ctrl+C と同じく **AI follow-up しない**）。
- **`Approval::All`（a 一括）/ `AskEach`（1 つずつ y）の両モードで一様**（`wait_for_command_completion` は承認モードを知らない）。**Ctrl+D（0x04）は対象外**: 実行中の対話プログラム（cat / REPL 等）では EOF として正当なので転送のみ。SIGINT を無視するコマンドはプロンプトに戻らず待ち続ける（従来のハング挙動と同じ）。

### 15.4 minibuffer 描画・キャンセル・ペースト

- **aishプロンプト表示中は PTY 出力の画面描画を抑制**（`MINIBUFFER_ACTIVE` フラグ）。ただしリングバッファへの記録は継続する。
- **`show_minibuffer` は入口で DSR（`\x1b[6n`）を投げて cursor row を取得し、`row == rows`（画面下端）のときだけ `\n` を出して scroll 退避**。画面上半分のときは入口 scroll しない。終了時は DECSC/DECRC ではなく取得した `(row, col)` を `\x1b[{row - total_scrolled};{col}H` で**絶対座標復元**する。`total_scrolled` は入口 scroll（1 if was_at_bottom else 0）+ 表示中の grow scroll の累積で、`read_minibuffer_line` → `redraw_minibuffer` に `&mut u16` で渡し grow ごとに加算。
- `redraw_minibuffer` の grow scroll は **cursor を実画面最下行（`term_rows`）に置いた LF の全画面 scroll** で行い、**DECSTBM の scroll region は使わない**（minibuffer 表示中は描画抑制で region で守る相手がいない。終了時 `\x1b[r` は防御的リセットのみ）。region scroll にしない理由: scroll region が全画面でない間のスクロールは上端から押し出された行を scrollback に保存せず破棄するのが xterm 系（Ghostty 含む）標準挙動で、旧実装は「minibuffer が 1 行伸びるたびに直上の行が恒久消失し終了時 cursor 復元も 1 行ズレる」不具合があった（ユーザ報告: 複数行入力で上の行が消える）。**cursor を `term_rows` に置くことと DECSTBM 不使用は不可分**。grow scroll は `was_at_bottom` に関わらず発火（minibuffer は常に画面最下行起点）。grow-shrink-grow の再 scroll は `reserved_rows`（高水位マーク。shrink でも減らさない）で抑止し、**shrink branch の `\x1b[2K` 行クリアはこの空白不変条件を支えるので撤去しないこと**。saved_row が小さいのに grow が多いと `saturating_sub` の clamp でプロンプト行が scrollback へ逃げるが内容は失われない既知の劣化モード。grow/shrink/DECSTBM 非出力のバイト列は golden test 群（`minibuffer_grow_*` / `minibuffer_shrink_*`）で固定。DSR 応答は `\x1b[{row};{col}R` を 80ms timeout で読む（`query_cursor_position_dsr`、`ManuallyDrop` で fd 0 を借り `libc::poll` 非ブロッキング）。応答なし端末では `was_at_bottom = false` fallback で安全側。すべて stdout 専用で PTY には送らない。alt screen（vim 等）中に Ctrl+/ で出すと崩れるが `Ctrl+L` で redraw 可能な許容仕様。
- **キャンセル（ESC / Ctrl+C / Ctrl+/ / "exit" / 空 Ctrl+D）時は minibuffer 跡を clear + cursor 復元したあと、`InputEvent::MinibufferCancelled` を main loop に送り、main loop が `pty.refresh_prompt()`（打ちかけ消去 Ctrl+A+Ctrl+K → 改行）でシェルプロンプトを再表示する**（AI 対話終了 / slash command の refresh_prompt 経路と一貫）。`show_minibuffer` 側は **stdout に `\n` を出さない**（旧実装は出していた）: bash 自身が refresh_prompt の改行で新プロンプトを描くので二重の空行を防ぐ。cursor は saved 位置（= bash readline の認識位置）に復元済みなので refresh_prompt の行消去 redisplay + 改行が正しい位置に当たる。**打ちかけは消える**が refresh_prompt は kill_line してから改行するので**未承認コマンドを submit しない**（信頼の根幹を保持）。「打ちかけ温存」より「クリーンなプロンプト再表示」を優先する仕様（旧仕様は逆）。キャンセル直後に `PassthroughEnded` も届き両方で再 arm するのは送信経路（AiPrompt + PassthroughEnded）と同じ既存パターン。refresh_prompt は `MinibufferCancelled` arm でのみ呼ぶので二重実行しない。既知の制約: 全画面 TUI 表示中にキャンセルすると refresh_prompt の `0x01,0x0b` + 改行がその TUI に届く（slash / 対話終了の refresh_prompt と同性質。「minibuffer を TUI 上に出すのは崩れる許容仕様」の範囲）。
- **minibuffer は bracketed paste マーカー（`ESC[200~`/`ESC[201~` = `Tok::PasteStart`/`PasteEnd`）を honor する**。マーカー間の改行（`Tok::Enter`）は送信ではなく `\n` としてバッファに挿入し（複数行ペーストが最初の改行で途中送信されない）、ペースト外の本物の Enter だけが送信する。CRLF（`\r\n`）は `ev.raw` で判別して 1 つの `\n` に正規化。ペースト本文中の他トークン（Esc/Ctrl/矢印等）は誤爆防止のため無視（Tab 等はドロップ）。**aish 自身は bracketed paste を有効化しない**（`ESC[?2004h` を送らない = 端末状態を変えない原則）。shell が readline で有効化済みのマーカーを利用する。そのため shell が無効化している環境（古い bash / dash 等）では複数行ペーストは最初の改行で送信される（既知の制約）。passthrough はマーカーも raw 転送するので inner program のペーストは不変。

### 15.5 打ちかけ消去 / `refresh_prompt`

- **打ちかけ入力消去（Ctrl+A + Ctrl+K = `0x01 0x0b`）は `PtyHandler::kill_line()` / `refresh_prompt()`（`src/pty_handler.rs`）にカプセル化し、`0x01,0x0b` リテラルは pty_handler.rs 以外に書かないこと**（grep で機械検査可能に保つ）。消去は AI 提案コマンドの「最初の実行」直前 1 回だけ送る（`confirm_and_execute` の `executed.is_empty()`）。show_minibuffer 終了時（質問送信時）には送らない。
- **シェルプロンプトのリフレッシュ改行（slash 処理後 / AI 対話終了後 / minibuffer キャンセル）は必ず `refresh_prompt()` を使い、素の `pty.write(b"\n")` を書かないこと**（refresh_prompt が改行の直前に行消去する。消去側 write エラーは握りつぶし改行側の Result だけ返す旧コード互換仕様）。さもないと Ctrl+/ 前の打ちかけ（passthrough で既に bash readline に届いている）を改行が勝手に submit してしまう（= 未承認コマンド実行。信頼の根幹に触れる）。Ctrl+C（`0x03`）でなく Ctrl+A+Ctrl+K を使うのは SIGINT を発火させず行消去だけしたいため（vim/top 等を意図せず kill しない）。bash readline 以外（vim 子プロセス等）に届くと `^A^K` がリテラルで流れる副作用は残るが SIGINT 直撃より穏当。**打ちかけが温存されるのは minibuffer を空 Enter で抜けた場合だけ**（改行を PTY に送らない）。cancel は上記のとおり消去する。
- **AI 対話を始める直前（`AiConversation::run` 冒頭、`get_unsent_for` の直前）で打ちかけを `0x01,0x0b` で消去してから drain する**。カーソルが bash の readline モデルと同期しているこのタイミングでしか正しく消せないため必須（`show_minibuffer` が DSR で実カーソルを打ちかけ末尾に絶対座標復元した直後で、まだ AI 出力を 1 文字も描いていない = 実カーソル == readline カーソル）。これを入れないと、打ちかけが端末幅を超えて折り返している場合、対話終了後リフレッシュの Ctrl+A に対し bash が `ESC[A` を折り返し行数ぶん吐き、その上移動が aish が出した `Exec?` 行の上で起きてプロンプトが `Exec?` 行を上書きする（旧不具合: 折り返し打ちかけ + n キャンセルでプロンプトがコマンド表示を上書き）。消去 redisplay バイトは stdout に転送せず drain して捨てる（cursor 制御を今流すと AI 出力開始位置がずれる）。`ring_buffer` には追記し不変条件を保つ。`sleep(150ms)` は消去 redisplay の到着待ち。**撤去する場合は折り返し打ちかけ + n キャンセルで `Exec?` 行が上書きされないことを pyte ドライバ等で必ず再検証すること**。

### 15.6 PTY drain / 入力スレッド再開

- **PTY 出力の吸い出しは全て `pty_drain::drain_pty`（`src/pty_drain.rs`）経由**。手書きの `while let Ok(data) = pty_rx.try_recv()` ループを main.rs に再導入しないこと。「表示の有無・先頭改行の除去（`skip_leading_newline` は表示のみ trim）に関わらず、吸い出した data は必ず trim 前の完全な形で ring_buffer に記録される」不変条件は drain_pty 内で一元保証。チャンク内処理順（debug → 表示 → flush → 記録 → sniffer）と「表示 write 失敗はそのチャンクを記録せず伝播」も単体テストで固定。AI 対話直前の打ちかけ消去 + 消去 redisplay の吸収は `discard_stale_readline_input`（`src/conversation.rs`）に関数化。
- **通常動作中は PTY 出力に aish 独自の文字列を一切挿入しない**（パススルーに徹する）。
- **入力スレッド再開の 3 状態（idle / pending / 静音タイマ）は `InputGate`（`src/input_gate.rs`）に集約し、再 arm は `rearm_on_drop()` RAII guard で行う**。idle に戻る arm（AiPrompt / Line / PassthroughEnded / MinibufferCancelled）の入口で `let _rearm = gate.rearm_on_drop();` を取ると、continue / break / `?` を含む全離脱経路で Drop が再 arm する。`arm_passthrough` は private（旧実装は全 exit point への手書きで呼び忘れ = 入力ハングが 2 回発生）。PtyData arm（入力スレッド継続中）では guard を取得しない。発行判定+送信+フラグ遷移は `maybe_request_passthrough()` に一体化（ばらの bool を main loop に再導入しない）。

### 15.7 trust ガード（承認 = 実行の保証）

- **AI 提案コマンドの制御文字ガードは `VettedCommand` 型（`src/vetted_command.rs`）に型化**。実行ループ先頭（`confirm_and_execute` の `Approval` 分岐より前）で `VettedCommand::vet` が検証し、制御文字（改行/CR/ESC/NUL/TAB/その他 C0/DEL/C1）を含むコマンドは Y/n/a/q に載せず `print_rejected_command` で表示して `continue`、PTY に送らない。検証後は表示（`print_single_confirm_prompt`）と送信（`send_approved_command`）の両方が `&VettedCommand` しか受け付けないため、**「画面で承認した物 = サーバで実行される物」が型レベルで保たれ、ガードの撤去・迂回は型エラーになる**（vet は検証のみで文字列を変形しない。`as_str()` が同一スライスを返すことをテストで固定）。`[a]` 経路もこのガードを通る。確認結果は `ConfirmDecision`（Run/Skip/RunRest/QuitRest/AbortNoAi）、自動承認モードは `Approval`（AskEach/All）、抜けた理由は `ExecOutcome`（Completed/Quit/Abort）で表現。複数行 / heredoc の明示承認は未実装（1 コマンド = 1 行を enforce）。
- **AI 由来の `message` / `commands` は端末描画前に制御文字を caret 可視化する**（`visualize_control_line`、`print_ai_message` / `print_ai_commands` / `print_single_confirm_prompt` で適用）。`\r` で行頭復帰・`\x1b[2K` で行消去して「見た目 ≠ 送るバイト」に偽装するのを防ぐ（ESC→`^[`、CR→`^M`、TAB→`^I`、NUL→`^@`）。AI 出力はプロンプトインジェクション経由で未信頼になり得るので**生 `println!` に戻さないこと**。`message` は複数行説明が正当なので `.lines()` 分割は維持し行内のみ可視化。
- **AI 提案コマンドの完了判定は `PromptSniffer` による passive 検出**。ユーザ承認文字列をそのまま PTY に送り、出力末尾がプロンプト形（`[$#>%➜❯»][空白]+`）になり 200ms 静音したら完了。

### 15.8 その他の UI ルール

- **TUI アプリ（vim / less / top 等）終了後は aish 側から何も出さない**。bash 単体と同じ「alt screen から戻った状態」に任せる。元はステータスバー復旧のため `Ctrl+L` を撃っていたが、ステータスバー廃止（commit 7d13700）で動機が消え全廃した。再導入する場合は vim insert モード中の誤発火（バッファに `^L` 混入）を避けるため passive 検出だけで一意に「終了」と判定できる根拠を用意すること。`\x1b[2J`・DECSTBM・alt screen 突入は TUI 動作中にも出るので終了判定に使えない。
- **Shift+Enter による改行は非対応**（端末間で CSI u / legacy が揃わない）。改行は `Alt+Enter` のみ。詳細は § 14。
- **IME の未確定文字（preedit）は aish からは取得不能**。fcitx / mozc / 各 OS IME は OS の入力メソッド層で preedit を保持し、確定（commit）まで stdin に 1 バイトも流れない（terminal emulator が overlay 描画するだけで PTY に到達しない）。Kitty keyboard protocol / CSI u を有効化しても変わらない（IME が下流）。「IME 入力中にリアルタイム反応」は構造上不可能で、確定済み文字でマッチするのが現実解。

### 15.9 ring_buffer / backend 解決

- **未送信 cursor は backend ごとに独立**。`get_unsent_for(kind)` / `mark_sent_for(kind)` を経由し、`/ai` 切替時に新 AI が会話の続きを catch-up できる。**sent_marks は `HashMap<usize, u64>`**（Generic を ordinal `6 + idx` で扱うため。旧固定長 `[u64; COUNT]` から変更）。entry が無いキーは 0（起動以降全部 catch-up）扱い。`mark_sent_all()` は `all_native()` + `all_generics()` 両方を回す。**新規 native backend は `all_native()` に追加**（実行ファイル名が enum 名と異なる場合は `binary()` に分岐追加。spawn / `check_installed` は `binary()`、設定 / 表示は `as_str()`）。
- **`BackendKind::parse(s)` は 2 段階**: (1) `parse_native` で built-in 6 種を直接 match、(2) hit しなければ `GENERIC_REGISTRY` を線形検索。`main::run()` で `Config::load` → `init_generics(&providers)` → 以降 parse の順序を守ること。`validate_providers` が native 予約語との衝突を起動時に reject。テストでは Generic resolution を直接検証しない（`OnceLock` がプロセス共有で並列テストが干渉）。Generic 動作確認は `src/ai/generic.rs` の単体テストで `Box::leak(Box::new(recipe))` 直書き。
- **Generic backend（`BackendKind::Generic(u8)`）は `[[ai.providers]]` registry 経由で動的解決**。registry は `init_generics` で OnceLock に 1 度だけ populate し、`Box::leak` で `&'static str`（display_name = recipe.name、binary）と `&'static ProviderRecipe` を確保（プロセス全期間生存）。init 前 / index 範囲外は `"?"` フォールバック（panic はしないが spawn 失敗）。ユーザは built-in と generic を同じ flat namespace で参照（`generic:` prefix なし）。
- **AI 応答受信後の注釈記録は `RingBuffer::record_ai_exchange`（`src/ring_buffer.rs`）経由**。注釈（`[aish→<kind>]> ...` / `[ai/<kind>]> ...` / `[ai/<kind> suggests] ...`）の append → `mark_sent_for(current_kind)` の順序不変条件（current AI は再受信せず、他 backend は次回 catch-up で受信。逆順だと current AI が自分の発話をループ受信）はこのメソッドに閉じる。ばらの `append_text` + `mark_sent_for` を再導入しない。ラベル書式は `record_ai_exchange` が唯一の定義で単体テスト（`record_ai_exchange_format_is_stable`）で固定。
- **`/clear` は `mark_sent_all()` で全 backend の cursor を末尾に進める**が、AI CLI 内部の session/history は current backend のみリセット（他 backend instance を保持していないため）。「全 AI を仕切り直す」セマンティクスを守りつつ副作用最小化する妥協。
- ring_buffer の `[link]` ライクな PTY 文字列と注釈ラベル（`[aish→...]` / `[ai/...]`）は衝突しうる（AI は文脈で区別する想定。多発したら XML 風へ変更余地）。

### 15.10 AI backends（ツール抑制 / 引数の罠）

- **Claude の system prompt（`--append-system-prompt`）は初回ターンのみ**（resume では二重 append を避け付けない）。**毎ターン守らせたいルール（出力フォーマットや `commands` の入れ方）は system prompt でなく `--json-schema` の field description に書く**（スキーマは毎ターン送られる）。`commands` description には「本文で実行コマンドを出したら配列にも入れる / 提案が無ければ空配列」という整合性ルールを入れる。**「commands を空にするな」式の強制は付けない**（不要時にモデルがコマンドを捻り出し余計な確認が出る）。もう 1 つの整合性ルールは「独立した複数コマンドを `;` 連結 1 本にせず `commands` 配列の別要素に分割」（codex 対策。共有 system prompt `build_system_prompt` と Claude の `commands` description の両方に入れる）。**ただし `&&`/`||` と `for`/`while`/`until`/`case`/`if` 等の制御構文内の `;` は 1 コマンドとして維持する例外を必ず併記**（分割すると `for i in 1 2 3; do …; done` 等が壊れる）。
- **Claude の `--disallowedTools` は `MANDATORY_DENY`（Bash/Edit/Write）を常に union する**（`effective_disallowed_tools`）。`[ai.claude].disallowed_tools` は単一文字列の全置換なので空にすると安全集合が消える footgun。`allow_unsafe_tools = false`（既定）の間は baseline を必ず混ぜ、`disallowed_tools = ""` でも Bash/Edit/Write は deny（Read だけ外せる）。`true` のときのみ verbatim（危険設定）。**`--disallowedTools` は args の末尾で push**（`--output-format`/`--json-schema`/`extra_args`/`--model`/`--effort` の後）。`extra_args = ["--disallowedTools", ""]` を後置きされても CLI 後勝ちでこちらが勝ち baseline を non-removable に保つため。**末尾 push を前方に戻したり union を外したりしないこと**。codex/copilot/cursor の deny 系は固定埋め込み（extra_args 後置き上書きは未対策）。
- **cursor backend は `--trust` を常時付与**（headless `-p` は `--trust` 無しだと `Workspace Trust Required` で拒否し非ゼロ終了）。config 不可で固定。`--yolo`/`-f`（Run Everything）は絶対に付けない。**ツール抑制は `--mode plan` + system prompt の二段**（cursor-agent に個別ツール無効化フラグが無いため。`[ai.cursor].sandbox = "enabled"` を併用可）。`mode = ""`（通常モード）は確認 UI を迂回するリスクがあり非推奨。Free プランは Named models 不可で `auto` のみ（`--model sonnet-4` 等は `EmptyOutput` エラー）。
- **copilot backend は `-p` フラグを付けない**（`-p` は positional/stdin と排他で stdin 渡しだと `too many arguments` で死ぬ。CLI が stdin 自動検出）。他の `-p` 必須 backend（claude, cursor）と逆。**ツール抑制は四段**: `--allow-all-tools`（非対話必須）+ `--deny-tool=shell` + `--deny-tool=write` + `--no-ask-user` + `--mode plan`（deny が allow に優先）。config 不可で固定。`--yolo`/`--allow-all` は絶対に付けない。**`--output-format json` は JSONL**（行ごとに `parse_jsonl_envelope` で走査し `assistant.message` の `data.content` と `result` の `sessionId` を取る。ephemeral 行は無視）。組織ポリシーで CLI 拒否されることがある（`Access denied by policy settings`）。
- **Generic backend は安全フラグ（`--deny-tool` 等）を自動付与しない**。recipe 著者が `args` に `--mode plan` 等を明示する想定。信頼できない CLI を登録すると確認 UI を迂回して shell 実行される可能性がある（claude/codex/copilot のような自動 deny は無い）。§ 6.0 / `[[ai.providers]]` 安全性節を参照。

### 15.11 セルフアップデート 2 チャネル

- **`aish --update` は安定版 / 最新版の 2 チャネル**（`src/update.rs` の `UpdateChannel`）。`--stable`（既定）は GitHub `/releases/latest`（prerelease 除外の最新）、`--prerelease` は `/releases` 一覧の先頭 `[0]`（prerelease 含む絶対最新）。**命名の罠**: GitHub API では「安定版」が `/releases/latest`（"latest" を含む）、「先端」が `/releases` の先頭。"latest" がぶつかるのでユーザ向け flag をあえて `--prerelease` にしている（`--latest` だと逆に見える）。**この向きを逆にしないこと**。チャネル区別は `release.yml` の `prerelease: ${{ contains(tag, '-') }}` 依存（SemVer ハイフン付きタグを prerelease=true で公開し `Latest` バッジを付けない）。**prerelease リリース時は Cargo.toml の `version` にも識別子を含める**（`0.9.0-rc.1`）。タグだけ `-rc.1` で Cargo.toml が数値のみだと `run_update` の `latest == current` 比較が一致せず常に「更新あり」と誤判定される。target 解決 / DL / SHA256 検証 / 置換はチャネル非依存で共通。
