use super::common::{
    build_system_prompt_claude, expand_tilde, extract_json, extract_model_from_args,
    resolve_option_list, run_cli_capture_stdout,
};
use super::types::{AiBackend, AiError, AiRequest, AiResponse};
use crate::config::{AiConfig, LogConfig, OptionLists};

/// `/effort` ピッカーの組み込み既定 (config 未設定時)。claude CLI の `--effort`。
const EFFORT_DEFAULTS: &[&str] = &["low", "medium", "high"];

/// `/model` ピッカーの組み込み既定 (config 未設定時)。値は流動的なので best-effort
/// (検証せず `--model <値>` に渡すだけ。誤りの実害は CLI 起動エラー程度)。更新はリリース必要。
const MODEL_DEFAULTS: &[&str] = &["claude-opus-4-8", "claude-sonnet-4-6", "claude-haiku-4-5"];

const AI_RESPONSE_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "message": { "type": "string", "description": "ユーザへの説明" },
    "commands": {
      "type": "array",
      "items": { "type": "string" },
      "description": "ユーザに実行を提案するコマンドのリスト。message 本文で実行コマンドを提示したら同じものを必ずここにも入れる(本文だけに書かない)。独立した複数のコマンドは ; で1つに連結せず配列の別要素に分割する(ただし &&・|| や for/while/case 等の制御構文内の ; は1コマンドとして維持)。1つのコマンドが複数行になる場合(heredoc やスクリプト等)は無理に1行へ詰めず改行を保持して1要素にする。提案すべきコマンドが無ければ空配列。"
    }
  },
  "required": ["message", "commands"]
}"#;

pub struct ClaudeBackend {
    session_id: Option<String>,
    system_prompt: String,
    disallowed_tools: String,
    /// ユーザの `[ai.claude].extra_args` をそのまま保持。`-m` 等が含まれていてもここでは触らない。
    /// 最終的な model / effort 指定は `model` / `effort` フィールド経由で send() 時に追記する。
    base_extra_args: Vec<String>,
    /// runtime モデル指定 (`/model` で書き換え可能)。`Some` のとき send() 時に `--model <m>` を追加。
    model: Option<String>,
    /// runtime effort 指定 (`/effort` で書き換え可能)。`Some` のとき send() 時に `--effort <e>` を追加。
    effort: Option<String>,
    /// `/model` `/effort` ピッカーの候補リスト設定。
    options: OptionLists,
    log_path: Option<String>,
}

/// AI 自身が直接握ると「提案→確認→PTY 送信」の信頼境界を迂回できてしまうツール。
/// `allow_unsafe_tools=false` の間は、ユーザの `disallowed_tools` 設定に関わらず常に deny する。
/// (Read はファイル読み取りのみで相対的に低リスクなので baseline に含めず、default 値で deny する。)
const MANDATORY_DENY: &[&str] = &["Bash", "Edit", "Write"];

/// claude に渡す実効 `--disallowedTools` 値を計算する。
/// - `allow_unsafe == false` (既定): `configured` に [`MANDATORY_DENY`] を union する
///   (baseline を先頭に、重複排除)。`configured` が空でも Bash/Edit/Write は必ず残る。
/// - `allow_unsafe == true`: `configured` を verbatim で返す (上級者が完全制御)。
fn effective_disallowed_tools(configured: &str, allow_unsafe: bool) -> String {
    if allow_unsafe {
        return configured.to_string();
    }
    let mut tools: Vec<String> = MANDATORY_DENY.iter().map(|s| s.to_string()).collect();
    for t in configured.split(',') {
        let t = t.trim();
        if !t.is_empty() && !tools.iter().any(|x| x == t) {
            tools.push(t.to_string());
        }
    }
    tools.join(",")
}

impl ClaudeBackend {
    pub fn new(cfg: &AiConfig, log: &LogConfig) -> Self {
        let log_path = if log.enabled {
            Some(expand_tilde(&log.path))
        } else {
            None
        };
        let system_prompt = build_system_prompt_claude(&cfg.system_prompt, &cfg.language);
        Self {
            session_id: None,
            system_prompt,
            disallowed_tools: effective_disallowed_tools(
                &cfg.claude.disallowed_tools,
                cfg.claude.allow_unsafe_tools,
            ),
            base_extra_args: cfg.claude.extra_args.clone(),
            model: (!cfg.model.is_empty()).then(|| cfg.model.clone()),
            effort: (!cfg.effort.is_empty()).then(|| cfg.effort.clone()),
            options: cfg.claude.options.clone(),
            log_path,
        }
    }
}

impl AiBackend for ClaudeBackend {
    fn name(&self) -> &'static str {
        "claude"
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
            EFFORT_DEFAULTS,
            &self.log_path,
        )
    }

    fn clear_history(&mut self) {
        // claude は CLI 側 session で履歴を持つため、resume を切るだけで新規セッションになる。
        self.session_id = None;
    }

    fn resume_command(&self) -> Option<String> {
        self.session_id
            .as_ref()
            .map(|sid| format!("claude --resume {sid}"))
    }

    fn send(&mut self, req: &AiRequest) -> Result<AiResponse, AiError> {
        let prompt = if req.terminal_context.is_empty() {
            req.user_prompt.to_string()
        } else {
            format!(
                "```terminal\n{}\n```\n\n{}",
                req.terminal_context, req.user_prompt
            )
        };

        // 共通フラグ + 初回 vs resume の差分を組み立てる。
        // 安全制約 (--disallowedTools) と出力形式は毎回明示する。
        // --append-system-prompt は append 動作のため初回のみ（resume でも付けると二重に追加される）。
        let mut args: Vec<String> = vec!["-p".to_string()];

        if let Some(ref sid) = self.session_id {
            args.push("--resume".to_string());
            args.push(sid.clone());
        } else {
            // `self.system_prompt` (= build_system_prompt_claude → build_system_prompt) に
            // 安全制約・JSON フォーマット指示が全て含まれている。inline で追記する必要なし。
            // `--append-system-prompt` は append 動作なので初回のみ (resume では二重追加になる)。
            args.push("--append-system-prompt".to_string());
            args.push(self.system_prompt.clone());
        }

        args.push("--output-format".to_string());
        args.push("json".to_string());
        args.push("--json-schema".to_string());
        args.push(AI_RESPONSE_SCHEMA.to_string());
        args.extend(self.base_extra_args.iter().cloned());
        // runtime model / effort は base_extra_args の後に追加 (CLI は通常後勝ち)。
        if let Some(m) = &self.model {
            args.push("--model".to_string());
            args.push(m.clone());
        }
        if let Some(e) = &self.effort {
            args.push("--effort".to_string());
            args.push(e.clone());
        }
        // 安全制約 (--disallowedTools) は args の末尾で付与する。extra_args に
        // `--disallowedTools ""` 等を後置きされても CLI 後勝ちでこちらが勝ち、
        // baseline (Bash/Edit/Write) を non-removable に保つため。値は new() で
        // effective_disallowed_tools により MANDATORY_DENY を union 済み。
        args.push("--disallowedTools".to_string());
        args.push(self.disallowed_tools.clone());
        // prompt は引数ではなく stdin で渡す。
        // ターミナルコンテキストを含む prompt が ARG_MAX (~2MB) を超えると
        // execve() が E2BIG (`Argument list too long`, os error 7) で失敗するため。

        let stdout = run_cli_capture_stdout("claude", &args, &prompt, &self.log_path)?;
        let stdout_trimmed = stdout.trim();

        // claude CLIの出力にJSON以外のテキストが含まれる場合があるため、
        // JSON部分を抽出する
        let json_str = extract_json(stdout_trimmed).ok_or_else(|| AiError::NoJson {
            raw: stdout_trimmed.to_string(),
        })?;

        let claude_output: serde_json::Value =
            serde_json::from_str(json_str).map_err(|e| AiError::ParseFailure {
                raw: stdout_trimmed.to_string(),
                source: e,
            })?;

        if let Some(sid) = claude_output["session_id"].as_str() {
            if self.session_id.is_none() {
                self.session_id = Some(sid.to_string());
            }
        }

        // --json-schema使用時はstructured_outputにレスポンスが入る
        // structured_outputがなければresultにフォールバック
        let result_value = if claude_output["structured_output"].is_object() {
            &claude_output["structured_output"]
        } else {
            &claude_output["result"]
        };

        let ai_response = match result_value {
            serde_json::Value::Object(_) => {
                serde_json::from_value::<AiResponse>(result_value.clone()).unwrap_or_else(|_| {
                    AiResponse {
                        message: result_value.to_string(),
                        commands: vec![],
                    }
                })
            }
            serde_json::Value::String(s) => {
                let s = s.trim();
                if s.is_empty() {
                    return Err(AiError::Other(format!(
                        "claude returned empty result.\nFull output: {stdout_trimmed}"
                    )));
                }
                serde_json::from_str::<AiResponse>(s).unwrap_or_else(|_| AiResponse {
                    message: s.to_string(),
                    commands: vec![],
                })
            }
            _ => {
                return Err(AiError::Other(format!(
                    "Unexpected result from claude.\nFull output: {stdout_trimmed}"
                )));
            }
        };
        Ok(ai_response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contains_tool(value: &str, tool: &str) -> bool {
        value.split(',').any(|t| t.trim() == tool)
    }

    #[test]
    fn empty_config_still_denies_baseline() {
        // disallowed_tools="" でも Bash/Edit/Write は必ず残る (footgun 防止)。
        let v = effective_disallowed_tools("", false);
        assert!(contains_tool(&v, "Bash"));
        assert!(contains_tool(&v, "Edit"));
        assert!(contains_tool(&v, "Write"));
    }

    #[test]
    fn config_extras_are_unioned() {
        // default 値はそのまま全部含む。
        let v = effective_disallowed_tools("Bash,Edit,Write,Read", false);
        for t in ["Bash", "Edit", "Write", "Read"] {
            assert!(contains_tool(&v, t), "missing {t} in {v}");
        }
        // Read のみ指定 → baseline と union され Read も Bash/Edit/Write も含む。
        let v = effective_disallowed_tools("Read", false);
        for t in ["Bash", "Edit", "Write", "Read"] {
            assert!(contains_tool(&v, t), "missing {t} in {v}");
        }
    }

    #[test]
    fn no_duplicate_baseline_entries() {
        // configured が baseline と重複しても二重に出さない。
        let v = effective_disallowed_tools("Bash,Bash,Edit", false);
        let count = v.split(',').filter(|t| t.trim() == "Bash").count();
        assert_eq!(count, 1, "Bash duplicated in {v}");
    }

    #[test]
    fn allow_unsafe_is_verbatim() {
        // opt-in 時は baseline を強制せず verbatim。
        assert_eq!(effective_disallowed_tools("", true), "");
        assert_eq!(effective_disallowed_tools("Read", true), "Read");
    }

    #[test]
    fn model_defaults_present_without_config() {
        // config 空でも組み込み既定で `/model` ピッカーの候補が出る (既定消失の回帰防止)。
        let backend = ClaudeBackend::new(&AiConfig::default(), &LogConfig::default());
        assert!(!backend.available_models().is_empty());
    }
}
