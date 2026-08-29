use super::common::{
    build_full_prompt, build_system_prompt, expand_tilde, extract_model_from_args,
    parse_ai_response_lossy, parse_jsonl_with_paths, resolve_option_list, run_cli_capture_stdout,
};
use super::types::{AiBackend, AiError, AiRequest, AiResponse};
use crate::config::{AiConfig, LogConfig, OptionLists};

/// `/effort` ピッカーの組み込み既定 (config 未設定時)。copilot CLI の `--effort`。
const EFFORT_DEFAULTS: &[&str] = &["none", "low", "medium", "high", "xhigh", "max"];

/// `/model` ピッカーの組み込み既定 (config 未設定時)。値は流動的なので best-effort。更新はリリース必要。
/// 2026-08 現況スナップショット (copilot 未インストールのため slug は docs 準拠の best-effort)。
/// CLI に auto model selection もあるが headless での `--model auto` 可否が未実測なので採用見送り。
const MODEL_DEFAULTS: &[&str] = &[
    "claude-sonnet-5",
    "claude-opus-5",
    "claude-fable-5",
    "gpt-5.6-terra",
    "gpt-5.5",
    "gemini-3.7-flash",
];

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
    /// `/model` `/effort` ピッカーの候補リスト設定。
    options: OptionLists,
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
            options: cfg.copilot.options.clone(),
        }
    }

    /// copilot CLI の引数を組み立てる (send から機械抽出した純関数。golden test 対象)。
    /// 信頼の根幹: 四段 deny (`--allow-all-tools` + `--deny-tool=shell` + `--deny-tool=write`
    /// + `--no-ask-user`) は無条件に含める。`-p` は付けない (stdin 渡しと排他)。
    fn build_args(&self) -> Vec<String> {
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
        args
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
            build_full_prompt(
                &self.system_prompt,
                &[],
                req.terminal_context,
                req.user_prompt,
            )
        };

        let args = self.build_args();

        let stdout = run_cli_capture_stdout("copilot", &args, &prompt, &self.log_path)?;

        // copilot の JSONL は assistant.message 行の data.content が最終応答、
        // result 行の sessionId が session UUID。共通 parser に委譲。
        let (assistant_text, session_id) = parse_jsonl_with_paths(
            &stdout,
            "assistant.message:data.content",
            "result:sessionId",
        );
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

// copilot の JSONL parser (`assistant.message:data.content` + `result:sessionId`) は
// `common::parse_jsonl_with_paths` に一般化済み。テストは common.rs 側に集約。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_defaults_present_without_config() {
        // config 空でも組み込み既定で `/model` ピッカーの候補が出る (既定消失の回帰防止)。
        let backend = CopilotBackend::new(&AiConfig::default(), &LogConfig::default());
        assert!(!backend.available_models().is_empty());
    }

    #[test]
    fn args_always_contain_four_stage_deny() {
        // 信頼の根幹: 四段 deny が既定 config で必ず含まれる (退行 = AI が shell/write を実行し得る)。
        let backend = CopilotBackend::new(&AiConfig::default(), &LogConfig::default());
        let args = backend.build_args();
        for required in [
            "--allow-all-tools",
            "--deny-tool=shell",
            "--deny-tool=write",
            "--no-ask-user",
        ] {
            assert!(args.iter().any(|a| a == required), "missing {required}");
        }
        // 既定 mode=plan も付く。
        let mode_pos = args.iter().position(|a| a == "--mode").expect("--mode");
        assert_eq!(args[mode_pos + 1], "plan");
    }

    #[test]
    fn args_never_contain_dangerous_flags() {
        // `-p` は stdin 渡しと排他 (too many arguments)。--yolo/--allow-all は承認迂回。
        let backend = CopilotBackend::new(&AiConfig::default(), &LogConfig::default());
        let args = backend.build_args();
        for forbidden in ["-p", "--yolo", "--allow-all"] {
            assert!(!args.iter().any(|a| a == forbidden), "found {forbidden}");
        }
    }

    #[test]
    fn args_include_model_effort_and_resume_when_set() {
        let mut backend = CopilotBackend::new(&AiConfig::default(), &LogConfig::default());
        backend.set_model(Some("gpt-5"));
        backend.set_effort(Some("high"));
        backend.session_id = Some("sid-123".to_string());
        let args = backend.build_args();
        let m = args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(args[m + 1], "gpt-5");
        let e = args.iter().position(|a| a == "--effort").unwrap();
        assert_eq!(args[e + 1], "high");
        let r = args.iter().position(|a| a == "--resume").unwrap();
        assert_eq!(args[r + 1], "sid-123");
    }
}
