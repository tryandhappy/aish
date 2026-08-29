use super::common::{
    build_full_prompt, build_system_prompt, expand_tilde, parse_ai_response_lossy,
    resolve_option_list, run_cli_capture_stdout, trim_history,
};
use super::types::{AiBackend, AiError, AiRequest, AiResponse};
use crate::config::{AiConfig, LogConfig, OptionLists};

const MAX_HISTORY_TURNS: usize = 8;

/// API key を読む環境変数 (NVIDIA 公式ドキュメントの慣習)。config には書かせない
/// (config.toml は平文なのでキー漏洩を避ける)。
const ENV_API_KEY: &str = "NVIDIA_API_KEY";
/// model を env でも上書きできる (`/model` / `[ai].model` が無いときの既定)。
const ENV_MODEL: &str = "NVIDIA_MODEL";

/// OpenAI 互換 chat completions エンドポイント (build.nvidia.com のホスト API)。
const API_URL: &str = "https://integrate.api.nvidia.com/v1/chat/completions";

/// model 未指定時の既定。広く使える instruct モデル (実機検証済み)。
const DEFAULT_MODEL: &str = "meta/llama-3.3-70b-instruct";

/// `/model` ピッカーの組み込み既定 (config 未設定時)。値は流動的なので best-effort。更新はリリース必要。
/// 全一覧は `GET /v1/models` (config の models_command 例を参照)。
/// 2026-08 現況: nemotron-3.5-lightning (2026-08-11, slug 確認済み) を追加。
const MODEL_DEFAULTS: &[&str] = &[
    "meta/llama-3.3-70b-instruct",
    "nvidia/nemotron-3.5-lightning-30b-a3b",
    "nvidia/llama-3.3-nemotron-super-49b-v1.5",
    "nvidia/nemotron-3-super-120b-a12b",
];

/// NVIDIA NIM backend (build.nvidia.com / integrate.api.nvidia.com)。
///
/// 戦略は Cloudflare Workers AI backend と同じ:
/// - HTTP クレートを足さず `curl` を `run_cli_capture_stdout` 経由で叩く
///   (Ctrl+C 中断 / ログ / 確認フローを再利用、追加 crate 依存ゼロ)。
/// - REST: OpenAI 互換 `POST /v1/chat/completions` + `Authorization: Bearer <key>`。
///   key は環境変数のみ (config 不可)。
/// - session resume 機構を持たないため、内部で履歴を保持して毎回プロンプトに含める
///   (gemini / qwen と同型)。
/// - JSON Schema / dangerous tool 無効化フラグは無いので、system prompt で JSON 出力と
///   「ツール非使用」を強く指示する (= テキスト生成のみ。サーバ側は一切実行しない)。
pub struct NvidiaNimBackend {
    system_prompt: String,
    log_path: Option<String>,
    /// runtime モデル指定 (`/model` / `[ai].model`)。
    model: Option<String>,
    /// runtime effort 指定 (`/effort`)。NIM API には effort 概念が無いので保存のみ。
    effort: Option<String>,
    /// `/model` `/effort` ピッカーの候補リスト設定 (effort は組み込み既定なし)。
    options: OptionLists,
    history: Vec<(String, String)>,
}

impl NvidiaNimBackend {
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
            model: (!cfg.model.is_empty()).then(|| cfg.model.clone()),
            effort: (!cfg.effort.is_empty()).then(|| cfg.effort.clone()),
            options: cfg.nvidia_nim.options.clone(),
            history: Vec::new(),
        }
    }

    /// 有効なモデル名を解決する。`/model` / `[ai].model` → env `NVIDIA_MODEL` → 組み込み既定。
    fn resolve_model(&self) -> String {
        self.model
            .clone()
            .or_else(|| std::env::var(ENV_MODEL).ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| DEFAULT_MODEL.to_string())
    }
}

/// request body (OpenAI 互換) を組み立てる (send から機械抽出した純関数。golden test 対象)。
/// max_tokens 既定はモデル依存で小さいことがあるため、JSON 応答が切れないよう明示する。
fn build_body(model: &str, prompt: &str) -> String {
    serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": prompt }],
        "max_tokens": 4096
    })
    .to_string()
}

/// curl 引数を組み立てる (send から機械抽出した純関数。golden test 対象)。
/// `-f` は付けない: HTTP エラーでも NIM が返すエラーボディを読みたいため
/// (JSON `{"status":403,...}` の他、素のテキスト "404 page not found" も返る)。
/// body は stdin (`--data-binary @-`) で渡す (ARG_MAX / クォート回避)。
fn build_curl_args(key: &str) -> Vec<String> {
    vec![
        "-sS".to_string(),
        "-X".to_string(),
        "POST".to_string(),
        API_URL.to_string(),
        "-H".to_string(),
        format!("Authorization: Bearer {key}"),
        "-H".to_string(),
        "Content-Type: application/json".to_string(),
        "--data-binary".to_string(),
        "@-".to_string(),
    ]
}

impl AiBackend for NvidiaNimBackend {
    fn name(&self) -> &'static str {
        "nvidia"
    }

    fn model(&self) -> Option<String> {
        Some(self.resolve_model())
    }

    fn effort(&self) -> Option<String> {
        self.effort.clone()
    }

    fn set_model(&mut self, model: Option<&str>) {
        self.model = model.map(str::to_string);
    }

    fn set_effort(&mut self, effort: Option<&str>) {
        // NIM API には reasoning effort フラグが無いので保存のみ (実リクエストには反映されない)。
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
        // === 認証情報 (env のみ) ===
        let key = std::env::var(ENV_API_KEY)
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                AiError::Other(format!(
                    "{ENV_API_KEY} が未設定です。build.nvidia.com の API Key (nvapi-...) を環境変数に設定してください。"
                ))
            })?;
        let model = self.resolve_model();

        // === prompt 組み立て (system prompt に出力フォーマット指示が焼き込まれている) ===
        let prompt = build_full_prompt(
            &self.system_prompt,
            &self.history,
            req.terminal_context,
            req.user_prompt,
        );

        let body = build_body(&model, &prompt);
        let args = build_curl_args(&key);

        let stdout = run_cli_capture_stdout("curl", &args, &body, &self.log_path)?;

        // === response 解析 ===
        // 成功時は OpenAI 互換 `choices[0].message.content`。エラーは JSON とは限らない
        // (存在しない model は素のテキスト "404 page not found") ため、parse 失敗も
        // 生ボディごとエラー化する。
        let envelope: serde_json::Value = serde_json::from_str(&stdout)
            .map_err(|_| AiError::Other(format!("NVIDIA NIM request failed: {}", stdout.trim())))?;

        let response_text = envelope
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AiError::Other(format!(
                    "NVIDIA NIM 応答に choices[0].message.content が見つかりません。Raw: {stdout}"
                ))
            })?;

        // モデルが {message, commands} JSON を吐けばコマンド提案化、吐かなければ全文 message + commands=[]。
        let response = parse_ai_response_lossy(response_text);
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
        let backend = NvidiaNimBackend::new(&AiConfig::default(), &LogConfig::default());
        assert!(!backend.available_models().is_empty());
    }

    #[test]
    fn resolve_model_falls_back_to_default() {
        // /model も [ai].model も無ければ DEFAULT_MODEL。
        let backend = NvidiaNimBackend::new(&AiConfig::default(), &LogConfig::default());
        // env NVIDIA_MODEL の影響を受けないよう、未設定前提のテストは値の一致でなく非空を見る。
        assert!(!backend.resolve_model().is_empty());
    }

    #[test]
    fn set_model_overrides() {
        let mut backend = NvidiaNimBackend::new(&AiConfig::default(), &LogConfig::default());
        backend.set_model(Some("nvidia/llama-3.3-nemotron-super-49b-v1.5"));
        assert_eq!(
            backend.resolve_model(),
            "nvidia/llama-3.3-nemotron-super-49b-v1.5"
        );
    }

    #[test]
    fn body_is_openai_compatible_with_max_tokens() {
        let body: serde_json::Value =
            serde_json::from_str(&build_body("meta/llama-3.3-70b-instruct", "hello")).unwrap();
        assert_eq!(body["model"], "meta/llama-3.3-70b-instruct");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hello");
        assert_eq!(body["max_tokens"], 4096, "JSON 応答の途切れ防止に必須");
    }

    #[test]
    fn curl_args_use_stdin_body_and_no_fail_flag() {
        let args = build_curl_args("nvapi-test");
        assert!(args.iter().any(|a| a == API_URL));
        assert!(args.iter().any(|a| a == "@-"), "body は stdin 渡し");
        assert!(
            !args.iter().any(|a| a == "-f" || a == "--fail"),
            "-f はエラーボディを読めなくするので付けない"
        );
        assert!(args.iter().any(|a| a == "Authorization: Bearer nvapi-test"));
    }
}
