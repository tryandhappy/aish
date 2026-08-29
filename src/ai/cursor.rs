use super::common::{
    build_full_prompt, build_system_prompt, expand_tilde, extract_json, extract_model_from_args,
    parse_ai_response_lossy, resolve_option_list, run_cli_capture_stdout,
};
use super::types::{AiBackend, AiError, AiRequest, AiResponse};
use crate::config::{AiConfig, LogConfig, OptionLists};

/// `/model` ピッカーの組み込み既定 (config 未設定時)。値は流動的なので best-effort。更新はリリース必要。
/// (Free プランでは `auto` のみ受理。`/model -` で既定に戻せる。)
/// 先頭 `auto` は cursor 側で最新へルーティングするエイリアス = 陳腐化しない。
/// slug は dash 形式 (`cursor-agent --help` の実例 `claude-opus-4-8`)。composer-2.5 は要実測。
/// `cursor-agent models` (要 auth) で一覧取得可能だが出力形式未実測のため組み込み自動取得はしない
/// (config の `models_command` で任意設定は可能 — SPEC § 15.12)。
const MODEL_DEFAULTS: &[&str] = &[
    "auto",
    "composer-2.5",
    "claude-opus-4-8",
    "gpt-5.5",
    "gemini-3-pro",
];

/// Cursor CLI (`cursor-agent`) backend。
///
/// 実機調査 (2026 年版 `cursor-agent --help`):
/// - `-p, --print` — headless 出力モード
/// - `--output-format json` — 1 行 JSON `{"type":"result","result":"<text>","session_id":"<uuid>", ...}` 形式
/// - `--trust` — **headless モードで必須**。未指定だと "Workspace Trust Required" で実行を拒否される
/// - `--mode plan` — read-only / planning モード (no edits)。aish の「提案のみ」と方針が一致する安全プリミティブ
/// - `--mode ask` — Q&A モード (read-only)、`plan` よりさらに厳格
/// - `--sandbox enabled|disabled` — OS レベルサンドボックス (別軸の defense-in-depth)
/// - `--resume <chatId>` — 既存 session の継続。`--output-format json` 応答内 `session_id` をそのまま渡せる
/// - `--model <name>` — モデル指定 (Free プランでは `auto` のみ)
///
/// 戦略:
/// - 必須フラグ (`-p --output-format json --trust`) と config 由来の `--mode` / `--sandbox` を毎回付ける。
/// - 初回 send 後、応答 JSON から `session_id` を捕獲し、2 回目以降は `--resume <sid>` で連結する
///   (claude / codex と同様の native resume 方式)。これで terminal context を再送する負担と
///   token 消費を抑える。`--append-system-prompt` 相当が無いので system prompt は初回プロンプト先頭に
///   `build_full_prompt` で焼き込む。resume 後は terminal context + user prompt のみ送る。
/// - 個別ツール無効化フラグ (codex の `--disable shell_tool` 相当) は cursor-agent には無いので、
///   ツール抑制は `--mode plan` (デフォルト) + system prompt の best-effort 指示の二段構え。
/// - reasoning effort フラグは cursor-agent 側に無いので保存のみで実リクエストには反映しない。
pub struct CursorBackend {
    system_prompt: String,
    log_path: Option<String>,
    base_extra_args: Vec<String>,
    /// `[ai.cursor].mode`。空でなければ `--mode <value>` を毎回追加。"plan" を推奨。
    mode: String,
    /// `[ai.cursor].sandbox`。空でなければ `--sandbox <value>` を毎回追加。
    sandbox: String,
    /// runtime モデル指定 (`/model`)。`Some` のとき send() 時に `--model <m>` を追加。
    model: Option<String>,
    /// runtime effort 指定 (`/effort`)。cursor-agent には該当フラグが無いので保存のみで適用しない。
    effort: Option<String>,
    /// 初回 send で捕獲した session_id。2 回目以降は `--resume <sid>` で連結する。
    session_id: Option<String>,
    /// `/model` `/effort` ピッカーの候補リスト設定 (effort は組み込み既定なし)。
    options: OptionLists,
}

impl CursorBackend {
    pub fn new(cfg: &AiConfig, log: &LogConfig) -> Self {
        let log_path = if log.enabled {
            Some(expand_tilde(&log.path))
        } else {
            None
        };
        let system_prompt = build_system_prompt(&cfg.system_prompt, &cfg.language);
        Self {
            system_prompt,
            log_path,
            base_extra_args: cfg.cursor.extra_args.clone(),
            mode: cfg.cursor.mode.clone(),
            sandbox: cfg.cursor.sandbox.clone(),
            model: (!cfg.model.is_empty()).then(|| cfg.model.clone()),
            effort: (!cfg.effort.is_empty()).then(|| cfg.effort.clone()),
            session_id: None,
            options: cfg.cursor.options.clone(),
        }
    }

    /// cursor-agent の引数を組み立てる (send から機械抽出した純関数。golden test 対象)。
    /// `--trust` は headless 必須のため無条件、ツール抑制は `--mode plan` (既定 config)。
    fn build_args(&self) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "-p".to_string(),
            "--output-format".to_string(),
            "json".to_string(),
            // headless では --trust が無いと workspace trust 確認で実行拒否される。
            "--trust".to_string(),
        ];
        if !self.mode.is_empty() {
            args.push("--mode".to_string());
            args.push(self.mode.clone());
        }
        if !self.sandbox.is_empty() {
            args.push("--sandbox".to_string());
            args.push(self.sandbox.clone());
        }
        if let Some(sid) = &self.session_id {
            args.push("--resume".to_string());
            args.push(sid.clone());
        }
        args.extend(self.base_extra_args.iter().cloned());
        if let Some(m) = &self.model {
            args.push("--model".to_string());
            args.push(m.clone());
        }
        args
    }
}

impl AiBackend for CursorBackend {
    fn name(&self) -> &'static str {
        "cursor"
    }

    fn model(&self) -> Option<String> {
        self.model
            .clone()
            .or_else(|| extract_model_from_args(&self.base_extra_args))
    }

    fn effort(&self) -> Option<String> {
        self.effort.clone()
    }

    fn set_model(&mut self, model: Option<&str>) {
        self.model = model.map(str::to_string);
    }

    fn set_effort(&mut self, effort: Option<&str>) {
        // cursor-agent には reasoning effort フラグが無いので保存のみ。
        self.effort = effort.map(str::to_string);
    }

    fn available_models(&self) -> Vec<String> {
        resolve_option_list(
            &self.options.models,
            &self.options.models_command,
            MODEL_DEFAULTS,
            &self.log_path,
        )
    }

    fn available_efforts(&self) -> Vec<String> {
        resolve_option_list(
            &self.options.efforts,
            &self.options.efforts_command,
            &[],
            &self.log_path,
        )
    }

    fn clear_history(&mut self) {
        // 次回 send は session_id を None のままで新規セッションを開始する
        // (system prompt も再注入される)。
        self.session_id = None;
    }

    fn resume_command(&self) -> Option<String> {
        self.session_id
            .as_ref()
            .map(|sid| format!("cursor-agent --resume {sid}"))
    }

    fn send(&mut self, req: &AiRequest) -> Result<AiResponse, AiError> {
        // 初回: system prompt + terminal context + user prompt を全部焼き込む。
        // 2 回目以降: cursor-agent 側が system + 履歴を持っているので、
        //             terminal context (差分) + user prompt のみ。
        let prompt = if self.session_id.is_some() {
            if req.terminal_context.is_empty() {
                req.user_prompt.to_string()
            } else {
                format!(
                    "```terminal\n{}\n```\n\n{}",
                    req.terminal_context, req.user_prompt
                )
            }
        } else {
            build_full_prompt(
                &self.system_prompt,
                &[],
                req.terminal_context,
                req.user_prompt,
            )
        };

        let args = self.build_args();

        let stdout = run_cli_capture_stdout("cursor-agent", &args, &prompt, &self.log_path)?;

        // 外側ラッパ `{"type":"result", "result":"<text>", "session_id":"..."}` を剥がす。
        let (assistant_text, session_id) = unwrap_cursor_envelope(&stdout);

        // 初回応答で session_id を捕獲。以降は --resume <sid> で連結。
        // resume 中も応答に同じ session_id が返るが、上書きしても挙動は変わらない。
        if let Some(sid) = session_id {
            if self.session_id.is_none() {
                self.session_id = Some(sid);
            }
        }

        // assistant_text が取れなければ生 stdout を渡してフォールバック解析。
        let response = parse_ai_response_lossy(assistant_text.as_deref().unwrap_or(&stdout));
        Ok(response)
    }
}

/// cursor-agent の `--output-format json` 出力から `(result text, session_id)` を取り出す。
/// 外側 JSON が見つからない / `result` が無い場合は `(None, None)` を返す
/// (呼び出し側で生 stdout フォールバック)。
fn unwrap_cursor_envelope(raw: &str) -> (Option<String>, Option<String>) {
    let Some(envelope_str) = extract_json(raw) else {
        return (None, None);
    };
    let Ok(envelope) = serde_json::from_str::<serde_json::Value>(envelope_str) else {
        return (None, None);
    };
    let result = envelope
        .get("result")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let session_id = envelope
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    (result, session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_always_contain_trust_and_plan_mode() {
        // --trust は headless 必須、既定 config では --mode plan (read-only) が付く。
        let backend = CursorBackend::new(&AiConfig::default(), &LogConfig::default());
        let args = backend.build_args();
        assert!(args.iter().any(|a| a == "--trust"));
        let m = args.iter().position(|a| a == "--mode").expect("--mode");
        assert_eq!(args[m + 1], "plan");
    }

    #[test]
    fn args_never_contain_run_everything_flags() {
        // --yolo / -f (Run Everything) は承認 UI 迂回 = 信頼の根幹違反。
        let backend = CursorBackend::new(&AiConfig::default(), &LogConfig::default());
        let args = backend.build_args();
        for forbidden in ["--yolo", "-f", "--force"] {
            assert!(!args.iter().any(|a| a == forbidden), "found {forbidden}");
        }
    }

    #[test]
    fn args_empty_mode_omits_mode_flag() {
        // mode="" (非推奨だが許容) では --mode を出さない挙動を固定。
        let mut cfg = AiConfig::default();
        cfg.cursor.mode = String::new();
        let backend = CursorBackend::new(&cfg, &LogConfig::default());
        let args = backend.build_args();
        assert!(!args.iter().any(|a| a == "--mode"));
        assert!(
            args.iter().any(|a| a == "--trust"),
            "--trust は mode 非依存"
        );
    }

    #[test]
    fn unwraps_result_field() {
        let s = r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"message\":\"hi\",\"commands\":[]}","session_id":"abc-123"}"#;
        let (result, sid) = unwrap_cursor_envelope(s);
        assert_eq!(result.as_deref(), Some(r#"{"message":"hi","commands":[]}"#));
        assert_eq!(sid.as_deref(), Some("abc-123"));
    }

    #[test]
    fn returns_none_when_no_envelope() {
        let (result, sid) = unwrap_cursor_envelope("plain text no json");
        assert!(result.is_none());
        assert!(sid.is_none());
    }

    #[test]
    fn returns_none_when_envelope_lacks_result() {
        let s = r#"{"type":"result","session_id":"abc"}"#;
        let (result, sid) = unwrap_cursor_envelope(s);
        assert!(result.is_none());
        assert_eq!(sid.as_deref(), Some("abc"));
    }

    #[test]
    fn model_defaults_present_without_config() {
        // config 空でも組み込み既定で `/model` ピッカーの候補が出る (既定消失の回帰防止)。
        let backend = CursorBackend::new(&AiConfig::default(), &LogConfig::default());
        assert!(!backend.available_models().is_empty());
    }
}
