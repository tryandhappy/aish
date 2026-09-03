use super::common::{
    build_full_prompt, build_system_prompt, expand_tilde, extract_model_from_args,
    parse_ai_response_lossy, resolve_option_list, run_cli_capture_stdout, trim_history,
};
use super::types::{AiBackend, AiError, AiRequest, AiResponse};
use crate::config::{AiConfig, LogConfig, OptionLists};

const MAX_HISTORY_TURNS: usize = 8;

/// `/model` ピッカーの組み込み既定 (config 未設定時)。xAI の公式 Grok CLI (`grok`, x.ai/cli)。
/// 値は流動的なので best-effort スナップショット (更新にリリースが要る)。
/// xAI の `<name>-latest` エイリアスは modelname 単位でしか解決しない (grok-4-latest は 4.5/4.6 に
/// ならない)。xAI が grok-4.5 / grok-4.6 と modelname を改番したためエイリアスは陳腐化回避に効かず、
/// `grok-4-latest` は撤回してスナップショット運用に戻した (SPEC § 15.12)。
const MODEL_DEFAULTS: &[&str] = &[
    "grok-4.6",
    "grok-4.5",
    "grok-4.3",
    "grok-4.20-0309-reasoning",
    "grok-build-0.1",
];

/// xAI Grok CLI backend (`grok`、https://x.ai/cli、`--ai grok`)。
///
/// 戦略 (gemini/qwen と同じ system-prompt-only 方式):
/// - `grok -p`: headless (非対話) モード。prompt は stdin に流す (ARG_MAX 回避)。
/// - read-only / plan の permission-layer 強制は headless で保証できないため、claude の
///   ようなツール deny フラグは使わず、system prompt で「ツール非使用・提案のみ」を強く指示する
///   (gemini/qwen と同型の安全posture)。`--always-approve` (auto-approve) は絶対に付けない。
/// - reasoning effort フラグは持たないので保存のみ (実リクエストには反映しない)。model 指定は `-m`。
/// - 非対話 session resume が安定しないため、内部で履歴 (user_prompt, ai_message) を保持して
///   毎回プロンプトに含める。
///
/// 注意: `grok` はコミュニティ製 `@vibe-kit/grok-cli` (npm) ともバイナリ名が衝突しうる。
/// 公式 CLI を使っているか `which -a grok` で確認すること。
pub struct GrokBackend {
    system_prompt: String,
    log_path: Option<String>,
    base_extra_args: Vec<String>,
    /// runtime モデル指定 (`/model`)。`Some` のとき send() 時に `-m <m>` を追加。
    model: Option<String>,
    /// runtime effort 指定 (`/effort`)。Grok CLI には該当フラグが無いので保存のみで適用しない。
    effort: Option<String>,
    /// `/model` `/effort` ピッカーの候補リスト設定 (effort は組み込み既定なし)。
    options: OptionLists,
    history: Vec<(String, String)>,
}

impl GrokBackend {
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
            base_extra_args: cfg.grok.extra_args.clone(),
            model: (!cfg.model.is_empty()).then(|| cfg.model.clone()),
            effort: (!cfg.effort.is_empty()).then(|| cfg.effort.clone()),
            options: cfg.grok.options.clone(),
            history: Vec::new(),
        }
    }

    /// grok CLI の引数を組み立てる (send から抽出した純関数。golden test 対象)。
    /// `-p` (headless) を先頭に固定し、model を後置きする。
    fn build_args(&self) -> Vec<String> {
        let mut args: Vec<String> = vec!["-p".to_string()];
        args.extend(self.base_extra_args.iter().cloned());
        if let Some(m) = &self.model {
            args.push("-m".to_string());
            args.push(m.clone());
        }
        args
    }
}

impl AiBackend for GrokBackend {
    fn name(&self) -> &'static str {
        "grok"
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
        // grok CLI には reasoning effort フラグが無いので保存のみ (実リクエストには反映されない)。
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
        self.history.clear();
    }

    fn send(&mut self, req: &AiRequest) -> Result<AiResponse, AiError> {
        let prompt = build_full_prompt(
            &self.system_prompt,
            &self.history,
            req.terminal_context,
            req.user_prompt,
        );

        let args = self.build_args();
        // prompt は引数ではなく stdin で渡す (ARG_MAX 回避)。
        let stdout = run_cli_capture_stdout("grok", &args, &prompt, &self.log_path)?;
        let response = parse_ai_response_lossy(&stdout);
        self.history
            .push((req.user_prompt.to_string(), response.message.clone()));
        trim_history(&mut self.history, MAX_HISTORY_TURNS);
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_defaults_present_without_config() {
        // config 空でも組み込み既定で `/model` ピッカーの候補が出る (既定消失の回帰防止)。
        let backend = GrokBackend::new(&AiConfig::default(), &LogConfig::default());
        assert!(!backend.available_models().is_empty());
    }

    #[test]
    fn args_use_headless_flag_and_never_auto_approve() {
        // 信頼の根幹: headless は `-p`、auto-approve 系フラグは絶対に付けない。
        let cfg = AiConfig {
            model: "grok-4".to_string(),
            ..AiConfig::default()
        };
        let backend = GrokBackend::new(&cfg, &LogConfig::default());
        let args = backend.build_args();
        assert_eq!(args.first().map(String::as_str), Some("-p"));
        assert!(args.iter().any(|a| a == "-m"));
        assert!(!args
            .iter()
            .any(|a| a == "--always-approve" || a == "--yolo"));
    }
}
