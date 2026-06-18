use std::process::Command;

const REPO_OWNER: &str = "tryandhappy";
const REPO_NAME: &str = "aish";

/// 自己更新で追従するリリースチャネル。
/// `Stable` は GitHub の `Latest` リリース (= prerelease を除いた最新)、
/// `Prerelease` は prerelease を含む絶対最新を取得する。命名の注意:
/// GitHub API では「安定版」が `/releases/latest` (単数) で取れるのに対し、
/// 「prerelease 含む最新」は `/releases` (一覧) の先頭。"latest" という語が
/// 両者でぶつかって紛らわしいので、ユーザ向け flag は `--stable` / `--prerelease`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateChannel {
    Stable,
    Prerelease,
}

fn detect_target() -> Result<&'static str, String> {
    target_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// OS と ARCH からリリースアセットの target triple を決める純関数。
/// `std::env::consts::{OS,ARCH}` はコンパイル時にビルドターゲットへ固定されるので、
/// 配布バイナリは自分のプラットフォームを必ず正しく自己申告する (実行時探索は不要)。
/// Rust の ARCH は Apple Silicon でも "aarch64" (shell の `uname -m` の "arm64" 問題は無い)。
fn target_for(os: &str, arch: &str) -> Result<&'static str, String> {
    match (os, arch) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => Err(format!("Unsupported platform: {os}/{arch}")),
    }
}

/// GitHub REST API へ `curl` で GET し、レスポンス JSON を返す共通ヘルパ。
fn github_api_get(url: &str) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let output = Command::new("curl")
        .args(["-fsSL", "-H", "Accept: application/vnd.github+json", url])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to fetch release info: {stderr}").into());
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    Ok(json)
}

/// `/releases/latest` (単一オブジェクト) から `tag_name` を取り出す純関数。
fn parse_latest_tag(json: &serde_json::Value) -> Result<String, String> {
    json["tag_name"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "tag_name not found in response".to_string())
}

/// `/releases` (新しい順の配列) の先頭要素から `tag_name` を取り出す純関数。
/// prerelease を含む絶対最新を返す。配列が空ならエラー。
fn parse_newest_tag_from_list(json: &serde_json::Value) -> Result<String, String> {
    let arr = json
        .as_array()
        .ok_or_else(|| "expected a JSON array of releases".to_string())?;
    let first = arr.first().ok_or_else(|| "no releases found".to_string())?;
    first["tag_name"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "tag_name not found in first release".to_string())
}

/// 指定チャネルの取得すべきリリースタグを返す。
/// `Stable` は `/releases/latest` (prerelease 除外の最新)、
/// `Prerelease` は `/releases` 一覧の先頭 (prerelease 含む絶対最新)。
fn fetch_version(channel: UpdateChannel) -> Result<String, Box<dyn std::error::Error>> {
    match channel {
        UpdateChannel::Stable => {
            let url =
                format!("https://api.github.com/repos/{REPO_OWNER}/{REPO_NAME}/releases/latest");
            let json = github_api_get(&url)?;
            parse_latest_tag(&json).map_err(|e| e.into())
        }
        UpdateChannel::Prerelease => {
            let url = format!("https://api.github.com/repos/{REPO_OWNER}/{REPO_NAME}/releases");
            let json = github_api_get(&url)?;
            parse_newest_tag_from_list(&json).map_err(|e| e.into())
        }
    }
}

/// `sha256sum file > file.sha256` の出力形式（"<64-hex>  filename"）から
/// 先頭の 64文字 SHA256 を取り出す。生のハッシュ文字列だけでもOK。
fn parse_sha256_hash(content: &str) -> Result<String, String> {
    let hash = content
        .split_whitespace()
        .next()
        .ok_or_else(|| "Empty checksum content".to_string())?
        .to_lowercase();
    if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("Invalid checksum format: {hash}"));
    }
    Ok(hash)
}

fn fetch_expected_sha256(url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("curl").args(["-fsSL", url]).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to fetch checksum: {stderr}").into());
    }
    let content = String::from_utf8_lossy(&output.stdout);
    parse_sha256_hash(&content).map_err(|e| e.into())
}

fn compute_sha256(path: &str) -> Result<String, Box<dyn std::error::Error>> {
    // Linux は sha256sum (coreutils / Alpine は busybox applet)、macOS は sha256sum が
    // 無いので shasum -a 256 にフォールバックする。sha256sum を先に試すことで Linux の
    // 挙動は従来どおり維持し、コマンド不在 (spawn 失敗) のときだけ shasum に落ちる。
    // 出力形式はどちらも "<64-hex>  <filename>" で parse_sha256_hash と互換。
    let output = match Command::new("sha256sum").arg(path).output() {
        Ok(o) => o,
        Err(_) => Command::new("shasum").args(["-a", "256", path]).output()?,
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("checksum command failed: {stderr}").into());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_sha256_hash(&stdout).map_err(|e| e.into())
}

pub fn run_update(channel: UpdateChannel) -> Result<(), Box<dyn std::error::Error>> {
    let current = env!("CARGO_PKG_VERSION");
    let channel_label = match channel {
        UpdateChannel::Stable => "stable",
        UpdateChannel::Prerelease => "prerelease",
    };
    println!("aish v{current} (channel: {channel_label})");

    let target = detect_target()?;
    let tag = fetch_version(channel)?;
    let latest = tag.strip_prefix('v').unwrap_or(&tag);

    if latest == current {
        println!("Already up to date.");
        return Ok(());
    }

    println!("Updating to v{latest} ...");

    let binary_name = format!("aish-{target}");
    let download_url = format!(
        "https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/download/{tag}/{binary_name}"
    );
    let checksum_url = format!("{download_url}.sha256");

    // Download to temp file
    let tmpfile = std::env::temp_dir()
        .join(format!("aish-update-{}", std::process::id()))
        .to_string_lossy()
        .to_string();

    let dl_status = Command::new("curl")
        .args(["-fSL", "-o", &tmpfile, &download_url])
        .status()?;
    if !dl_status.success() {
        let _ = std::fs::remove_file(&tmpfile);
        return Err("Failed to download binary".into());
    }

    // Verify SHA256 checksum from the matching .sha256 file in the release.
    println!("Verifying checksum ...");
    let expected = fetch_expected_sha256(&checksum_url).map_err(|e| {
        let _ = std::fs::remove_file(&tmpfile);
        format!("Failed to fetch {checksum_url}: {e}")
    })?;
    let actual = compute_sha256(&tmpfile).map_err(|e| {
        let _ = std::fs::remove_file(&tmpfile);
        format!("Failed to compute checksum: {e}")
    })?;
    if expected != actual {
        let _ = std::fs::remove_file(&tmpfile);
        return Err(
            format!("Checksum mismatch.\n  expected: {expected}\n  actual:   {actual}").into(),
        );
    }

    // Install to current executable path
    let exe_path = std::env::current_exe()?;
    let exe_path_str = exe_path.to_string_lossy();
    println!("Installing to {exe_path_str} ...");

    // Set executable permission
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmpfile, std::fs::Permissions::from_mode(0o755)).map_err(
            |e| {
                let _ = std::fs::remove_file(&tmpfile);
                format!("Failed to set permissions: {e}")
            },
        )?;
    }

    // Replace current binary
    let result = std::fs::rename(&tmpfile, &exe_path).or_else(|_| {
        // rename may fail across filesystems, try copy
        let copy_result = std::fs::copy(&tmpfile, &exe_path).map(|_| ());
        let _ = std::fs::remove_file(&tmpfile);
        copy_result
    });

    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmpfile);
        return Err(format!("Failed to install binary: {e}").into());
    }

    println!("Updated to v{latest}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_for_linux() {
        assert_eq!(
            target_for("linux", "x86_64").unwrap(),
            "x86_64-unknown-linux-musl"
        );
        assert_eq!(
            target_for("linux", "aarch64").unwrap(),
            "aarch64-unknown-linux-musl"
        );
    }

    #[test]
    fn target_for_macos() {
        assert_eq!(
            target_for("macos", "x86_64").unwrap(),
            "x86_64-apple-darwin"
        );
        assert_eq!(
            target_for("macos", "aarch64").unwrap(),
            "aarch64-apple-darwin"
        );
    }

    #[test]
    fn target_for_unsupported() {
        assert!(target_for("windows", "x86_64").is_err());
        assert!(target_for("linux", "riscv64").is_err());
        assert!(target_for("freebsd", "x86_64").is_err());
    }

    #[test]
    fn parse_hash_alone() {
        let h = "a".repeat(64);
        assert_eq!(parse_sha256_hash(&h).unwrap(), h);
    }

    #[test]
    fn parse_hash_with_filename() {
        let line = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789  some-file";
        assert_eq!(
            parse_sha256_hash(line).unwrap(),
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        );
    }

    #[test]
    fn parse_hash_uppercase_normalized() {
        let h = "F".repeat(64);
        assert_eq!(parse_sha256_hash(&h).unwrap(), "f".repeat(64));
    }

    #[test]
    fn parse_hash_too_short() {
        let h = "a".repeat(63);
        assert!(parse_sha256_hash(&h).is_err());
    }

    #[test]
    fn parse_hash_too_long() {
        let h = "a".repeat(65);
        assert!(parse_sha256_hash(&h).is_err());
    }

    #[test]
    fn parse_hash_non_hex() {
        let h = "z".repeat(64);
        assert!(parse_sha256_hash(&h).is_err());
    }

    #[test]
    fn parse_hash_empty() {
        assert!(parse_sha256_hash("").is_err());
    }

    #[test]
    fn parse_hash_with_leading_whitespace() {
        let line = "   abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        assert_eq!(
            parse_sha256_hash(line).unwrap(),
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        );
    }

    #[test]
    fn latest_tag_from_object() {
        let json = serde_json::json!({ "tag_name": "v0.9.0", "prerelease": false });
        assert_eq!(parse_latest_tag(&json).unwrap(), "v0.9.0");
    }

    #[test]
    fn latest_tag_missing() {
        let json = serde_json::json!({ "name": "v0.9.0" });
        assert!(parse_latest_tag(&json).is_err());
    }

    #[test]
    fn newest_tag_from_list_picks_first() {
        // GitHub の /releases は新しい順。先頭が prerelease でもそれを採用する。
        let json = serde_json::json!([
            { "tag_name": "v0.10.0-rc.1", "prerelease": true },
            { "tag_name": "v0.9.0", "prerelease": false },
        ]);
        assert_eq!(parse_newest_tag_from_list(&json).unwrap(), "v0.10.0-rc.1");
    }

    #[test]
    fn newest_tag_from_empty_list() {
        let json = serde_json::json!([]);
        assert!(parse_newest_tag_from_list(&json).is_err());
    }

    #[test]
    fn newest_tag_from_non_array() {
        let json = serde_json::json!({ "tag_name": "v0.9.0" });
        assert!(parse_newest_tag_from_list(&json).is_err());
    }

    #[test]
    fn newest_tag_first_missing_tag_name() {
        let json = serde_json::json!([{ "name": "v0.9.0" }]);
        assert!(parse_newest_tag_from_list(&json).is_err());
    }
}
