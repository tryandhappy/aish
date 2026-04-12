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

    // Download to temp file
    let tmp_output = Command::new("mktemp").output()?;
    let tmpfile = String::from_utf8(tmp_output.stdout)?.trim().to_string();

    let dl_status = Command::new("curl")
        .args(["-fSL", "-o", &tmpfile, &download_url])
        .status()?;
    if !dl_status.success() {
        let _ = std::fs::remove_file(&tmpfile);
        return Err("Failed to download binary".into());
    }

    // Install to current executable path
    let exe_path = std::env::current_exe()?;
    let exe_path_str = exe_path.to_string_lossy();
    println!("Installing to {} ...", exe_path_str);

    // Set executable permission
    let chmod = Command::new("chmod")
        .args(["755", &tmpfile])
        .status()?;
    if !chmod.success() {
        let _ = std::fs::remove_file(&tmpfile);
        return Err("Failed to set permissions".into());
    }

    // Replace current binary
    let result = std::fs::rename(&tmpfile, &exe_path)
        .or_else(|_| {
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
