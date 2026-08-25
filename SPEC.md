# aish 仕様書

CLI SSH + AI (Claude Code) ツール。クライアント側の Claude Code から、ローカルシェルまたは SSH 接続先サーバを調査・操作する対話型 UI。

---

## 0. 用語

- **パススルーモード**: 通常のシェル操作状態。キー入力を PTY にそのまま転送。
- **aishプロンプト**（ミニバッファ）: `Ctrl+/` で開く `[aish]` 入力欄。最下行に表示し AI への質問を入力。ESC / Ctrl+C / Ctrl+/ でキャンセル。
- **ステータスバー**: 最下行の `aish v{version} | Ctrl+/ for AI` 行。
- **スピナー**: AI 応答待ちアニメーション（`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` + `Thinking...`）。
- **確認プロンプト**: AI 提案コマンドの実行可否を問う `Exec? {cmd} [y/n/A/q]`（最後／単一は `[Y/n]`）。
- **ReadLineモード**: 確認プロンプト応答など、ライン編集付き入力状態。

---

## 1. アーキテクチャ（ファイル構成）

| ファイル | 役割 |
|---|---|
| `main.rs` | メインループ（PTY 読み取り / ユーザ入力 / イベントの 3 スレッド）。slash command 処理 |
| `conversation.rs` | AI 対話 1 セッションの制御フロー (`AiConversation::run`)。打ちかけ消去 → send → Y/n/a 確認 → 実行 → 完了 passive 検出 → follow-up → 終了 refresh |
| `ui.rs` | ターミナル制御。raw モード（セッション全体で維持）、ライン編集、パススルー、ANSI 色、ミニバッファ |
| `input.rs` | 低レベル端末入力の framing（唯一の場所）。`ByteSource` → `next_event` → `Tok` |
| `input_gate.rs` | パススルー入力スレッド再開の 3 状態管理 (`InputGate` + `rearm_on_drop` RAII guard) |
| `ai/` | AI backend 層。`AiBackend` trait + claude/codex/gemini/qwen/cursor/copilot/cloudflare/nvidia/antigravity/grok/generic 実装、factory、共通 prompt/spawn (`common.rs`) |
| `config.rs` | TOML 設定ロード |
| `pty_handler.rs` | portable-pty による SSH / ローカルシェル起動。実端末サイズ + SIGWINCH 追従。`kill_line` / `refresh_prompt` / `send_approved_command` |
| `pty_drain.rs` | PTY 出力吸い出しの一元化 (`drain_pty` + `DrainOpts`) |
| `prompt_sniffer.rs` | シェルプロンプト復帰の passive 検出（終端文字学習つき） |
| `vetted_command.rs` | AI 提案コマンドの制御文字検証 newtype (`VettedCommand`) |
| `update.rs` | セルフアップデート (`--update`) |
| `ring_buffer.rs` | 1MB リングバッファ。ANSI 除去、backend 別差分送信、AI 注釈記録 (`record_ai_exchange`) |
| `mode.rs` | `Local` / `Remote` の 2 モード定義 |

---

## 2. 動作モード

| モード | 起動条件 | 挙動 |
|---|---|---|
| **Local** | SSH 引数なし (`aish`) | `$SHELL`（未定義なら `/bin/bash`）を PTY 起動 |
| **Remote** | SSH 引数あり (`aish user@host`) | `ssh` を PTY 起動。引数はそのまま ssh へ |

終了は `exit` または PTY プロセス終了。

---

## 3. コマンドラインオプション

| オプション | 意味 |
|---|---|
| `--version` / `-V` | バージョン表示して終了 |
| `--update` | 自己更新（§12）。`--stable`（既定）/ `--prerelease` の 2 チャネル |
| `--help` | ヘルプ表示して終了 |
| `--config <path>` | 設定ファイルパス（既定 `~/.aish/config.toml`） |
| `--ai <name>` | backend 選択。built-in または `[[ai.providers]]` の `name`。`[ai].backend` を上書き。built-in 名は予約語 |
| `--model <name>` | モデル名。`[ai].model` と `extra_args` の `-m` より優先 |
| `--effort <level>` | reasoning effort。claude → `--effort`、codex → `-c model_reasoning_effort=`、copilot → `--effort`、antigravity → `--effort`。gemini/qwen/cursor/grok は CLI 非対応で無視 |
| `--list-providers` | native + 組み込み + config の全 backend を出所タグ付き一覧表示 |
| それ以外 | SSH 引数として `ssh` に渡す |

---

## 4. UI 要素

### 4.1 ステータスバー
- 最下行に常時表示の 1 行。DECSTBM (`\x1b[1;{rows-1}r`) でスクロール領域を最下行除外に制限し `\x1b[{rows};1H` に描画。
- PTY 出力 50ms 静音で再描画 (`resize_status_bar`)。`\x1b7`/`\x1b8` でシェル側カーソル保全。SIGWINCH でも再設定。終了時 `\x1b[r` + `\x1b[2K`。

### 4.2 aishプロンプト（ミニバッファ）
- `Ctrl+/` (0x1F) で開く最下行入力欄。`[aish] ` ラベル + 入力テキスト。表示中は `MINIBUFFER_ACTIVE` で PTY 描画抑制（リング記録は継続）。
- 確定時: `[aish] {text}` を各論理行の先頭ラベル付きでエコー → 履歴追加（直前と同一なら追加せず）→ `InputEvent::AiPrompt(text)`。
- キャンセル経路: 単独 ESC / Ctrl+C / Ctrl+/ / 入力が `exit` のまま Enter。空 Enter は無操作（ステータスバー復元のみ）。
- 開く直前にシェル側コマンド入力中（`at_line_start == false`）なら、キャンセル/確定時に `0x03` を PTY に送り部分入力を破棄。

### 4.3 マルチライン入力
- 入力長に応じ縦拡張（最大 `term_rows / 2` 行）。`compute_visual_layout` が論理行・折り返しを計算（第1論理行はラベル幅 `label_width` 差し引き、継続行はインデント/ラベル付き）。
- 伸長時の行確保は全画面 LF スクロール、縮小時は `\x1b[2K` クリア、確保済み行数は高水位 `reserved_rows` で追跡（理由・不変条件は §15.4）。
- 総可視行数が `max_rows` 超過時はカーソル行が見えるまでスクロール (`scroll_top`)。

### 4.4 起動時の表示確保
- `setup_status_bar` は DECSTBM 設定前に `\n` を 1 回出しステータスバー 1 行分を確保。
- ターミナルタイトル `\x1b]2;[aish] {ssh_args}\x07`（Local は `[aish]` のみ）。終了時に空タイトルで復元。
- 通常動作中は PTY 出力に aish 独自の文字列を一切挿入しない。

### 4.5 スピナー
- ステータスバー行に 80ms 周期で `{thinking_color}{frame} {thinking_message}\x1b[0m`。`\x1b7`/`\x1b8` でシェル入力欄保全。`stop()` / Drop でステータスバー再描画。

### 4.6 確認プロンプト
- AI 提案 `commands` を番号付き全件表示（プラン提示）後、各コマンドごとに `Exec? {cmd} [y/n/A/q] ` を `confirm_color` で表示し 1 キー即確定（`read_confirm_key`）。最後／単一は `[Y/n]`。キーの意味は §15.2。

---

## 5. キー入力

### 5.1 パススルーモード
| キー | 動作 |
|---|---|
| `Ctrl+/` (0x1F) | aishプロンプトを開く |
| それ以外 | PTY へ直送（Enter, Ctrl+C, Tab, 矢印, ESC シーケンス等すべて） |
| フォーカスイベント `\x1b[I` / `\x1b[O` | 破棄（PTY へ送らない） |
| UTF-8 マルチバイト | 全バイト読み取り PTY へ |

`passthrough_read_raw` は `Ctrl+/` 受信か PTY EOF までループ継続。

### 5.2 aishプロンプト（ミニバッファ）
| キー | 動作 |
|---|---|
| `Enter` (`\r`/`\n`) | 確定。`exit` のみならキャンセル扱い |
| `Alt+Enter` (`\x1b\r`/`\x1b\n`) / `Shift+Enter` (CSI u `\x1b[13;Nu`) | 改行挿入（Shift+Enter は端末依存で届かないことあり） |
| `ESC` 単独 / `Ctrl+C` / `Ctrl+/` | キャンセル |
| `Ctrl+D` | 空ならキャンセル、否ならカーソル位置文字削除 |
| `BS`/`DEL` | カーソル左の文字削除 |
| `Ctrl+A`/`Home` / `Ctrl+E`/`End` | 行頭 / 行末 |
| `Ctrl+B`/`←` / `Ctrl+F`/`→` | 1 文字左 / 右 |
| `Ctrl+U` / `Ctrl+K` | カーソルより左 / 右を削除 |
| `Ctrl+W` | 直前の単語（空白区切り）削除 |
| `↑`/`↓` | 履歴ナビゲーション（新規入力は退避） |
| `Delete` (`\x1b[3~`) | カーソル位置文字削除 |

### 5.3 ReadLineモード（確認プロンプト応答）
- 矢印↑↓は履歴、編集キーは aishプロンプトと同等。`exit` で終了、他は `UserInput::ShellCommand` として PTY へ。

### 5.4 Slash command（aishプロンプト内）

先頭 `/` の入力は AI に送らずローカル処理し、結果を dim grey 表示。aish 側実装で AI CLI 自身の対話モード `/<cmd>` とは独立（CLI は非対話モード起動）。

| コマンド | 動作 |
|---|---|
| `/help` | 一覧表示 |
| `/effort [LEVEL]` | reasoning effort を runtime 変更（次回 send 以降）。引数なし=候補ピッカー（§15.12）、`-`/`clear`=クリア、その他=検証せず set。gemini/qwen/cursor/grok は保存のみ（antigravity は native 適用） |
| `/model [NAME]` | モデルを runtime 変更（session/history 維持）。引数の扱いは `/effort` と同じ |
| `/clear` | 会話履歴/セッションをクリア。claude/codex/cursor/copilot/generic(native resume) は session_id を None、gemini/qwen/cloudflare/nvidia/antigravity/grok/generic(非 native) は内部 history を空に |
| `/ai <NAME>` | backend 切替。`create_backend` で新規構築し現セッション破棄 |
| `/<unknown>` | 未知なら入力をそのまま AI へ（`/root/test.txt` 等も AI に届く） |

### 5.5 シグナル
- `SIGWINCH` → `sigwinch_handler` が `SIGWINCH_RECEIVED` をセットしメインループで非同期消費。
- SIGINT は OS 既定（raw モードで ISIG 無効のためキーボード Ctrl+C は SIGINT を発行しない）。

---

## 6. AI 連携

trait `AiBackend` で対応:
- **native backend**: Claude Code / OpenAI Codex CLI / Google Gemini CLI / Alibaba Qwen Code / Cursor Agent CLI / GitHub Copilot CLI / Cloudflare Workers AI / NVIDIA NIM / Google Antigravity CLI（`agy`）/ xAI Grok CLI（`grok`）（`src/ai/<name>.rs` に bespoke 実装）。
- **Generic CLI backend**: 任意 CLI を `src/ai/generic.rs` の単一 driver で解釈。recipe の出所は (a) 組み込みデフォルト recipe（`config::builtin_providers()` 同梱、zero-config で `--ai <name>` 可）、(b) ユーザの `[[ai.providers]]`。`name` は native 予約語と衝突不可。

各 backend は JSON で `{message, commands[]}` 相当を返し、aish は提案ベースで動作。透明性・サーバ無書き込みの原則は trait 実装側の責任。選択優先順位: `--ai` > `[ai].backend` > `claude`（既定）。

**組み込みデフォルト recipe**: aish が著者として安全フラグ込みで同梱（同梱基準は §15.10）。ユーザの `[[ai.providers]]` は同名エントリにフィールド単位マージ。マージ・検証は `AiConfig::resolve_providers`（`Config::load` 内）→ `resolved_providers` → `init_generics`。現同梱: `kimi`（§11.4）。

### 6.0 バックエンド能力差

| 機能 | Claude | Codex | Gemini | Qwen | Cursor | Copilot |
|---|---|---|---|---|---|---|
| 実行ファイル | `claude` | `codex` | `gemini` | `qwen` | `cursor-agent` | `copilot` |
| 非対話モード | `claude -p` | `codex exec -` | `gemini` (stdin) | `qwen` (stdin) | `cursor-agent -p --trust` (stdin) | `copilot` (stdin、`-p` 不可) |
| JSON 出力強制 | `--json-schema` | なし | なし | なし | `--output-format json`（外側ラッパのみ） | `--output-format json`（**JSONL**） |
| 危険ツール無効化 | `--disallowedTools` | `-s read-only` + `--disable` 12 種 | system prompt のみ | system prompt のみ | `--mode plan` + `--sandbox` + system prompt | 四段 deny（§15.10） |
| セッション再開 | `--resume <sid>`（JSON `session_id`） | `exec resume <UUID>`（rollout ファイル名） | best-effort (`--resume latest`) | best-effort (`--continue`) | `--resume <sid>`（JSON `session_id`） | `--resume <sid>`（JSONL `result.sessionId`） |
| reasoning effort | `--effort` | `-c model_reasoning_effort=` | なし | なし | なし | `--effort`（`none/low/medium/high/xhigh/max`） |

- プロンプト渡しは全 backend stdin。cloudflare/nvidia は表外（curl 経由 REST、§15.10）。
- **Antigravity（`agy`）/ Grok（`grok`）も表外**（Gemini 行と同型の system-prompt-only backend、§15.10）。Antigravity=`agy -p`（stdin）/ 実行ファイル `agy` / JSON 強制なし / 危険ツール無効化は system prompt のみ / resume `agy --continue`（best-effort）/ **reasoning effort `--effort low|medium|high` は native 対応**。Grok=`grok -p`（stdin）/ 実行ファイル `grok` / JSON 強制なし / 危険ツール無効化は system prompt のみ / resume なし / effort フラグなし / model は `-m`。両者とも headless で read-only/plan の permission-layer 強制を持たないため `--dangerously-skip-permissions` / `--always-approve` は絶対に付けない。
- JSON Schema 強制が無い backend は system prompt で `{"message":..., "commands":[...]}` 単独出力を指示し `extract_json` で抽出。失敗時は出力全体を `message` / `commands: []` でフォールバック。

**セッション履歴の持ち方**:
- Claude/Cursor/Copilot: 初回 send で `session_id` 捕獲、以降 `--resume <sid>`。Cursor/Copilot は `--append-system-prompt` 相当が無いので system prompt を初回プロンプト先頭に焼き込む（resume 後は再送しない）。Copilot は JSONL 行走査で最後の `assistant.message` の `data.content` を応答、`result` 行の `sessionId` を捕獲。
- Codex: 初回後 `~/.codex/sessions/.../rollout-...-<UUID>.jsonl` から UUID 捕獲し `codex exec resume <UUID>`（`--ephemeral` は付けない）。
- Gemini/Qwen/Cloudflare/Nvidia/Antigravity/Grok: 非対話の session resume が不安定/無いため、backend 内部で直近 8 ターン (user, ai) を保持し毎回プロンプトに再送（ring buffer 差分と合わせ multi-step の文脈を保つ）。

**安全性の差**（最大安全は `--ai claude` か `--ai copilot`）:
- **Claude**: `--disallowedTools "Bash,Edit,Write,Read"` でフラグレベル拒否。最も強力。
- **Codex**: `codex exec` はエージェントなので、確認 UI を迂回しないようツール系 feature 12 種を `--disable`（shell_tool / unified_exec / browser_use / computer_use / multi_agent / image_generation / tool_search / tool_suggest / plugins / apps / skill_mcp_dependency_install / tool_call_mcp_elicitation）+ `-s read-only`。純粋 LLM に退化。
- **Copilot**: claude 同等（四段 deny、§15.10）。
- **Cursor**: 個別ツール無効化フラグ無し。`--mode plan`（read-only、aish の提案セマンティクスと一致）既定 + `[ai.cursor].sandbox` + system prompt。`--trust` は headless 必須で常時付与。
- **Gemini/Qwen/Antigravity/Grok**: フラグ制約なし、system prompt の「ツール禁止」指示のみ（Antigravity/Grok の read-only 非強制の判断経緯は §15.10）。

### 6.1 起動
- 選択 backend のバイナリを `--version` で確認し、失敗なら「Please install ...」表示して終了（Claude のみインストールコマンドも表示）。実行ファイル名は `BackendKind::binary()`（cursor=`cursor-agent`、cloudflare=`curl`、他は `as_str()` と同じ）。

### 6.2 リクエスト引数（claude 例）
- 初回: `claude -p --append-system-prompt "{system_prompt} + 提案ルール" --output-format json --disallowedTools "..." --json-schema <SCHEMA> "<prompt>"`。
- 2 回目以降: `--append-system-prompt` を外し `--resume <session_id>` を付ける。`--disallowedTools` / `--json-schema` は毎回明示。
- 提案ルール要点: 「直接実行せず提案」「1 レスポンス 1 コマンド推奨」「`&&`/`||` は 1 コマンド維持」「独立コマンドは `commands` 配列に分割」。

### 6.3 JSON Schema
```json
{ "type": "object",
  "properties": { "message": {"type":"string"}, "commands": {"type":"array","items":{"type":"string"}} },
  "required": ["message", "commands"] }
```

### 6.4 プロンプト組み立て
- ` ```terminal\n{リングバッファのマーク以降（ANSI 除去済み）}\n``` ` + ユーザ入力。リングバッファが空なら `terminal` フェンスなし。

### 6.5 コマンド実行ループ
1. `message` を `ai_color` で表示。`commands` 空なら対話終了。
2. `commands` を番号付き全件表示。複数返っても 1 件ずつ確認。
3. **各コマンドを 1 つずつ** `Exec? {cmd} [y/n/A/q]` で確認（キーの意味は §15.2）し、承認分を**そのまま** `<cmd>\n` で PTY に送信（変形・ラップしない＝透明性が信頼の根幹）。
4. **完了待ちループ**（約 20ms 周期）: PTY ドレイン（表示 + ring 追記 + `PromptSniffer.feed()`）/ stdin→PTY 転送（ノンブロッキング。パスワード入力・Ctrl+C 中断可）/ SIGWINCH / 完了判定（`matches_prompt()` 真 + 200ms 静音）。
5. 1 つも実行されず、または Ctrl+C/Ctrl+D 中止なら AI に問わず終了。
6. 1 つ以上実行し Ctrl+C/Ctrl+D 以外で抜けたら、各コマンド実行サマリ + 出力本体（`terminal` フェンス）を AI へ送信（`q` 中止時は「残りを中止した」旨に切替）。1 へ戻る。
7. ループ終了後 PTY に `\n` を送りシェルプロンプト再描画。

`PromptSniffer` は ANSI 除去後末尾 256 バイトを保持し、最終行が `[終端文字][空白]+` で終わるか構造判定。既定終端文字 `$ # > % ➜ ❯ »`（`:` は ssh password prompt 誤検出で除外）。検出時 `record_match()` が終端文字を学習し、多段 SSH や `sudo bash` の PS1 変化にも追従。

**サーバ側に何の書き込み・注入もしない**（`PROMPT_COMMAND` 改変・history 抑制・shell 統合の自動セットアップ等は一切しない）。完了判定は passive 観察のみ。代償として exit code は取れず、AI は出力から成否を推測。

#### 既知の制約
- 終端が既定セット外のカスタムテーマ（oh-my-zsh robbyrussell 等）は初回検出で見逃す（`record_match` で学習可）。
- `tail -f` 等の連続出力は完了判定されない。ユーザ Ctrl+C で抜ける運用。
- 出力途中の偽プロンプト風文字列は 200ms 静音条件で大半救済。

### 6.6 キャンセル（AI プロセス実行中）
- stdin をノンブロッキング poll し `0x03` 検知で `child.kill()`。`"Cancelled"` 扱いで `^C` 表示し対話終了（aish は継続）。

### 6.7 セッション再開コマンド表示
- 終了時 `AiBackend::resume_command()` が `Some(cmd)` なら stderr に `Resume this <kind> session with:\n  <cmd>`。
- claude `claude --resume <UUID>` / codex `codex resume <UUID>` / gemini `gemini --resume latest` / qwen `qwen --continue` / cursor `cursor-agent --resume <UUID>` / copilot `copilot --resume <UUID>` / antigravity `agy --continue`（best-effort） / generic `<binary> <resume_flag> <sid>`。grok は resume コマンドを出さない（None）。
- gemini/qwen/antigravity は非対話 session 永続化が CLI 仕様で保証されず読み戻せないことがある。cursor/copilot は実機確認済み。

### 6.8 JSON 抽出
- `extract_json` が最外の `{...}` をバランス解析で抽出（前後テキスト混入対応）。
- `structured_output` があればそれ、無ければ `result` を採用。`result` が文字列なら JSON パース試行、失敗で `message: <そのまま>, commands: []`。

### 6.9 ログ
- `[log] enabled = true` 時、コマンドライン / レスポンス本文 / `[stderr] ...` を `path` に追記。各エントリは `=== YYYY-MM-DD HH:MM:SS ===` ヘッダ（ローカル TZ）。

---

## 7. リングバッファ

- 固定 1MB。書き込み位置 / 未送信位置（`sent_pos`）を保持。
- `append`: `strip_ansi_escapes::strip` で ANSI 除去後に格納。`get_unsent`: `sent_pos` 以降を `from_utf8_lossy` で返す。`mark_sent`: AI 応答取得成功時に呼ぶ。
- 満杯で未送信長が capacity 超なら `sent_pos = 0`（最新 1MB を送る）。

---

## 8. スレッド構成

- **PTY 読み取りスレッド**: `pty_reader.read(buf[4096])` ループ → `pty_tx`。EOF/エラーで `alive_tx.send(())`。
- **入力スレッド**: `prompt_rx` から `Passthrough` / `ReadLine` を受け、`PassthroughEnded` / `Line` で通知。
- **メインループ**（約 1ms ポーリング）: ① SIGWINCH ② PTY ドレイン（minibuffer 中は描画抑制）③ 50ms 静音でステータスバー再描画 ④ 入力 idle + 同条件でリクエスト送信 ⑤ PTY 終了検知 ⑥ 入力イベント処理。

---

## 9. 入力イベント管理

| フラグ / 状態 | 役割 |
|---|---|
| `pending_input` | 入力リクエストを次の安定点で送るべきか |
| `input_idle` | 入力スレッドが `recv()` 待機中か（キュー重複防止） |
| `MINIBUFFER_ACTIVE` | ミニバッファ表示中（画面描画抑制） |
| `SIGWINCH_RECEIVED` | 端末リサイズ要求 |
| `TERM_ROWS` | 端末高さキャッシュ |

- AI 対話終了でパススルーへ戻る直前、確認プロンプト ReadLine で `input_idle` が false のままなので、メインループ側で明示的に `input_idle = true` へ戻す（忘れるとリクエスト再送されずハング）。
- `Ctrl+/` 受信時は `PassthroughEnded` が届き `input_idle = true` に戻る。

---

## 10. ターミナル制御

### 10.1 termios
- `save_terminal_settings` で起動時 termios 保存と同時に raw モード化（`VMIN=1, VTIME=0`）。raw はセッション全体で維持し `restore_terminal_settings` で終了時復元。詳細は §15.1。

### 10.2 ANSI エスケープ
- DECSTBM `\x1b[r`: ミニバッファ終了時の防御的フルリセットのみ（aish は scroll region を設定しない。§15.4）。
- DECSC/DECRC `\x1b7`/`\x1b8`、CUP `\x1b[{row};{col}H`、EL `\x1b[K`/`\x1b[2K`、SGR `\x1b[0m` + ユーザ色（256色・TrueColor）。

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
| `prompt_label` / `prompt_color` | `[aish]` / `\x1b[38;5;208;48;2;50;35;20m` |
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
| `model` | `""` | 空で CLI 既定。`--model` で上書き可 |
| `effort` | `""` | claude/codex/copilot に変換、gemini/qwen/cursor は無視。`--effort` で上書き可 |
| `system_prompt` / `language` | `""` | 空ならトップレベルにフォールバック |

各 backend の設定例（`backend` 行を切り替えるだけ。`--ai <name>` でも同値を指定可）:

```toml
[ai]
backend = "claude"     # Claude Code (既定)
backend = "codex"      # Codex
backend = "gemini"     # Gemini
backend = "antigravity" # Antigravity CLI (`agy`, Gemini CLI 後継)
backend = "qwen"       # Qwen
backend = "cursor"     # Cursor
backend = "copilot"    # GitHub Copilot
backend = "grok"       # xAI Grok (`grok`, https://x.ai/cli)
backend = "kimi"       # Kimi (同梱 recipe)
backend = "opencode"   # OpenCode (同梱 recipe)
backend = "cloudflare" # Cloudflare Workers AI (認証は環境変数: CLOUDFLARE_ACCOUNT_ID / CLOUDFLARE_API_TOKEN)
backend = "nvidia"     # NVIDIA NIM (認証は環境変数: NVIDIA_API_KEY)
```

##### `/model` `/effort` 候補リスト（全 backend 共通フィールド）

各 backend テーブル（`[ai.claude]` 等 + `[ai.cloudflare-workers-ai]` + `[[ai.providers]]`）に `#[serde(flatten)]` で `OptionLists` が埋め込まれ、ピッカー候補を決める:

| キー | 既定値 | 説明 |
|---|---|---|
| `models` | `[]` | model 候補 (static) |
| `models_command` | `""` | model 候補取得コマンド（unix `sh -c` / windows `cmd /C` でローカル実行）。stdout 1 行 = 1 候補、失敗時は候補なし |
| `efforts` / `efforts_command` | `[]` / `""` | effort 候補（同上） |

候補解決の優先順位（`common::resolve_option_list`）: **static list 非空 > 取得コマンド > backend 組み込み既定**。取得コマンドはピッカーを開く時だけ実行。

**組み込み既定**: effort は claude `low/medium/high`、codex `minimal/low/medium/high`、copilot `none/low/medium/high/xhigh/max`、他は無し（effort 非適用 or recipe 由来のみ）。model は全 native backend に同梱（各 `MODEL_DEFAULTS` const。例: claude `claude-opus-4-8/sonnet-4-6/haiku-4-5`、codex `gpt-5.5/...`、cursor `auto/composer-1/…`、antigravity `gemini-3-pro/gemini-3-flash/…`、grok `grok-4/grok-4-fast/…`）。値は best-effort で更新にはリリースが必要（§15.12）。generic は recipe 由来のみ。

#### `[ai.claude]`
| キー | 既定値 | 説明 |
|---|---|---|
| `disallowed_tools` | `"Bash,Edit,Write,Read"` | `--disallowedTools` の値（単一文字列の全置換）。`MANDATORY_DENY`(Bash/Edit/Write) は常に union され空にできない（§15.10） |
| `allow_unsafe_tools` | `false` | `true` のときのみ `disallowed_tools` を verbatim 使用（危険） |
| `extra_args` | `[]` | 追加引数（ビルトイン引数の後ろ） |

#### `[ai.codex]` / `[ai.gemini]` / `[ai.qwen]` / `[ai.antigravity]` / `[ai.grok]`
- `extra_args` (`[]`): 各 CLI への追加引数（例 `["-m", "gpt-5.5"]`）。
- `[ai.antigravity]` は `models` / `efforts`、`[ai.grok]` は `models` のピッカー候補も持つ（`OptionLists`）。

#### `[ai.cursor]`
| キー | 既定値 | 説明 |
|---|---|---|
| `extra_args` | `[]` | ビルトイン引数（`-p --output-format json --trust`、`--mode <m>`、`--sandbox <s>`、`--resume <sid>`）の後ろ |
| `mode` | `"plan"` | `--mode` 値（`"plan"`/`"ask"`/`""`）。`"plan"` = read-only の安全側既定。`""` は危険 |
| `sandbox` | `""` | `--sandbox` 値（`"enabled"`/`"disabled"`）。空なら付けない（defense-in-depth 用） |

- `--trust` は headless 必須のため常時自動付与（config 不可）。Free プランは Named models 不可で `auto` のみ → `[ai].model = "auto"` か `--model auto`。

#### `[ai.copilot]`
| キー | 既定値 | 説明 |
|---|---|---|
| `extra_args` | `[]` | ビルトイン引数の後ろ |
| `mode` | `"plan"` | `--mode` 値（`"plan"`/`"interactive"`/`"autopilot"`/`""`）。`"plan"` が安全側既定 |

- 四段 deny + `--output-format json`(JSONL) は常時自動付与・config 不可（§15.10）。
- 認証は `gh auth login` / `copilot login` / `COPILOT_GITHUB_TOKEN`/`GH_TOKEN`/`GITHUB_TOKEN` env。組織ポリシーで拒否され得る（`Access denied by policy settings`）。

#### `[ai.cloudflare-workers-ai]`
- `OptionLists`（`/model` 候補）のみ。認証・モデルは環境変数 `CLOUDFLARE_ACCOUNT_ID` / `CLOUDFLARE_API_TOKEN` / `CLOUDFLARE_MODEL`（§15.10）。

#### 組み込みデフォルト recipe（zero-config provider）

`config::builtin_providers()` がバイナリ同梱の recipe 一覧を返す（追加・更新はこの 1 関数に集約）。設定なしで `--ai <name>` / `/ai <name>` 可。同梱基準は §15.10。

| name | binary | args（安全フラグ込み） | 備考 |
|---|---|---|---|
| `kimi` | `kimi` | `--plan --quiet` | MoonshotAI/kimi-cli。`--plan`=read-only tools、`--quiet`=非対話。prompt 渡し・出力形式は実機検証が要 |

`aish --list-providers` で出所タグ（`built-in` / `built-in, overridden` / `config`）付き一覧。

#### `[[ai.providers]]`（組み込み上書き / 新規 Generic CLI backend レシピ配列）

ユーザが書く上書き / 追加エントリ（`ProviderOverride`、各フィールド `Option` で presence 判定）。`AiConfig::resolve_providers` が組み込みにマージし最終 `ProviderRecipe` 一覧（`resolved_providers`）を作る: `name` が組み込みと一致 → `Some` フィールドだけ上書き（フィールド単位マージ、`args` 等 Vec は丸ごと置換）、不一致 → 新規 generic backend（`binary` 必須）。

`GenericCliBackend` が読むレシピのキー:

| キー | 既定値 | 説明 |
|---|---|---|
| `name` | (必須) | provider 一意識別子 |
| `binary` | (必須) | 実行ファイル名（PATH 検索）または絶対パス |
| `args` | `[]` | 固定引数。aish が動的引数（resume/model/effort/prompt）を後ろに追加 |
| `prompt_delivery` | `"stdin"` | `"stdin"` / `"arg"`(positional 末尾) / `"flag"`(`prompt_flag` の値) |
| `prompt_flag` | `""` | `prompt_delivery="flag"` のとき必須 |
| `parse` | `"lossy"` | `"lossy"` / `"extract_json"` / `"jsonl"` |
| `jsonl_content_path` / `jsonl_session_path` | `""` | `parse="jsonl"` 時の `"type:dot.path"` |
| `session_id_path` | `""` | `parse="extract_json"` 時の session_id フィールド名（top-level） |
| `resume_flag` | `""` | resume 引数名。native resume は本キーと session id パス（`parse` に応じ `jsonl_session_path` / `session_id_path`）の両方非空で有効 |
| `model_flag` / `effort_flag` | `""` | 空なら渡さない（effort は保存のみ） |
| `color` | `208` | 256-color（ラベル・banner 色） |
| `system_prompt_inline` | `true` | `true`: 初回プロンプト先頭に焼き込み。`false`: 毎回 history + system + context 再構築 |
| `history_turns` | `8` | native resume 無効時の内部保持ターン数 |

起動時 `validate_recipes` で検証（配列長 ≤ 256 / `name` 一意 / native 予約語と非衝突 / `binary` 非空 / `parse`・`prompt_delivery` 妥当性 / `flag` 時 `prompt_flag` 非空）。予約語は `BackendKind::all_native()` から導出。不正なら `Invalid [[ai.providers]] in <path>: ...` で起動拒否。同名ユーザエントリは重複エラーにせず後勝ちマージ。

- **安全性**: 新規 provider は強制 deny フラグを付けない。利用者が `args` に `--mode plan` 等を明示する想定。**信頼できる CLI のみ登録**（§15.10）。
- **メモリ**: 各エントリは起動時 `Box::leak` で `&'static` 化（reload なし）。ordinal は `NATIVE_COUNT + index`（native 0..=(NATIVE_COUNT-1) の後。ring_buffer の sent_marks キー。NATIVE_COUNT は native backend 追加のたびに増える）。
- トップレベル `system_prompt` / `language` は後方互換（`[ai]` 側が空ならコピー）。

---

## 12. セルフアップデート (`--update`)

1. `detect_target()`（`target_for(os, arch)` 純関数）が `std::env::consts::{OS, ARCH}`（コンパイル時固定なので正確）からターゲット決定: linux/x86_64 → `x86_64-unknown-linux-musl`、linux/aarch64 → `aarch64-unknown-linux-musl`、macos/x86_64 → `x86_64-apple-darwin`、macos/aarch64 → `aarch64-apple-darwin`。他は `Unsupported platform`。
2. `curl` でリリース API から `tag_name` 取得（チャネルは §15.11）。現バージョン一致なら `"Already up to date."`。
3. `aish-{target}` をインストール先と同一ディレクトリの隠し一時ファイル（`.aish-update-{pid}`）へ DL（§15.11）。
4. **SHA256 検証**: 同リリースの `aish-{target}.sha256`（`<64-hex>  <filename>`）を取得し、ローカルは `sha256sum` → 失敗時（macOS）`shasum -a 256` フォールバック（`parse_sha256_hash` 共通）。不一致／`.sha256` 未公開は fail-closed でエラー終了。
5. `chmod 0755` → `current_exe()` へ `rename`（原子置換のみ、copy fallback なし、§15.11）。成功で `"Updated to v{latest}"`。

インストール先規約: **手動/self-update は `/usr/local/bin/aish`**（パッケージ管理外の FHS 正規位置、macOS SIP 回避、PATH 優先）、**deb/rpm は `/usr/bin/aish`**（`[package.metadata.deb]`、パッケージ管理下）。意図的に異なる。

`release.yml` が `.sha256` を生成・公開。build matrix は Linux musl 2 種（cross + deb/rpm）+ macOS darwin 2 種（cargo ビルド・tar.gz と生バイナリ）。macOS は `shasum -a 256` で生成（`sha256sum` 互換形式）。

---

## 13. エラー時の挙動

| 状況 | 挙動 |
|---|---|
| AI CLI 未インストール | 起動時エラー表示 + `exit 1` |
| 設定エラー（既定パス） | 警告して `Config::default()` で続行 |
| 設定エラー（`--config` 明示） | エラー終了（`exit 1`） |
| `--update` SHA256 検証失敗 / `.sha256` 取得失敗 | fail-closed でエラー終了 |
| AI CLI 実行失敗（非ゼロ終了） | `[{ai}] AI CLI failed: ...` + `Please check your login or usage limit.`、ループ継続 |
| AI 出力が空 / JSON なし | `... returned empty output` / `No JSON found in ... output: ...` |
| AI キャンセル（Ctrl+C） | `^C` 表示後対話ループ終了。aish 継続 |
| PTY 終了 | 残り PTY 出力（logout 等）表示後 aish 終了 |

---

## 14. 既知の制約

- **Shift+Enter 改行**: kitty keyboard protocol (`\x1b[>1u`) は Enter/Esc/BS も別形式になり既存ハンドラと不整合、端末横断で不安定なため**非対応**。改行は `Alt+Enter`。
- **Windows**: `pty_handler` は portable-pty 対応だが UI 部は Unix 限定。`read_line_cooked` フォールバックのみ。
- **リングバッファの UTF-8 境界**: マルチバイト切断時は `from_utf8_lossy` で置換文字。
- **シェル互換性**: readline / emacs 互換の行編集を持つ対話シェル前提（bash / zsh emacs モード）。打ちかけ消去の `0x01,0x0b` は emacs 行編集依存で、**zsh vi モード (`bindkey -v`) のみ** `^A^K` がリテラルで残る劣化あり（未承認コマンド実行には至らない）。プロンプト検出は `%`/`#` を含むので zsh でも機能。

---

## 15. 実装ノート（落とし穴）

コードから直ちに読み取れず後から間違えやすい注意。CLAUDE.md「実装上の注意」の 1 行ルールが参照する詳細（理由・過去バグ・エッジケース）の本体。

### 15.1 端末入力 framing / termios

- **raw モードはセッション全体で維持**（`save_terminal_settings`）。個別の再設定・復元はしない。
- **framing は `src/input.rs` が唯一の場所**: `ByteSource` から 1 byte ずつ読み、`next_event` が UTF-8 組み立て / ESC・CSI・SS3 解析 / poll を行い `InEvent { raw, tok }` を返す。`read_confirm_key` / `passthrough_read_raw` / `read_minibuffer_line` は `Tok` を消費する薄い層。**fd 0 直読み（`ManuallyDrop::from_raw_fd(0)`）を増やさない**（例外は `drain_stdin_nonblocking` と `query_cursor_position_dsr` のみ。passthrough 停止中に同スレッドが読むので競合しない）。
- **`raw`（元バイト列）が主・`tok` は副**: passthrough は必ず `ev.raw` を PTY へ送り、`Tok::Char` を再エンコードしない（invalid UTF-8 / Alt+非ASCII / paste / マウスシーケンスで壊れる）。focus event（`ESC[I`/`ESC[O` = `Tok::FocusIn/FocusOut`）の破棄判断は消費側（passthrough のみ破棄）。byte→Tok は golden test で固定。
- **termios は `c_lflag`(ICANON|ECHO|ISIG|IEXTEN) + `c_iflag` raw 化群(IGNBRK|BRKINT|PARMRK|ISTRIP|INLCR|IGNCR|ICRNL|IXON) を落とす**。ICRNL が残ると Enter(`\r`) が `\n` に化け、`\r` のみを確定にバインドする prompt_toolkit 系ピッカーで Enter 無反応（過去バグ: `aws configure sso` で再現）。**`c_oflag`(OPOST) は触らない**（`writeln!` が端末の NL→CRLF 変換に依存）。
- **PTY へ転送する ESC/CSI/SS3 は完全形まで読み切って一括送信**（CSI=`ESC [ … <0x40-0x7E>`、SS3=`ESC O <1 byte>`）。分割 write は受信側の ESC タイムアウト誤解釈を招く（例: vim で `ESC O`+`H` = open line above）。Home/End・F1〜F4・アプリケーションカーソル矢印が該当。
- **追加 byte 読みは全て poll(50ms=`POLL_TIMEOUT_MS`) 付き**（最初の 1 byte のみブロッキング）。blocking だと partial sequence で全入力ハング — raw mode では Ctrl+C も単なる `0x03`（過去バグ: whiptail の断片送信で再現）。timeout 時は不完全なまま PTY 転送（transparent proxy 原則）。上限 `MAX_SEQ_LEN = 64`。`Ok(1)` で厳密判定し `Ok(0)`(EOF) の未初期化 byte を push しない。
- **win32-input-mode（`ESC[Vk;Sc;Uc;Kd;Cs;Rc_`、`_`=0x5F 終端）のデコード**: Windows Terminal + PowerShell では **PSReadLine がこのモードを有効化**し、キー入力が `ReadConsoleInputW` の KEY_EVENT でなくこの CSI シーケンスのバイト列で aish に届く（2026-07 実測: Ctrl+/ = `ESC[191;53;0;1;40;1_`、Vk=191=0xBF=VK_OEM_2、Cs=0x28 に LEFT_CTRL_PRESSED=0x8）。`_` は CSI 終端範囲(0x40-0x7E)なので `decode_csi` が既に 1 つの `InEvent`(raw 保持) に framing 済み → `classify_csi` の先頭で `final_byte==b'_'` なら純関数 `classify_win32_input_mode(params)` に委譲する。**`#[cfg]` を付けない**（Unix では来ず無害、ubuntu CI で golden test を回すため）。パラメータを `Vk;Sc;Uc;Kd;Cs;Rc` に数値化し、**key-down(Kd≠0) のみ `Some(Tok)`**、key-up(Kd=0)・非数値・フィールド 4 未満は `None`→`EscSeq` に落とす（全 tok 消費者が EscSeq を無視し passthrough は raw を送るので、1 キー=down/up 2 連でも二重入力にならず新 Tok variant も不要）。マッピング: Ctrl(Cs に 0x8/0x4)+（Vk=0xBF or Uc∈{0x1f,0x2f}）→ `Ctrl(0x1f)`（term/windows.rs の pump 正規化と同値・エントリキー）、Vk で Enter/Backspace/Esc/矢印/Home/End/Delete、Uc<0x20 → `Ctrl(uc)`（Ctrl+C 等）、印字可能 BMP → `Char`。**非 BMP=サロゲート（1 record=1 UTF-16 unit）は None フォールバック**（稀。passthrough は raw で無事）。これがないと **Ctrl+/ が PowerShell に素通りしミニバッファが開かない**（実測: Ctrl+/ 後の入力が PowerShell コマンド化し CommandNotFoundException、文字が PSReadLine の構文ハイライトで黄色くなる）。全 UI（confirm/picker/minibuffer/passthrough）が `next_event`→`ev.tok` 経由なので 1 箇所で直る。
- 「bash readline」= readline / emacs 互換シェルの意（§14）。

### 15.2 確認プロンプト y/n/A/q（`read_confirm_key`）

- **1 キー即確定**（`src/ui.rs`、Enter 不要）。byte 解析は `input::next_event` に集約。
- 受理: `y/Y/n/N/a/A/q/Q` + IME 全角 `ｙＹｎＮａＡｑＱ` + ひらがな `あ`(=a) / `ん`(=n)。Space はデフォルト Yes（文脈に依らず）。**未知キーは無視して再読み取り**（打ち間違いを No にしない）。
- **Enter は `b < 0x20` 判定より先に `Tok::Enter` に分類**（「Enter が効かない」回帰が過去 2 回。golden test `enter_is_not_swallowed_by_control_filter` 等で固定）。
- **Enter のデフォルトは文脈依存**（2026-08 変更。ユーザ要望「複数コマンド時は Enter で残り全部承認したい」）。**残コマンドあり = プロンプト `[y/n/A/q]` で Enter=All（残り自動承認）、echo `A`**。**最後／単一コマンド = プロンプト `[Y/n]` で Enter=Yes、echo `Y`**。文脈は `AiConversation::confirm_and_execute` が `i + 1 < total`（= `print_single_confirm_prompt` の `index < total`）を `InputRequest::ReadConfirmKey { default_all }` で入力スレッドへ渡し、`read_confirm_key(default_all)` の `Tok::Enter` 分岐で分ける。`default_all` は Enter のみに効き、Space（常に Yes）・明示キーには影響しない。プロンプト文字列の大文字（`A` / `Y`）と echo される既定文字を一致させて「Enter で何が起きるか」を視覚的に一致させている。
- **キー semantics**: `y`/Space=実行、Enter=デフォルト（上記）、`n`=1 回スキップ、`a`=残り自動承認、`q`=残り中止（実行済みあれば AI follow-up）、`Ctrl+C`/`Ctrl+D`=残り中止かつ **AI に問わない**。**ESC 単独は `n` と同じ 1 回スキップ**（旧「残り全部キャンセル」から変更。**Ctrl+C 系 abort arm に戻さない**）。Quit と Abort の差は follow-up の有無のみ（`ExecOutcome::{Quit,Abort}`。Abort は executed 非空でも follow-up せず `break`）。
- **AI 応答の `command_result_followup: false` は実行後の AI 自動問い合わせ（follow-up）を抑制**（2026-07 追加）。「コマンドを教えてほしいだけ」のとき実行結果を AI に送り返す待ち時間が煩わしい、という要望に対し AI 自身が「出力確認が必要か」を毎ターン宣言する設計。`AiConversation::run` の follow-up 送信直前で `!response.command_result_followup → break`。**false のときは `q`（Quit）でも follow-up しない**（false = 一切 follow-up なしで一貫。Abort は従来どおり常になし）。**欠落時は true（従来動作）**: `#[serde(default)]` で、フラグを出さないモデル・lossy フォールバック（この場合 commands 空で follow-up 自体起きない）でも調査ループが壊れない後方互換。follow-up しなくても実行結果は ring_buffer の未送信 cursor に残り、次のユーザ質問時に terminal コンテキストとして送られる（情報は失われない）。承認ゲート Y/n/a/q は不変（信頼の根幹への影響なし）。
- **Ctrl+C/Ctrl+D キャンセルは抜ける前に必ず stdout へ `\n` を 1 つ出す**（`Tok::Eof` は対象外）。出さないと直後のリフレッシュが `Exec? …` 行を上書きして消す（過去バグ）。他キーは `echo_confirm` 末尾 `\n` でクリーン。
- echo は押されたキー 1 文字を大小そのまま + `\n` で手動描画（raw mode で ECHO off のため）。Enter は `'Y'`、ESC は `'n'` を固定 echo、Space は UTF-8 デコード経由。**`echo_confirm` は `match` を持たず `write!("{c}\x1b[0m\n")` だけ。大小区別の分岐を足さない**（「押下が常に大文字化」旧バグと区別不能になる）。

### 15.3 AI 提案コマンド実行中の Ctrl+C 中断

- **完了待ち中の Ctrl+C(0x03) は実行中コマンドを中断し残りも中止**。検知は `wait_for_command_completion`（`src/conversation.rs`）の stdin→PTY 転送部: `interrupted` を立てつつ**バイトは PTY へ転送**（SIGINT を実行中コマンドへ届ける。即 return は `^C`+復帰出力を取りこぼし画面/ring がずれるため、判定はプロンプト復帰まで遅延）。復帰後 `CommandWait::Interrupted` → `confirm_and_execute` が当該コマンドを `executed` に積み `ExecOutcome::Abort` で即 return（残り送らず・AI follow-up なし）。
- **`Approval::All`(a) / `AskEach`(y) 両モードで一様**（wait 側は承認モードを知らない）。**Ctrl+D(0x04) は対象外**（対話プログラムでは EOF として正当。転送のみ）。SIGINT を無視するコマンドは従来どおり待ち続ける。

### 15.4 minibuffer 描画・キャンセル・ペースト

- **表示中は PTY 出力の画面描画を抑制**（`MINIBUFFER_ACTIVE`）。リング記録は継続。
- **入口で DSR(`\x1b[6n`) により cursor row を取得し、`row == rows`（画面下端）のときだけ `\n` で 1 行退避**。終了時は DECSC/DECRC でなく `\x1b[{row - total_scrolled};{col}H` で**絶対座標復元**（`total_scrolled` = 入口 + grow scroll の累積。`read_minibuffer_line` → `redraw_minibuffer` に `&mut u16` で伝搬）。DSR 応答は 80ms timeout で読み（`query_cursor_position_dsr`）、無応答端末は `was_at_bottom = false` fallback。stdout 専用で PTY に送らない。alt screen 中に開くと崩れるが `Ctrl+L` で redraw 可の許容仕様。
- **grow scroll は cursor を実画面最下行(`term_rows`)に置いた LF の全画面 scroll。DECSTBM の scroll region は使わない**。全画面でない region scroll は押し出し行を scrollback に保存せず破棄するのが xterm 系標準（過去バグ: 1 行伸びるたび直上行が恒久消失 + cursor 復元 1 行ズレ）。**cursor を `term_rows` に置くことと DECSTBM 不使用は不可分**。grow は `was_at_bottom` に関わらず発火（minibuffer は常に最下行起点）。grow-shrink-grow の再 scroll は高水位 `reserved_rows`（shrink でも減らさない）で抑止し、**shrink branch の `\x1b[2K` 行クリアはこの空白不変条件を支えるので撤去しない**。saved_row が小さく grow が多いと clamp でプロンプト行が scrollback へ逃げる既知の劣化（内容は残る）。golden test `minibuffer_grow_*`/`minibuffer_shrink_*` で固定。
- **キャンセル（ESC / Ctrl+C / Ctrl+/ / "exit" / 空 Ctrl+D）は跡を clear + cursor 復元後、`InputEvent::MinibufferCancelled` → main loop が `pty.refresh_prompt()`（打ちかけ消去 → 改行）でプロンプト再表示**（他の refresh 経路と一貫）。`show_minibuffer` 側は **stdout に `\n` を出さない**（bash が新プロンプトを描くので二重空行防止）。**打ちかけは消えるが kill_line 後の改行なので未承認 submit しない**（信頼の根幹）。「クリーンな再表示」を「打ちかけ温存」より優先（旧仕様は逆）。refresh_prompt は `MinibufferCancelled` arm でのみ呼ぶ（二重実行しない）。既知: 全画面 TUI 中のキャンセルは `0x01,0x0b`+改行がその TUI に届く（許容）。
- **bracketed paste マーカー（`ESC[200~`/`201~` = `Tok::PasteStart/End`）を honor**: マーカー間の Enter は送信でなく `\n` 挿入（複数行ペーストの途中送信防止）、CRLF は `ev.raw` で判別し 1 つの `\n` に正規化、ペースト中の他トークンは無視。**aish 自身は `ESC[?2004h` を出さない**（端末状態を変えない原則。shell が有効化済みのマーカーを利用）。readline 無効環境（古い bash / dash）は最初の改行で送信される既知の制約。passthrough はマーカーも raw 転送。

### 15.5 打ちかけ消去 / `refresh_prompt`

- **打ちかけ消去（Ctrl+A+Ctrl+K = `0x01 0x0b`）は `PtyHandler::kill_line()` / `refresh_prompt()` にカプセル化し、リテラルを `pty_handler.rs` 以外に書かない**（grep で機械検査可能に保つ）。消去は AI 提案の「最初の実行」直前 1 回だけ（`confirm_and_execute` の `executed.is_empty()`）。minibuffer 確定（質問送信）時は送らない。
- **リフレッシュ改行（slash 処理後 / AI 対話終了後 / minibuffer キャンセル）は必ず `refresh_prompt()`（行消去→改行）。素の `pty.write(b"\n")` は Ctrl+/ 前の打ちかけ（既に readline に到達済み）を submit する**（= 未承認コマンド実行。信頼の根幹）。`0x03` でなく `0x01,0x0b` なのは SIGINT を発火させないため（vim/top を kill しない）。非 readline プログラムに `^A^K` リテラルが流れる副作用は SIGINT より穏当。**打ちかけ温存は minibuffer を空 Enter で抜けた場合だけ**（改行を送らない）。消去側 write エラーは握りつぶし改行側 Result のみ返す。
- **AI 対話開始直前（`AiConversation::run` 冒頭、`get_unsent_for` 直前）に打ちかけを消去してから drain**。実カーソル == readline カーソルはこの瞬間だけ（DSR 絶対復元直後・AI 出力前）なので、ここでしか正しく消せない。入れないと折り返し打ちかけ + `n` キャンセルで bash の `ESC[A` 上移動が `Exec?` 行を上書きする（過去バグ）。消去 redisplay バイトは表示せず drain して捨てる（cursor 制御を流すと AI 出力開始位置がずれる）が `ring_buffer` には追記（不変条件）。`sleep(150ms)` は redisplay 到着待ち。**撤去時は折り返し打ちかけ + n キャンセルを pyte 等で必ず再検証**。

### 15.6 PTY drain / 入力スレッド再開

- **PTY 吸い出しは全て `pty_drain::drain_pty` 経由**。手書き `try_recv` ループを main.rs に再導入しない。不変条件「表示有無・先頭改行 trim（`skip_leading_newline` は表示のみ）に関わらず data は trim 前の完全形で ring_buffer に記録」を一元保証。チャンク内処理順（debug → 表示 → flush → 記録 → sniffer）と「表示 write 失敗はそのチャンクを記録せず伝播」は単体テストで固定。AI 対話直前の消去+吸収は `discard_stale_readline_input`（`src/conversation.rs`）に関数化。
- **通常動作中は PTY 出力に aish 独自の文字列を挿入しない**。
- **入力スレッド再開の 3 状態（idle / pending / 静音タイマ）は `InputGate` に集約し、再 arm は `rearm_on_drop()` RAII guard**。idle に戻る arm（AiPrompt / Line / PassthroughEnded / MinibufferCancelled）の入口で `let _rearm = gate.rearm_on_drop();` を取れば continue / break / `?` 全経路で Drop が再 arm。`arm_passthrough` は private（過去バグ: 手書き呼び出しの呼び忘れで入力ハング 2 回）。PtyData arm（入力スレッド継続中）では guard を取らない。判定+送信+フラグ遷移は `maybe_request_passthrough()` に一体化（ばらの bool を再導入しない）。

### 15.7 trust ガード（承認 = 実行の保証）

- **制御文字ガードは `VettedCommand` 型（`src/vetted_command.rs`）**。実行ループ先頭（`Approval` 分岐前）で `VettedCommand::vet` が検証し、制御文字（LF/CR/ESC/NUL/TAB/他 C0/DEL/C1）入りは確認に載せず `print_rejected_command` → `continue`（PTY に送らない）。検証後は表示（`print_single_confirm_prompt`）も送信（`send_approved_command`）も `&VettedCommand` のみ受理 → **「画面で承認した物 = サーバで実行される物」が型レベルで保たれ、撤去・迂回は型エラー**（vet は検証のみで変形しない。`as_str()` が同一スライスを返すことをテストで固定）。`[a]` 経路も通る。関連型: `ConfirmDecision`（Run/Skip/RunRest/QuitRest/AbortNoAi）/ `Approval`（AskEach/All）/ `ExecOutcome`（Completed/Quit/Abort）。複数行 / heredoc の明示承認は未実装（1 コマンド = 1 行を enforce）。
- **AI 由来の `message` / `commands` は描画前に制御文字を caret 可視化**（`visualize_control_line`、`print_ai_message` / `print_ai_commands` / `print_single_confirm_prompt` で適用。ESC→`^[`、CR→`^M`、TAB→`^I`、NUL→`^@`）。`\r` 行頭復帰 + `\x1b[2K` 行消去による「見た目 ≠ 送るバイト」偽装を防ぐ。AI 出力はプロンプトインジェクションで未信頼になり得るので**生 `println!` に戻さない**。`message` は複数行が正当なので `.lines()` 分割を維持し行内のみ可視化。
- **完了判定は `PromptSniffer` の passive 検出**（§6.5）。承認文字列はそのまま PTY へ。

### 15.8 その他の UI ルール

- **起動バナー（`print_startup_banner`）は PTY spawn より前に描画する**。Windows Terminal + PowerShell（ConPTY）では、**ConPTY が spawn 時点の実カーソル位置を基準に子シェルの描画をアンカーする**。spawn 後にバナーを出すと、ConPTY 内で生成された初回プロンプト（row1 への絶対位置 `\x1b[H` 等を含む）が最初の `drain_pty` で流れてバナーを上書きする（2026-07 実測: 起動バナーが一瞬出て PowerShell プロンプトに消える）。バナーを spawn 前に出し、カーソルをバナー下へ進めてから spawn すれば ConPTY はバナーの下にアンカーされる（`main.rs` で `print_startup_banner` を `spawn_local_shell` の直前に配置）。**追補（2026-08 実測）**: spawn 前バナー配置をしても「起動バナーが最初のプロンプトで消える」現象が Windows Terminal + PowerShell 5.1 で残っていた。`AISH_DEBUG_PTY` 相当の実出力採取で原因が判明: バナーは上書きされておらず（スクロールで戻ると残存）、**ConPTY 内で起動した子 `powershell.exe` が自分の起動ロゴ（`Windows PowerShell` / `Copyright ...` / `新機能と改善のために最新の PowerShell をインストールしてください!`）を再出力**し、その 4〜5 行ぶんビューポートが下へスクロールして aish バナーが画面外へ押し出されていた（＝アンカリング/上書きではなくスクロールアウト）。対策として **PowerShell 系子シェル（`powershell`/`pwsh`）を `-NoLogo` 付きで spawn** する（`pty_handler::spawn_local_shell` が `is_powershell_shell` で shell 名を検出し `-NoLogo` を付与。cmd/他 shell には付けない）。`-NoLogo` は起動ロゴ抑止のみでプロンプト形・履歴・コマンド実行を変えず、`PromptSniffer` にも非干渉。多くの環境では更新通知（`aka.ms/PSWindows`）も `-NoLogo` で一緒に消えるが、残る環境向けの `POWERSHELL_UPDATECHECK=Off` 注入は「子 env を触る」ため保留（ユーザ合意 2026-08、まず `-NoLogo` 単独で運用）。**Bug 2（未修正・調査中）**: aish プロンプト→AI 応答→確認 Enter でコマンド実行した瞬間、AI 応答行が上書きされる。原因は PSReadLine が承認コマンドの入力行を自分のプロンプトアンカー行（= minibuffer 前の shell プロンプト行 R）に再描画するが、aish は cursor を R に復元して AI 応答を R 以降に直接描くため。ConPTY は aish の直接 stdout 描画で動いた実カーソルを追跡できず絶対位置で R を指す（Unix の bash は相対再描画なので起きない）。trust-critical な minibuffer/描画コード（§15.4/15.5）なので、`AISH_DEBUG_PTY` で ConPTY の実シーケンスを捕捉してから修正する方針。
- **PTY 出力の実測は `AISH_DEBUG_PTY=1`**: `drain_pty` が受け取った生チャンクを `debug_bytes` で escape して **stderr** に出す（`main::debug_pty`。`AISH_DEBUG_KEYS` と対で既定無効・OnceLock キャッシュ）。`/tmp/aish-debug.log` に書く `AISH_DEBUG`（Unix 前提）とは別系統で、**stderr 出力なので Windows でも `aish 2> pty.log` で取れる**。ConPTY のカーソル位置指定シーケンス調査用。
- **TUI（vim / less / top 等）終了後は aish から何も出さない**。旧ステータスバー復旧の `Ctrl+L` はステータスバー廃止（commit 7d13700）で全廃。再導入するなら vim insert モードへの `^L` 混入を避けるため passive 検出だけで「終了」を一意判定できる根拠が必要（`\x1b[2J`・DECSTBM・alt screen 突入は TUI 動作中にも出るので使えない）。
- **Shift+Enter 改行は非対応**（端末間で CSI u / legacy が揃わない）。改行は `Alt+Enter`（§14）。
- **IME の未確定文字（preedit）は取得不能**。preedit は OS 入力メソッド層が保持し確定まで stdin に 1 バイトも流れない（端末が overlay 描画するだけ）。Kitty keyboard protocol でも変わらない。確定済み文字でマッチするのが現実解。

### 15.9 ring_buffer / backend 解決

- **未送信 cursor は backend ごと独立**（`get_unsent_for(kind)` / `mark_sent_for(kind)`。`/ai` 切替時に新 AI が catch-up）。**sent_marks は `HashMap<usize, u64>`**（Generic を ordinal `NATIVE_COUNT + idx` で扱うため。entry なし = 0 = 全 catch-up）。`mark_sent_all()` は `all_native()` + `all_generics()` を回す。**新規 native backend は `all_native()` に追加**（実行ファイル名 ≠ enum 名なら `binary()` に分岐。spawn / `check_installed` は `binary()`、設定 / 表示は `as_str()`）。
- **`BackendKind::parse(s)` は `parse_native` → `GENERIC_REGISTRY` 線形検索の 2 段**。`main::run()` は `Config::load`（`resolve_providers` + 検証）→ `init_generics(&config.ai.resolved_providers)` → parse の順序を守る。registry へ渡すのは生 `providers` でなく**マージ済み `resolved_providers`**。テストで Generic resolution を直接検証しない（`OnceLock` がプロセス共有で並列テスト干渉）。Generic の動作確認は `src/ai/generic.rs` 単体テストで `Box::leak(Box::new(recipe))` 直書き。
- **backend 自動フォールバック（`ai::auto_detect_backend`、2026-07）**: parse で決めた `kind`（既定 `claude` / `--ai` / `[ai].backend`）が `check_installed` で未検出のとき、実際に使える AI CLI を探して切り替える（`main::run` の check_installed 分岐）。探索順は純関数 `auto_detect_order()` = `[claude, codex, gemini, antigravity, copilot, cursor, qwen]`（Claude Code → Codex → Gemini → Antigravity（Gemini CLI 後継なので直後）→ 以降は人気順）→ その後 `all_generics()`（registry 登録順）を `check_installed` で順に試し**最初に見つかったものを採用**。発動条件は「選択 backend が未インストールなら常に」（明示指定の未インストールでもフォールバックする ＝ 初回導入者が何か 1 つ入れれば動く体験を優先）。**REST backend（Cloudflare/Nvidia）は `binary()`="curl" でほぼ常に検出成功し、かつ API key 必須で AI CLI ではないため `auto_detect_order` から除外**（含めると curl 在中の全環境で Cloudflare に化ける）。**Grok も除外**（バイナリ名 `grok` がコミュニティ製 `@vibe-kit/grok-cli` と衝突しうるため誤検出を避け、明示 `--ai grok` 指定に限定）。切替時は `println!` で `AI backend \`X\` not found — using \`Y\`.` を 1 行表示（banner と同じく raw モードでも OPOST(Unix)/newline-auto-return(Windows) で `\n`→CRLF。切替後の `kind` は `print_startup_banner` に反映）。1 つも見つからなければ **`ai::install_guide(color)`**（先頭 `No AI agent found. Please install one:` + `  ▸ 名前` の次行に `      URL` を段差表示。`auto_detect_order` と同じ 7 種を人気順: Claude Code=code.claude.com/docs/ja/quickstart, Codex=learn.chatgpt.com/docs/codex/cli, Gemini=github.com/google-gemini/gemini-cli, Antigravity=antigravity.google/docs/cli/install, GitHub Copilot=docs.github.com/copilot/…/install-copilot-cli, Cursor=cursor.com/docs/cli/installation, Qwen=github.com/QwenLM/qwen-code）を Err で返す（`main` が他の Err と同じく `Error:` プレフィックス付きで出力し非ゼロ終了。cleanup 経路を通すため直接 exit しない）。**着色は `main::stderr_use_color()`（`std::io::stderr().is_terminal()` かつ `NO_COLOR` 未設定。libc/Console API 直叩きでなく std の `IsTerminal` を使う）で判定**し、true のとき install_guide が名前太字(`\x1b[1m`)/URL 淡色(`\x1b[2m`)、main の error 分岐が `Error:` を赤(`\x1b[31m`)にする。名前と URL の交互行で「どこがリストか」が読みにくいという指摘への対応（2026-07）。`unknown` backend 名の parse エラー（フォールバック前段）とは区別する。順序不変条件は `auto_detect_order_is_stable`、URL 一覧/レイアウト/着色有無は `install_guide_lists_all_backends` で固定。
- **Generic（`BackendKind::Generic(u8)`）は registry で動的解決**。`init_generics` が OnceLock に 1 度 populate し `Box::leak` で `&'static` 化。init 前 / 範囲外は `"?"` フォールバック（panic せず spawn 失敗）。built-in と flat namespace（`generic:` prefix なし）。
- **AI 応答の注釈記録は `RingBuffer::record_ai_exchange` 経由**。注釈（`[aish→<kind>]> ...` / `[ai/<kind>]> ...` / `[ai/<kind> suggests] ...`）の append → `mark_sent_for(current)` の順序不変条件（逆順だと current AI が自分の発話をループ受信。他 backend は次回 catch-up）をこのメソッドに閉じ、ばらの `append_text` + `mark_sent_for` を再導入しない。ラベル書式はここが唯一の定義（テスト `record_ai_exchange_format_is_stable`）。
- **`/clear` は `mark_sent_all()` で全 backend の cursor を進める**が、AI CLI 内部 session/history は current backend のみリセット（他 backend の instance を保持していないため）。
- PTY 文字列と注釈ラベルは衝突しうる（AI が文脈で区別。多発したら XML 風へ変更余地）。

### 15.10 AI backends（ツール抑制 / 引数の罠）

- **Claude の system prompt（`--append-system-prompt`）は初回ターンのみ**（resume での二重 append 回避）。**毎ターン守らせるルールは `--json-schema` の field description に書く**（スキーマは毎ターン送られる）。`command_result_followup`（§ 15.2）は Claude では schema の **required に含めて毎ターン明示判定**させ、他 backend は `build_system_prompt` の応答ルール + JSON 例で指示し serde default（欠落=true）で吸収する。判定基準の文言は両経路で同一:「提案コマンドの実行後、その出力を見て分析・調査・操作を続行する必要があるなら true。ユーザにコマンドを教える・提示するだけで出力の確認が不要なら false」。`commands` description の整合性ルール: 「本文で実行コマンドを出したら配列にも入れる / 提案が無ければ空配列」（「空にするな」式の強制は付けない — 不要時にコマンドを捻り出す）+ 「独立した複数コマンドを `;` 連結せず配列の別要素に分割。**ただし `&&`/`||` と `for`/`while`/`until`/`case`/`if` 等制御構文内の `;` は 1 コマンド維持**」（分割すると `for i in 1 2 3; do …; done` が壊れる。codex 対策で共有 `build_system_prompt` にも併記）。
- **Claude の `--disallowedTools` は `MANDATORY_DENY`(Bash/Edit/Write) を常に union**（`effective_disallowed_tools`）。`disallowed_tools` は単一文字列の全置換で、空にすると安全集合が消える footgun のため `allow_unsafe_tools = false`（既定）の間は baseline を必ず混ぜる（Read だけ外せる）。`true` のときのみ verbatim（危険）。**args の末尾で push**（`extra_args` 等の後。`extra_args = ["--disallowedTools", ""]` を後置きされても CLI 後勝ちで baseline が non-removable）。**前方へ戻したり union を外したりしない**。codex/copilot/cursor の deny 系は固定埋め込み（extra_args 上書きは未対策）。
- **cursor は `--trust` を常時付与**（headless `-p` は無いと `Workspace Trust Required` で非ゼロ終了）。config 不可。`--yolo`/`-f`（Run Everything）は絶対に付けない。ツール抑制は `--mode plan` + system prompt の二段（個別無効化フラグが無い）+ 任意 `--sandbox`。`mode = ""` は確認 UI 迂回リスクで非推奨。Free プランは `auto` のみ（Named model 指定は `EmptyOutput` エラー）。
- **copilot は `-p` フラグを付けない**（stdin 渡しと排他で `too many arguments` になる。CLI が stdin 自動検出。他の `-p` 必須 backend と逆）。**ツール抑制は四段**: `--allow-all-tools`（非対話必須）+ `--deny-tool=shell` + `--deny-tool=write` + `--no-ask-user` + `--mode plan`（deny が allow に優先）。config 不可。`--yolo`/`--allow-all` は絶対に付けない。**`--output-format json` は JSONL**（`parse_jsonl_envelope` で行走査、ephemeral 行は無視）。
- **Generic backend（ユーザの新規 `[[ai.providers]]`）は安全フラグを自動付与しない**。recipe 著者が `args` に `--mode plan` 等を明示する想定。信頼できない CLI は確認 UI を迂回して shell 実行し得る。
- **組み込みデフォルト recipe（`config::builtin_providers()`）は aish が著者なので安全フラグを `args` に焼き込んで同梱**（generic 原則の例外）。**read-only / plan 相当を強制できない CLI は同梱しない**（強制できないと承認 UI を迂回してサーバを変更し得る = 信頼の根幹違反）。同梱前に read-only モードの有無を実機検証（例: kimi の `--plan`）。追加・更新は `builtin_providers()` 1 関数に集約。
  - **注記: read-only 強制の必須は「builtin generic recipe」の話**。native backend（gemini/qwen/antigravity/grok）は歴史的に system-prompt-only の安全 posture を許容してきた（フラグ制約なし、system prompt の「ツール禁止」指示のみ）。この住み分けは §15.10 の Antigravity/Grok 項参照。
- **OpenCode（`opencode`、anomalyco/opencode = 旧 sst/opencode）は generic recipe として同梱**（2026-07、v1.17.13 で実機検証済み）。read-only 強制の設計:
  - **CLI フラグ単独では read-only を強制できない**。built-in の `plan` agent は edit/bash が deny でなく **ask** で、headless で ask に達すると無限ハングする既知 issue（anomalyco/opencode#14473）があるため **`--agent plan` を根拠にしない**。`--auto`（auto-approve。"explicit deny 以外を自動承認"）は `--yolo` 系と同類で**絶対に付けない**。
  - **強制経路は環境変数 `OPENCODE_CONFIG_CONTENT`**（インライン JSON config）。opencode の config マージ順は remote → global → `OPENCODE_CONFIG` → project → `.opencode` → **`OPENCODE_CONFIG_CONTENT`（最後）** → managed で、ユーザのグローバル/プロジェクト設定の allow を上書きできる。この注入のために `ProviderRecipe` に汎用 `env` フィールドを追加した（`run_cli_capture_stdout_env`。透明性のため env もログに記録）。
  - **agent 側 permission はトップレベル permission より優先される**仕様があるため、トップレベル deny だけでは project config の緩い agent に負けうる。対策として CONFIG_CONTENT 内に **deny 付き専用 agent `aish`** を定義し `--agent aish` で指定（同名 agent を project 側に定義されても CONFIG_CONTENT が後勝ち）。さらに **`task`（サブエージェント起動）と `todowrite` を `tools` で無効化**（task 経由で project 定義の緩い agent に迂回してファイル書き込みされる穴を塞ぐ）。
  - 実機検証結果（scratch dir、`--auto` なし）: deny された edit/bash は**ツールセット自体から除去**され（モデルが呼ぶと "unavailable tool"）、ファイル不変・ハングなしで拒否説明がテキストで返る。残るツールは glob/grep/read/skill/webfetch/websearch のみ（web 系は非 Claude 系 backend の意図的緩和方針どおり許可）。
  - recipe 詳細: `prompt_delivery="arg"`（`opencode run [message..]` の positional、複数行 OK）、`parse="lossy"`（stdout はヘッダ行 `> aish · <model>` + 応答テキストで、`{message,commands}` JSON が extract 可能なことを実機確認）、`model_flag="--model"`（`provider/model` 形式）、`effort_flag="--variant"`（provider 依存: high/max/minimal 等）、models_command=`opencode models`（1 行 1 候補の `provider/model` 形式）。session resume は `--session <id>` があるが `--format json`（イベントストリーム）の構造検証をしていないため初版は内部 history fallback（`--format json` の jsonl パスが検証でき次第 native resume 化を検討可）。
  - バイナリ名 `opencode` は**元祖 Go 版 opencode（Charm 買収後 charmbracelet/crush に改名）の残骸と衝突しうる**。ユーザには `which -a opencode` での実体確認を促す（npm パッケージ名は `opencode-ai`）。
- **組み込みのユーザ上書きは「フィールド単位マージ」**（`ProviderOverride` の `Some` フィールドだけ `apply_to` で反映、`args` 等 Vec は丸ごと置換）。`name` 不一致は新規 provider（`binary` 必須）。同名重複は後勝ちマージ。実装は `AiConfig::resolve_providers`（テストは `resolve_with_builtins` で builtins 差し込み）。ユーザが上書きで安全フラグを外すのは自己責任。
- **Cloudflare Workers AI（`cloudflare` / `src/ai/cloudflare_workers_ai.rs`）は REST を `curl` サブプロセスで叩く native backend**。HTTP/TLS クレートを足さない（`--update` と同方針 = 追加 crate 依存ゼロ）。`run_cli_capture_stdout("curl", …)` で Ctrl+C 中断・ログ・確認フローを再利用。`binary()`="curl"（`check_installed` は `curl --version`）。session/resume 無しで gemini/qwen 同型の内部 history を毎ターン前置き。
  - **認証は環境変数のみ**（`CLOUDFLARE_ACCOUNT_ID` / `CLOUDFLARE_API_TOKEN`、任意 `CLOUDFLARE_MODEL`）。config.toml は平文なのでトークンを置かせない（`[ai.cloudflare-workers-ai]` は `OptionLists` だけ）。**呼び出し名は `cloudflare`、設定セクション/ファイル名は `cloudflare-workers-ai` 系**（Rust モジュールはハイフン不可で `cloudflare_workers_ai.rs`、TOML は serde `rename = "cloudflare-workers-ai"`）。
  - **curl に `-f`(`--fail`) を付けない**: HTTP エラー時も JSON エラーボディ（`success:false` + `errors`）を読んでメッセージ化する。`success` 確認 → `result.response` → `parse_ai_response_lossy`（`{message,commands}` JSON なら提案化、否なら全文 message）。body は `--data-binary @-` で stdin 経由（ARG_MAX / クォート回避）。
  - 既知トレードオフ: Bearer token が argv に乗り `ps` で見え得る（ローカル・本人資格情報なので許容。気になれば `-H @-` 方式へ）。提案品質はモデル依存（大きめ instruct モデル推奨）。テキスト生成のみでサーバ側実行はなく、提案は従来どおり Y/n/a/q でゲート。
- **NVIDIA NIM（`nvidia` / `src/ai/nvidia_nim.rs`）は cloudflare と同方式の curl REST native backend**（2026-07 追加、実機検証済み）。OpenAI 互換 `POST https://integrate.api.nvidia.com/v1/chat/completions`、`binary()`="curl"、内部 history（gemini/qwen 同型）、system prompt でツール非使用 + JSON 出力を指示。
  - **認証は環境変数 `NVIDIA_API_KEY`（nvapi-...）のみ**、任意で `NVIDIA_MODEL`。config セクションは `[ai.nvidia-nim]`（OptionLists のみ。呼び出し名 `nvidia` と住み分け — cloudflare の命名規則に準拠）。既定モデル `meta/llama-3.3-70b-instruct`、ピッカー既定は MODEL_DEFAULTS 4 種、全カタログは `GET /v1/models`（config.toml.example に models_command 例）。
  - **成功判定は `choices[0].message.content` の有無**。cloudflare の `success` フィールドに相当する物が無く、**エラーボディは JSON とは限らない**（認証失敗は `{"status":403,"title":"Forbidden",...}` だが、存在しない model は素のテキスト `404 page not found`）ため、JSON parse 失敗も生ボディごとエラー化する。`max_tokens: 4096` を明示（モデル依存の小さい既定で JSON 応答が切れるのを防ぐ）。
- **Google Antigravity CLI（`antigravity` / `src/ai/antigravity.rs`）は gemini/qwen と同型の system-prompt-only native backend**（2026-08 追加。呼び出し名 `antigravity`、実行ファイルは **`agy`**＝`binary()` で分岐、cursor と同じ enum名≠バイナリ名パターン）。`agy -p`（headless）で prompt を stdin 渡し、`parse_ai_response_lossy` で `{message,commands}` 抽出、内部 history 8 ターン。Gemini CLI の後継（2026、Gemini 系モデル）で `auto_detect_order` は Gemini の直後に挿入。
  - **model は `--model`、reasoning effort は `--effort low|medium|high` を native 対応**（gemini/qwen と違い effort が効く。EFFORT_DEFAULTS=low/medium/high）。MODEL_DEFAULTS は Gemini 系 4 種の best-effort（`agy models` で最新確認）。resume は `agy --continue`（best-effort、履歴があるときだけ提示）。
  - **read-only / plan の permission-layer 強制は headless では未提供**（Antigravity Issue #45。`-p` は非対話ユーザが居ないため write/exec を自動承認しうる。`--sandbox` は shell のみ制限で `write_file` は通り、`--mode plan` は `/plan` prompt 接頭辞で permission 層の deny ではない）。よって**同梱 generic recipe の「read-only 強制必須」ルールは適用せず**、gemini/qwen と同じ system-prompt-only 姿勢で native 化した。**判断根拠（ユーザ合意、2026-08）**: aish の信頼の根幹（サーバ保護）はコマンド承認 UI（Y/n/a/q）で担保され、AI backend の read-only 有無に依存しない。read-only 強制はローカルの AI CLI が承認 UI を迂回してクライアント側で write/exec する余地を塞ぐ defense-in-depth に過ぎず、リモート実行時のサーバ安全性は損なわれない。**`--dangerously-skip-permissions`（auto-approve）は絶対に付けない**（`args_use_headless_flag_and_never_bypass_permissions` で固定）。
  - **未検証点（実機確認が取れ次第調整）**: `agy -p` が prompt を stdin から読むか（claude の `-p` と同じ boolean フラグ想定。値必須なら要調整）、非対話出力のフォーマット（現状は素のテキスト＝system prompt で bare JSON 指示 → lossy 抽出）、正確な model slug。config は `[ai.antigravity]`（extra_args + models/efforts）。
- **xAI Grok CLI（`grok` / `src/ai/grok.rs`）は Antigravity と同型の system-prompt-only native backend**（2026-08 追加。呼び出し名 = 実行ファイル名 `grok`）。`grok -p`（headless）stdin 渡し、lossy 抽出、内部 history 8 ターン。model は `-m`、reasoning effort フラグは無し（保存のみ）。resume なし（None）。MODEL_DEFAULTS は grok 系 4 種の best-effort。
  - read-only 非強制の姿勢・`--always-approve` 禁止は Antigravity と同じ理由（上記）。**`grok` はコミュニティ製 `@vibe-kit/grok-cli`（npm、別ツール、read-only モード無し）とバイナリ名が衝突しうる**ため `auto_detect_order` からは除外し（誤検出防止）、明示 `--ai grok` 指定に限定。ユーザには `which -a grok` で公式 CLI か確認を促す。config は `[ai.grok]`（extra_args + models）。以前は `config.toml.example` のコメントアウト generic recipe 例だったが native 化に伴い削除（`[ai.grok]` を参照）。

### 15.11 セルフアップデート 2 チャネル

- **`--update` は `--stable`（既定）= `/releases/latest`（prerelease 除外の最新）/ `--prerelease` = `/releases` 先頭 `[0]`（prerelease 含む絶対最新）**（`src/update.rs` の `UpdateChannel`）。命名の罠: GitHub API では「安定版」側が "latest" を含むエンドポイントなので、ユーザ向け flag はあえて `--prerelease`（`--latest` だと逆に見える）。**この向きを逆にしない**。
- チャネル区別は `release.yml` の `prerelease: ${{ contains(tag, '-') }}` 依存（prerelease=true は `Latest` バッジが付かず `/releases/latest` から除外）。**prerelease リリース時は Cargo.toml の `version` にも識別子を含める**（`0.9.0-rc.1`）。タグだけ `-rc.1` だと `latest == current` 比較が一致せず常に「更新あり」と誤判定。target 解決 / DL / SHA256 / 置換はチャネル非依存。
- **更新バイナリの tmp はインストール先（`current_exe()` の親）に置き `rename()` で原子置換。`/tmp` 経由 + copy fallback にしない**。過去バグ: `/tmp` は別 FS のため `rename()` が EXDEV → copy fallback が実行中バイナリの inode へ直接書き込み `Text file busy`(ETXTBSY) で死んだ。`rename()` は同一 FS ならエントリ差し替えだけで旧 inode に触れず、実行中でも必ず成功する。tmp 名は `.aish-update-{pid}`（エラー時 `remove_file` クリーンアップ）。

### 15.12 `/model` `/effort` 対話ピッカー

- **`ui::show_picker` は confirm と同型: main スレッドが fd0 を直接読む同期ブロッキング関数**。`/model` `/effort` は main loop の `AiPrompt` arm 内（入力スレッドが `rearm_on_drop` guard で parked 中）で処理するので、`InputEvent`/`InputRequest` チャネル経由にしない。**termios は触らない**（raw はセッション維持）。stdout 専用で PTY に書かない。表示中は同スレッドが drain しないので PTY の上書きは起きず、専用抑制フラグも不要。
- **描画モデル**: 先に `\n`×`total_lines` で領域確保（最下行なら scroll を先に発生）→ `\x1b[<total_lines>A` で原点へ。`render_picker` は原点開始・**最終行末で止まる**（末尾改行を出さない = 予約超え scroll を起こさない）。再描画は `picker_move_to_origin`（`\x1b[<L-1>A\r`）。終了時は原点 + `\x1b[0J` 消去（後続の `print_slash_result` が原点から出す）。**DECSTBM 不使用**（minibuffer と同じ理由）。長い候補は簡易ビューポート（可視 = 端末高 − 2）。
- **キー**: ↑↓=移動（クランプ、wrap なし）、Home/End=端、Enter=確定、**Esc / Ctrl+C / Ctrl+D=取消**（confirm と揃える）。他は無視。**ナビは純関数 `picker_step` に分離し golden test**（`picker_step_navigation`）。
- **候補解決**: trait `available_models` / `available_efforts` → `common::resolve_option_list`。優先順位・実行シェル・失敗時挙動は §11.4。取得コマンドは**ピッカーを開く時だけローカル実行**（起動時に走らせない。サーバ書き込み・承認フローと無関係）。
- **model 組み込み既定（`MODEL_DEFAULTS`、§11.4）**: 動的取得は断念して静的リストを同梱（codex/copilot/cursor/gemini/qwen いずれも「stdout 1 行 1 モデル」の非対話一覧コマンドを持たない。copilot は未解決 Issue #700）。best-effort で**更新にはリリースが必要**。ユーザは `[ai.<backend>].models` で上書き可。
- **`/model` `/effort` ハンドラ（`run_option_picker`）は常に `Some(...)` を返す**（None だと通常 AI プロンプト扱いになる）。引数なし=候補解決 → 空なら hint / 非空ならピッカー（末尾に `(default)` 疑似エントリ = 選べばクリア）、`-`/`clear`=クリア、その他=**検証せず set**（一覧外の値も許可）。取消時は変更しない。

### 15.13 Windows ネイティブ対応（platform 層 / term）

- **アーキテクチャ**: 低レベル端末操作（raw mode 設定/復元・poll 付き 1byte 読み・端末サイズ・DSR・リサイズ検出・Ctrl+C 検出・PID 生存確認・TZ オフセット）を `src/term/`（mod.rs + unix.rs + windows.rs）に集約（2026-07）。ui.rs は `pub use crate::term::{...}` で従来名を維持し、confirm/picker/minibuffer/passthrough は `input::StdinSource`（unix=`Fd0Source` / windows=`ConsoleSource` の型エイリアス）経由で cfg フリー化した。**term/unix.rs は ui.rs/main.rs/ai/common.rs からの純移動で、termios フラグ・poll timeout・DSR 80ms/10ms・EINTR 処理は 1 バイトも変えていない**（golden test 210 件の無変更通過で担保）。旧 Windows フォールバック（`read_line_cooked` / `read_confirm_key_cooked` / `show_picker_cooked` / `InputEvent::Line`）は本実装に置き換えて撤去（死蔵コード防止。非コンソール環境は `term::console_ok()` が起動時に拒否）。
- **Windows コンソールモード**（`save_terminal_settings`）: stdin は `ENABLE_LINE_INPUT|ECHO_INPUT|PROCESSED_INPUT|QUICK_EDIT_MODE|MOUSE_INPUT` を落とし `ENABLE_VIRTUAL_TERMINAL_INPUT|WINDOW_INPUT|EXTENDED_FLAGS` を立てる。PROCESSED off で **Ctrl+C が生 0x03** になり Unix と同一経路（confirm キャンセル・実行中転送・`check_stdin_cancel`）で動く。**ただしこれは素の conhost 限定**: Windows Terminal + PSReadLine の win32-input-mode 下では Ctrl+C も他キー同様マルチバイトシーケンスで届くため、`check_stdin_cancel`/実行中転送の Ctrl+C 検出は `input::bytes_contain_ctrl_c` を介す必要がある（詳細は本節後半の落とし穴参照）。**`ENABLE_MOUSE_INPUT` を明示的に落とすのが重要**: 既定コンソールモードには通常マウス入力が立っており、`QUICK_EDIT_MODE` を off にしただけではマウスイベントがアプリへ配送され続ける。しかも `ENABLE_VIRTUAL_TERMINAL_INPUT` 有効下ではマウス移動が **VK=0 の KEY_EVENT レコード（VT マウスシーケンス `\x1b[<...M`）** として届くため、pump の KEY_EVENT アームで `unit != 0` のバイト列がそのまま入力キューへ **injection** され（マウス操作で偽入力が混入）、`AISH_DEBUG_KEYS` ダンプが埋め尽くされ、`WaitForSingleObject` がマウス毎にシグナルして `read_stdin_byte` ループが空回りする。aish はマウスを一切使わない（Unix でも `ESC[?1000h` 等を出さずマウスが来ない）ので、その挙動に揃えてマウス入力自体を無効化する（`_ => {}` の MOUSE 破棄は保険として残す）。stdout は `ENABLE_VIRTUAL_TERMINAL_PROCESSING` のみ追加し、**`DISABLE_NEWLINE_AUTO_RETURN` は設定しない**（aish は `writeln!` の `\n`→CRLF 端末変換に依存 = Unix の OPOST 不可触と同義）。CP は入出力とも 65001 (UTF-8)。元モード/CP は OnceLock 保存で終了時復元。
- **入力ポンプ**（term/windows.rs）: `WaitForSingleObject` + `GetNumberOfConsoleInputEvents` + `ReadConsoleInputW` の 1 本に統一。ReadFile/ReadConsoleW は使わない（コンソールハンドルの Wait は key-up・マウス・フォーカスでもシグナルが立つため「Wait→record 読み→フィルタ→空なら再 Wait」のループが必須。ReadFile はコードページ依存で非 ASCII が壊れる既知問題）。KEY_EVENT の UTF-16 `uChar` をサロゲート処理付きで UTF-8 化し `wRepeatCount` 回キューへ。**Ctrl+/（aish のエントリキー）は経路によって `uChar` が揺れる — native conhost=0x1f / 一部 VT 端末=0 / RDP・Remmina 等のレイアウト変換=0x2f（実測: KVM+Windows 11 ゲストへ Remmina(RDP) 接続で Ctrl+/ を押すと `/` がそのまま入力された。他 Ctrl 系（Ctrl+C/Ctrl+L）は届くのに OEM キーだけスキャンコード変換でズレる）。そのため `if unit==0` ガードより前で、`ctrl` 押下下に **`wVirtualKeyCode==VK_OEM_2(0xBF)` または `uChar ∈ {0x1f, 0x2f}`** を 0x1f に正規化する**（エントリキーの生命線。Shift は見ないので Ctrl+Shift+/=Ctrl+? も同経路で拾う。`dwControlKeyState` に CTRL が立つ前提 = 立たない環境は Remmina 側キーボード設定の問題）。**VK だけでなく uChar でも拾うのは、`ENABLE_VIRTUAL_TERMINAL_INPUT` 有効下では KEY_EVENT が VK=0・uChar のみで届くことがあり、その uChar が US では 0x1f、JIS 実機では `/`=0x2f になる（VK_OEM_2 条件だけだと JIS の Ctrl+/ を取りこぼす。2026-07 実測: `[Console]::ReadKey` は VT モード OFF なので `Oem2+Control` に見えるが、VT モード ON の aish 実受信は別物だった）**ため。**受信不明時は `AISH_DEBUG_KEYS=1` で pump が生 KEY_EVENT(`vk`/`char`/`ctrl`)を stderr にダンプ**（既定無効、`debug_keys_enabled()` で env を 1 度キャッシュ）。**ただし Windows Terminal + PowerShell では PSReadLine が win32-input-mode を有効化し、キーが KEY_EVENT でなく `ESC[Vk;Sc;Uc;Kd;Cs;Rc_` のバイト列で届く（pump ダンプは全行 `vk=0 ctrl=0` になり、この pump 正規化では Ctrl+/ を拾えない）。その経路は `input.rs` の `classify_win32_input_mode` がデコードする（§ 15.1 参照）。この pump 正規化は win32-input-mode 非対応の素の conhost 用に残す（両経路が共存）。** `WINDOW_BUFFER_SIZE_EVENT` → `record_resize()`（SIGWINCH 代替。`check_and_clear_resize` はイベント + サイズ差分ポーリングの OR で取りこぼしを保険）。
- **kill_line / refresh_prompt**（pty_handler.rs）: Windows は `0x01,0x0b` でなく **ESC(0x1b)** を送る。PSReadLine の Windows edit mode では Ctrl+A=SelectAll 等で kill-line にならず、**打ちかけ未消去のまま改行 = 未承認 submit（信頼の根幹違反）** になり得るため。ESC は cmd.exe / PSReadLine 共通の入力行クリア。バイト列は `KILL_LINE_BYTES` 定数として pty_handler.rs 内に維持（リテラルを外に出さないルール続行）。**落とし穴（2026-07 実機報告 + `AISH_DEBUG_PTY` ログで確認）**: `confirm_and_execute`（conversation.rs）が最初の AI 提案コマンド実行直前に送っていた `kill_line()` の単独 ESC の**直後にコマンド文字列を送信すると**、コマンドが `[` で始まり 2 文字目が CSI の終端バイトとして解釈できる場合（`[System.Environment]::OSVersion.Version` の `[S` = `ESC` `[` `S` = SU=Scroll Up 等）、PowerShell 側の VT 入力パーサが `ESC`+続く数バイトを**完成した CSI シーケンスとして即座に消費**してしまい、**承認したコマンド文字列の先頭 1-2 文字が欠落したまま実行される**（実測: `[System.Environment]::...` → `ystem.Environment]::...` が実行され `CommandNotFoundException`）。ユーザが画面で承認した文字列と実際に PowerShell が解釈する文字列が食い違う trust 上の問題。**当初 `kill_line()` 送信後に 150ms 待ってから送る対策を試したが効果が無かった**（実機再現ログで確認）: `ESC[S` は待っても待たなくても届いた瞬間に完成した合法シーケンスとして解釈される（曖昧さ解消のタイムアウトが要る半端なシーケンスではない）ため、遅延では原理的に直らない。**正しい対策**: 単独 `kill_line()` でなく `refresh_prompt()`（ESC + Enter バイト）を使う。Enter バイトは CSI の継続バイトになり得ないため、ESC の直後に挟むと ESC はそこで打ち切られて単独キー扱いになり、後続のコマンド文字列は新しい行から送られるので `[` が衝突しない。Unix は kill_line が `0x01,0x0b` でこの衝突自体が起きないが、分岐を増やさず両 OS で同じ `refresh_prompt()` を使う（**Enter バイト自体は Unix=`\n` / Windows=`\r` で異なる。理由は次項参照**）。
- **落とし穴（2026-07 実機報告 + `portable_pty` での直接検証で確認）: Windows で bare `\n` は Enter として機能せず PSReadLine が `>> ` 継続プロンプトのまま固まる**。`refresh_prompt()`/`send_approved_command()` は元々 Enter に `\n`(LF) を使っていたが、Windows Terminal + PSReadLine (win32-input-mode) 環境では **`\n` 単体を送ると「行確定 (Enter)」でなく「複数行入力の改行」として扱われ、`PS ...>` でなく `>> ` の継続プロンプトが表示されたまま次の入力を待ち続ける**（ユーザ報告: AI応答をCtrl+Cでキャンセルした後 `refresh_prompt()` を呼んでも自動でプロンプトが戻らず、手動で Enter を押すまで固まる）。`AISH_DEBUG_PTY` 越しの再現ログだけでは PTY 側で何が起きているか切り分けにくかったため、`portable_pty` で PowerShell を直接起動し `kill_line()`→Enter 相当のバイトだけを送る最小再現プログラムを作って検証した: **`\n` 単体（空行でもコマンド文字列付きでも）は `>> ` のまま何秒待っても自然には解消しない**一方、**`\r`(CR) を送ると即座に `PS ...>` へ戻り、コマンド文字列 + `\r` も即座に実行される**（実機の対話セッションでは `send_approved_command` の `\n` 終端コマンドがなぜか実行されていたが、その正確なメカニズムは特定できていない — おそらく後続のコマンド確認フローや PTY 出力ドレインのタイミングに依存した偶発的な回復であり、頼るべきではない）。**対策**: `pty_handler.rs` に `ENTER_BYTE` 定数（Windows=`\r` / Unix=`\n`、`KILL_LINE_BYTES` と同じ `cfg!(windows)` パターン）を追加し、`refresh_prompt()` と `send_approved_command()` の両方をこれに統一した。Unix の `\n` は元々問題なく動いていたため変更しない。
- **落とし穴（2026-07 実機報告 + `AISH_DEBUG_KEYS` ログで確認）: Thinking 中 / コマンド実行中の Ctrl+C が win32-input-mode 下で無反応になる**。`ai/common.rs` の `run_cli_capture_stdout`（AI CLI 応答待ち）と `conversation.rs` の `wait_for_command_completion`（コマンド完了待ち）は、どちらも Ctrl+C 検出を素の `byte == 0x03` 比較（`check_stdin_cancel` / `stdin_bytes.contains(&0x03)`）に頼っていた。Windows Terminal + PSReadLine の win32-input-mode が有効な間（§ 15.1 参照、`\e[?9001h` を子シェルが送った時点でセッション全体が対象になる）、Ctrl+C は生の `0x03` でなく `ESC[67;46;3;1;8;1_`（Vk=67='C', Uc=3, Cs に LEFT_CTRL）のようなマルチバイトシーケンスで届くため、素の比較では一致せず**キー入力自体は `AISH_DEBUG_KEYS` の pump ダンプに残るのに Ctrl+C として検出されない**（実測: 1 セッション中 1988 件のキーイベントすべてで `ctrl=0x00000000`、つまり全キーが win32-input-mode 経由で ctrl フラグがネイティブ `dwControlKeyState` でなくシーケンス内の数値パラメータに埋まっていた）。コマンド実行中の Ctrl+C が「見た目は効いている」ように見えるのは、aish がバイト列を素通しで PTY へ転送し、**PowerShell 側の conpty がそのシーケンスを自前でデコードして Ctrl+C として処理している**ためであり、aish 自身の `interrupted` フラグ（follow-up 抑制の判定に使う）は実際には立っていなかった。**対策**: `input.rs` に `bytes_contain_ctrl_c(&[u8]) -> bool` を追加し、`next_event`（win32-input-mode デコード込みの唯一のデコーダ）にバッファを再生させて `Tok::Ctrl(0x03)` の有無で判定する。`check_stdin_cancel`（term/windows.rs）と `wait_for_command_completion` の `interrupted` 判定の両方をこれに置き換えた。Unix は元々 `Tok` 相当のデコード不要 (`next_event` の `b if b < 0x20 => Ctrl(b)` がそのまま同じ結果になる) なので挙動は変わらない。
- **既知の制約（根本修正見送り）: ConPTY の内部仮想スクリーンと実端末のズレによる表示崩れ**（2026-07 実機報告 + `AISH_DEBUG_PTY` ログで確認）。aish は AI メッセージ/提案コマンド/`Exec?` 確認行を PTY を経由せず直接 stdout に書く設計だが、Windows ConPTY は子プロセス (PowerShell) 向けに実端末とは別の内部仮想スクリーンバッファ（絶対座標での再描画・`Write-Progress` 等が基準にする座標系）を保持しており、**aish が直接書いた分だけ実画面がスクロールしても ConPTY はそれを一切知らない**。セッションが実端末の高さを超えてスクロールした状態で `Write-Progress`（`Get-ComputerInfo` 等）や PSReadLine の絶対座標再描画が発生すると、ConPTY が「起動直後の内部状態」を基準に描画してしまい、実測では起動バナー文言が本来無関係な行に復元されるなど表示が崩れる（Unix の PTY にはこの二重バッファが無いため発生しない、Windows ConPTY 固有の構造的制約）。根本修正には aish 自身が Windows 上で VT 解釈をして自前の画面モデルを持つ必要があり大掛かりなため、2026-07 時点では対応を見送り、既知の制約として記録するに留める。
- **既定シェル**: Windows は `$SHELL` 未設定時 cmd.exe でなく **powershell.exe**（`PS C:\> ` は末尾空白があり sniffer の通常規則で検出可能）。cmd.exe の `C:\path>`（末尾空白なし）は `is_cmd_style_prompt`（ドライブレター始まり + `>` 終端の純関数）で **Windows 実行時のみ** 特例判定（`cfg!(windows)` 実行時分岐なので Unix は挙動不変）。誤学習防止のため `record_match` の学習対象にしない。
- **スコープ外（既知の制約）**: `--update` 自己更新は Windows では実行しない（実行中 exe の rename 不可・sha256sum 不在など独立課題）。**ただし bare error でなく、`run_update` 冒頭の `cfg!(windows)` 分岐で `windows_update_hint()` を表示して `Ok(())` 終了する**（`detect_target()` の `Unsupported platform` Err より前に分岐）。hint は同梱インストーラ `install.ps1`（repo 直下）を使う one-liner。**Windows は自己更新非対応でユーザが選ぶため、`--update`/`--prerelease` の指定に関わらず両チャネルのコマンドを併記する（`windows_update_hint()` は引数なし = channel を見ない）**: prerelease 含む最新=`irm …/main/install.ps1 | iex`（install.ps1 の既定 `/releases[0]`）、stable=`& ([scriptblock]::Create((irm …))) -Stable`（`| iex` は引数を渡せないので scriptblock 化）。当初は channel 追従だったが、stable 版から `aish --update`（既定 stable）すると `-Stable` コマンドしか出ず prerelease へ更新する導線が無かったため併記に変更（2026-07）。手打ちの `$tag` 展開コマンドを案内しない理由: 変数設定行を飛ばして `Invoke-WebRequest` だけ実行すると空 `$tag`/`$arch` で URL が壊れ GitHub の HTML ページが `aish.exe` として保存される事故があったため（2026-07 実測）。純関数なので `windows_update_hint_lists_both_channels` で内容固定。**インストーラ本体は `install.ps1`（+ cmd ラッパ `install.cmd`）**: arch 判定（`PROCESSOR_ARCHITEW6432` 優先）→ チャネル別にタグ取得（既定 `/releases[0]`、`-Stable` で `/releases/latest`）→ exe DL → `Get-FileHash`（PowerShell 標準、追加ツール不要）で `.sha256` 照合 → `%LOCALAPPDATA%\Programs\aish` 配置 → user PATH 追加。**落とし穴**: GitHub は `.sha256` を `application/octet-stream` で返すため `Invoke-WebRequest ... .Content` が **`Byte[]` になり `.Trim()` が落ちる**（2026-07 実測）。`-is [byte[]]` を見て `[Text.Encoding]::UTF8.GetString(...)` でデコードしてから hash を切り出す（string 前提で書かない）。`install.cmd` は sibling の `install.ps1` を `-ExecutionPolicy Bypass` で実行、無ければ raw URL から取得。リリースへの Windows バイナリ（x86_64/aarch64 の生 exe + zip）は `release.yml` の matrix に同梱済み（2026-07 実機検証完了後に追加）。**落とし穴 2**: 既存 `aish.exe` を上書きする最後の `Move-Item -Path $tmp -Destination $dest -Force` が **`ERROR_ALREADY_EXISTS`（「既に存在するファイルを作成することはできません」）で落ちる**（2026-07 実測、Windows 11 で v1.0.5→再インストール時）。`-Force` でも既存ファイルの上書きが不安定な上、実行中の `aish.exe` は上書き自体が不可。Windows ではロック中の exe でも **rename は可能**なので、退避 → `Move-Item` で新 exe 配置 → 残存 `*.old` を best-effort 削除の順にする。**退避名は一意（`aish.exe.<GetRandomFileName>.old`）にすること**（2026-07 実測: 固定名 `aish.exe.old` にしたら、前回失敗で残った `aish.exe.old` がロックされていて `Remove-Item -EA SilentlyContinue` が黙って失敗 → `Rename-Item -Force` は既存ターゲットを上書きしないため `RenameItemIOError`（ERROR_ALREADY_EXISTS）で落ちた。一意名なら残存 `.old` と衝突しない）。**プレリリースの rpm 落とし穴**: `cargo generate-rpm` は RPM version 規約で `-` を許さない（`invalid version "1.0.6-beta.1": contains invalid character`）ため、`-` 付きタグでは `release.yml` の rpm step が version の `-`→`~`（RPM の prerelease 慣習、安定版より小さくソート）に変換してから呼ぶ。deb は `-` を許容するので変換しない。**変換は `tr '-' '~'` で行う**: bash の `${var//-/~}` は replacement の `~` が tilde 展開されて `$HOME`（`/home/runner`）に化け、slash が混入して sed の `s///` デリミタを壊す（2026-07 実測、beta.1 再ビルドで `sed: unknown option to 's'`）。この変換を怠ると Linux ジョブが落ち release ジョブ全体が失敗し **GitHub Release 自体が作られない**（2026-07 実測、v1.0.6-beta.1）。Ctrl+Break は console ctrl event として aish を即殺し得る（必要なら SetConsoleCtrlHandler で無視、初期は文書化のみ）。legacy conhost / mintty は対象外（`console_ok()` が拒否）。
- **実機検証チェックリスト（2026-07 実機検証完了）**:
  1. **trust 最優先**: 打ちかけ入力 → Ctrl+/ → AI 承認 → 打ちかけが実行されない（ESC 方式の実効性。PSReadLine / cmd 両方）
  2. Ctrl+/ で minibuffer が開く（US/JIS 配列）、**Remmina/RDP 経由でも開く（`uChar=0x2f` 正規化の実効性）**、矢印・Alt+Enter・IME 日本語確定（サロゲート含む）・ペースト
  3. Y/n/a/q 1 キー確認、Ctrl+C（生 0x03）での確認キャンセル・実行中コマンド中断
  4. minibuffer/picker の描画（DSR 座標復元が ConPTY の再レンダリング下で正しいか）、ウィンドウリサイズ追従
  5. vim/top 等 TUI パススルー、SSH 接続（Windows OpenSSH）、cmd.exe プロンプト検出特例
