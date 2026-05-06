use super::common::{
    build_full_prompt, build_proposal_system_prompt, expand_tilde, extract_model_from_args,
    override_model_in_args, parse_ai_response_lossy, run_cli_capture_stdout, trim_history,
    unique_tmp_path, write_log,
};
use super::types::{AiBackend, AiError, AiRequest, AiResponse};
use crate::config::{AiConfig, LogConfig};

/// 直近何ターン分の会話履歴をプロンプトに含めるか。
const MAX_HISTORY_TURNS: usize = 8;

/// codex の自律エージェント挙動を無効化するための feature 一覧。
/// `codex features list` で stable / experimental の tool 系をすべて落とす。
/// これを付けないと codex は内部で shell 等を実行して結果だけを返してしまい、
/// aish の「提案 → 確認 → 実行」モデルを迂回する。
const DISABLE_TOOL_FEATURES: &[&str] = &[
    "shell_tool",                  // shell 実行
    "unified_exec",                // 別系統の exec
    "browser_use",                 // ブラウザ操作
    "computer_use",                // computer use
    "multi_agent",                 // sub-agent (sub-agent がツールを持つ恐れ)
    "image_generation",            // 画像生成
    "tool_search",                 // ツール探索
    "tool_suggest",                // ツール提案
    "plugins",                     // プラグイン (任意ツール経由)
    "apps",                        // app 連携
    "skill_mcp_dependency_install", // MCP 依存インストール
    "tool_call_mcp_elicitation",   // MCP ツール呼び出し
];

/// Codex CLI (`codex exec`) backend。
///
/// 戦略:
/// - `codex exec --disable <tool features...> -s read-only --skip-git-repo-check --ephemeral -o <tmp> -`
///   - `--disable shell_tool` 等で **codex のツール群をすべて切り**、純粋な LLM 応答だけを得る。
///     これにより codex が aish の確認 UI を経由せずローカルでコマンド実行する経路を塞ぐ。
///   - `-s read-only`: 万一切り漏れたツールが残っても sandbox で write を防ぐ defense-in-depth。
///   - `--skip-git-repo-check`: aish は git リポジトリ外で起動されることがある。
///   - `--ephemeral`: セッションファイルを永続化させない。aish 側で session 管理しない方針。
///   - `-o <tmp>`: 最終 assistant メッセージだけをファイルに書き出す。stdout はイベントログ。
///   - `-`: PROMPT を stdin から読む。
/// - JSON Schema 強制機能は無いので system prompt で `{message, commands}` 出力を強く指示し、
///   `extract_json` で抽出する。失敗時は全文を message としてフォールバック。
///
/// session resume は使わず、内部で履歴 (user_prompt, ai_message) を保持して毎回プロンプトに含める。
pub struct CodexBackend {
    system_prompt: String,
    log_path: Option<String>,
    extra_args: Vec<String>,
    history: Vec<(String, String)>,
}

impl CodexBackend {
    pub fn new(cfg: &AiConfig, log: &LogConfig) -> Self {
        let log_path = if log.enabled {
            Some(expand_tilde(&log.path))
        } else {
            None
        };
        let system_prompt = build_proposal_system_prompt(&cfg.system_prompt, &cfg.language);
        let override_model = (!cfg.model.is_empty()).then_some(cfg.model.as_str());
        Self {
            system_prompt,
            log_path,
            extra_args: override_model_in_args(&cfg.codex.extra_args, override_model),
            history: Vec::new(),
        }
    }
}

impl AiBackend for CodexBackend {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn model(&self) -> Option<String> {
        extract_model_from_args(&self.extra_args)
    }

    fn send(&mut self, req: &AiRequest) -> Result<AiResponse, AiError> {
        let prompt = build_full_prompt(
            &self.system_prompt,
            &self.history,
            req.terminal_context,
            req.user_prompt,
        );

        let last_msg_path = unique_tmp_path(".txt");

        let mut args: Vec<String> = vec!["exec".to_string()];
        // 全ツール feature を無効化して codex を「LLM のみ」にする (信頼原則の根幹)。
        for feat in DISABLE_TOOL_FEATURES {
            args.push("--disable".to_string());
            args.push(feat.to_string());
        }
        args.extend([
            "-s".to_string(),
            "read-only".to_string(),
            "--skip-git-repo-check".to_string(),
            "--ephemeral".to_string(),
            "-o".to_string(),
            last_msg_path.clone(),
            "-".to_string(),
        ]);
        args.extend(self.extra_args.iter().cloned());

        let stdout_result = run_cli_capture_stdout("codex", &args, &prompt, &self.log_path);

        // どの結果でも tmp ファイルは消す
        let last_msg_content = std::fs::read_to_string(&last_msg_path).ok();
        let _ = std::fs::remove_file(&last_msg_path);

        // codex は --output-last-message にだけ最終応答を書き、stdout は空のことがある。
        // EmptyOutput でも last-message ファイルがあるならそちらを採用する。
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
        self.history
            .push((req.user_prompt.to_string(), response.message.clone()));
        trim_history(&mut self.history, MAX_HISTORY_TURNS);
        Ok(response)
    }
}
