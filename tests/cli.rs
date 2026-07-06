//! 実バイナリの統合テスト (tty 不要の CLI 経路のみ)。
//! `CARGO_BIN_EXE_aish` は cargo test がビルドした aish 本体のパス。
//! 対話 UI (PTY/raw mode) はここでは起動しない — その検証は tests/e2e/ (pexpect) 側。

use std::process::Command;

fn aish() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aish"));
    // aish セッション内から cargo test を実行しても nested 検出に当たらないよう除去
    // (aish の子シェルは AISH_PID を継承している)。
    cmd.env_remove("AISH_PID");
    cmd
}

#[test]
fn version_prints_and_exits_zero() {
    let out = aish().arg("--version").output().expect("run aish");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")), "got: {stdout}");
}

#[test]
fn help_prints_usage() {
    let out = aish().arg("--help").output().expect("run aish");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.to_lowercase().contains("usage"), "got: {stdout}");
}

#[test]
fn list_providers_includes_native_and_builtin_recipes() {
    let out = aish().arg("--list-providers").output().expect("run aish");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    // native 全種と同梱 recipe が列挙される (新 backend の載せ忘れ検知)。
    for name in [
        "claude",
        "codex",
        "gemini",
        "qwen",
        "cursor",
        "copilot",
        "cloudflare",
        "nvidia",
        "kimi",
        "opencode",
    ] {
        assert!(stdout.contains(name), "missing {name} in: {stdout}");
    }
}

#[test]
fn unknown_ai_backend_fails_with_available_list() {
    let out = aish()
        .args(["--ai", "nonexistent-backend-xyz"])
        .output()
        .expect("run aish");
    assert!(!out.status.success(), "should fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown backend") && stderr.contains("claude"),
        "エラーに利用可能一覧が含まれること: {stderr}"
    );
}
