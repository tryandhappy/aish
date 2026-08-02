use super::common::{
    build_full_prompt, build_system_prompt, expand_tilde, extract_model_from_args,
    parse_ai_response_lossy, resolve_option_list, run_cli_capture_stdout, trim_history,
};
use super::types::{AiBackend, AiError, AiRequest, AiResponse};
use crate::config::{AiConfig, LogConfig, OptionLists};

const MAX_HISTORY_TURNS: usize = 8;

/// `/effort` ピッカーの組み込み既定 (config 未設定時)。agy CLI の `--effort low|medium|high`。
const EFFORT_DEFAULTS: &[&str] = &["low", "medium", "high"];

/// `/model` ピッカーの組み込み既定 (config 未設定時)。agy は Gemini CLI の後継で
/// Gemini 系モデルを使う。値は流動的なので best-effort (`agy models` で最新を確認)。更新はリリース必要。
const MODEL_DEFAULTS: &[&str] = &[
    "gemini-3-pro",
    "gemini-3-flash",
    "gemini-2.5-pro",
    "gemini-2.5-flash",
];

/// Google Antigravity CLI backend (`agy`)。Gemini CLI の後継 (2026、`--ai antigravity`)。
///
/// 戦略 (gemini/qwen と同じ system-prompt-only 方式):
/// - `agy -p`: headless (非対話) モード。prompt は stdin に流す (`-p` は print モードの
///   フラグで、prompt 本体は stdin。ターミナルコンテキストが ARG_MAX を超えても安全)。
/// - read-only / plan の permission-layer 強制は headless では未提供 (Issue #45)。よって
///   claude のようなツール deny フラグは使わず、system prompt で「ツール非使用・提案のみ」
///   を強く指示する (gemini/qwen と同型の安全posture)。`--dangerously-skip-permissions`
///   (auto-approve) は絶対に付けない。
/// - `--effort low|medium|high` は native 対応 (gemini と違い effort が効く)。
/// - session resume 機構が非対話で安定しないため、内部で履歴 (user_prompt, ai_message) を
///   保持して毎回プロンプトに含める。
pub struct AntigravityBackend {
    system_prompt: String,
    log_path: Option<String>,
    base_extra_args: Vec<String>,
    /// runtime モデル指定 (`/model`)。`Some` のとき send() 時に `--model <m>` を追加。
    model: Option<String>,
    /// runtime effort 指定 (`/effort`)。`Some` のとき send() 時に `--effort <e>` を追加 (native 対応)。
    effort: Option<String>,
    /// `/model` `/effort` ピッカーの候補リスト設定。
    options: OptionLists,
    history: Vec<(String, String)>,
}

impl AntigravityBackend {
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
            base_extra_args: cfg.antigravity.extra_args.clone(),
            model: (!cfg.model.is_empty()).then(|| cfg.model.clone()),
            effort: (!cfg.effort.is_empty()).then(|| cfg.effort.clone()),
            options: cfg.antigravity.options.clone(),
            history: Vec::new(),
        }
    }

    /// agy CLI の引数を組み立てる (send から抽出した純関数。golden test 対象)。
    /// `-p` (headless) を先頭に固定し、model / effort を後置きする。
    fn build_args(&self) -> Vec<String> {
        // `-p` は headless (print) モードのトグル。prompt 本体は stdin で渡す。
        let mut args: Vec<String> = vec!["-p".to_string()];
        args.extend(self.base_extra_args.iter().cloned());
        if let Some(m) = &self.model {
            args.push("--model".to_string());
            args.push(m.clone());
        }
        if let Some(e) = &self.effort {
            args.push("--effort".to_string());
            args.push(e.clone());
        }
        args
    }
}

impl AiBackend for AntigravityBackend {
    fn name(&self) -> &'static str {
        "antigravity"
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
        self.history.clear();
    }

    fn resume_command(&self) -> Option<String> {
        // agy は `--continue` / `-c` で直近の会話を継続できる (best-effort)。
        // 1 ターンも会話していなければ資料がないので None。
        if self.history.is_empty() {
            None
        } else {
            Some("agy --continue".to_string())
        }
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
        let stdout = run_cli_capture_stdout("agy", &args, &prompt, &self.log_path)?;
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
        let backend = AntigravityBackend::new(&AiConfig::default(), &LogConfig::default());
        assert!(!backend.available_models().is_empty());
    }

    #[test]
    fn effort_defaults_present_without_config() {
        // agy は native `--effort` 対応なので effort ピッカーにも候補が出る (gemini/qwen と違う点)。
        let backend = AntigravityBackend::new(&AiConfig::default(), &LogConfig::default());
        assert_eq!(backend.available_efforts(), vec!["low", "medium", "high"]);
    }

    #[test]
    fn args_use_headless_flag_and_never_bypass_permissions() {
        // 信頼の根幹: headless は `-p`、auto-approve 系フラグは絶対に付けない。
        let cfg = AiConfig {
            model: "gemini-3-pro".to_string(),
            effort: "high".to_string(),
            ..AiConfig::default()
        };
        let backend = AntigravityBackend::new(&cfg, &LogConfig::default());
        let args = backend.build_args();
        assert_eq!(args.first().map(String::as_str), Some("-p"));
        assert!(args.iter().any(|a| a == "--model"));
        assert!(args.iter().any(|a| a == "--effort"));
        assert!(!args
            .iter()
            .any(|a| a == "--dangerously-skip-permissions" || a == "--sandbox=off"));
    }
}
