use super::common::{
    build_full_prompt, build_system_prompt, expand_tilde, parse_ai_response_lossy,
    resolve_option_list, run_cli_capture_stdout, trim_history,
};
use super::types::{AiBackend, AiError, AiRequest, AiResponse};
use crate::config::{AiConfig, LogConfig, OptionLists};

const MAX_HISTORY_TURNS: usize = 8;

/// account/token を読む環境変数 (Cloudflare 公式 / wrangler 慣習)。config には書かせない
/// (config.toml は平文なのでトークン漏洩を避ける)。
const ENV_ACCOUNT_ID: &str = "CLOUDFLARE_ACCOUNT_ID";
const ENV_API_TOKEN: &str = "CLOUDFLARE_API_TOKEN";
/// model を env でも上書きできる (`/model` / `[ai].model` が無いときの既定)。
const ENV_MODEL: &str = "CLOUDFLARE_MODEL";

/// model 未指定時の既定。広く使える instruct モデル。`{message, commands}` の JSON
/// 出力フォーマットに追従させたいなら `@cf/meta/llama-3.3-70b-instruct-fp8-fast` 等の
/// 大きめモデルを `/model` で選ぶ。
const DEFAULT_MODEL: &str = "@cf/meta/llama-3.1-8b-instruct";

/// `/model` ピッカーの組み込み既定 (config 未設定時)。値は流動的なので best-effort。更新はリリース必要。
const MODEL_DEFAULTS: &[&str] = &[
    "@cf/meta/llama-3.3-70b-instruct-fp8-fast",
    "@cf/meta/llama-3.1-8b-instruct",
    "@cf/qwen/qwen2.5-coder-32b-instruct",
    "@cf/mistralai/mistral-small-3.1-24b-instruct",
];

/// Cloudflare Workers AI backend。
///
/// 戦略:
/// - aish は HTTP クライアントを持たず自己アップデートも curl をサブプロセスで叩く方針なので、
///   ここも `curl` を `run_cli_capture_stdout` 経由で叩く (Ctrl+C 中断 / ログ / 確認フローを再利用、
///   追加 crate 依存ゼロ)。
/// - REST: `POST .../accounts/{account}/ai/run/{model}` + `Authorization: Bearer <token>`。
///   account / token は環境変数のみ (config 不可)。
/// - session resume 機構を持たないため、内部で履歴を保持して毎回プロンプトに含める
///   (gemini / qwen と同型)。
/// - JSON Schema / dangerous tool 無効化フラグは無いので、system prompt で JSON 出力と
///   「ツール非使用」を強く指示する (= テキスト生成のみ。サーバ側は一切実行しない)。
pub struct CloudflareWorkersBackend {
    system_prompt: String,
    log_path: Option<String>,
    /// runtime モデル指定 (`/model` / `[ai].model`)。
    model: Option<String>,
    /// runtime effort 指定 (`/effort`)。Cloudflare Workers AI には effort 概念が無いので保存のみ。
    effort: Option<String>,
    /// `/model` `/effort` ピッカーの候補リスト設定 (effort は組み込み既定なし)。
    options: OptionLists,
    history: Vec<(String, String)>,
}

impl CloudflareWorkersBackend {
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
            options: cfg.cloudflare_workers.options.clone(),
            history: Vec::new(),
        }
    }

    /// 有効なモデル名を解決する。`/model` / `[ai].model` → env `CLOUDFLARE_MODEL` → 組み込み既定。
    fn resolve_model(&self) -> String {
        self.model
            .clone()
            .or_else(|| std::env::var(ENV_MODEL).ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| DEFAULT_MODEL.to_string())
    }
}

impl AiBackend for CloudflareWorkersBackend {
    fn name(&self) -> &'static str {
        "cloudflare"
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
        // Cloudflare Workers AI には reasoning effort フラグが無いので保存のみ (実リクエストには反映されない)。
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
        let account = std::env::var(ENV_ACCOUNT_ID)
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                AiError::Other(format!(
                    "{ENV_ACCOUNT_ID} が未設定です。Cloudflare の Account ID を環境変数に設定してください。"
                ))
            })?;
        let token = std::env::var(ENV_API_TOKEN)
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                AiError::Other(format!(
                    "{ENV_API_TOKEN} が未設定です。Cloudflare の API Token を環境変数に設定してください。"
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

        // === request body ===
        let body = serde_json::json!({
            "messages": [{ "role": "user", "content": prompt }]
        })
        .to_string();

        // === curl 引数 ===
        // `-f` は付けない: HTTP エラーでも Cloudflare が返す JSON エラーボディを読みたいため。
        // body は stdin (`--data-binary @-`) で渡す (ARG_MAX / クォート回避)。
        let url = format!("https://api.cloudflare.com/client/v4/accounts/{account}/ai/run/{model}");
        let args: Vec<String> = vec![
            "-sS".to_string(),
            "-X".to_string(),
            "POST".to_string(),
            url,
            "-H".to_string(),
            format!("Authorization: Bearer {token}"),
            "-H".to_string(),
            "Content-Type: application/json".to_string(),
            "--data-binary".to_string(),
            "@-".to_string(),
        ];

        let stdout = run_cli_capture_stdout("curl", &args, &body, &self.log_path)?;

        // === response 解析 ===
        let envelope: serde_json::Value = serde_json::from_str(&stdout).map_err(|e| {
            AiError::Other(format!(
                "Cloudflare 応答の JSON parse に失敗: {e}\nRaw: {stdout}"
            ))
        })?;

        if envelope.get("success").and_then(|v| v.as_bool()) != Some(true) {
            let errors = envelope
                .get("errors")
                .map(|e| e.to_string())
                .unwrap_or_else(|| "(no error detail)".to_string());
            return Err(AiError::Other(format!(
                "Cloudflare Workers AI request failed: {errors}"
            )));
        }

        let response_text = envelope
            .get("result")
            .and_then(|r| r.get("response"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AiError::Other(format!(
                    "Cloudflare 応答に result.response が見つかりません。Raw: {stdout}"
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
        let backend = CloudflareWorkersBackend::new(&AiConfig::default(), &LogConfig::default());
        assert!(!backend.available_models().is_empty());
    }

    #[test]
    fn resolve_model_falls_back_to_default() {
        // /model も [ai].model も無ければ DEFAULT_MODEL。
        let backend = CloudflareWorkersBackend::new(&AiConfig::default(), &LogConfig::default());
        // env CLOUDFLARE_MODEL の影響を受けないよう、未設定前提のテストは値の一致でなく非空を見る。
        assert!(!backend.resolve_model().is_empty());
    }

    #[test]
    fn set_model_overrides() {
        let mut backend =
            CloudflareWorkersBackend::new(&AiConfig::default(), &LogConfig::default());
        backend.set_model(Some("@cf/meta/llama-3.1-8b-instruct"));
        assert_eq!(backend.resolve_model(), "@cf/meta/llama-3.1-8b-instruct");
    }
}
