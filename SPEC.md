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
| `--aish-config <path>` | 設定ファイルのパスを指定（デフォルト `~/.aish/config.toml`）|
| `--aish-*` (未知) | 警告を出して無視 |
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

### 5.4 シグナル
| シグナル | ハンドラ | 動作 |
|---|---|---|
| `SIGWINCH` | `sigwinch_handler` | `SIGWINCH_RECEIVED` をセット |

メインループ側で非同期に消費する。SIGINT は独自に処理せず、OS デフォルトに委ねる（rawモードでは ISIG 無効のためキーボード Ctrl+C は SIGINT を発行しない）。

---

## 6. AI連携（Claude Code CLI）

### 6.1 起動
- aish起動時に `claude --version` を実行し、失敗なら「Please install Claude Code」を表示して終了。

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
4. **各コマンドを1つずつ** `Execute [i/N]: {cmd} (Y/n)` で確認し、承認されたものは **マーカーラッパ** で包んで PTY に送信:
   ```sh
   { <cmd>; }; printf '\n__AISH_DONE_<id>_%03d__\n' "$?"
   ```
   `<id>` は aish プロセス ID + ナノ秒タイムスタンプの 24-hex（実行毎にユニーク）。
   `MarkerScanner` が PTY 出力を流しながらマーカー行を検出し、画面・リングバッファからは除去する。
5. **完了待ちループ**は約 20ms 周期で以下を並行処理する:
   - PTY 出力ドレイン（マーカースキャナを通して画面表示 + リングバッファ追記）。
   - `stdin → PTY` 転送（ノンブロッキング poll で fd 0 を直読）。実行中コマンドへのキー入力（パスワード入力・対話プロンプト応答）と Ctrl+C による中断（PTY 経由でシェルが SIGINT を発行）が可能。
   - SIGWINCH 検知（端末リサイズ追従）。
   - 完了判定:
     - **マーカー方式**: スキャナがマーカー行を検出 → exit code を取得 → 即時完了。
     - **フォールバック**（マーカー方式が使えない場合）: PTY 出力が 500ms 無音になったら完了とみなす。
6. **マーカー方式が使えないコマンド**（フォールバック対象）:
   - ヒアドキュメント（`<<` を含む）
   - 末尾 `&` でバックグラウンド実行
   - 未閉じのクォート / 行末バックスラッシュ
   - 空コマンド
7. すべて拒否された場合（1つも実行されなかった場合）は対話終了。
8. 少なくとも1つ実行した場合、followup プロンプトに各コマンドの実行サマリ（`` `cmd` (exit N) ``）を含めて AI へ送信し、出力本体は `terminal` フェンスでリングバッファから渡す。
9. 2へ戻る（空提案でループ終了）。
10. ループ終了後、PTYに `\n` を送信してシェルプロンプトを再描画。

マーカーラッパは PTY の echo によりユーザの画面に1行ぶん見えてしまうため、`EchoSkipper` で除去している（送信した改行数だけバイトをスキップしてから passthrough）。`stty -echo` 等で echo 無効になっている環境では `max_bytes` (4KB) で諦めて passthrough する。

### 6.7 キャンセル
- AIプロセス実行中、stdinをノンブロッキングpollして `0x03` 検知で `child.kill()`。エラー `"Cancelled"` として扱い、`^C` を表示して対話終了。

### 6.8 セッションID表示
- aish終了時、AIセッションが確立していた場合 `Resume this session with:\nclaude --resume {session_id}` を stderr に表示。

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

---

## 12. セルフアップデート (`--update`)

1. `std::env::consts::ARCH` で対応ターゲットを決定（`x86_64-unknown-linux-musl` / `aarch64-unknown-linux-musl`）。他は拒否。
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

CIワークフロー（`.github/workflows/release.yml`）側で `sha256sum aish-{target} > aish-{target}.sha256` を生成し、リリースアセットとして公開する。

---

## 13. エラー時の挙動

| 状況 | 挙動 |
|---|---|
| claude 未インストール | 起動時エラー表示＋`exit 1` |
| 設定ファイルパースエラー（デフォルトパス） | 警告を出して `Config::default()` で続行 |
| 設定ファイル読み込みエラー（デフォルトパス） | 同上 |
| 設定ファイルパース／読み込みエラー（`--aish-config` 明示） | エラー終了（`exit 1`） |
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
