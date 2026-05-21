use super::common::{
    build_full_prompt, build_system_prompt, expand_tilde, extract_model_from_args,
    parse_ai_response_lossy, run_cli_capture_stdout,
};
use super::types::{AiBackend, AiError, AiRequest, AiResponse};
use crate::config::{AiConfig, LogConfig};

/// GitHub Copilot CLI (`copilot`) backend。
///
/// 実機調査 (copilot 1.0.51, `copilot --help`):
/// - `-p, --prompt <text>` フラグもあるが、**stdin が pipe されているとそちらが優先**される
///   ため aish は stdin 渡しに統一する (codex / cursor / gemini / qwen と同じパス)。
///   `-p` を付けてさらに stdin もあると "too many arguments" でエラーになるので `-p` は付けない。
/// - `--output-format json` は **JSONL** (1 行 1 JSON object)。`assistant.message` line の
///   `data.content` が最終応答テキスト、`result` line の `sessionId` が session UUID。
///   ephemeral な delta / status line は無視する。
/// - `--mode plan` / `--mode interactive` / `--mode autopilot`。aish では `"plan"` (read-only,
///   propose-only) を既定とする。
/// - `--allow-all-tools` は **非対話モードで必須**。代わりに `--deny-tool=shell --deny-tool=write`
///   で shell 実行とファイル書き込みを完全拒否する (deny は allow に優先)。これで信頼の根幹を守る。
/// - `--no-ask-user` で ask_user tool を無効化 (会話は aish が仕切る前提)。
/// - `--resume <chatId>` / `--continue` / `--session-id <id>` で session 再開できる (claude / codex / cursor と同形)。
/// - `--effort <level>` で reasoning effort を `none/low/medium/high/xhigh/max` から指定 (claude / codex 同等の native 対応)。
/// - `--model <name>` でモデル指定 (env `COPILOT_MODEL` も可)。
///
/// 戦略:
/// - 必須安全フラグ (`--output-format json --allow-all-tools --deny-tool=shell --deny-tool=write
///   --no-ask-user`) と config の `--mode` を毎回付与。
/// - 初回 send で `result.sessionId` を捕獲、2 回目以降 `--resume <sid>` で連結。
/// - system prompt は初回プロンプト先頭に焼き込む (resume 後は copilot 側が記憶しているので再送しない)。
/// - JSONL から最終 `assistant.message` の `data.content` を抽出 → `parse_ai_response_lossy` で
///   `{message, commands}` JSON を取り出す。失敗時は content 全体を message としてフォールバック。
pub struct CopilotBackend {
    system_prompt: String,
    log_path: Option<String>,
    base_extra_args: Vec<String>,
    /// `[ai.copilot].mode`。空でなければ `--mode <value>` を毎回追加。"plan" を推奨。
    mode: String,
    /// runtime モデル指定 (`/model`)。`Some` のとき send() 時に `--model <m>` を追加。
    model: Option<String>,
    /// runtime effort 指定 (`/effort`)。Copilot CLI は native `--effort` を持つので
    /// `Some` のとき `--effort <level>` で実リクエストに反映する。
    effort: Option<String>,
    /// 初回 send で捕獲した session_id。2 回目以降は `--resume <sid>` で連結する。
    session_id: Option<String>,
}

impl CopilotBackend {
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
            base_extra_args: cfg.copilot.extra_args.clone(),
            mode: cfg.copilot.mode.clone(),
            model: (!cfg.model.is_empty()).then(|| cfg.model.clone()),
            effort: (!cfg.effort.is_empty()).then(|| cfg.effort.clone()),
            session_id: None,
        }
    }
}

impl AiBackend for CopilotBackend {
    fn name(&self) -> &'static str {
        "copilot"
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
        self.effort = effort.map(str::to_string);
    }

    fn clear_history(&mut self) {
        // 次回 send は session_id を None のままで新規セッションを開始する。
        self.session_id = None;
    }

    fn resume_command(&self) -> Option<String> {
        self.session_id
            .as_ref()
            .map(|sid| format!("copilot --resume {sid}"))
    }

    fn send(&mut self, req: &AiRequest) -> Result<AiResponse, AiError> {
        // 初回: system prompt + terminal context + user prompt を全部焼き込む。
        // 2 回目以降: copilot 側が system + 履歴を持っているので terminal context + user prompt のみ。
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
            build_full_prompt(&self.system_prompt, &[], req.terminal_context, req.user_prompt)
        };

        let mut args: Vec<String> = vec![
            "--output-format".to_string(),
            "json".to_string(),
            // 信頼の根幹: shell 実行・書き込みを完全拒否。deny は --allow-all-tools より優先される。
            "--allow-all-tools".to_string(),
            "--deny-tool=shell".to_string(),
            "--deny-tool=write".to_string(),
            // 会話は aish が仕切るので copilot 側から user に質問させない。
            "--no-ask-user".to_string(),
        ];
        if !self.mode.is_empty() {
            args.push("--mode".to_string());
            args.push(self.mode.clone());
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
        if let Some(e) = &self.effort {
            args.push("--effort".to_string());
            args.push(e.clone());
        }

        let stdout = run_cli_capture_stdout("copilot", &args, &prompt, &self.log_path)?;

        let (assistant_text, session_id) = parse_jsonl_envelope(&stdout);
        if let Some(sid) = session_id {
            if self.session_id.is_none() {
                self.session_id = Some(sid);
            }
        }

        // assistant_text が取れなければ生 stdout でフォールバック解析。
        let response = parse_ai_response_lossy(assistant_text.as_deref().unwrap_or(&stdout));
        Ok(response)
    }
}

/// copilot の JSONL 出力 (`--output-format json`) を行ごとに走査し、
/// `(最終 assistant.message.data.content, result.sessionId)` を取り出す。
///
/// JSONL 中の関心オブジェクト:
/// - `{"type":"assistant.message", "data":{"content":"...","toolRequests":[]}}` (non-ephemeral)
///   `content` が最終応答テキスト。複数ターンが含まれる場合は最後のものを採用。
/// - `{"type":"result", "sessionId":"<uuid>", "exitCode":0}` (1 行だけ末尾に出る)
///
/// 無関係な type (session.*, assistant.message_start/delta (ephemeral=true), assistant.reasoning,
/// assistant.turn_start/end, user.message, ...) は無視する。
/// 行が JSON として parse できなければスキップ (連続スペースで分断された壊れ行への防御)。
fn parse_jsonl_envelope(raw: &str) -> (Option<String>, Option<String>) {
    let mut content: Option<String> = None;
    let mut session_id: Option<String> = None;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let type_str = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match type_str {
            "assistant.message" => {
                // ephemeral な delta 等は別 type なのでここでは見ない。
                if let Some(c) = obj
                    .get("data")
                    .and_then(|d| d.get("content"))
                    .and_then(|v| v.as_str())
                {
                    if !c.is_empty() {
                        content = Some(c.to_string());
                    }
                }
            }
            "result" => {
                if let Some(sid) = obj.get("sessionId").and_then(|v| v.as_str()) {
                    session_id = Some(sid.to_string());
                }
            }
            _ => {}
        }
    }
    (content, session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_assistant_message_and_session_id() {
        let jsonl = concat!(
            r#"{"type":"session.mcp_server_status_changed","data":{"serverName":"github-mcp-server","status":"connected"},"ephemeral":true}"#,
            "\n",
            r#"{"type":"user.message","data":{"content":"hi"}}"#,
            "\n",
            r#"{"type":"assistant.message","data":{"messageId":"x","content":"hello world","toolRequests":[]}}"#,
            "\n",
            r#"{"type":"result","sessionId":"abc-123","exitCode":0}"#,
            "\n",
        );
        let (content, sid) = parse_jsonl_envelope(jsonl);
        assert_eq!(content.as_deref(), Some("hello world"));
        assert_eq!(sid.as_deref(), Some("abc-123"));
    }

    #[test]
    fn last_assistant_message_wins_when_multiple() {
        let jsonl = concat!(
            r#"{"type":"assistant.message","data":{"content":"first"}}"#,
            "\n",
            r#"{"type":"assistant.message","data":{"content":"second"}}"#,
            "\n",
            r#"{"type":"result","sessionId":"sid","exitCode":0}"#,
            "\n",
        );
        let (content, sid) = parse_jsonl_envelope(jsonl);
        assert_eq!(content.as_deref(), Some("second"));
        assert_eq!(sid.as_deref(), Some("sid"));
    }

    #[test]
    fn skips_malformed_lines() {
        let jsonl = concat!(
            "garbage line\n",
            r#"{"type":"assistant.message","data":{"content":"ok"}}"#,
            "\n",
            "{not json\n",
            r#"{"type":"result","sessionId":"sid"}"#,
            "\n",
        );
        let (content, sid) = parse_jsonl_envelope(jsonl);
        assert_eq!(content.as_deref(), Some("ok"));
        assert_eq!(sid.as_deref(), Some("sid"));
    }

    #[test]
    fn returns_none_when_no_assistant_message() {
        let jsonl = concat!(
            r#"{"type":"session.mcp_servers_loaded","data":{}}"#,
            "\n",
            r#"{"type":"result","sessionId":"sid"}"#,
            "\n",
        );
        let (content, sid) = parse_jsonl_envelope(jsonl);
        assert!(content.is_none());
        assert_eq!(sid.as_deref(), Some("sid"));
    }

    #[test]
    fn ignores_empty_content() {
        // 空文字 content は採用せず None のまま (フォールバックで生 stdout に流れる)。
        let jsonl = concat!(
            r#"{"type":"assistant.message","data":{"content":""}}"#,
            "\n",
        );
        let (content, _) = parse_jsonl_envelope(jsonl);
        assert!(content.is_none());
    }
}
