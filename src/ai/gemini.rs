use super::common::{
    build_full_prompt, build_system_prompt, expand_tilde, extract_model_from_args,
    parse_ai_response_lossy, run_cli_capture_stdout, trim_history,
};
use super::types::{AiBackend, AiError, AiRequest, AiResponse};
use crate::config::{AiConfig, LogConfig};

const MAX_HISTORY_TURNS: usize = 8;

/// Gemini CLI backend。
///
/// 戦略:
/// - `gemini`: stdin に prompt を流す。`-p` で引数渡しも可能だがターミナルコンテキストが
///   ARG_MAX を超えやすいので stdin 一択。
/// - JSON Schema / dangerous tool 無効化フラグは無いので、`-y/--yolo` は付けず提案ベースで動かし、
///   system prompt で JSON 出力と「ツール非使用」を強く指示する。
/// - session resume 機構を持たないため、内部で履歴 (user_prompt, ai_message) を保持して
///   毎回プロンプトに含める。
pub struct GeminiBackend {
    system_prompt: String,
    log_path: Option<String>,
    base_extra_args: Vec<String>,
    /// runtime モデル指定 (`/model`)。`Some` のとき send() 時に `--model <m>` を追加。
    model: Option<String>,
    /// runtime effort 指定 (`/effort`)。Gemini CLI には該当フラグが無いので保存のみで適用しない。
    effort: Option<String>,
    history: Vec<(String, String)>,
}

impl GeminiBackend {
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
            base_extra_args: cfg.gemini.extra_args.clone(),
            model: (!cfg.model.is_empty()).then(|| cfg.model.clone()),
            effort: (!cfg.effort.is_empty()).then(|| cfg.effort.clone()),
            history: Vec::new(),
        }
    }
}

impl AiBackend for GeminiBackend {
    fn name(&self) -> &'static str {
        "gemini"
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
        // gemini CLI には reasoning effort フラグが無いので保存のみ (実リクエストには反映されない)。
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

        // stdin から prompt を流すので `-p` は付けない (引数で渡すと ARG_MAX 制限に当たる)。
        let mut args: Vec<String> = self.base_extra_args.clone();
        if let Some(m) = &self.model {
            args.push("--model".to_string());
            args.push(m.clone());
        }

        let stdout = run_cli_capture_stdout("gemini", &args, &prompt, &self.log_path)?;
        let response = parse_ai_response_lossy(&stdout);
        self.history
            .push((req.user_prompt.to_string(), response.message.clone()));
        trim_history(&mut self.history, MAX_HISTORY_TURNS);
        Ok(response)
    }
}
