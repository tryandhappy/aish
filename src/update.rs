use std::process::Command;

const REPO_OWNER: &str = "tryandhappy";
const REPO_NAME: &str = "aish";

fn detect_target() -> Result<&'static str, String> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x86_64-unknown-linux-musl"),
        "aarch64" => Ok("aarch64-unknown-linux-musl"),
        arch => Err(format!("Unsupported architecture: {}", arch)),
    }
}

fn fetch_latest_version() -> Result<String, Box<dyn std::error::Error>> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/releases/latest",
        REPO_OWNER, REPO_NAME
    );
    let output = Command::new("curl")
        .args(["-fsSL", "-H", "Accept: application/vnd.github+json", &url])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to fetch release info: {}", stderr).into());
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let tag = json["tag_name"]
        .as_str()
        .ok_or("tag_name not found in response")?;
    Ok(tag.to_string())
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
        return Err(format!("Invalid checksum format: {}", hash));
    }
    Ok(hash)
}

fn fetch_expected_sha256(url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("curl").args(["-fsSL", url]).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to fetch checksum: {}", stderr).into());
    }
    let content = String::from_utf8_lossy(&output.stdout);
    parse_sha256_hash(&content).map_err(|e| e.into())
}

fn compute_sha256(path: &str) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("sha256sum").arg(path).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("sha256sum failed: {}", stderr).into());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_sha256_hash(&stdout).map_err(|e| e.into())
}

pub fn run_update() -> Result<(), Box<dyn std::error::Error>> {
    let current = env!("CARGO_PKG_VERSION");
    println!("aish v{}", current);

    let target = detect_target()?;
    let tag = fetch_latest_version()?;
    let latest = tag.strip_prefix('v').unwrap_or(&tag);

    if latest == current {
        println!("Already up to date.");
        return Ok(());
    }

    println!("Updating to v{} ...", latest);

    let binary_name = format!("aish-{}", target);
    let download_url = format!(
        "https://github.com/{}/{}/releases/download/{}/{}",
        REPO_OWNER, REPO_NAME, tag, binary_name
    );
    let checksum_url = format!("{}.sha256", download_url);

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
        format!("Failed to fetch {}: {}", checksum_url, e)
    })?;
    let actual = compute_sha256(&tmpfile).map_err(|e| {
        let _ = std::fs::remove_file(&tmpfile);
        format!("Failed to compute checksum: {}", e)
    })?;
    if expected != actual {
        let _ = std::fs::remove_file(&tmpfile);
        return Err(format!(
            "Checksum mismatch.\n  expected: {}\n  actual:   {}",
            expected, actual
        )
        .into());
    }

    // Install to current executable path
    let exe_path = std::env::current_exe()?;
    let exe_path_str = exe_path.to_string_lossy();
    println!("Installing to {} ...", exe_path_str);

    // Set executable permission
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmpfile, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmpfile);
                format!("Failed to set permissions: {}", e)
            })?;
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
        return Err(format!("Failed to install binary: {}", e).into());
    }

    println!("Updated to v{}", latest);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hash_alone() {
        let h = "a".repeat(64);
        assert_eq!(parse_sha256_hash(&h).unwrap(), h);
    }

    #[test]
    fn parse_hash_with_filename() {
        let line =
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789  some-file";
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
}
