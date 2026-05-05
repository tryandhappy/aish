use super::claude::ClaudeBackend;
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
        BackendKind::Codex => Err(AiError::NotInstalled {
            kind,
            hint: "Codex backend not yet implemented in aish".to_string(),
        }),
        BackendKind::Gemini => Err(AiError::NotInstalled {
            kind,
            hint: "Gemini backend not yet implemented in aish".to_string(),
        }),
        BackendKind::Qwen => Err(AiError::NotInstalled {
            kind,
            hint: "Qwen backend not yet implemented in aish".to_string(),
        }),
    }
}

pub fn check_installed(kind: BackendKind) -> bool {
    Command::new(kind.as_str())
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
