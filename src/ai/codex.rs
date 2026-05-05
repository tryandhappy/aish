use super::common::{
    build_full_prompt, build_proposal_system_prompt, expand_tilde, parse_ai_response_lossy,
    run_cli_capture_stdout, trim_history, unique_tmp_path, write_log,
};
use super::types::{AiBackend, AiError, AiRequest, AiResponse};
use crate::config::{AiConfig, LogConfig};

/// 直近何ターン分の会話履歴をプロンプトに含めるか。
const MAX_HISTORY_TURNS: usize = 8;

/// Codex CLI (`codex exec`) backend。
///
/// 戦略:
/// - `codex exec -s read-only --skip-git-repo-check --ephemeral -o <tmp> -`
///   - `-s read-only`: 万が一モデルがツール呼び出しを試みても read-only sandbox で防ぐ。
///   - `--skip-git-repo-check`: aish は git リポジトリ外で起動されることがある。
///   - `--ephemeral`: セッションファイルを永続化させない。aish 側で session 管理しない方針。
///   - `-o <tmp>`: 最終 assistant メッセージだけをファイルに書き出す。stdout はイベントログ。
///   - `-`: PROMPT を stdin から読む。
/// - JSON Schema 強制機能は無いので system prompt で `{message, commands}` 出力を強く指示し、
///   `extract_json` で抽出する。失敗時は全文を message としてフォールバック。
///
/// 安全性の限界 (SPEC.md §6 参照):
/// - codex CLI の `-s read-only` は **shell 実行を防ぐが、ツール呼び出し自体を禁止する仕組みではない**。
///   モデルが reasoning 中に read-only コマンド (`ls`, `cat` 等) を発火させ、aish の確認なしに
///   ローカル情報を観測する可能性がある。書き込みやサーバ側変更は read-only sandbox で防げる。
///   完全な「ツール禁止」は system prompt の指示に依存するため、最大限の安全性が必要なら
///   `--aish-ai claude` (`--disallowedTools` でフラグレベル拒否) を使うこと。
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
        Self {
            system_prompt,
            log_path,
            extra_args: cfg.codex.extra_args.clone(),
            history: Vec::new(),
        }
    }
}

impl AiBackend for CodexBackend {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn send(&mut self, req: &AiRequest) -> Result<AiResponse, AiError> {
        let prompt = build_full_prompt(
            &self.system_prompt,
            &self.history,
            req.terminal_context,
            req.user_prompt,
        );

        let last_msg_path = unique_tmp_path(".txt");

        let mut args: Vec<String> = vec![
            "exec".to_string(),
            "-s".to_string(),
            "read-only".to_string(),
            "--skip-git-repo-check".to_string(),
            "--ephemeral".to_string(),
            "-o".to_string(),
            last_msg_path.clone(),
            "-".to_string(),
        ];
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
