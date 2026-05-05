use super::claude::ClaudeBackend;
use super::codex::CodexBackend;
use super::gemini::GeminiBackend;
use super::qwen::QwenBackend;
use super::types::{AiBackend, AiError, BackendKind};
use crate::config::{AiConfig, LogConfig};
use std::process::Command;

pub fn create_backend(
    kind: BackendKind,
    cfg: &AiConfig,
    log: &LogConfig,
) -> Result<Box<dyn AiBackend>, AiError> {
    match kind {
        BackendKind::Claude => Ok(Box::new(ClaudeBackend::new(cfg, log))),
        BackendKind::Codex => Ok(Box::new(CodexBackend::new(cfg, log))),
        BackendKind::Gemini => Ok(Box::new(GeminiBackend::new(cfg, log))),
        BackendKind::Qwen => Ok(Box::new(QwenBackend::new(cfg, log))),
    }
}

pub fn check_installed(kind: BackendKind) -> bool {
    Command::new(kind.as_str())
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
