use super::common::{
    build_full_prompt, build_system_prompt, expand_tilde, extract_model_from_args,
    parse_ai_response_lossy, resolve_option_list, run_cli_capture_stdout, unique_tmp_path,
    write_log,
};
use super::types::{AiBackend, AiError, AiRequest, AiResponse};
use crate::config::{AiConfig, LogConfig, OptionLists};

/// `/effort` ピッカーの組み込み既定 (config 未設定時)。codex の `model_reasoning_effort`。
const EFFORT_DEFAULTS: &[&str] = &["minimal", "low", "medium", "high"];

/// `/model` ピッカーの組み込み既定 (config 未設定時)。値は流動的なので best-effort。更新はリリース必要。
/// (codex 公式は ChatGPT ログイン時のモデル pin を非推奨。`/model -` で既定に戻せる。)
const MODEL_DEFAULTS: &[&str] = &["gpt-5.5", "gpt-5.4", "gpt-5.4-mini", "gpt-5.2-codex"];

/// codex の自律エージェント挙動を無効化するための feature 一覧。
/// `codex features list` で stable / experimental の tool 系をすべて落とす。
/// これを付けないと codex は内部で shell 等を実行して結果だけを返してしまい、
/// aish の「提案 → 確認 → 実行」モデルを迂回する。
const DISABLE_TOOL_FEATURES: &[&str] = &[
    "shell_tool",                   // shell 実行
    "unified_exec",                 // 別系統の exec
    "browser_use",                  // ブラウザ操作
    "computer_use",                 // computer use
    "multi_agent",                  // sub-agent (sub-agent がツールを持つ恐れ)
    "image_generation",             // 画像生成
    "tool_search",                  // ツール探索
    "tool_suggest",                 // ツール提案
    "plugins",                      // プラグイン (任意ツール経由)
    "apps",                         // app 連携
    "skill_mcp_dependency_install", // MCP 依存インストール
    "tool_call_mcp_elicitation",    // MCP ツール呼び出し
];

/// Codex CLI (`codex exec`) backend。
///
/// 戦略:
/// - 初回: `codex exec --disable <features...> -s read-only --skip-git-repo-check -o <tmp> -`
///   で system prompt + 初回ユーザ入力を送り、応答を得る。`--ephemeral` は付けない
///   ので codex は `~/.codex/sessions/YYYY/MM/DD/rollout-...-<UUID>.jsonl` にセッションを
///   永続化する。応答取得後、最新の rollout ファイル名から session UUID を捕獲する。
/// - 2 回目以降: `codex exec resume <UUID> --disable ... -o <tmp> -` で前セッションに新ターンを
///   追記する形で実行。codex 側が system + 履歴を持っているので、aish 側からは新しい
///   terminal context + user prompt のみ送る。
/// - JSON Schema 強制機能は無いので system prompt で `{message, commands}` 出力を強く指示し、
///   `extract_json` で抽出する。失敗時は全文を message としてフォールバック。
/// - aish 終了後は `codex resume <UUID>` で同じセッションをインタラクティブに再開できる。
pub struct CodexBackend {
    system_prompt: String,
    log_path: Option<String>,
    base_extra_args: Vec<String>,
    /// runtime モデル指定 (`/model`)。`Some` のとき send() 時に `--model <m>` を追加。
    model: Option<String>,
    /// runtime effort 指定 (`/effort`)。`Some` のとき send() 時に `-c model_reasoning_effort=<e>` を追加。
    effort: Option<String>,
    /// codex 側 session UUID。初回 send 後に `~/.codex/sessions/` 配下から捕獲する。
    /// `Some` のときは `codex exec resume <UUID>` で連結し、aish 側からは履歴を再送しない。
    session_id: Option<String>,
    /// `/model` `/effort` ピッカーの候補リスト設定。
    options: OptionLists,
}

impl CodexBackend {
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
            base_extra_args: cfg.codex.extra_args.clone(),
            model: (!cfg.model.is_empty()).then(|| cfg.model.clone()),
            effort: (!cfg.effort.is_empty()).then(|| cfg.effort.clone()),
            session_id: None,
            options: cfg.codex.options.clone(),
        }
    }
}

impl AiBackend for CodexBackend {
    fn name(&self) -> &'static str {
        "codex"
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
        // 次回 send で新セッションが作られる (system prompt 再付与込み)。
        self.session_id = None;
    }

    fn resume_command(&self) -> Option<String> {
        self.session_id
            .as_ref()
            .map(|sid| format!("codex resume {sid}"))
    }

    fn send(&mut self, req: &AiRequest) -> Result<AiResponse, AiError> {
        // 初回は system prompt + terminal context + user prompt を送る。
        // 再開時は codex 側が system + 履歴を持っているので、新しい terminal context + user prompt のみ。
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

        let last_msg_path = unique_tmp_path(".txt");

        let mut args: Vec<String> = vec!["exec".to_string()];
        if let Some(sid) = &self.session_id {
            args.push("resume".to_string());
            args.push(sid.clone());
        }
        // 全ツール feature を無効化して codex を「LLM のみ」にする (初回も resume 後も同じ)。
        for feat in DISABLE_TOOL_FEATURES {
            args.push("--disable".to_string());
            args.push(feat.to_string());
        }
        // sandbox は初回のみ指定 (resume では元セッションの設定を継承する)。
        if self.session_id.is_none() {
            args.push("-s".to_string());
            args.push("read-only".to_string());
        }
        args.extend([
            "--skip-git-repo-check".to_string(),
            "-o".to_string(),
            last_msg_path.clone(),
            "-".to_string(),
        ]);
        args.extend(self.base_extra_args.iter().cloned());
        if let Some(m) = &self.model {
            args.push("--model".to_string());
            args.push(m.clone());
        }
        if let Some(e) = &self.effort {
            args.push("-c".to_string());
            args.push(format!("model_reasoning_effort={e}"));
        }

        let stdout_result = run_cli_capture_stdout("codex", &args, &prompt, &self.log_path);

        let last_msg_content = std::fs::read_to_string(&last_msg_path).ok();
        let _ = std::fs::remove_file(&last_msg_path);

        let raw = match (stdout_result, last_msg_content) {
            (Ok(stdout), Some(content)) if !content.trim().is_empty() => {
                write_log(&self.log_path, &format!("[last-message]\n{content}"));
                let _ = stdout;
                content
            }
            (Ok(stdout), _) => stdout,
            (Err(AiError::EmptyOutput { .. }), Some(content)) if !content.trim().is_empty() => {
                write_log(&self.log_path, &format!("[last-message]\n{content}"));
                content
            }
            (Err(e), _) => return Err(e),
        };

        let response = parse_ai_response_lossy(&raw);

        // 初回 send が成功したら最新の rollout ファイルから UUID を捕獲。
        if self.session_id.is_none() {
            self.session_id = find_latest_codex_session_id();
            if let Some(sid) = &self.session_id {
                write_log(&self.log_path, &format!("[codex session captured] {sid}"));
            }
        }

        Ok(response)
    }
}

/// `~/.codex/sessions/` 配下を再帰的に走査し、最新の `rollout-*.jsonl` の UUID を返す。
/// 見つからなければ None。
fn find_latest_codex_session_id() -> Option<String> {
    let home = dirs::home_dir()?;
    let sessions = home.join(".codex").join("sessions");
    if !sessions.exists() {
        return None;
    }
    let mut newest: Option<(std::time::SystemTime, String)> = None;
    walk_codex_sessions(&sessions, &mut newest);
    newest.map(|(_, uuid)| uuid)
}

fn walk_codex_sessions(
    dir: &std::path::Path,
    newest: &mut Option<(std::time::SystemTime, String)>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_codex_sessions(&path, newest);
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if let Some(uuid) = parse_codex_session_uuid(name) {
                if let Ok(meta) = entry.metadata() {
                    if let Ok(mtime) = meta.modified() {
                        match newest {
                            Some((t, _)) if *t >= mtime => {}
                            _ => {
                                *newest = Some((mtime, uuid));
                            }
                        }
                    }
                }
            }
        }
    }
}

/// `rollout-2026-04-25T18-08-16-019dc3e5-954b-7bb1-be7f-6549613b7488.jsonl` のような
/// ファイル名から末尾の UUID 部分 (`019dc3e5-954b-7bb1-be7f-6549613b7488`) を抽出する。
fn parse_codex_session_uuid(filename: &str) -> Option<String> {
    let stem = filename.strip_suffix(".jsonl")?;
    let stem = stem.strip_prefix("rollout-")?;
    // UUID は末尾 5 つの '-' 区切りグループ (8-4-4-4-12 hex)。
    let parts: Vec<&str> = stem.rsplitn(6, '-').collect();
    if parts.len() < 5 {
        return None;
    }
    let lens = [
        parts[0].len(),
        parts[1].len(),
        parts[2].len(),
        parts[3].len(),
        parts[4].len(),
    ];
    if lens != [12, 4, 4, 4, 8] {
        return None;
    }
    if !parts
        .iter()
        .take(5)
        .all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
    {
        return None;
    }
    Some(format!(
        "{}-{}-{}-{}-{}",
        parts[4], parts[3], parts[2], parts[1], parts[0]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_uuid_from_full_filename() {
        let f = "rollout-2026-04-25T18-08-16-019dc3e5-954b-7bb1-be7f-6549613b7488.jsonl";
        assert_eq!(
            parse_codex_session_uuid(f),
            Some("019dc3e5-954b-7bb1-be7f-6549613b7488".into())
        );
    }

    #[test]
    fn parse_uuid_rejects_non_uuid() {
        assert!(parse_codex_session_uuid("rollout-foo-bar.jsonl").is_none());
        assert!(
            parse_codex_session_uuid("notrollout-019dc3e5-954b-7bb1-be7f-6549613b7488.jsonl")
                .is_none()
        );
        assert!(parse_codex_session_uuid("rollout-2026-04-25T18-08-16.jsonl").is_none());
    }

    #[test]
    fn model_defaults_present_without_config() {
        // config 空でも組み込み既定で `/model` ピッカーの候補が出る (既定消失の回帰防止)。
        let backend = CodexBackend::new(&AiConfig::default(), &LogConfig::default());
        assert!(!backend.available_models().is_empty());
    }
}
