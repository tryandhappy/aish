use super::common::{
    build_full_prompt, build_system_prompt, expand_tilde, extract_json,
    extract_model_from_args, parse_ai_response_lossy, run_cli_capture_stdout, trim_history,
};
use super::types::{AiBackend, AiError, AiRequest, AiResponse};
use crate::config::{AiConfig, LogConfig};

const MAX_HISTORY_TURNS: usize = 8;

/// Cursor CLI (`cursor-agent`) backend。
///
/// 戦略 (gemini と同等のスタンス):
/// - `cursor-agent -p --output-format json` を都度 spawn し、prompt は stdin に流す。
/// - cursor-agent には codex の `--disable shell_tool ...` に相当する「全ツール無効化」フラグが
///   無いので、ツール非使用は **system prompt の best-effort 指示頼み** にする。
///   sandbox は `--sandbox enabled` / `disabled` をユーザ設定で渡せるようにする。
///   (`[ai.cursor].sandbox = "enabled"` 推奨)
/// - JSON Schema 強制も無いので system prompt で `{message, commands}` 出力を指示。
/// - session resume は使わず、内部で履歴 (user_prompt, ai_message) を保持して毎回再送する
///   (gemini / qwen と同じ方式)。cursor-agent 自身は `--resume <chatId>` を持つが、
///   session 維持の安定性 / ツール挙動への影響を慎重に切り分けるため初版では使わない。
///
/// 出力パース:
/// - cursor-agent の `--output-format json` は外側に `{"type":"result","subtype":"success",
///   "is_error":false,"result":"<assistant text>","session_id":"<uuid>",...}` というラッパを
///   返す。assistant text (= 我々が欲しい `{message, commands}` JSON) は `result` フィールドの
///   中に入っている。
/// - 外側 JSON を抽出 → `result` 文字列を取り出して中身を `parse_ai_response_lossy` に流す。
///   外側 JSON が見つからなければ全文をそのまま `parse_ai_response_lossy` に渡すフォールバック。
pub struct CursorBackend {
    system_prompt: String,
    log_path: Option<String>,
    base_extra_args: Vec<String>,
    /// `[ai.cursor].sandbox` の値。空でなければ `--sandbox <value>` として send 時に追加。
    sandbox: String,
    /// runtime モデル指定 (`/model`)。`Some` のとき send() 時に `--model <m>` を追加。
    model: Option<String>,
    /// runtime effort 指定 (`/effort`)。cursor-agent には該当フラグが無いので保存のみで適用しない。
    effort: Option<String>,
    /// 直近のレスポンスに含まれていた cursor-agent 側 session_id (UUID)。
    /// 履歴ベースで動くので送信時には参照しないが、aish 終了時の resume_command 案内に使う。
    last_session_id: Option<String>,
    history: Vec<(String, String)>,
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
            sandbox: cfg.cursor.sandbox.clone(),
            model: (!cfg.model.is_empty()).then(|| cfg.model.clone()),
            effort: (!cfg.effort.is_empty()).then(|| cfg.effort.clone()),
            last_session_id: None,
            history: Vec::new(),
        }
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
        // cursor-agent には reasoning effort フラグが無いので保存のみ (実リクエストには反映されない)。
        self.effort = effort.map(str::to_string);
    }

    fn clear_history(&mut self) {
        self.history.clear();
        self.last_session_id = None;
    }

    fn resume_command(&self) -> Option<String> {
        // cursor-agent はレスポンスごとに session_id を返す。最後に捕獲した ID で resume を案内。
        self.last_session_id
            .as_ref()
            .map(|sid| format!("cursor-agent --resume {sid}"))
    }

    fn send(&mut self, req: &AiRequest) -> Result<AiResponse, AiError> {
        let prompt = build_full_prompt(
            &self.system_prompt,
            &self.history,
            req.terminal_context,
            req.user_prompt,
        );

        // 必須フラグ + ユーザ指定。stdin から prompt を流すので positional arg は付けない。
        let mut args: Vec<String> = vec![
            "-p".to_string(),
            "--output-format".to_string(),
            "json".to_string(),
        ];
        if !self.sandbox.is_empty() {
            args.push("--sandbox".to_string());
            args.push(self.sandbox.clone());
        }
        args.extend(self.base_extra_args.iter().cloned());
        if let Some(m) = &self.model {
            args.push("--model".to_string());
            args.push(m.clone());
        }

        let stdout = run_cli_capture_stdout("cursor-agent", &args, &prompt, &self.log_path)?;

        // 外側ラッパ `{"type":"result", "result":"<text>", "session_id":"..."}` を剥がす。
        let (assistant_text, session_id) = unwrap_cursor_envelope(&stdout);
        if let Some(sid) = session_id {
            self.last_session_id = Some(sid);
        }
        // assistant_text が取れなければ生 stdout を渡してフォールバック解析。
        let response = parse_ai_response_lossy(assistant_text.as_deref().unwrap_or(&stdout));

        self.history
            .push((req.user_prompt.to_string(), response.message.clone()));
        trim_history(&mut self.history, MAX_HISTORY_TURNS);
        Ok(response)
    }
}

/// cursor-agent の `--output-format json` 出力から `(result text, session_id)` を取り出す。
/// 外側 JSON が見つからない / `result` が無い場合は `(None, None)` を返す (呼び出し側で生 stdout フォールバック)。
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
    fn unwraps_result_field() {
        let s = r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"message\":\"hi\",\"commands\":[]}","session_id":"abc-123"}"#;
        let (result, sid) = unwrap_cursor_envelope(s);
        assert_eq!(
            result.as_deref(),
            Some(r#"{"message":"hi","commands":[]}"#)
        );
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
}
