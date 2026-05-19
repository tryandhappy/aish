use super::claude::ClaudeBackend;
use super::codex::CodexBackend;
use super::gemini::GeminiBackend;
use super::qwen::QwenBackend;
use super::sandbox;
use super::types::{AiBackend, AiError, BackendKind};
use crate::config::{AiConfig, LogConfig};
use std::process::Command;

pub fn create_backend(
    kind: BackendKind,
    cfg: &AiConfig,
    log: &LogConfig,
) -> Result<Box<dyn AiBackend>, AiError> {
    // サンドボックス設定を解決 (glocal [ai.sandbox] と per-backend [ai.<name>.sandbox] をマージ)。
    // macOS で mode = "bwrap" 等の不正設定が来たらここでエラーになる。
    let sb = sandbox::resolve(kind, cfg).map_err(AiError::Other)?;
    match kind {
        BackendKind::Claude => Ok(Box::new(ClaudeBackend::new(cfg, log, sb))),
        BackendKind::Codex => Ok(Box::new(CodexBackend::new(cfg, log, sb))),
        BackendKind::Gemini => Ok(Box::new(GeminiBackend::new(cfg, log, sb))),
        BackendKind::Qwen => Ok(Box::new(QwenBackend::new(cfg, log, sb))),
    }
}

pub fn check_installed(kind: BackendKind) -> bool {
    Command::new(kind.as_str())
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
