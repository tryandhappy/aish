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

/// AI CLI が 1 つも見つからないときに表示する導入案内。`auto_detect_order` と同じ
/// backend を同順（人気順）に、各公式インストール/クイックスタート URL 付きで列挙する。
/// レイアウトは `  ▸ 名前` + 次行に `      URL`（名前と URL を段差で分離してリストと
/// 分かるようにする）。REST(Cloudflare/Nvidia)と generic は対象外（インストール概念が
/// 無い / URL が定まらない）。`color=true` のとき名前を太字・URL を淡色に着色する
/// （呼び出し側が stderr の TTY / NO_COLOR を見て決める）。先頭見出しの `Error:` 着色は
/// main 側（他エラーと共通の経路）が行う。
pub fn install_guide(color: bool) -> String {
    // 順序は auto_detect_order と一致させる（テスト install_guide_lists_all_backends）。
    let entries: [(&str, &str); 6] = [
        ("Claude Code", "https://code.claude.com/docs/ja/quickstart"),
        (
            "Codex",
            "https://learn.chatgpt.com/docs/codex/cli#getting-started",
        ),
        ("Gemini", "https://github.com/google-gemini/gemini-cli"),
        (
            "GitHub Copilot",
            "https://docs.github.com/copilot/how-tos/set-up/install-copilot-cli",
        ),
        ("Cursor", "https://cursor.com/docs/cli/installation"),
        ("Qwen", "https://github.com/QwenLM/qwen-code"),
    ];
    // 太字 (名前) / 淡色 (URL)。color=false なら無装飾。
    let (nb, ne, ub, ue) = if color {
        ("\x1b[1m", "\x1b[0m", "\x1b[2m", "\x1b[0m")
    } else {
        ("", "", "", "")
    };
    let mut s = String::from("No AI agent found. Please install one:\n");
    for (name, url) in entries {
        s.push_str(&format!("\n  \u{25B8} {nb}{name}{ne}\n      {ub}{url}{ue}"));
    }
    s
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

    #[test]
    fn install_guide_lists_all_backends() {
        let g = install_guide(false);
        assert!(g.starts_with("No AI agent found. Please install one:"));
        // auto_detect_order の 6 backend を人気順で列挙し、各 URL を含む。
        for needle in [
            "Claude Code",
            "https://code.claude.com/docs/ja/quickstart",
            "Codex",
            "https://learn.chatgpt.com/docs/codex/cli#getting-started",
            "Gemini",
            "https://github.com/google-gemini/gemini-cli",
            "GitHub Copilot",
            "https://docs.github.com/copilot/how-tos/set-up/install-copilot-cli",
            "Cursor",
            "https://cursor.com/docs/cli/installation",
            "Qwen",
            "https://github.com/QwenLM/qwen-code",
        ] {
            assert!(g.contains(needle), "install_guide should contain {needle}");
        }
        // 箇条書きレイアウト (▸ + 名前)。
        assert!(g.contains("\u{25B8} Claude Code"));
        // Claude が Codex より前 (人気順)。
        assert!(g.find("Claude Code").unwrap() < g.find("Codex").unwrap());
        // color=false は ANSI を含まない。
        assert!(!g.contains('\x1b'), "no ANSI escapes when color=false");
        // color=true は ANSI (太字/淡色) を含む。
        assert!(
            install_guide(true).contains('\x1b'),
            "ANSI escapes present when color=true"
        );
    }
}
