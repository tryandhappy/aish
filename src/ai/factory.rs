use super::claude::ClaudeBackend;
use super::cloudflare_workers_ai::CloudflareWorkersAiBackend;
use super::codex::CodexBackend;
use super::copilot::CopilotBackend;
use super::cursor::CursorBackend;
use super::gemini::GeminiBackend;
use super::generic::GenericCliBackend;
use super::nvidia_nim::NvidiaNimBackend;
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
        BackendKind::Nvidia => Ok(Box::new(NvidiaNimBackend::new(cfg, log))),
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

/// 自動検出で試す native backend の優先順 (Claude Code → Codex → Gemini → 人気順)。
/// REST backend (Cloudflare/Nvidia) は `binary()`="curl" でほぼ常に検出成功してしまい、
/// かつ API key が必須なので自動選択から除外する (curl は AI CLI ではない)。
/// 純関数なので順序を golden test で固定する。
pub fn auto_detect_order() -> [BackendKind; 6] {
    [
        BackendKind::Claude,
        BackendKind::Codex,
        BackendKind::Gemini,
        BackendKind::Copilot,
        BackendKind::Cursor,
        BackendKind::Qwen,
    ]
}

/// 選択した backend が未インストールのときに、実際に使える AI CLI を探す。
/// `auto_detect_order()` (native) → registered generic backend の順に `--version` で
/// 実在確認し、最初に見つかったものを返す。1 つも無ければ None。
pub fn auto_detect_backend() -> Option<BackendKind> {
    auto_detect_order()
        .into_iter()
        .find(|&k| check_installed(k))
        // generic recipe (opencode 等) は registry 登録順で後追い。
        .or_else(|| {
            BackendKind::all_generics()
                .into_iter()
                .find(|&k| check_installed(k))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_detect_order_is_stable() {
        // 検出順の不変条件を固定 (Claude Code → Codex → Gemini → 人気順)。
        // REST backend (Cloudflare/Nvidia) は含めない。
        let order = auto_detect_order();
        assert_eq!(
            order.map(|k| k.as_str()),
            ["claude", "codex", "gemini", "copilot", "cursor", "qwen"]
        );
    }
}
