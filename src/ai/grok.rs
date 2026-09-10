use super::common::{
    build_full_prompt, build_system_prompt, expand_tilde, extract_json, extract_model_from_args,
    parse_ai_response_lossy, resolve_option_list, run_cli_capture_stdout, trim_history,
    AI_RESPONSE_SCHEMA,
};
use super::types::{AiBackend, AiError, AiRequest, AiResponse};
use crate::config::{AiConfig, LogConfig, OptionLists};

const MAX_HISTORY_TURNS: usize = 8;

/// `/effort` ピッカーの組み込み既定 (config 未設定時)。公式 grok CLI の
/// `--reasoning-effort`(別名 `--effort`) が受理する値 (grok 1.0.25 実測、無効値はエラー)。
const EFFORT_DEFAULTS: &[&str] = &["low", "medium", "high", "xhigh"];

/// `/model` ピッカーの組み込み既定。通常は `grok models` の実測パース
/// (`available_models`) を使い、未ログイン/取得失敗時のみこの best-effort スナップショットに
/// fallback する (更新にリリースが要る)。grok 1.0.25 / grok.com アカウントで実在確認 (2026-09)。
/// xAI の `<name>-latest` エイリアスは modelname 単位でしか解決せず (grok-4-latest は 4.5/4.6 に
/// ならない)、改番で陳腐化回避に効かないため撤回済み (SPEC § 15.12)。
const MODEL_DEFAULTS: &[&str] = &["grok-4.6", "grok-4.5"];

/// `grok models` の出力から model slug だけを取り出す (純関数、golden test 対象)。
///
/// 出力例:
/// ```text
/// You are logged in with grok.com.
///
/// Default model: grok-4.6
///
/// Available models:
///   * grok-4.6 (default)
///   - grok-4.5
/// ```
/// 候補行は `*`/`-` マーカー始まりの行のみ。マーカーと `(default)` 等の注記を落として
/// 先頭トークン (= slug) を採る。ヘッダ行 (`You are…` / `Default model:` / `Available models:`)
/// はマーカーが無いので自然に除外される。
fn parse_grok_models(stdout: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        let rest = trimmed
            .strip_prefix("* ")
            .or_else(|| trimmed.strip_prefix("- "));
        if let Some(rest) = rest {
            if let Some(slug) = rest.split_whitespace().next() {
                if !slug.is_empty() {
                    out.push(slug.to_string());
                }
            }
        }
    }
    out
}

/// grok の `--json-schema`(= `--output-format json`) 出力から `AiResponse` を取り出す純関数。
///
/// 封筒形式 (grok 1.0.25 実測):
/// ```json
/// { "text": "{\"message\":\"…\",\"commands\":[…],\"command_result_followup\":true}",
///   "stopReason": "end_turn", "sessionId": "…", "usage": {…}, … }
/// ```
/// `text` に schema 準拠 JSON 文字列が入る。段階フォールバックで頑健さを保つ (grok は従来 lossy
/// backend なので、schema が効かない/出力が崩れても hard error にせず lossy 解釈に落とす):
/// 1. 封筒の `text` を `AiResponse` としてパース → 2. `text` を lossy 解釈 →
/// 3. 封筒直下を `AiResponse` として試行 → 4. stdout 全体を lossy 解釈。
fn parse_grok_response(stdout: &str) -> AiResponse {
    if let Some(json_str) = extract_json(stdout.trim()) {
        if let Ok(envelope) = serde_json::from_str::<serde_json::Value>(json_str) {
            if let Some(text) = envelope["text"].as_str() {
                if let Ok(resp) = serde_json::from_str::<AiResponse>(text.trim()) {
                    return resp;
                }
                return parse_ai_response_lossy(text);
            }
            if let Ok(resp) = serde_json::from_value::<AiResponse>(envelope) {
                return resp;
            }
        }
    }
    parse_ai_response_lossy(stdout)
}

/// xAI Grok CLI backend (公式 `grok`、https://x.ai/cli、`--ai grok`)。
///
/// 戦略 (gemini/qwen と同じ system-prompt-only 方式):
/// - headless (非対話) は `grok --prompt-file <PATH>`。stdin をそのまま読ませたいので Unix は
///   `--prompt-file /dev/stdin` (複数行 OK・ARG_MAX 安全)。Windows は /dev/stdin が無いので
///   send() 側で `-p <prompt>` 引数にフォールバックする。**`-p` 単独 + stdin は不可**
///   (公式 CLI の `-p/--single` は PROMPT を引数値として要求し stdin を読まない — grok 1.0.25 実測)。
/// - read-only / plan の permission-layer 強制は使わず、system prompt で「ツール非使用・提案のみ」を
///   強く指示する (gemini/qwen と同型の安全 posture)。`--always-approve` / `--permission-mode
///   bypassPermissions` 等の auto-approve 系は絶対に付けない。
/// - reasoning effort は `--reasoning-effort <low|medium|high|xhigh>` を send() で付与 (実測対応)。
///   model 指定は `-m`。
/// - 出力は `--json-schema`(= `--output-format json`) で `AiResponse` 形を強制し、封筒の `text` を
///   構造化パース (claude と同格の信頼性)。schema が効かない場合は lossy 解釈へ段階フォールバック
///   (`parse_grok_response`)。
/// - 非対話 session resume (`-r`/`--continue`) は使わず、内部で履歴 (user_prompt, ai_message) を
///   保持して毎回プロンプトに含める (backend 横断の統一方針)。
///
/// 注意: `grok` はコミュニティ製 `@vibe-kit/grok-cli` (npm、別ツール) ともバイナリ名が衝突しうる。
/// 公式 CLI を使っているか `which -a grok` で確認すること。
pub struct GrokBackend {
    system_prompt: String,
    log_path: Option<String>,
    base_extra_args: Vec<String>,
    /// runtime モデル指定 (`/model`)。`Some` のとき send() 時に `-m <m>` を追加。
    model: Option<String>,
    /// runtime effort 指定 (`/effort`)。`Some` のとき send() 時に `--reasoning-effort <e>` を追加。
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

    /// prompt 配送フラグ以外の共通引数 (構造化出力 + model / effort / extra_args) を組み立てる純関数。
    /// prompt の渡し方 (`--prompt-file /dev/stdin` or `-p <prompt>`) は send() が
    /// プラットフォーム別に前置する。golden test 対象。
    fn build_args(&self) -> Vec<String> {
        // `--json-schema`(= `--output-format json` を含意) で AiResponse 形を強制し、
        // 従来の lossy 抽出でなく構造化パースを可能にする (claude と同じ姿勢)。明示的に
        // `--output-format json` も併記 (claude に合わせる)。
        let mut args: Vec<String> = vec![
            "--output-format".to_string(),
            "json".to_string(),
            "--json-schema".to_string(),
            AI_RESPONSE_SCHEMA.to_string(),
        ];
        args.extend(self.base_extra_args.iter().cloned());
        if let Some(m) = &self.model {
            args.push("-m".to_string());
            args.push(m.clone());
        }
        if let Some(e) = &self.effort {
            args.push("--reasoning-effort".to_string());
            args.push(e.clone());
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
        // send() で `--reasoning-effort <e>` として実リクエストに反映する。
        self.effort = effort.map(str::to_string);
    }

    fn available_models(&self) -> Vec<String> {
        // config 明示 (static list / ユーザ models_command) があれば従来通り最優先。
        if !self.options.models.is_empty() || !self.options.models_command.is_empty() {
            return resolve_option_list(
                &self.options.models,
                &self.options.models_command,
                MODEL_DEFAULTS,
                &self.log_path,
            );
        }
        // 既定: `grok models` を実行して実在モデルをパース (ピッカーを開く時だけローカル実行)。
        // 未ログイン/取得失敗/空は best-effort スナップショットへ fallback。
        let parsed = run_cli_capture_stdout("grok", &["models".to_string()], "", &self.log_path)
            .map(|out| parse_grok_models(&out))
            .unwrap_or_default();
        if parsed.is_empty() {
            MODEL_DEFAULTS.iter().map(|s| s.to_string()).collect()
        } else {
            parsed
        }
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

    fn send(&mut self, req: &AiRequest) -> Result<AiResponse, AiError> {
        let prompt = build_full_prompt(
            &self.system_prompt,
            &self.history,
            req.terminal_context,
            req.user_prompt,
        );

        let common = self.build_args();
        // Unix: prompt を stdin (`--prompt-file /dev/stdin`) で渡す (複数行 OK・ARG_MAX 安全)。
        #[cfg(unix)]
        let stdout = {
            let mut args = vec!["--prompt-file".to_string(), "/dev/stdin".to_string()];
            args.extend(common);
            run_cli_capture_stdout("grok", &args, &prompt, &self.log_path)?
        };
        // Windows: /dev/stdin が無いので prompt を `-p <prompt>` 引数で渡す (stdin 未使用)。
        #[cfg(not(unix))]
        let stdout = {
            let mut args = vec!["-p".to_string(), prompt.clone()];
            args.extend(common);
            run_cli_capture_stdout("grok", &args, "", &self.log_path)?
        };
        let response = parse_grok_response(&stdout);
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
    fn model_defaults_present() {
        // `grok models` 取得失敗時の fallback スナップショットが空にならない (既定消失の回帰防止)。
        assert!(!MODEL_DEFAULTS.is_empty());
        assert!(!EFFORT_DEFAULTS.is_empty());
    }

    #[test]
    fn build_args_carry_model_effort_and_never_auto_approve() {
        // 信頼の根幹: auto-approve / permission-bypass 系フラグは絶対に付けない。
        // model は `-m`、effort は `--reasoning-effort` を後置する。
        let cfg = AiConfig {
            model: "grok-4.6".to_string(),
            effort: "high".to_string(),
            ..AiConfig::default()
        };
        let backend = GrokBackend::new(&cfg, &LogConfig::default());
        let args = backend.build_args();
        // 構造化出力を強制 (lossy でなく schema パス)。
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--output-format" && w[1] == "json"));
        assert!(args.iter().any(|a| a == "--json-schema"));
        assert!(args.windows(2).any(|w| w[0] == "-m" && w[1] == "grok-4.6"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--reasoning-effort" && w[1] == "high"));
        assert!(!args
            .iter()
            .any(|a| a == "--always-approve" || a == "--yolo" || a == "--permission-mode"));
    }

    #[test]
    fn parse_grok_response_extracts_from_json_envelope() {
        // 封筒の text(schema 準拠 JSON 文字列) から message/commands/followup を取り出す。
        let envelope = r#"{"text":"{\"message\":\"disk と memory を確認\",\"commands\":[\"df -h\",\"free -h\"],\"command_result_followup\":true}","stopReason":"end_turn","sessionId":"x"}"#;
        let resp = parse_grok_response(envelope);
        assert_eq!(resp.message, "disk と memory を確認");
        // 裸文字列 commands は後方互換で ProposedCommand(説明なし・Yellow 既定) になる。
        let cmds: Vec<&str> = resp.commands.iter().map(|c| c.command.as_str()).collect();
        assert_eq!(cmds, vec!["df -h", "free -h"]);
        assert_eq!(resp.commands[0].risk, crate::ai::Risk::Yellow);
        assert!(resp.command_result_followup);
    }

    #[test]
    fn parse_grok_response_falls_back_to_lossy_on_plain_text() {
        // 封筒でない素テキストでも hard error にせず lossy 解釈へ落ちる (頑健さ維持)。
        let resp = parse_grok_response("ここに JSON は無い、ただの説明文");
        assert!(!resp.message.is_empty());
    }

    #[test]
    fn parse_grok_models_extracts_slugs_only() {
        // `grok models` のヘッダ行/マーカー/(default) 注記を落として slug だけ採る。
        let out = "You are logged in with grok.com.\n\nDefault model: grok-4.6\n\nAvailable models:\n  * grok-4.6 (default)\n  - grok-4.5\n";
        assert_eq!(parse_grok_models(out), vec!["grok-4.6", "grok-4.5"]);
        // マーカー無し (ヘッダのみ) は空。
        assert!(parse_grok_models("Available models:\nDefault model: grok-4.6\n").is_empty());
    }
}
