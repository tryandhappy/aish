use super::common::{
    build_full_prompt, build_system_prompt, expand_tilde, extract_json, extract_model_from_args,
    parse_ai_response_lossy, parse_jsonl_with_paths, run_cli_capture_stdout, trim_history,
};
use super::types::{AiBackend, AiError, AiRequest, AiResponse};
use crate::config::{AiConfig, LogConfig, ProviderRecipe};

/// Config 駆動の generic CLI backend。
///
/// `[[ai.providers]]` 1 エントリ (`ProviderRecipe`) を読んで以下を動的に組み立てる:
/// - args: `recipe.args` + 動的引数 (resume/model/effort/prompt-as-flag)
/// - prompt 渡し: stdin / arg / flag (`recipe.prompt_delivery`)
/// - 出力 parse: lossy / extract_json / jsonl (`recipe.parse`)
/// - session 管理: `recipe.session_id_path` (or `jsonl_session_path`) が空でなければ
///   native resume (claude / cursor / copilot と同形)、空なら内部 history fallback
///   (gemini / qwen と同形)
///
/// 安全性: aish の「信頼の根幹」 (= AI が勝手に実行しない、サーバに書き込まない) は
/// **config 側に責任を委譲** している。recipe 著者が `--mode plan` 等の安全フラグを
/// `args` に含めるか、もしくは利用者が信頼できる CLI のみを provider 登録する想定。
/// native backend (claude / codex / copilot 等) のような自動の deny フラグ付与は行わない。
pub struct GenericCliBackend {
    recipe: &'static ProviderRecipe,
    system_prompt: String,
    log_path: Option<String>,
    /// runtime モデル指定 (`/model`)。`Some` のとき `recipe.model_flag` が空でなければ反映。
    model: Option<String>,
    /// runtime effort 指定 (`/effort`)。`recipe.effort_flag` が空なら保存のみ。
    effort: Option<String>,
    /// native resume が有効な場合の session_id (初回 send で捕獲)。
    session_id: Option<String>,
    /// 内部 history fallback (native resume 無効時のみ使用)。
    history: Vec<(String, String)>,
}

impl GenericCliBackend {
    pub fn new(recipe: &'static ProviderRecipe, cfg: &AiConfig, log: &LogConfig) -> Self {
        let log_path = if log.enabled {
            Some(expand_tilde(&log.path))
        } else {
            None
        };
        let system_prompt = build_system_prompt(&cfg.system_prompt, &cfg.language);
        Self {
            recipe,
            system_prompt,
            log_path,
            model: (!cfg.model.is_empty()).then(|| cfg.model.clone()),
            effort: (!cfg.effort.is_empty()).then(|| cfg.effort.clone()),
            session_id: None,
            history: Vec::new(),
        }
    }

    /// このレシピが native session resume を要求しているか。
    /// `parse=jsonl` のときは `jsonl_session_path` を、それ以外は `session_id_path` を見る。
    /// resume_flag も非空である必要がある (resume 引数を組み立てられないため)。
    fn uses_native_resume(&self) -> bool {
        if self.recipe.resume_flag.is_empty() {
            return false;
        }
        match self.recipe.parse.as_str() {
            "jsonl" => !self.recipe.jsonl_session_path.is_empty(),
            _ => !self.recipe.session_id_path.is_empty(),
        }
    }
}

impl AiBackend for GenericCliBackend {
    fn name(&self) -> &'static str {
        // BackendKind::as_str() 経由で参照されないので、ここでは static fallback。
        // 診断用ログ等で個別の provider 名を出したい場合は recipe.name を直接読む。
        "generic"
    }

    fn model(&self) -> Option<String> {
        self.model
            .clone()
            .or_else(|| extract_model_from_args(&self.recipe.args))
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
        self.session_id = None;
        self.history.clear();
    }

    fn resume_command(&self) -> Option<String> {
        // native resume が有効でかつ session_id を捕獲できていれば、その backend を
        // 再現するためのコマンドラインを案内する。
        if let Some(sid) = &self.session_id {
            if !self.recipe.resume_flag.is_empty() {
                return Some(format!(
                    "{} {} {}",
                    self.recipe.binary, self.recipe.resume_flag, sid
                ));
            }
        }
        None
    }

    fn send(&mut self, req: &AiRequest) -> Result<AiResponse, AiError> {
        // === prompt 組み立て ===
        let prompt = if self.uses_native_resume() && self.session_id.is_some() {
            // 2 回目以降: CLI 側 session が system + 履歴を持っているので
            // terminal context + user prompt のみ送る。
            if req.terminal_context.is_empty() {
                req.user_prompt.to_string()
            } else {
                format!(
                    "```terminal\n{}\n```\n\n{}",
                    req.terminal_context, req.user_prompt
                )
            }
        } else if self.recipe.system_prompt_inline {
            // 初回 (native resume) or 毎回 (system_prompt_inline=true かつ resume なし)。
            // history はネイティブ resume が有効なら空、無効なら蓄積分。
            build_full_prompt(
                &self.system_prompt,
                &self.history,
                req.terminal_context,
                req.user_prompt,
            )
        } else {
            // system_prompt_inline=false: aish 側で system prompt を一切付けない。
            // 利用者が CLI 側 (config file 等) で system prompt を管理する想定。
            build_full_prompt(
                "",
                &self.history,
                req.terminal_context,
                req.user_prompt,
            )
        };

        // === args 組み立て ===
        let mut args: Vec<String> = self.recipe.args.clone();

        // resume を最優先で付ける (順序自体は CLI 依存だが、recipe の args の後に置く)。
        if let Some(sid) = &self.session_id {
            if !self.recipe.resume_flag.is_empty() {
                args.push(self.recipe.resume_flag.clone());
                args.push(sid.clone());
            }
        }

        // model / effort は recipe にフラグ名が指定されている場合のみ付与。
        if let Some(m) = &self.model {
            if !self.recipe.model_flag.is_empty() {
                args.push(self.recipe.model_flag.clone());
                args.push(m.clone());
            }
        }
        if let Some(e) = &self.effort {
            if !self.recipe.effort_flag.is_empty() {
                args.push(self.recipe.effort_flag.clone());
                args.push(e.clone());
            }
        }

        // prompt を args 側に積むかどうかは prompt_delivery で決定。
        let stdin_input = match self.recipe.prompt_delivery.as_str() {
            "stdin" => prompt.clone(),
            "flag" => {
                // 例: ["-p", "<prompt text>"]
                args.push(self.recipe.prompt_flag.clone());
                args.push(prompt.clone());
                String::new()
            }
            "arg" => {
                // 例: positional 末尾に追加
                args.push(prompt.clone());
                String::new()
            }
            other => {
                return Err(AiError::Other(format!(
                    "generic provider `{}`: unknown prompt_delivery `{}`",
                    self.recipe.name, other
                )));
            }
        };

        let stdout =
            run_cli_capture_stdout(&self.recipe.binary, &args, &stdin_input, &self.log_path)?;

        // === parse ===
        let (assistant_text, session_id) = match self.recipe.parse.as_str() {
            "jsonl" => parse_jsonl_with_paths(
                &stdout,
                &self.recipe.jsonl_content_path,
                &self.recipe.jsonl_session_path,
            ),
            "extract_json" => {
                // 最外 JSON を抜き出し → 全文を text として返しつつ session_id を読む。
                let envelope = extract_json(&stdout);
                let session = envelope
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                    .and_then(|v| {
                        if self.recipe.session_id_path.is_empty() {
                            None
                        } else {
                            v.get(&self.recipe.session_id_path)
                                .and_then(|f| f.as_str())
                                .map(str::to_string)
                        }
                    });
                // assistant_text は envelope 全体を返す (parse_ai_response_lossy がさらに
                // {message, commands} を抽出する)。
                (envelope.map(str::to_string), session)
            }
            _ /* "lossy" */ => (None, None),
        };

        // session_id 捕獲 (native resume 有効時のみ意味あり)。
        if self.uses_native_resume() {
            if let Some(sid) = session_id {
                if self.session_id.is_none() {
                    self.session_id = Some(sid);
                }
            }
        }

        // 応答テキスト → AiResponse:
        // - assistant_text が取れていればそれを lossy パース
        // - 取れなければ生 stdout を lossy パース
        let response =
            parse_ai_response_lossy(assistant_text.as_deref().unwrap_or(&stdout));

        // native resume を使わない backend は内部 history に積む (gemini/qwen と同じ)。
        if !self.uses_native_resume() {
            self.history
                .push((req.user_prompt.to_string(), response.message.clone()));
            trim_history(&mut self.history, self.recipe.history_turns);
        }

        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderRecipe;

    fn dummy_recipe() -> ProviderRecipe {
        ProviderRecipe {
            name: "dummy".into(),
            binary: "/bin/cat".into(),
            args: vec![],
            prompt_delivery: "stdin".into(),
            prompt_flag: String::new(),
            parse: "lossy".into(),
            jsonl_content_path: String::new(),
            jsonl_session_path: String::new(),
            session_id_path: String::new(),
            resume_flag: String::new(),
            model_flag: String::new(),
            effort_flag: String::new(),
            color: 208,
            system_prompt_inline: true,
            history_turns: 8,
        }
    }

    #[test]
    fn cat_echo_with_system_prompt_extracts_example_json() {
        // /bin/cat に prompt をそのまま出力させると、`build_system_prompt` がプロンプト先頭に
        // 焼き込んでいる出力フォーマットの例 (`{"message":"ユーザへの説明","commands":["提案コマンド"]}`)
        // が `extract_json` で取り出されて AiResponse になる、というのを期待する。
        // これは「lossy parse の "JSON 抽出成功" 経路」が動くことの検証。
        let recipe = Box::leak(Box::new(dummy_recipe()));
        let log_cfg = LogConfig {
            enabled: false,
            path: String::new(),
        };
        let ai_cfg = AiConfig::default();
        let mut backend = GenericCliBackend::new(recipe, &ai_cfg, &log_cfg);

        let req = AiRequest {
            terminal_context: "",
            user_prompt: "hello-from-test",
        };
        let resp = backend.send(&req).expect("cat should succeed");
        assert_eq!(resp.message, "ユーザへの説明");
        assert_eq!(resp.commands, vec!["提案コマンド".to_string()]);
    }

    #[test]
    fn cat_echo_without_inline_system_falls_back_to_raw() {
        // system_prompt_inline=false + 空 system_prompt なら build_full_prompt は
        // history + context + user_prompt のみを連結。JSON が無いので lossy パースは
        // 生文字列を message に詰めて commands=[] でフォールバックする。
        let mut r = dummy_recipe();
        r.system_prompt_inline = false;
        let recipe = Box::leak(Box::new(r));
        let log_cfg = LogConfig {
            enabled: false,
            path: String::new(),
        };
        let ai_cfg = AiConfig {
            system_prompt: String::new(),
            language: String::new(),
            ..AiConfig::default()
        };
        let mut backend = GenericCliBackend::new(recipe, &ai_cfg, &log_cfg);

        let req = AiRequest {
            terminal_context: "",
            user_prompt: "hello-from-test",
        };
        let resp = backend.send(&req).expect("cat should succeed");
        assert!(
            resp.message.contains("hello-from-test"),
            "message did not contain prompt: {}",
            resp.message
        );
        assert!(resp.commands.is_empty());
    }

    #[test]
    fn uses_native_resume_logic() {
        // resume_flag + session_id_path が両方あれば true。
        let mut recipe = dummy_recipe();
        recipe.resume_flag = "--resume".into();
        recipe.session_id_path = "session_id".into();
        let leaked = Box::leak(Box::new(recipe)) as &'static ProviderRecipe;
        let cfg = AiConfig::default();
        let log = LogConfig {
            enabled: false,
            path: String::new(),
        };
        let backend = GenericCliBackend::new(leaked, &cfg, &log);
        assert!(backend.uses_native_resume());

        // resume_flag だけ / path だけ では false。
        let mut recipe2 = dummy_recipe();
        recipe2.resume_flag = "--resume".into();
        let leaked2 = Box::leak(Box::new(recipe2)) as &'static ProviderRecipe;
        let backend2 = GenericCliBackend::new(leaked2, &cfg, &log);
        assert!(!backend2.uses_native_resume());

        let mut recipe3 = dummy_recipe();
        recipe3.session_id_path = "session_id".into();
        let leaked3 = Box::leak(Box::new(recipe3)) as &'static ProviderRecipe;
        let backend3 = GenericCliBackend::new(leaked3, &cfg, &log);
        assert!(!backend3.uses_native_resume());
    }

    #[test]
    fn uses_native_resume_jsonl_variant() {
        // parse=jsonl のときは jsonl_session_path を見る。
        let mut recipe = dummy_recipe();
        recipe.parse = "jsonl".into();
        recipe.resume_flag = "--resume".into();
        recipe.jsonl_session_path = "result:sessionId".into();
        // session_id_path は空でも JSONL なら true。
        let leaked = Box::leak(Box::new(recipe)) as &'static ProviderRecipe;
        let cfg = AiConfig::default();
        let log = LogConfig {
            enabled: false,
            path: String::new(),
        };
        let backend = GenericCliBackend::new(leaked, &cfg, &log);
        assert!(backend.uses_native_resume());
    }

    #[test]
    fn history_accumulated_when_no_native_resume() {
        // /bin/cat + lossy + no resume → 内部 history が増えることを検証。
        let recipe = Box::leak(Box::new(dummy_recipe()));
        let cfg = AiConfig::default();
        let log = LogConfig {
            enabled: false,
            path: String::new(),
        };
        let mut backend = GenericCliBackend::new(recipe, &cfg, &log);
        let req = AiRequest {
            terminal_context: "",
            user_prompt: "first",
        };
        backend.send(&req).unwrap();
        assert_eq!(backend.history.len(), 1);
        assert_eq!(backend.history[0].0, "first");

        let req2 = AiRequest {
            terminal_context: "",
            user_prompt: "second",
        };
        backend.send(&req2).unwrap();
        assert_eq!(backend.history.len(), 2);
    }
}
