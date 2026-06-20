# aish 仕様書

CLI SSH + AI (Claude Code) ツール。クライアント側の Claude Code から、ローカルシェルまたは SSH 接続先サーバを調査・操作する対話型 UI。

---

## 0. 用語

- **パススルーモード**: 通常のシェル操作状態。キー入力を PTY にそのまま転送。
- **aishプロンプト**（ミニバッファ）: `Ctrl+/` で開く `[aish]` 入力欄。最下行に表示し AI への質問を入力。ESC / Ctrl+C / Ctrl+/ でキャンセル。
- **ステータスバー**: 最下行の `aish v{version} | Ctrl+/ for AI` 行。
- **スピナー**: AI 応答待ち中のアニメーション（`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` + `Thinking...`）。
- **確認プロンプト**: AI 提案コマンドの実行可否を問う `Exec? {cmd} [Y/n/a/q]`。
- **ReadLineモード**: 確認プロンプト応答など、ライン編集付き入力状態。

---

## 1. アーキテクチャ（ファイル構成）

| ファイル | 役割 |
|---|---|
| `main.rs` | メインループ。PTY 読み取り / ユーザ入力 / イベントループの 3 スレッド構成。slash command 処理 |
| `conversation.rs` | AI 対話 1 セッションの制御フロー (`AiConversation::run`)。打ちかけ消去 → send → Y/n/a 確認 → 実行 → 完了 passive 検出 → follow-up → 終了 refresh |
| `ui.rs` | ターミナル制御。raw モード（セッション全体で維持）、ライン編集、パススルー、ANSI 色、ミニバッファ |
| `input.rs` | 低レベル端末入力の framing（唯一の場所）。`ByteSource` → `next_event` → `Tok` |
| `input_gate.rs` | パススルー入力スレッド再開の 3 状態管理 (`InputGate` + `rearm_on_drop` RAII guard) |
| `ai/` | AI backend 層。`AiBackend` trait + claude/codex/gemini/qwen/cursor/copilot/generic 実装、factory、共通 prompt/spawn (`common.rs`) |
| `config.rs` | TOML 設定ロード |
| `pty_handler.rs` | portable-pty による SSH / ローカルシェル起動。実端末サイズで起動し SIGWINCH 追従。`kill_line` / `refresh_prompt` / `send_approved_command` |
| `pty_drain.rs` | PTY 出力吸い出しの一元化 (`drain_pty` + `DrainOpts`)。表示方針・先頭改行 trim・sniffer 連携 |
| `prompt_sniffer.rs` | シェルプロンプト復帰の passive 検出（終端文字学習つき） |
| `vetted_command.rs` | AI 提案コマンドの制御文字検証 newtype (`VettedCommand`)。「承認した物 = 実行される物」の型保証 |
| `update.rs` | セルフアップデート (`--update`)。GitHub Releases から最新バイナリ取得 |
| `ring_buffer.rs` | 1MB リングバッファ。ANSI 除去、backend 別差分送信、AI 注釈記録 (`record_ai_exchange`) |
| `mode.rs` | `Local` / `Remote` の 2 モード定義 |

---

## 2. 動作モード

| モード | 起動条件 | 挙動 |
|---|---|---|
| **Local** | SSH 引数なし (`aish`) | `$SHELL`（未定義なら `/bin/bash`）を PTY 起動 |
| **Remote** | SSH 引数あり (`aish user@host`) | `ssh` を PTY 起動。引数はそのまま ssh へ |

両モードとも終了は `exit` または PTY プロセス終了。

---

## 3. コマンドラインオプション

| オプション | 意味 |
|---|---|
| `--version` / `-V` | バージョン表示して終了 |
| `--update` | 自己更新（§12）。`--stable`（既定）/ `--prerelease` の 2 チャネル |
| `--help` | ヘルプ表示して終了 |
| `--config <path>` | 設定ファイルパス（既定 `~/.aish/config.toml`） |
| `--ai <name>` | backend 選択。built-in `claude`/`codex`/`gemini`/`qwen`/`cursor`/`copilot` または `[[ai.providers]]` の `name`。`[ai].backend` を上書き。built-in 名は予約語 |
| `--model <name>` | モデル名。`[ai].model` および `extra_args` の `-m` より優先 |
| `--effort <level>` | reasoning effort。claude → `--effort`、codex → `-c model_reasoning_effort=`、copilot → `--effort`。gemini/qwen/cursor は CLI 非対応で無視 |
| それ以外 | SSH 引数として `ssh` に渡す |

---

## 4. UI 要素

### 4.1 ステータスバー
- 最下行に常時表示の 1 行。
- DECSTBM (`\x1b[1;{rows-1}r`) でスクロール領域を最下行除外に制限し、`\x1b[{rows};1H` にラベルを描画。
- PTY 出力が 50ms 静音で再描画 (`resize_status_bar`)。`\x1b7`/`\x1b8` で囲みシェル側カーソルを保全。SIGWINCH でも再設定。
- 終了時に `\x1b[r`（領域解除）+ `\x1b[2K`（行クリア）。

### 4.2 aishプロンプト（ミニバッファ）
- `Ctrl+/` (0x1F) で開く、最下行を置き換える入力欄。表示は `[aish] ` ラベル + 入力テキスト。
- 表示中は `MINIBUFFER_ACTIVE` が立ち PTY 出力の画面描画を抑制（リングバッファ記録は継続）。
- 確定時: スクロール領域に `[aish] {text}` を**各論理行の先頭にラベル付きで**エコー → 履歴追加（直前と同一なら追加せず）→ `InputEvent::AiPrompt(text)` を送信。
- キャンセル経路: 単独 ESC / Ctrl+C / Ctrl+/ / 入力が `exit` のまま Enter。空 Enter は無操作（ステータスバー復元のみ）。
- 開く直前にシェル側コマンド入力中だった場合（`at_line_start == false`）、キャンセル/確定時に `0x03` を PTY に送り部分入力を破棄。

### 4.3 マルチライン入力
- 入力長に応じてミニバッファが縦方向に拡張（最大 `term_rows / 2` 行）。`compute_visual_layout` が論理行と折り返しを計算。
  - 第1論理行はラベル幅 `label_width` を差し引いた幅で折り返し。継続行は `label_width` 分のインデント／ラベル付き。
- 伸長時は cursor を実画面最下行に置いた LF の**全画面スクロール**で行確保（DECSTBM の region scroll は使わない。理由は §15.4）。押し出された行は通常出力同様 scrollback へ。
- 縮小時は不要行を `\x1b[2K` でクリア（スクロールは戻さない）。確保済み行数は高水位 `reserved_rows`（縮小でも減らさない）で追跡し、縮小→再伸長では高水位超過まで再スクロールしない。
- 総可視行数が `max_rows` 超過時はカーソル行が見える位置までスクロール (`scroll_top`)。

### 4.4 起動時の表示確保
- `setup_status_bar` で DECSTBM 設定前に `\n` を 1 回出しステータスバー 1 行分を確保。
- ターミナルタイトル: `\x1b]2;[aish] {ssh_args}\x07`（Local では `[aish]` のみ）。終了時に空タイトルで復元。
- 通常動作中は PTY 出力に aish 独自の文字列を一切挿入しない（パススルーに徹する）。

### 4.5 スピナー
- ステータスバー行で点滅。フレーム `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`、80ms ごと更新。表示 `{thinking_color}{frame} {thinking_message}\x1b[0m`。
- `\x1b7`/`\x1b8` でカーソル保存・復元しシェル入力欄を保全。`stop()` / Drop でステータスバー再描画。

### 4.6 確認プロンプト
- AI 提案 `commands` を番号付きで全件表示（プラン提示）。
- 各コマンドごとに `Exec? {cmd} [Y/n/a/q] ` を `confirm_color` で表示し 1 キー即確定（`read_confirm_key`）。最後／単一は `[Y/n]`。
- キーの意味は §6.6 step 4 参照。

---

## 5. キー入力

### 5.1 パススルーモード
| キー | 動作 |
|---|---|
| `Ctrl+/` (0x1F) | aishプロンプトを開く |
| それ以外 | PTY へ直送（Enter, Ctrl+C, Tab, Ctrl+L, Ctrl+R, 矢印, ESC シーケンス等すべて） |
| フォーカスイベント `\x1b[I` / `\x1b[O` | 破棄（PTY へ送らない） |
| UTF-8 マルチバイト | 先頭バイトから長さ判定し全バイト読み取り PTY へ |

`passthrough_read_raw` は `Ctrl+/` 受信か PTY EOF までループ継続（Enter/Ctrl+C 等でも抜けない）。

### 5.2 aishプロンプト（ミニバッファ）
| キー | 動作 |
|---|---|
| `Enter` (`\r`/`\n`) | 確定。`exit` のみならキャンセル扱い |
| `Alt+Enter` (`\x1b\r`/`\x1b\n`) | 改行挿入 |
| `Shift+Enter` (CSI u `\x1b[13;Nu`) | 改行挿入（端末依存で届かないことあり） |
| `ESC` 単独 / `Ctrl+C` / `Ctrl+/` | キャンセル |
| `Ctrl+D` (0x04) | 空ならキャンセル、否ならカーソル位置文字削除 |
| `BS`/`DEL` (0x08/0x7F) | カーソル左の文字削除 |
| `Ctrl+A`/`Home` / `Ctrl+E`/`End` | 行頭 / 行末へ |
| `Ctrl+B`/`←` / `Ctrl+F`/`→` | 1 文字左 / 右へ |
| `Ctrl+U` / `Ctrl+K` | カーソルより左 / 右をすべて削除 |
| `Ctrl+W` | カーソル直前の単語（空白区切り）削除 |
| `↑`/`↓` | 履歴ナビゲーション（新規入力は退避） |
| `Delete` (`\x1b[3~`) | カーソル位置文字削除 |

### 5.3 ReadLineモード（確認プロンプト応答）
- パススルーと同じ raw モード。矢印↑↓は履歴、他編集キーは aishプロンプトと同等。
- `exit` で終了、他は `UserInput::ShellCommand` として PTY へ。

### 5.4 Slash command（aishプロンプト内）

先頭 `/` の入力は AI に送らずローカル処理。`AiBackend` trait のメソッドを呼び結果を dim grey 表示。

| コマンド | 動作 |
|---|---|
| `/help` | slash command 一覧表示 |
| `/effort [LEVEL]` | reasoning effort を runtime 変更（次回 send 以降）。省略でクリア。gemini/qwen/cursor は保存のみ、claude/codex/copilot は native 反映 |
| `/model [NAME]` | モデルを runtime 変更（session_id / history 維持）。省略でクリア |
| `/clear` | 会話履歴 / セッションをクリア。claude/codex/cursor/copilot/generic(native resume) は session_id を None、gemini/qwen/generic(非 native) は内部 history を空に |
| `/ai <NAME>` | backend 切替（built-in または `[[ai.providers]]` の `name`）。`create_backend` で新規構築し現セッション破棄 |
| `/<unknown>` | 未知なら入力をそのまま AI へ（ファイルパス `/root/test.txt` や自然文 `/foo bar` も AI に届く） |

aish の slash command は AI CLI 自身の対話モード `/<cmd>` とは独立に aish 側で実装（aish は CLI を非対話モード起動するため CLI 側 slash command は届かない）。

### 5.5 シグナル
- `SIGWINCH` → `sigwinch_handler` が `SIGWINCH_RECEIVED` をセット、メインループで非同期消費。
- SIGINT は独自処理せず OS 既定に委ねる（raw モードで ISIG 無効のためキーボード Ctrl+C は SIGINT を発行しない）。

---

## 6. AI 連携

trait `AiBackend` を介して対応:
- **native backend 6 種**: Claude Code / OpenAI Codex CLI / Google Gemini CLI / Alibaba Qwen Code / Cursor Agent CLI / GitHub Copilot CLI（`src/ai/<name>.rs` に bespoke 実装）。
- **Generic CLI backend**: `[[ai.providers]]` に登録した任意 CLI を `--ai <NAME>` で使う。コード変更なしで追加可能（`src/ai/generic.rs` の単一 driver が recipe を解釈）。`name` は built-in 予約語と衝突不可。

各 backend は JSON で `{message, commands[]}` 相当を返し、aish は提案ベースで動作。CLI 非依存の原則（透明性・サーバ無書き込み）は trait 実装側の責任。

選択優先順位: `--ai` > `[ai].backend` > `claude`（既定）。

### 6.0 バックエンド能力差

| 機能 | Claude | Codex | Gemini | Qwen | Cursor | Copilot |
|---|---|---|---|---|---|---|
| 実行ファイル | `claude` | `codex` | `gemini` | `qwen` | `cursor-agent` | `copilot` |
| 非対話モード | `claude -p` | `codex exec -` | `gemini` (stdin) | `qwen` (stdin) | `cursor-agent -p --trust` (stdin) | `copilot` (stdin、`-p` 不可) |
| JSON 出力強制 | `--json-schema` | なし | なし | なし | `--output-format json`（外側ラッパのみ） | `--output-format json`（**JSONL**） |
| 危険ツール無効化 | `--disallowedTools` | `-s read-only` + `--disable` 12 種 | system prompt のみ | system prompt のみ | `--mode plan` + `--sandbox` + system prompt | `--allow-all-tools --deny-tool=shell --deny-tool=write --no-ask-user --mode plan` |
| セッション再開 | `--resume <sid>`（JSON `session_id`） | `exec resume <UUID>`（rollout ファイル名から） | best-effort (`--resume latest`) | best-effort (`--continue`) | `--resume <sid>`（JSON `session_id`） | `--resume <sid>`（JSONL `result.sessionId`） |
| reasoning effort | `--effort` | `-c model_reasoning_effort=` | なし | なし | なし | `--effort`（`none/low/medium/high/xhigh/max`） |

- プロンプト渡しは全 backend stdin。
- JSON Schema 強制が無い backend は system prompt で `{"message":..., "commands":[...]}` 単独出力を強く指示し `extract_json` で抽出。失敗時は出力全体を `message` / `commands: []` でフォールバック。

**セッション履歴の持ち方**:
- Claude/Cursor/Copilot: 初回 send で `session_id` を捕獲し 2 回目以降 `--resume <sid>` で連結。Cursor/Copilot は `--append-system-prompt` 相当が無いので system prompt を初回プロンプト先頭に焼き込む（resume 後は再送しない）。Copilot は JSONL を行走査し最後の `assistant.message` の `data.content` を応答、`result` 行の `sessionId` を捕獲。
- Codex: 初回後 `~/.codex/sessions/.../rollout-...-<UUID>.jsonl` から UUID 捕獲し `codex exec resume <UUID>` で連結（`--ephemeral` は付けない）。
- Gemini/Qwen: 非対話モードで session resume が安定しないため、各 backend 内部で直近 8 ターン (user, ai) を履歴保持し毎回プロンプトに再送。ring buffer 差分と合わせ multi-step の文脈を保つ。

**安全性の差**（最大安全が必要なら `--ai claude` か `--ai copilot`）:
- **Claude**: `--disallowedTools "Bash,Edit,Write,Read"` でフラグレベル拒否。最も強力。
- **Codex**: `codex exec` は本来エージェントなので、確認 UI を迂回しないようツール系 feature 12 種を `--disable`（shell_tool / unified_exec / browser_use / computer_use / multi_agent / image_generation / tool_search / tool_suggest / plugins / apps / skill_mcp_dependency_install / tool_call_mcp_elicitation）+ `-s read-only`。純粋 LLM に退化。
- **Copilot**: claude 同等。`--allow-all-tools`（非対話必須）+ `--deny-tool=shell`/`--deny-tool=write`（deny が優先）+ `--no-ask-user` + `--mode plan`（既定）の四段。
- **Cursor**: 個別ツール無効化フラグ無し。`--mode plan`（read-only）を既定付与（aish の提案セマンティクスと一致）+ `[ai.cursor].sandbox` で `--sandbox enabled` 可 + system prompt。`--trust` は headless 必須で常時付与。
- **Gemini/Qwen**: フラグ制約なし、system prompt の「ツール禁止」指示のみ。

### 6.1 起動
- 選択 backend のバイナリを `--version` で確認し、失敗なら「Please install ...」表示して終了（Claude のみインストールコマンドも表示）。実行ファイル名は `BackendKind::binary()`（cursor のみ `cursor-agent`、他は `as_str()` と同じ）。

### 6.2 リクエスト引数（claude 例）
- 初回: `claude -p --append-system-prompt "{system_prompt} + 提案ルール" --output-format json --disallowedTools "..." --json-schema <SCHEMA> "<prompt>"`。
- 2 回目以降: `--append-system-prompt` を外し `--resume <session_id>` を付ける（append は初回のみ、resume は既存システムプロンプトを再利用）。
- `--disallowedTools` / `--json-schema` は毎回明示。`session_id` は出力 JSON の `session_id` から捕獲。
- 提案ルール要点: 「直接実行せず提案」「1 レスポンス 1 コマンド推奨」「`&&`/`||` は 1 コマンドとして維持」「独立コマンドは `commands` 配列に分割」。

### 6.3 JSON Schema
```json
{ "type": "object",
  "properties": { "message": {"type":"string"}, "commands": {"type":"array","items":{"type":"string"}} },
  "required": ["message", "commands"] }
```

### 6.4 プロンプト組み立て
- ` ```terminal\n{リングバッファのマーク以降（ANSI 除去済み）}\n``` ` の後にユーザ入力プロンプトを続ける。リングバッファが空なら `terminal` フェンスを付けずプロンプトのみ。

### 6.5 コマンド実行ループ
1. `message` を `ai_color` で表示。
2. `commands` 空なら対話終了。
3. `commands` を番号付き全件表示（プラン提示）。複数返っても 1 件ずつ確認。
4. **各コマンドを 1 つずつ** `Exec? {cmd} [Y/n/a/q]` で確認し、承認分を**そのまま** `<cmd>\n` で PTY に送信（変形・ラップしない＝**透明性が信頼の根幹**）。キー意味:
   - `Y`/Enter/Space: 実行。`n`/`ESC`: このコマンド 1 回スキップ。`a`: 実行 + 以降自動承認。`q`: 残り中止（実行済みあれば AI follow-up、無ければ通常プロンプトへ）。`Ctrl+C`/`Ctrl+D`: 残り中止かつ**実行有無に関わらず AI に問わない**。
   - 残コマンドが無い最後／単一は `a`/`q` を畳んで `[Y/n]`。
5. **完了待ちループ**（約 20ms 周期で並行）: PTY ドレイン（表示 + ring 追記 + `PromptSniffer.feed()`）/ stdin→PTY 転送（ノンブロッキング poll で fd 0 直読。パスワード入力・Ctrl+C 中断可）/ SIGWINCH / 完了判定（`matches_prompt()` 真 + 200ms 静音）。
6. 1 つも実行されず、または Ctrl+C/Ctrl+D 中止なら AI に問わず終了。
7. 1 つ以上実行し Ctrl+C/Ctrl+D 以外で抜けたら、各コマンド実行サマリ（`` `cmd` ``）+ 出力本体（`terminal` フェンス）を AI へ送信（`q` 中止時は「残りを中止した」旨に切替）。
8. 2 へ戻る。9. ループ終了後 PTY に `\n` を送りシェルプロンプト再描画。

`PromptSniffer` は ANSI 除去後末尾 256 バイトを保持し、最終行が `[終端文字][空白]+` で終わるか構造判定。既定終端文字は `$ # > % ➜ ❯ »`（`:` は ssh password prompt で誤検出のため除外）。検出時 `record_match()` が終端文字を学習。多段 SSH や `sudo bash` で PS1 が変わっても終端構造が共通なので追従。

**サーバ側に何の書き込み・注入もしない**（`PROMPT_COMMAND` 改変・history 抑制・shell 統合の自動セットアップ等は一切しない）。完了判定は PTY 出力の passive 観察のみ。代償として exit code は取れず、AI は出力テキストから成否を推測。

#### 既知の制約
- カスタムテーマ（oh-my-zsh robbyrussell `➜  ~ ` 等、終端が既定セット外）は初回検出で見逃す。`record_match` で学習させるか設定で終端文字追加（後者は今後）。
- `tail -f` 等の連続出力はプロンプトに戻らず完了判定されない。ユーザ Ctrl+C で抜ける運用。
- 出力途中の偽プロンプト風文字列は 200ms 静音条件で大半救済。

### 6.6 キャンセル（AI プロセス実行中）
- stdin をノンブロッキング poll し `0x03` 検知で `child.kill()`。`"Cancelled"` 扱いで `^C` 表示し対話終了（aish は継続）。

### 6.7 セッション再開コマンド表示
- 終了時 `AiBackend::resume_command()` が `Some(cmd)` なら stderr に `Resume this <kind> session with:\n  <cmd>` を出力。
  - claude `claude --resume <UUID>` / codex `codex resume <UUID>` / gemini `gemini --resume latest` / qwen `qwen --continue` / cursor `cursor-agent --resume <UUID>` / copilot `copilot --resume <UUID>` / generic `<binary> <resume_flag> <sid>`（recipe.resume_flag + session_id 捕獲時）。
- gemini/qwen は非対話 session 永続化が CLI 仕様で保証されず、表示しても読み戻せないことがある。cursor/copilot は `--resume` 安定動作を実機確認済み（copilot は cache token も効く）。

### 6.8 JSON 抽出
- `extract_json` で最外の `{...}` をバランス解析で抽出（前後テキスト混入に対応）。
- `structured_output` があればそれ、無ければ `result` をボディに採用。`result` が文字列なら JSON パース試行、失敗で `message: <そのまま>, commands: []` フォールバック。

### 6.9 ログ
- `[log] enabled = true` 時、`claude {args}` / レスポンス本文 / `[stderr] ...` を `path`（既定 `~/.aish/logs/claude-code.log`）に追記。各エントリは `=== YYYY-MM-DD HH:MM:SS ===` ヘッダ付き（ローカル TZ）。

---

## 7. リングバッファ

- 固定 1MB。書き込み位置 / 未送信位置（`sent_pos`）を保持。
- `append`: `strip_ansi_escapes::strip` で ANSI 除去後に格納。
- `get_unsent`: `sent_pos` 以降を `String::from_utf8_lossy` で返す。`mark_sent`: AI 応答取得成功時に呼び次回に含めない。
- 満杯で未送信長が capacity 超なら `sent_pos = 0`（最新 1MB を送る）。

---

## 8. スレッド構成

- **PTY 読み取りスレッド**: `pty_reader.read(buf[4096])` をループし `pty_tx` へ送信。EOF/エラーで `alive_tx.send(())`。
- **入力スレッド**: `prompt_rx` から `Passthrough` / `ReadLine` を受け処理。`Passthrough` → `PassthroughEnded`、`ReadLine` → `Line` で通知。
- **メインループ**（約 1ms ポーリング）: ① SIGWINCH → リサイズ + ステータスバー再描画 ② PTY ドレイン（`minibuffer_active()` なら描画抑制）③ 50ms 静音でステータスバー再描画 ④ 入力 idle + 同条件でリクエスト送信 ⑤ PTY プロセス終了検知 ⑥ 入力イベント処理。

---

## 9. 入力イベント管理

| フラグ / 状態 | 役割 |
|---|---|
| `pending_input` | 入力リクエストを次の安定点で送るべきか |
| `input_idle` | 入力スレッドが `recv()` 待機中か（キュー重複防止） |
| `MINIBUFFER_ACTIVE` | ミニバッファ表示中（画面描画抑制） |
| `SIGWINCH_RECEIVED` | 端末リサイズ要求 |
| `TERM_ROWS` | 端末高さキャッシュ |

- AI 対話終了でパススルーへ戻る直前、確認プロンプト ReadLine で `input_idle` が false のため、メインループ側で**明示的に `input_idle = true` へ戻す**（忘れるとリクエスト再送されずハング）。
- `Ctrl+/` 受信時は `PassthroughEnded` が届き `input_idle = true` に戻り次のミニバッファ呼び出しを発行可能に。

---

## 10. ターミナル制御

### 10.1 termios
- `save_terminal_settings` で起動時 termios 保存と同時に raw モード化（`ICANON|ECHO|ISIG` 解除、`VMIN=1, VTIME=0`）。raw モードは**セッション全体で維持**（個別 `read_line`/`passthrough` で再設定しない）。`restore_terminal_settings` で終了時に復元。詳細は §15.1。

### 10.2 ANSI エスケープ
- DECSTBM `\x1b[r`: ミニバッファ終了時の防御的フルリセットのみ（aish は scroll region を設定しない。理由 §15.4）。
- DECSC/DECRC `\x1b7`/`\x1b8`: カーソル位置保存・復元。CUP `\x1b[{row};{col}H`: 位置指定。EL `\x1b[K`/`\x1b[2K`: 行末/行全体クリア。SGR `\x1b[0m` + ユーザ色（256色・TrueColor）。

### 10.3 可視幅計算
- `visible_width(s)`: ANSI 除去後 `UnicodeWidthChar::width` を合算（全角=2、半角=1、制御=0）。ラベル幅・折り返し・BS 消去幅に使用。

---

## 11. 設定ファイル (`~/.aish/config.toml`)

TOML 形式。未指定はデフォルト。サンプルは `config.toml.example`。

### 11.1 トップレベル
| キー | 既定値 | 説明 |
|---|---|---|
| `system_prompt` | `"あなたはLinuxサーバ管理の専門家です。SSHセッションの内容を把握しています。"` | AI のシステムプロンプト |
| `language` | `"Japanese"` | 空以外なら `Respond in {language}.` を付加 |

### 11.2 `[display]`
| キー | 既定値 |
|---|---|
| `shell_prefix_label` | `[aish]`（ターミナルタイトル先頭） |
| `header_color` | `\x1b[38;5;208m`（ステータスバー） |
| `prompt_label` / `prompt_color` | `[aish]` / `\x1b[38;5;208;48;2;50;35;20m`（前景+背景） |
| `thinking_message` / `thinking_color` | `Thinking...` / `\x1b[38;5;208m` |
| `ai_color` | `\x1b[38;5;216m` |
| `input_color` | `""`（ミニバッファ入力背景色） |
| `confirm_color` | `\x1b[38;5;228;48;5;239m` |

### 11.3 `[log]`
| キー | 既定値 |
|---|---|
| `enabled` | `false` |
| `path` | `~/.aish/logs/claude-code.log`（`~/` 展開） |

### 11.4 `[ai]`
| キー | 既定値 | 説明 |
|---|---|---|
| `backend` | `"claude"` | 使用 CLI。`--ai` で上書き可 |
| `model` | `""` | モデル名。空で CLI 既定。`--model` で上書き可 |
| `effort` | `""` | reasoning effort。claude/codex/copilot に変換、gemini/qwen/cursor は無視。`--effort` で上書き可 |
| `system_prompt` / `language` | `""` | 空ならトップレベルにフォールバック |

#### `[ai.claude]`
| キー | 既定値 | 説明 |
|---|---|---|
| `disallowed_tools` | `"Bash,Edit,Write,Read"` | `--disallowedTools` の値（単一文字列の全置換）。`MANDATORY_DENY`(Bash/Edit/Write) は常に union され空にできない（§15.10） |
| `allow_unsafe_tools` | `false` | `true` のときのみ `disallowed_tools` を verbatim 使用（危険） |
| `extra_args` | `[]` | 追加引数（ビルトイン引数の後ろ） |

#### `[ai.codex]` / `[ai.gemini]` / `[ai.qwen]`
- `extra_args` (`[]`): 各 CLI への追加引数（例 `["-m", "gpt-5.5"]`）。ビルトイン引数の後ろ。

#### `[ai.cursor]`
| キー | 既定値 | 説明 |
|---|---|---|
| `extra_args` | `[]` | ビルトイン引数（`-p --output-format json --trust`、`--mode <m>`、`--sandbox <s>`、`--resume <sid>`）の後ろ |
| `mode` | `"plan"` | `--mode` 値（`"plan"`/`"ask"`/`""`）。`"plan"` は read-only/propose-only の安全側既定。`""` は危険 |
| `sandbox` | `""` | `--sandbox` 値（`"enabled"`/`"disabled"`）。空なら付けない（defense-in-depth） |

- `--trust` は headless 必須のため常時自動付与（config 不可）。未指定だと `Workspace Trust Required` で拒否。
- Free プランは Named models 不可で `auto` のみ → `[ai].model = "auto"` か `--model auto`。

#### `[ai.copilot]`
| キー | 既定値 | 説明 |
|---|---|---|
| `extra_args` | `[]` | ビルトイン引数の後ろ（例 `["--disable-builtin-mcps"]`） |
| `mode` | `"plan"` | `--mode` 値（`"plan"`/`"interactive"`/`"autopilot"`/`""`）。`"plan"` が安全側既定 |

- 信頼の根幹に直結するため常時自動付与（config 不可）: `--output-format json`(JSONL)、`--allow-all-tools`(非対話必須)、`--deny-tool=shell`/`--deny-tool=write`(deny 優先)、`--no-ask-user`。
- 認証は `gh auth login` / `copilot login` / `COPILOT_GITHUB_TOKEN`/`GH_TOKEN`/`GITHUB_TOKEN` env。組織ポリシーで CLI 拒否され得る（`Access denied by policy settings`）。

#### `[[ai.providers]]`（Generic CLI backend レシピ配列）

`GenericCliBackend` が読む config 駆動レシピ。`--ai <NAME>` / `/ai <NAME>` で有効化（複数登録可、上限 256）。

| キー | 既定値 | 説明 |
|---|---|---|
| `name` | (必須) | provider 一意識別子 |
| `binary` | (必須) | 実行ファイル名（PATH 検索）または絶対パス |
| `args` | `[]` | 固定引数。aish が動的引数（resume/model/effort/prompt）を後ろに追加 |
| `prompt_delivery` | `"stdin"` | `"stdin"` / `"arg"`(positional 末尾) / `"flag"`(`prompt_flag` の値) |
| `prompt_flag` | `""` | `prompt_delivery="flag"` のとき必須 |
| `parse` | `"lossy"` | `"lossy"` / `"extract_json"` / `"jsonl"` |
| `jsonl_content_path` / `jsonl_session_path` | `""` | `parse="jsonl"` のとき `"type:dot.path"` で応答テキスト / session_id のパス |
| `session_id_path` | `""` | `parse="extract_json"` のとき session_id フィールド名（top-level key） |
| `resume_flag` | `""` | session_id 捕獲時の resume 引数。空 + session_id_path 空なら native resume なし |
| `model_flag` / `effort_flag` | `""` | model / effort 引数名。空なら渡さない（effort は保存のみ） |
| `color` | `208` | 256-color（ラベル・banner 色） |
| `system_prompt_inline` | `true` | `true`: 初回プロンプト先頭に焼き込む。`false`: 毎回 history + system + context で再構築 |
| `history_turns` | `8` | native resume 無効時の内部保持ターン数 |

起動時 `AiConfig::validate_providers()` で検証（配列長 ≤ 256 / `name` 一意 / built-in 予約語と非衝突 / `parse`・`prompt_delivery` 妥当性 / `prompt_delivery="flag"` で `prompt_flag` 非空）。不正なら `Invalid [[ai.providers]] in <path>: ...` で起動拒否。予約語は `BackendKind::all_native()` から導出。

- **安全性**: native と違い `--deny-tool` 相当の強制フラグは付けない。利用者が `args` に `--mode plan` 等を明示する想定。**信頼できる CLI のみ登録**（§15.10）。
- **メモリ**: 各エントリは起動時 `Box::leak` で `&'static` 化（プロセス全期間生存、reload なし）。ordinal は `6 + index`、ring_buffer の sent_marks HashMap キーに使う。
- トップレベル `system_prompt` / `language` は後方互換。`[ai]` 省略や空文字ならトップレベル値をコピー。

---

## 12. セルフアップデート (`--update`)

1. `detect_target()`（`target_for(os, arch)` 純関数）が `std::env::consts::{OS, ARCH}` からターゲット決定: linux/x86_64 → `x86_64-unknown-linux-musl`、linux/aarch64 → `aarch64-unknown-linux-musl`、macos/x86_64 → `x86_64-apple-darwin`、macos/aarch64 → `aarch64-apple-darwin`。他は `Unsupported platform` で拒否。`OS`/`ARCH` はコンパイル時固定なので自己申告は正確。
2. `curl` でリリース API を叩き `tag_name` 取得（チャネルは §15.11）。現バージョン一致なら `"Already up to date."`。
3. `aish-{target}` を一時ファイルへ DL。
4. **SHA256 検証**: 同リリースの `aish-{target}.sha256`（`<64-hex>  <filename>` 形式）を取得。ローカルは `sha256sum` を先に試し、spawn 失敗（macOS）なら `shasum -a 256` にフォールバック（出力形式同一、`parse_sha256_hash` 共通）。不一致／`.sha256` 未公開はいずれも fail-closed でエラー終了（インストールしない）。
5. `chmod 0755` → `current_exe()` へ `rename`（クロス FS は `copy` + 一時削除）。`current_exe()` 書き込みなのでインストール場所非依存、macOS でも実行中バイナリ置換可（旧 inode はプロセスが保持）。成功で `"Updated to v{latest}"`。

インストール先規約: **手動/self-update は `/usr/local/bin/aish`**（全 OS 共通、FHS のパッケージ管理外の正規位置、macOS SIP 回避、PATH 優先）。**deb/rpm の dest は `/usr/bin/aish`**（`[package.metadata.deb]`、パッケージ管理下なので FHS 上正しい）。両者は意図的に置き場が異なる。

`release.yml` が `aish-{target}.sha256` を生成しアセット公開。build matrix は Linux musl 2 種（cross + deb/rpm）+ macOS darwin 2 種（cargo ビルド・tar.gz と生バイナリ）。macOS は `shasum -a 256` で生成（形式は `sha256sum` 互換）。

---

## 13. エラー時の挙動

| 状況 | 挙動 |
|---|---|
| AI CLI 未インストール | 起動時エラー表示 + `exit 1` |
| 設定パース/読み込みエラー（既定パス） | 警告して `Config::default()` で続行 |
| 設定パース/読み込みエラー（`--config` 明示） | エラー終了（`exit 1`） |
| `--update` SHA256 検証失敗 / `.sha256` 取得失敗 | fail-closed でエラー終了 |
| AI CLI 実行失敗（非ゼロ終了） | `[{ai}] AI CLI failed: ...` + `Please check your login or usage limit.` 表示、ループ継続 |
| AI 出力が空 / JSON なし | `... returned empty output` / `No JSON found in ... output: ...` |
| AI キャンセル（Ctrl+C 中） | `^C` 表示後対話ループ終了。aish 継続 |
| PTY 終了 | 残り PTY 出力（logout 等）表示後 aish 終了 |

---

## 14. 既知の制約

- **Shift+Enter 改行**: kitty keyboard protocol (`\x1b[>1u`) 有効化が必要だが Enter/Esc/BS 等も別形式になり既存ハンドラと不整合。ターミナル横断で不安定なため**非対応**。改行は `Alt+Enter`。
- **Windows**: `pty_handler` は portable-pty 対応だが UI 部は Unix 限定。Windows は `read_line_cooked` フォールバックのみ。
- **リングバッファの UTF-8 境界**: マルチバイト切断時は `from_utf8_lossy` で置換文字。
- **シェル互換性**: readline / emacs 互換の行編集を持つ対話シェルを前提（bash / zsh emacs モード = macOS 既定）。打ちかけ消去の `Ctrl+A`+`Ctrl+K` (`0x01,0x0b`) は emacs 行編集依存のため、**zsh vi モード (`bindkey -v`) のみ**劣化し `^A^K` がリテラルで残ることがある（vim 等への流入と同じ穏当な失敗。クラッシュや未承認コマンド実行には至らない）。プロンプト戻り検出は `%`(zsh) / `#`(root) を終端集合に含むので zsh でも機能（§6.5）。

---

## 15. 実装ノート（落とし穴）

コードから直ちに読み取れず後から間違えやすい注意。CLAUDE.md「実装上の注意」の 1 行ルールが参照する詳細（理由・過去バグ経緯・エッジケース）の本体。

### 15.1 端末入力 framing / termios

- **raw モードはセッション全体で維持**（`save_terminal_settings`）。`passthrough` / `read_confirm_key` 個別の再設定・復元は不要。
- **低レベル入力 framing は `src/input.rs` に集約（唯一の場所）**。`ByteSource` から 1 byte ずつ読み、`next_event` が UTF-8 組み立て / ESC・CSI・SS3 解析 / poll+timeout を行い `InEvent { raw, tok }` を返す。`read_confirm_key` / `passthrough_read_raw` / `read_minibuffer_line` は `Tok` を消費するだけの薄い層。
- **新規コードで fd 0 直読み（`ManuallyDrop::from_raw_fd(0)`）を増やさない**（現状例外は `drain_stdin_nonblocking` と `query_cursor_position_dsr` の 2 つだけ。passthrough 停止中に同スレッドが読むので競合しない）。
- **`raw`（元バイト列）が主役、`tok`（分類）は副**: passthrough は必ず `ev.raw` をそのまま PTY へ送り、**`Tok::Char` を再エンコードして送らない**（invalid UTF-8 / Alt+非ASCII / paste / マウスシーケンスで壊れる）。focus event（`ESC[I`/`ESC[O`）は `Tok::FocusIn/FocusOut` を返し破棄判断は消費側（passthrough のみ破棄）。byte→Tok は golden test で固定（変えたらテストも更新）。partial CSI で blocking しハングした旧バグは全 poll 化で解消。
- **termios は `c_lflag`(ICANON|ECHO|ISIG|IEXTEN) に加え `c_iflag` raw 化群(IGNBRK|BRKINT|PARMRK|ISTRIP|INLCR|IGNCR|ICRNL|IXON) も落とす**。ICRNL を残すと Enter(`\r`) が driver 段で `\n` に変換され、`prompt_toolkit` 系ピッカー（`Keys.ControlM`=`\r` のみを確定にバインド）で Enter 無反応になる（`aws configure sso` のアカウント選択で再現）。**`c_oflag`(OPOST) は触らない**（`show_minibuffer` 等の `writeln!` が `\n` のみ書き端末の NL→CRLF 変換に依存）。
- **パススルーで PTY に転送する ESC シーケンスは完全な形まで読み切ってからまとめて送る**。CSI（`ESC [ ... <0x40-0x7E>`）も SS3（`ESC O <1 byte>`）も同様。分割 write すると受信側（vim 等）が ESC タイムアウトで別キー解釈（例: `ESC O`+`H` → `ESC`+`O`=open line above）。Home/End・F1〜F4・アプリケーションカーソルモード矢印が該当。
- **ESC sequence の追加 byte 読みはすべて poll(50ms) 付き**（継続 byte は `POLL_TIMEOUT_MS`、最初の 1 byte のみブロッキング）。blocking read だと partial sequence（whiptail のフォーカス切替断片送信等）で stdin read が固まり、raw mode 下では Ctrl+C も単なる `0x03` なので全キー入力が PTY に到達しなくなる（pi-hole installer + whiptail + Ghostty focus 切替で再現実績）。timeout したら溜めた seq_bytes を不完全なまま PTY 転送（transparent proxy 原則）。fail-safe で CSI バッファ長上限 `MAX_SEQ_LEN = 64`。`Ok(0)`(EOF/fd 切断) で未初期化 byte を push しないよう `Ok(1)` で厳密判定。
- 「bash readline」は readline / emacs 互換シェルの意（zsh emacs モード含む。詳細 §14）。

### 15.2 確認プロンプト Y/n/a/q（`read_confirm_key`）

- **1 キー即確定**（`src/ui.rs`、Enter 不要）。byte 読み・UTF-8・ESC 解析は `crate::input::next_event` に集約、`read_confirm_key` は `Tok` を解釈するだけ。
- 受理文字: ASCII `y/Y/n/N/a/A/q/Q` + IME 全角 `ｙ/Ｙ/ｎ/Ｎ/ａ/Ａ/ｑ/Ｑ` + ひらがな `あ`(= "a" の IME 確定) / `ん`(= "n")。Enter / Space はデフォルト Yes。**未知キーは無視して再読み取り**（打ち間違いで意図せず No になる事故を避ける）。
- **Enter が制御文字フィルタに飲まれる順序トラップ（過去 2 回再発）は `input::next_event` 側で構造解消済み**: Enter(`0x0a`/`0x0d`) は `b < 0x20` 判定より先に `Tok::Enter` に分類、golden test（`enter_is_not_swallowed_by_control_filter` 等）が回帰を防ぐ。
- **各キー semantics**: `y`/Enter/Space=実行、`n`=1 回スキップ、`a`=残り自動承認、`q`=残り中止（実行済みあれば AI follow-up・無ければ通常プロンプト）、`Ctrl+C`/`Ctrl+D`=残り中止かつ **AI に問わない**。**`ESC` 単独は `n` と同じ「1 回スキップ」**（旧「ESC=残り全部キャンセル」から変更。**ESC を Ctrl+C 系 abort arm に戻さない**）。q(`Quit`) と Ctrl+C/Ctrl+D(`ReadLineCancelled`) の唯一の違いは AI follow-up の有無で `ExecOutcome::{Quit,Abort}` として運ぶ（Abort は executed 非空でも follow-up せず `break`）。
- **キャンセル時（Ctrl+C/Ctrl+D）は抜ける前に必ず stdout へ `\n` を 1 つ出す**。出さないと直後のプロンプトリフレッシュがカーソルの残る `Exec? … [Y/n/a/q] ` 行を上書きして消す（ユーザ報告）。`n`/`y`/`a`/`q`/ESC は `echo_confirm` 末尾 `\n` でクリーン（ESC は char が無いので `'n'` を echo）。echo char を持たない Ctrl+C/Ctrl+D だけ abort arm で明示 `\n`（`Tok::Eof` は対象外）。
- **echo はマッチ入力 char をそのままの大小で 1 文字 + `\n` で手動描画**（raw mode で ECHO off）。「押したキー = 映る文字」を保つ。Enter は char が無い byte 段で先取りし `'Y'` を固定 echo（ESC も `'n'` を固定 echo）、Space は UTF-8 デコード経由で echo。**`echo_confirm` は `match` を持たず `write!("{c}\x1b[0m\n")` だけ**。大小区別の分岐を足さない（足すと「押下が常に大文字化される」旧バグと区別不能）。

### 15.3 AI 提案コマンド実行中の Ctrl+C 中断

- **確認後 PTY 送信・完了待ち中の Ctrl+C(0x03) は実行中コマンドを中断し残りも中止**。検知は `wait_for_command_completion`（`src/conversation.rs`）の stdin→PTY 転送部: drain した stdin に `0x03` があれば `interrupted` を立て、**バイトはそのまま PTY へ転送**（実行中コマンドへ Ctrl+C を届け SIGINT 中断。即 return すると `^C` + プロンプト復帰出力を取りこぼし画面/ring がずれるので転送だけして判定はプロンプト復帰まで遅延）。復帰時 `interrupted` なら `CommandWait::Interrupted` を返し、`confirm_and_execute` が当該コマンドを `executed` に積んだ上で `ExecOutcome::Abort` で即 return（残り送らず・**AI follow-up しない**）。
- **`Approval::All`(a) / `AskEach`(y) 両モードで一様**（`wait_for_command_completion` は承認モードを知らない）。**Ctrl+D(0x04) は対象外**（対話プログラムでは EOF として正当なので転送のみ）。SIGINT を無視するコマンドはプロンプトに戻らず待ち続ける（従来のハング挙動と同じ）。

### 15.4 minibuffer 描画・キャンセル・ペースト

- **aishプロンプト表示中は PTY 出力の画面描画を抑制**（`MINIBUFFER_ACTIVE`）。リングバッファ記録は継続。
- **`show_minibuffer` は入口で DSR(`\x1b[6n`) を投げ cursor row を取得、`row == rows`（画面下端）のときだけ `\n` で scroll 退避**。上半分なら入口 scroll しない。終了時は DECSC/DECRC でなく取得した `(row, col)` を `\x1b[{row - total_scrolled};{col}H` で**絶対座標復元**。`total_scrolled` = 入口 scroll(1/0) + grow scroll 累積で、`read_minibuffer_line` → `redraw_minibuffer` に `&mut u16` で渡し加算。
- **grow scroll は cursor を実画面最下行(`term_rows`)に置いた LF の全画面 scroll で行い DECSTBM の scroll region は使わない**（表示中は描画抑制で region で守る相手がいない。終了 `\x1b[r` は防御的リセットのみ）。理由: scroll region が全画面でない間のスクロールは上端から押し出された行を scrollback に保存せず破棄するのが xterm 系（Ghostty 含む）標準で、旧実装は「1 行伸びるたび直上行が恒久消失し終了 cursor 復元も 1 行ズレる」不具合があった（ユーザ報告）。**cursor を `term_rows` に置くことと DECSTBM 不使用は不可分**。grow は `was_at_bottom` に関わらず発火（minibuffer は常に画面最下行起点）。grow-shrink-grow の再 scroll は `reserved_rows`（高水位、shrink でも減らさない）で抑止し、**shrink branch の `\x1b[2K` 行クリアはこの空白不変条件を支えるので撤去しない**。saved_row が小さく grow が多いと `saturating_sub` clamp でプロンプト行が scrollback へ逃げるが内容は失われない既知の劣化。バイト列は golden test（`minibuffer_grow_*`/`minibuffer_shrink_*`）で固定。DSR 応答 `\x1b[{row};{col}R` は 80ms timeout で読む（`query_cursor_position_dsr`、`ManuallyDrop` で fd 0 を借り `libc::poll`）。応答なし端末は `was_at_bottom = false` fallback で安全側。すべて stdout 専用で PTY に送らない。alt screen 中に Ctrl+/ で出すと崩れるが `Ctrl+L` で redraw 可能な許容仕様。
- **キャンセル（ESC / Ctrl+C / Ctrl+/ / "exit" / 空 Ctrl+D）時は minibuffer 跡を clear + cursor 復元後、`InputEvent::MinibufferCancelled` を main loop に送り、main loop が `pty.refresh_prompt()`（打ちかけ消去 Ctrl+A+Ctrl+K → 改行）でシェルプロンプト再表示**（AI 対話終了 / slash command の refresh_prompt 経路と一貫）。`show_minibuffer` 側は **stdout に `\n` を出さない**（bash 自身が refresh_prompt の改行で新プロンプトを描くので二重空行を防ぐ）。cursor は saved 位置（bash readline の認識位置）に復元済みなので refresh_prompt の行消去 redisplay + 改行が正しい位置に当たる。**打ちかけは消えるが kill_line 後に改行するので未承認コマンドを submit しない**（信頼の根幹）。「打ちかけ温存」より「クリーンなプロンプト再表示」を優先（旧仕様は逆）。キャンセル直後に `PassthroughEnded` も届き両方で再 arm するのは送信経路と同じ既存パターン。refresh_prompt は `MinibufferCancelled` arm でのみ呼び二重実行しない。既知の制約: 全画面 TUI 表示中キャンセルすると refresh_prompt の `0x01,0x0b` + 改行がその TUI に届く（許容仕様）。
- **minibuffer は bracketed paste マーカー（`ESC[200~`/`ESC[201~` = `Tok::PasteStart`/`PasteEnd`）を honor**。マーカー間の改行（`Tok::Enter`）は送信でなく `\n` としてバッファ挿入（複数行ペーストが最初の改行で途中送信されない）、ペースト外の本物の Enter だけ送信。CRLF(`\r\n`) は `ev.raw` で判別し 1 つの `\n` に正規化。ペースト本文中の他トークン（Esc/Ctrl/矢印等）は誤爆防止で無視。**aish 自身は bracketed paste を有効化しない**（`ESC[?2004h` を送らない = 端末状態を変えない原則）。shell が readline で有効化済みのマーカーを利用するので、無効化環境（古い bash / dash）では複数行ペーストは最初の改行で送信される（既知の制約）。passthrough はマーカーも raw 転送するので inner program のペーストは不変。

### 15.5 打ちかけ消去 / `refresh_prompt`

- **打ちかけ消去（Ctrl+A + Ctrl+K = `0x01 0x0b`）は `PtyHandler::kill_line()` / `refresh_prompt()`（`src/pty_handler.rs`）にカプセル化し、`0x01,0x0b` リテラルを pty_handler.rs 以外に書かない**（grep で機械検査可能に保つ）。消去は AI 提案の「最初の実行」直前 1 回だけ（`confirm_and_execute` の `executed.is_empty()`）。show_minibuffer 終了（質問送信）時は送らない。
- **シェルプロンプトのリフレッシュ改行（slash 処理後 / AI 対話終了後 / minibuffer キャンセル）は必ず `refresh_prompt()` を使い素の `pty.write(b"\n")` を書かない**（refresh_prompt は改行直前に行消去する。消去側 write エラーは握りつぶし改行側 Result のみ返す旧コード互換）。さもないと Ctrl+/ 前の打ちかけ（passthrough で既に bash readline に到達済み）を改行が勝手に submit する（= 未承認コマンド実行。信頼の根幹）。Ctrl+C(`0x03`) でなく Ctrl+A+Ctrl+K を使うのは SIGINT を発火させず行消去だけしたいため（vim/top 等を意図せず kill しない）。bash readline 以外（vim 子プロセス等）に届くと `^A^K` リテラルが流れる副作用は残るが SIGINT 直撃より穏当。**打ちかけ温存は minibuffer を空 Enter で抜けた場合だけ**（改行を PTY に送らない）。cancel は上記のとおり消去。
- **AI 対話開始直前（`AiConversation::run` 冒頭、`get_unsent_for` 直前）で打ちかけを `0x01,0x0b` で消去してから drain する**。カーソルが bash readline モデルと同期しているこのタイミングでしか正しく消せないため必須（`show_minibuffer` が DSR で実カーソルを打ちかけ末尾に絶対座標復元した直後、まだ AI 出力を 1 文字も描いていない = 実カーソル == readline カーソル）。入れないと、打ちかけが端末幅を超え折り返している場合、対話終了後リフレッシュの Ctrl+A に対し bash が `ESC[A` を折り返し行数ぶん吐き、その上移動が aish の `Exec?` 行の上で起きてプロンプトが `Exec?` 行を上書きする（旧不具合: 折り返し打ちかけ + n キャンセルでプロンプトがコマンド表示を上書き）。消去 redisplay バイトは stdout に転送せず drain して捨てる（cursor 制御を今流すと AI 出力開始位置がずれる）。`ring_buffer` には追記し不変条件を保つ。`sleep(150ms)` は消去 redisplay の到着待ち。**撤去時は折り返し打ちかけ + n キャンセルで `Exec?` 行が上書きされないことを pyte ドライバ等で必ず再検証**。

### 15.6 PTY drain / 入力スレッド再開

- **PTY 出力の吸い出しは全て `pty_drain::drain_pty`（`src/pty_drain.rs`）経由**。手書きの `while let Ok(data) = pty_rx.try_recv()` ループを main.rs に再導入しない。「表示有無・先頭改行除去（`skip_leading_newline` は表示のみ trim）に関わらず吸い出した data は必ず trim 前の完全形で ring_buffer に記録される」不変条件を drain_pty 内で一元保証。チャンク内処理順（debug → 表示 → flush → 記録 → sniffer）と「表示 write 失敗はそのチャンクを記録せず伝播」も単体テストで固定。AI 対話直前の打ちかけ消去 + 消去 redisplay 吸収は `discard_stale_readline_input`（`src/conversation.rs`）に関数化。
- **通常動作中は PTY 出力に aish 独自の文字列を一切挿入しない**（パススルーに徹する）。
- **入力スレッド再開の 3 状態（idle / pending / 静音タイマ）は `InputGate`（`src/input_gate.rs`）に集約し、再 arm は `rearm_on_drop()` RAII guard で行う**。idle に戻る arm（AiPrompt / Line / PassthroughEnded / MinibufferCancelled）の入口で `let _rearm = gate.rearm_on_drop();` を取ると continue / break / `?` を含む全離脱経路で Drop が再 arm。`arm_passthrough` は private（旧実装は全 exit point への手書きで呼び忘れ = 入力ハングが 2 回発生）。PtyData arm（入力スレッド継続中）では guard を取らない。発行判定+送信+フラグ遷移は `maybe_request_passthrough()` に一体化（ばらの bool を再導入しない）。

### 15.7 trust ガード（承認 = 実行の保証）

- **AI 提案コマンドの制御文字ガードは `VettedCommand` 型（`src/vetted_command.rs`）に型化**。実行ループ先頭（`confirm_and_execute` の `Approval` 分岐より前）で `VettedCommand::vet` が検証し、制御文字（改行/CR/ESC/NUL/TAB/その他 C0/DEL/C1）を含むコマンドは Y/n/a/q に載せず `print_rejected_command` で表示して `continue`（PTY に送らない）。検証後は表示（`print_single_confirm_prompt`）と送信（`send_approved_command`）の両方が `&VettedCommand` しか受け付けないため、**「画面で承認した物 = サーバで実行される物」が型レベルで保たれ、ガードの撤去・迂回は型エラーになる**（vet は検証のみで文字列を変形しない。`as_str()` が同一スライスを返すことをテストで固定）。`[a]` 経路もこのガードを通る。確認結果は `ConfirmDecision`（Run/Skip/RunRest/QuitRest/AbortNoAi）、自動承認は `Approval`（AskEach/All）、抜けた理由は `ExecOutcome`（Completed/Quit/Abort）。複数行 / heredoc の明示承認は未実装（1 コマンド = 1 行を enforce）。
- **AI 由来の `message` / `commands` は端末描画前に制御文字を caret 可視化する**（`visualize_control_line`、`print_ai_message` / `print_ai_commands` / `print_single_confirm_prompt` で適用）。`\r` で行頭復帰・`\x1b[2K` で行消去して「見た目 ≠ 送るバイト」に偽装するのを防ぐ（ESC→`^[`、CR→`^M`、TAB→`^I`、NUL→`^@`）。AI 出力はプロンプトインジェクション経由で未信頼になり得るので**生 `println!` に戻さない**。`message` は複数行説明が正当なので `.lines()` 分割は維持し行内のみ可視化。
- **完了判定は `PromptSniffer` の passive 検出**。承認文字列をそのまま PTY に送り、出力末尾がプロンプト形（`[$#>%➜❯»][空白]+`）になり 200ms 静音で完了。

### 15.8 その他の UI ルール

- **TUI（vim / less / top 等）終了後は aish 側から何も出さない**。bash 単体と同じ「alt screen から戻った状態」に任せる。元はステータスバー復旧の `Ctrl+L` を撃っていたが、ステータスバー廃止（commit 7d13700）で動機が消え全廃。再導入する場合は vim insert モード中の誤発火（`^L` 混入）を避けるため passive 検出だけで一意に「終了」と判定できる根拠を用意すること（`\x1b[2J`・DECSTBM・alt screen 突入は TUI 動作中にも出るので終了判定に使えない）。
- **Shift+Enter 改行は非対応**（端末間で CSI u / legacy が揃わない）。改行は `Alt+Enter` のみ（詳細 §14）。
- **IME の未確定文字（preedit）は取得不能**。IME は OS 入力メソッド層で preedit を保持し確定まで stdin に 1 バイトも流れない（terminal emulator が overlay 描画するだけで PTY に到達しない）。Kitty keyboard protocol / CSI u でも変わらない（IME が下流）。確定済み文字でマッチするのが現実解。

### 15.9 ring_buffer / backend 解決

- **未送信 cursor は backend ごと独立**。`get_unsent_for(kind)` / `mark_sent_for(kind)` 経由で `/ai` 切替時に新 AI が会話の続きを catch-up。**sent_marks は `HashMap<usize, u64>`**（Generic を ordinal `6 + idx` で扱うため。旧固定長 `[u64; COUNT]` から変更）。entry が無いキーは 0（全部 catch-up）扱い。`mark_sent_all()` は `all_native()` + `all_generics()` 両方を回す。**新規 native backend は `all_native()` に追加**（実行ファイル名が enum 名と異なる場合は `binary()` に分岐追加。spawn / `check_installed` は `binary()`、設定 / 表示は `as_str()`）。
- **`BackendKind::parse(s)` は 2 段階**: (1) `parse_native` で built-in 6 種を直接 match、(2) hit しなければ `GENERIC_REGISTRY` を線形検索。`main::run()` で `Config::load` → `init_generics(&providers)` → 以降 parse の順序を守る。`validate_providers` が native 予約語衝突を起動時 reject。テストでは Generic resolution を直接検証しない（`OnceLock` がプロセス共有で並列テスト干渉）。Generic 動作確認は `src/ai/generic.rs` 単体テストで `Box::leak(Box::new(recipe))` 直書き。
- **Generic backend（`BackendKind::Generic(u8)`）は `[[ai.providers]]` registry 経由で動的解決**。registry は `init_generics` で OnceLock に 1 度 populate し `Box::leak` で `&'static`（display_name = recipe.name、binary）と `&'static ProviderRecipe` を確保。init 前 / index 範囲外は `"?"` フォールバック（panic せず spawn 失敗）。ユーザは built-in と generic を同じ flat namespace で参照（`generic:` prefix なし）。
- **AI 応答受信後の注釈記録は `RingBuffer::record_ai_exchange`（`src/ring_buffer.rs`）経由**。注釈（`[aish→<kind>]> ...` / `[ai/<kind>]> ...` / `[ai/<kind> suggests] ...`）の append → `mark_sent_for(current_kind)` の順序不変条件（current AI は再受信せず、他 backend は次回 catch-up で受信。逆順だと current AI が自分の発話をループ受信）をこのメソッドに閉じる。ばらの `append_text` + `mark_sent_for` を再導入しない。ラベル書式は `record_ai_exchange` が唯一の定義で単体テスト（`record_ai_exchange_format_is_stable`）で固定。
- **`/clear` は `mark_sent_all()` で全 backend の cursor を末尾に進める**が、AI CLI 内部 session/history は current backend のみリセット（他 backend instance を保持していないため）。「全 AI を仕切り直す」セマンティクスを守りつつ副作用最小化する妥協。
- ring_buffer の PTY 文字列と注釈ラベル（`[aish→...]` / `[ai/...]`）は衝突しうる（AI は文脈で区別する想定。多発したら XML 風へ変更余地）。

### 15.10 AI backends（ツール抑制 / 引数の罠）

- **Claude の system prompt（`--append-system-prompt`）は初回ターンのみ**（resume で二重 append を避ける）。**毎ターン守らせたいルール（出力フォーマットや `commands` の入れ方）は system prompt でなく `--json-schema` の field description に書く**（スキーマは毎ターン送られる）。`commands` description には「本文で実行コマンドを出したら配列にも入れる / 提案が無ければ空配列」の整合性ルールを入れる。**「commands を空にするな」式の強制は付けない**（不要時にモデルがコマンドを捻り出し余計な確認が出る）。もう 1 つの整合性ルールは「独立した複数コマンドを `;` 連結 1 本にせず `commands` 配列の別要素に分割」（codex 対策。共有 `build_system_prompt` と Claude の `commands` description の両方に入れる）。**ただし `&&`/`||` と `for`/`while`/`until`/`case`/`if` 等の制御構文内の `;` は 1 コマンドとして維持する例外を必ず併記**（分割すると `for i in 1 2 3; do …; done` 等が壊れる）。
- **Claude の `--disallowedTools` は `MANDATORY_DENY`(Bash/Edit/Write) を常に union する**（`effective_disallowed_tools`）。`[ai.claude].disallowed_tools` は単一文字列の全置換なので空にすると安全集合が消える footgun。`allow_unsafe_tools = false`（既定）の間は baseline を必ず混ぜ、`disallowed_tools = ""` でも Bash/Edit/Write は deny（Read だけ外せる）。`true` のときのみ verbatim（危険）。**`--disallowedTools` は args の末尾で push**（`--output-format`/`--json-schema`/`extra_args`/`--model`/`--effort` の後）。`extra_args = ["--disallowedTools", ""]` を後置きされても CLI 後勝ちでこちらが勝ち baseline を non-removable に保つため。**末尾 push を前方に戻したり union を外したりしない**。codex/copilot/cursor の deny 系は固定埋め込み（extra_args 後置き上書きは未対策）。
- **cursor backend は `--trust` を常時付与**（headless `-p` は `--trust` 無しだと `Workspace Trust Required` で非ゼロ終了）。config 不可で固定。`--yolo`/`-f`（Run Everything）は絶対に付けない。**ツール抑制は `--mode plan` + system prompt の二段**（個別ツール無効化フラグが無いため。`[ai.cursor].sandbox = "enabled"` 併用可）。`mode = ""`（通常モード）は確認 UI 迂回リスクで非推奨。Free プランは Named models 不可で `auto` のみ（`--model sonnet-4` 等は `EmptyOutput` エラー）。
- **copilot backend は `-p` フラグを付けない**（`-p` は positional/stdin と排他で stdin 渡しだと `too many arguments` で死ぬ。CLI が stdin 自動検出。他の `-p` 必須 backend と逆）。**ツール抑制は四段**: `--allow-all-tools`（非対話必須）+ `--deny-tool=shell` + `--deny-tool=write` + `--no-ask-user` + `--mode plan`（deny が allow に優先）。config 不可で固定。`--yolo`/`--allow-all` は絶対に付けない。**`--output-format json` は JSONL**（行ごとに `parse_jsonl_envelope` で走査し `assistant.message` の `data.content` と `result` の `sessionId` を取る。ephemeral 行は無視）。組織ポリシーで CLI 拒否され得る（`Access denied by policy settings`）。
- **Generic backend は安全フラグ（`--deny-tool` 等）を自動付与しない**。recipe 著者が `args` に `--mode plan` 等を明示する想定。信頼できない CLI を登録すると確認 UI を迂回して shell 実行される可能性がある（native のような自動 deny は無い）。§6.0 / `[[ai.providers]]` 安全性節を参照。

### 15.11 セルフアップデート 2 チャネル

- **`aish --update` は安定版 / 最新版の 2 チャネル**（`src/update.rs` の `UpdateChannel`）。`--stable`（既定）は GitHub `/releases/latest`（prerelease 除外の最新）、`--prerelease` は `/releases` 一覧の先頭 `[0]`（prerelease 含む絶対最新）。
- **命名の罠**: GitHub API では「安定版」が `/releases/latest`（"latest" を含む）、「先端」が `/releases` の先頭。"latest" がぶつかるのでユーザ向け flag をあえて `--prerelease` にしている（`--latest` だと逆に見える）。**この向きを逆にしない**。
- チャネル区別は `release.yml` の `prerelease: ${{ contains(tag, '-') }}` 依存（SemVer ハイフン付きタグを prerelease=true で公開し `Latest` バッジを付けない）。**prerelease リリース時は Cargo.toml の `version` にも識別子を含める**（`0.9.0-rc.1`）。タグだけ `-rc.1` で Cargo.toml が数値のみだと `run_update` の `latest == current` 比較が一致せず常に「更新あり」と誤判定される。target 解決 / DL / SHA256 検証 / 置換はチャネル非依存で共通。
