use super::common::{
    build_full_prompt, build_system_prompt, expand_tilde, extract_model_from_args,
    parse_ai_response_lossy, run_cli_capture_stdout, trim_history,
};
use super::types::{AiBackend, AiError, AiRequest, AiResponse};
use crate::config::{AiConfig, LogConfig};

const MAX_HISTORY_TURNS: usize = 8;

/// Qwen Code CLI backend。
///
/// Qwen Code は Gemini CLI のフォークが基になっており、CLI インタフェースは概ね Gemini と同形と想定。
/// 詳細フラグは未確認だが、stdin プロンプトの素朴な使い方で動く前提。
/// 実機で問題があれば `[ai.qwen].extra_args` で調整できる。
///
/// 戦略:
/// - stdin から prompt を流す (ARG_MAX 回避)。
/// - JSON 強制機能は無いので system prompt で `{message, commands}` 出力を指示。
/// - session resume は使わず、内部で履歴 (user_prompt, ai_message) を保持して毎回再送。
pub struct QwenBackend {
    system_prompt: String,
    log_path: Option<String>,
    base_extra_args: Vec<String>,
    /// runtime モデル指定 (`/model`)。`Some` のとき send() 時に `--model <m>` を追加。
    model: Option<String>,
    /// runtime effort 指定 (`/effort`)。Qwen CLI には該当フラグが無いので保存のみで適用しない。
    effort: Option<String>,
    history: Vec<(String, String)>,
}

impl QwenBackend {
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
            base_extra_args: cfg.qwen.extra_args.clone(),
            model: (!cfg.model.is_empty()).then(|| cfg.model.clone()),
            effort: (!cfg.effort.is_empty()).then(|| cfg.effort.clone()),
            history: Vec::new(),
        }
    }
}

impl AiBackend for QwenBackend {
    fn name(&self) -> &'static str {
        "qwen"
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
        // qwen CLI には reasoning effort フラグが無いので保存のみ (実リクエストには反映されない)。
        self.effort = effort.map(str::to_string);
    }

    fn clear_history(&mut self) {
        self.history.clear();
    }

    fn send(&mut self, req: &AiRequest) -> Result<AiResponse, AiError> {
        let prompt = build_full_prompt(
            &self.system_prompt,
            &self.history,
            req.terminal_context,
            req.user_prompt,
        );

        let mut args: Vec<String> = self.base_extra_args.clone();
        if let Some(m) = &self.model {
            args.push("--model".to_string());
            args.push(m.clone());
        }

        let stdout = run_cli_capture_stdout("qwen", &args, &prompt, &self.log_path)?;
        let response = parse_ai_response_lossy(&stdout);
        self.history
            .push((req.user_prompt.to_string(), response.message.clone()));
        trim_history(&mut self.history, MAX_HISTORY_TURNS);
        Ok(response)
    }
}
