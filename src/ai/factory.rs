use super::claude::ClaudeBackend;
use super::cloudflare_workers_ai::CloudflareWorkersAiBackend;
use super::codex::CodexBackend;
use super::copilot::CopilotBackend;
use super::cursor::CursorBackend;
use super::gemini::GeminiBackend;
use super::generic::GenericCliBackend;
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
        BackendKind::Cursor => Ok(Box::new(CursorBackend::new(cfg, log))),
        BackendKind::Copilot => Ok(Box::new(CopilotBackend::new(cfg, log))),
        BackendKind::Cloudflare => Ok(Box::new(CloudflareWorkersAiBackend::new(cfg, log))),
        BackendKind::Generic(_) => {
            let meta = kind.generic_meta().ok_or_else(|| {
                AiError::Other(format!(
                    "generic backend not registered (kind={kind:?}); did you call init_generics()?"
                ))
            })?;
            Ok(Box::new(GenericCliBackend::new(meta.recipe, cfg, log)))
        }
    }
}

pub fn check_installed(kind: BackendKind) -> bool {
    // 実行ファイル名は kind ごとに異なることがある (cursor → cursor-agent) ので `binary()` を使う。
    Command::new(kind.binary())
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
