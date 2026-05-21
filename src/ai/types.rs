use serde::Deserialize;
use std::fmt;

pub struct AiRequest<'a> {
    pub terminal_context: &'a str,
    pub user_prompt: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct AiResponse {
    pub message: String,
    pub commands: Vec<String>,
}

pub trait AiBackend: Send {
    /// バックエンド識別名 (診断・将来のステータス表示用)。
    #[allow(dead_code)]
    fn name(&self) -> &'static str;
    fn send(&mut self, req: &AiRequest) -> Result<AiResponse, AiError>;
    /// 設定から取得できるモデル名 (起動バナー表示用)。
    /// CLI に問い合わせず `extra_args` 等のローカル情報から判定するので、
    /// 取得できない backend は None を返す。
    fn model(&self) -> Option<String> {
        None
    }
    /// 現在の reasoning effort を返す (起動バナー / `/effort` 表示用)。
    fn effort(&self) -> Option<String> {
        None
    }
    /// runtime にモデルを差し替える (`/model <name>` 用)。
    /// 既存セッション (claude の session_id, codex/gemini/qwen の history) は維持する。
    fn set_model(&mut self, _model: Option<&str>) {}
    /// runtime に reasoning effort を差し替える (`/effort <level>` 用)。
    /// 該当 CLI フラグを持たない backend (gemini/qwen) は内部に保存するが
    /// 実際のリクエストには反映されない。
    fn set_effort(&mut self, _effort: Option<&str>) {}
    /// 会話履歴 / セッション ID をリセットする (`/clear` 用)。
    fn clear_history(&mut self) {}
    /// aish 終了時に表示する「このセッションを当該 CLI のインタラクティブモードで再開するための
    /// シェルコマンド例」。session_id を持たない / 永続化されていない場合は None。
    fn resume_command(&self) -> Option<String> {
        None
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum BackendKind {
    Claude,
    Codex,
    Gemini,
    Qwen,
    Cursor,
}

impl BackendKind {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "claude" => Ok(BackendKind::Claude),
            "codex" => Ok(BackendKind::Codex),
            "gemini" => Ok(BackendKind::Gemini),
            "qwen" => Ok(BackendKind::Qwen),
            "cursor" => Ok(BackendKind::Cursor),
            other => Err(format!(
                "unknown backend `{other}` (expected: claude|codex|gemini|qwen|cursor)"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Claude => "claude",
            BackendKind::Codex => "codex",
            BackendKind::Gemini => "gemini",
            BackendKind::Qwen => "qwen",
            BackendKind::Cursor => "cursor",
        }
    }

    /// 実行ファイル名 (`check_installed` / spawn 用)。
    /// `as_str` と異なる backend (cursor → cursor-agent) があるため別途定義。
    pub fn binary(self) -> &'static str {
        match self {
            BackendKind::Claude => "claude",
            BackendKind::Codex => "codex",
            BackendKind::Gemini => "gemini",
            BackendKind::Qwen => "qwen",
            BackendKind::Cursor => "cursor-agent",
        }
    }

    /// 固定長配列 (`[T; BackendKind::COUNT]`) のインデックスに使う。
    pub fn ordinal(self) -> usize {
        match self {
            BackendKind::Claude => 0,
            BackendKind::Codex => 1,
            BackendKind::Gemini => 2,
            BackendKind::Qwen => 3,
            BackendKind::Cursor => 4,
        }
    }

    pub const COUNT: usize = 5;

    #[allow(dead_code)]
    pub fn all() -> [BackendKind; Self::COUNT] {
        [
            BackendKind::Claude,
            BackendKind::Codex,
            BackendKind::Gemini,
            BackendKind::Qwen,
            BackendKind::Cursor,
        ]
    }
}

#[derive(Debug)]
pub enum AiError {
    Cancelled,
    Spawn(std::io::Error),
    NonZeroExit { stderr: String },
    EmptyOutput { stderr: String },
    NoJson { raw: String },
    ParseFailure { raw: String, source: serde_json::Error },
    Other(String),
}

impl fmt::Display for AiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AiError::Cancelled => write!(f, "Cancelled"),
            AiError::Spawn(e) => write!(f, "failed to spawn AI CLI: {e}"),
            AiError::NonZeroExit { stderr } => write!(f, "AI CLI failed: {stderr}"),
            AiError::EmptyOutput { stderr } => {
                write!(f, "AI CLI returned empty output. stderr: {stderr}")
            }
            AiError::NoJson { raw } => write!(f, "No JSON found in AI CLI output: {raw}"),
            AiError::ParseFailure { raw, source } => {
                write!(f, "Failed to parse AI CLI output: {source}\nRaw: {raw}")
            }
            AiError::Other(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for AiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AiError::Spawn(e) => Some(e),
            AiError::ParseFailure { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<std::io::Error> for AiError {
    fn from(e: std::io::Error) -> Self {
        AiError::Spawn(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known() {
        assert_eq!(BackendKind::parse("claude").unwrap(), BackendKind::Claude);
        assert_eq!(BackendKind::parse("codex").unwrap(), BackendKind::Codex);
        assert_eq!(BackendKind::parse("gemini").unwrap(), BackendKind::Gemini);
        assert_eq!(BackendKind::parse("qwen").unwrap(), BackendKind::Qwen);
        assert_eq!(BackendKind::parse("cursor").unwrap(), BackendKind::Cursor);
    }

    #[test]
    fn parse_unknown() {
        assert!(BackendKind::parse("xyz").is_err());
        assert!(BackendKind::parse("").is_err());
        assert!(BackendKind::parse("Claude").is_err()); // case-sensitive
    }

    #[test]
    fn as_str_roundtrip() {
        for kind in [
            BackendKind::Claude,
            BackendKind::Codex,
            BackendKind::Gemini,
            BackendKind::Qwen,
            BackendKind::Cursor,
        ] {
            assert_eq!(BackendKind::parse(kind.as_str()).unwrap(), kind);
        }
    }

    #[test]
    fn binary_overrides_as_str_for_cursor() {
        assert_eq!(BackendKind::Cursor.binary(), "cursor-agent");
        assert_eq!(BackendKind::Claude.binary(), "claude");
    }

    #[test]
    fn ordinals_are_unique_and_within_count() {
        let mut seen = [false; BackendKind::COUNT];
        for k in BackendKind::all() {
            let o = k.ordinal();
            assert!(o < BackendKind::COUNT, "ordinal {o} out of range");
            assert!(!seen[o], "duplicate ordinal {o}");
            seen[o] = true;
        }
        assert!(seen.iter().all(|&b| b));
    }

    #[test]
    fn cancelled_displays_as_cancelled() {
        assert_eq!(AiError::Cancelled.to_string(), "Cancelled");
    }
}
