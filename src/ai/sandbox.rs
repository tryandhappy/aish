use super::common::expand_tilde;
use super::types::BackendKind;
use crate::config::{AiConfig, SandboxConfig};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// `--share-net` 既定値。API 呼び出しのために共有する。
const DEFAULT_SHARE_NET: bool = true;
/// `home_root` 既定値。
const DEFAULT_HOME_ROOT: &str = "~/.aish/sandbox";

/// 解決済みのサンドボックスモード。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxMode {
    /// 隔離せず素直に exec。従来の aish 挙動。
    None,
    /// bwrap で `--unshare-all --share-net` 隔離して exec。
    Bwrap,
}

/// 解決済みサンドボックス仕様。
/// `resolve()` で `[ai.sandbox]` と `[ai.<backend>.sandbox]` をマージして作る。
#[derive(Debug, Clone)]
pub struct ResolvedSandbox {
    pub kind: BackendKind,
    pub mode: SandboxMode,
    /// AI ごとの HOME を作る親ディレクトリ (~ 展開済み)。
    pub home_root: PathBuf,
    pub share_net: bool,
    /// (src, dst) の rw bind 一覧 (~ 展開済み)。
    pub binds: Vec<(PathBuf, PathBuf)>,
    pub ro_binds: Vec<(PathBuf, PathBuf)>,
    pub setenv: HashMap<String, String>,
    pub unsetenv: Vec<String>,
    pub extra_bwrap_args: Vec<String>,
}

/// `AiConfig` から指定 backend のサンドボックス設定を解決する。
/// グローバル `[ai.sandbox]` と per-backend `[ai.<name>.sandbox]` をマージし、
/// `mode = "bwrap"` を Linux 以外で指定された場合はエラーを返す。
pub fn resolve(kind: BackendKind, cfg: &AiConfig) -> Result<ResolvedSandbox, String> {
    let per_backend = match kind {
        BackendKind::Claude => &cfg.claude.sandbox,
        BackendKind::Codex => &cfg.codex.sandbox,
        BackendKind::Gemini => &cfg.gemini.sandbox,
        BackendKind::Qwen => &cfg.qwen.sandbox,
    };
    let merged = SandboxConfig::merge_over(&cfg.sandbox, per_backend);

    let mode_str = merged.mode.as_deref().unwrap_or("none");
    let mode = match mode_str {
        "none" => SandboxMode::None,
        "bwrap" => {
            if !cfg!(target_os = "linux") {
                return Err(format!(
                    "sandbox.mode = \"bwrap\" is only supported on Linux. \
                     Use mode = \"none\" or run aish inside a Linux VM (Lima/OrbStack). \
                     (backend: {})",
                    kind.as_str()
                ));
            }
            SandboxMode::Bwrap
        }
        other => {
            return Err(format!(
                "unknown sandbox.mode `{other}` (expected: none|bwrap)"
            ));
        }
    };

    let home_root_raw = merged.home_root.as_deref().unwrap_or(DEFAULT_HOME_ROOT);
    let home_root = PathBuf::from(expand_tilde(home_root_raw));
    let share_net = merged.share_net.unwrap_or(DEFAULT_SHARE_NET);

    let binds = parse_bind_list(&merged.binds, "binds")?;
    let ro_binds = parse_bind_list(&merged.ro_binds, "ro_binds")?;

    Ok(ResolvedSandbox {
        kind,
        mode,
        home_root,
        share_net,
        binds,
        ro_binds,
        setenv: merged.setenv,
        unsetenv: merged.unsetenv,
        extra_bwrap_args: merged.extra_bwrap_args,
    })
}

fn parse_bind_list(items: &[String], field: &str) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let mut out = Vec::with_capacity(items.len());
    for entry in items {
        // "src:dst" 形式。dst は省略可で省略時は src と同じ。
        let (src, dst) = match entry.split_once(':') {
            Some((s, d)) if !d.is_empty() => (s, d),
            _ => (entry.as_str(), entry.as_str()),
        };
        if src.is_empty() {
            return Err(format!("empty src in sandbox.{field} entry: `{entry}`"));
        }
        out.push((
            PathBuf::from(expand_tilde(src)),
            PathBuf::from(expand_tilde(dst)),
        ));
    }
    Ok(out)
}

impl ResolvedSandbox {
    /// この backend 用のホスト側ディレクトリ (~/.aish/sandbox/<backend>/) を作る。
    /// 既存ならスキップ。mode == None なら何もしない。
    /// spawn の直前に呼ぶ。
    pub fn ensure_home_dir(&self) -> std::io::Result<PathBuf> {
        let dir = self.home_root.join(self.kind.as_str());
        if matches!(self.mode, SandboxMode::None) {
            // mode None でもパスは返すが、mkdir はしない。
            return Ok(dir);
        }
        fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        }
        Ok(dir)
    }

    /// `(program, args)` を sandbox 設定に応じて wrap した
    /// `(real_program, real_args)` を返す。
    /// `mode == None` なら入力をそのまま返す (アロケーション最小化)。
    pub fn wrap(&self, program: &str, args: &[String]) -> (String, Vec<String>) {
        match self.mode {
            SandboxMode::None => (program.to_string(), args.to_vec()),
            SandboxMode::Bwrap => self.wrap_bwrap(program, args),
        }
    }

    fn wrap_bwrap(&self, program: &str, args: &[String]) -> (String, Vec<String>) {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/root"));
        let home_str = home.to_string_lossy().to_string();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let cwd_str = cwd.to_string_lossy().to_string();
        let host_home_for_backend = self.home_root.join(self.kind.as_str());
        let sandbox_home_target = home.join(sandbox_home_subpath(self.kind));

        let mut bwrap_args: Vec<String> = Vec::new();
        let push = |v: &mut Vec<String>, s: &str| v.push(s.to_string());
        let bind_pair = |v: &mut Vec<String>, flag: &str, src: &Path, dst: &Path| {
            v.push(flag.to_string());
            v.push(src.to_string_lossy().into_owned());
            v.push(dst.to_string_lossy().into_owned());
        };

        // 隔離オプション
        push(&mut bwrap_args, "--unshare-all");
        if self.share_net {
            push(&mut bwrap_args, "--share-net");
        }
        push(&mut bwrap_args, "--die-with-parent");
        push(&mut bwrap_args, "--new-session");

        // システム必須 (merged-usr 前提)
        bind_pair(&mut bwrap_args, "--ro-bind", Path::new("/usr"), Path::new("/usr"));
        push(&mut bwrap_args, "--symlink");
        push(&mut bwrap_args, "usr/lib");
        push(&mut bwrap_args, "/lib");
        push(&mut bwrap_args, "--symlink");
        push(&mut bwrap_args, "usr/lib64");
        push(&mut bwrap_args, "/lib64");
        push(&mut bwrap_args, "--symlink");
        push(&mut bwrap_args, "usr/bin");
        push(&mut bwrap_args, "/bin");
        push(&mut bwrap_args, "--symlink");
        push(&mut bwrap_args, "usr/sbin");
        push(&mut bwrap_args, "/sbin");
        bind_pair(&mut bwrap_args, "--ro-bind", Path::new("/etc"), Path::new("/etc"));
        // systemd-resolved 環境向け (存在しなければ無視)
        bind_pair(
            &mut bwrap_args,
            "--ro-bind-try",
            Path::new("/run/systemd/resolve"),
            Path::new("/run/systemd/resolve"),
        );

        // 動的 FS
        push(&mut bwrap_args, "--proc");
        push(&mut bwrap_args, "/proc");
        push(&mut bwrap_args, "--dev");
        push(&mut bwrap_args, "/dev");
        push(&mut bwrap_args, "--tmpfs");
        push(&mut bwrap_args, "/tmp");
        // ホストの HOME を消してから必要部分だけ bind し直す
        push(&mut bwrap_args, "--tmpfs");
        push(&mut bwrap_args, &home_str);

        // AI 専用 HOME 配下 (ホスト側ディレクトリを sandbox 内の所定位置に被せる)
        bind_pair(
            &mut bwrap_args,
            "--bind",
            &host_home_for_backend,
            &sandbox_home_target,
        );

        // 現在の作業ディレクトリを bind (パスを一致させると AI の出力をそのまま使える)
        // 但し cwd が HOME 配下にあると上の --tmpfs $HOME で消えるので、その場合も再 bind で復活する。
        bind_pair(&mut bwrap_args, "--bind", &cwd, &cwd);
        push(&mut bwrap_args, "--chdir");
        push(&mut bwrap_args, &cwd_str);

        // ユーザ追加 bind
        for (src, dst) in &self.binds {
            bind_pair(&mut bwrap_args, "--bind", src, dst);
        }
        for (src, dst) in &self.ro_binds {
            bind_pair(&mut bwrap_args, "--ro-bind", src, dst);
        }

        // 環境変数
        push(&mut bwrap_args, "--setenv");
        push(&mut bwrap_args, "HOME");
        push(&mut bwrap_args, &home_str);
        if let Ok(path) = std::env::var("PATH") {
            push(&mut bwrap_args, "--setenv");
            push(&mut bwrap_args, "PATH");
            push(&mut bwrap_args, &path);
        }
        for (k, v) in &self.setenv {
            push(&mut bwrap_args, "--setenv");
            push(&mut bwrap_args, k);
            push(&mut bwrap_args, v);
        }
        // SSH agent forward は常に遮断 (隔離意義のため)
        push(&mut bwrap_args, "--unsetenv");
        push(&mut bwrap_args, "SSH_AUTH_SOCK");
        for k in &self.unsetenv {
            push(&mut bwrap_args, "--unsetenv");
            push(&mut bwrap_args, k);
        }

        // エスケープハッチ
        bwrap_args.extend(self.extra_bwrap_args.iter().cloned());

        // --
        push(&mut bwrap_args, "--");
        bwrap_args.push(program.to_string());
        bwrap_args.extend(args.iter().cloned());

        ("bwrap".to_string(), bwrap_args)
    }
}

/// sandbox 内で当該 backend の設定ディレクトリが置かれる HOME 相対パス。
/// 例: claude → ".claude"、gemini → ".config/gemini-cli"
fn sandbox_home_subpath(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Claude => ".claude",
        BackendKind::Codex => ".codex",
        // gemini-cli は ~/.config/gemini-cli を使う (CLI のバージョンによっては ~/.gemini)。
        // 一旦 .config/gemini-cli を採用し、必要なら ~/.gemini を ro_binds で追加する運用とする。
        BackendKind::Gemini => ".config/gemini-cli",
        BackendKind::Qwen => ".qwen",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config_with_global(mode: &str) -> AiConfig {
        let mut cfg = AiConfig::default();
        cfg.sandbox.mode = Some(mode.to_string());
        cfg
    }

    #[test]
    fn resolve_default_is_none() {
        let cfg = AiConfig::default();
        let r = resolve(BackendKind::Claude, &cfg).unwrap();
        assert_eq!(r.mode, SandboxMode::None);
        assert!(r.share_net);
    }

    #[test]
    fn resolve_per_backend_overrides_global() {
        let mut cfg = make_config_with_global("none");
        cfg.claude.sandbox.mode = Some("bwrap".to_string());
        let r_claude = resolve(BackendKind::Claude, &cfg);
        let r_codex = resolve(BackendKind::Codex, &cfg).unwrap();
        if cfg!(target_os = "linux") {
            assert_eq!(r_claude.unwrap().mode, SandboxMode::Bwrap);
        } else {
            assert!(r_claude.is_err()); // macOS 等では bwrap 不可
        }
        assert_eq!(r_codex.mode, SandboxMode::None);
    }

    #[test]
    fn resolve_unknown_mode_errors() {
        let mut cfg = AiConfig::default();
        cfg.sandbox.mode = Some("xyz".to_string());
        let err = resolve(BackendKind::Claude, &cfg).unwrap_err();
        assert!(err.contains("unknown sandbox.mode"));
    }

    #[test]
    fn parse_bind_list_supports_src_only() {
        let items = vec!["/foo".to_string()];
        let parsed = parse_bind_list(&items, "binds").unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, PathBuf::from("/foo"));
        assert_eq!(parsed[0].1, PathBuf::from("/foo"));
    }

    #[test]
    fn parse_bind_list_supports_src_colon_dst() {
        let items = vec!["/foo:/bar".to_string()];
        let parsed = parse_bind_list(&items, "binds").unwrap();
        assert_eq!(parsed[0].0, PathBuf::from("/foo"));
        assert_eq!(parsed[0].1, PathBuf::from("/bar"));
    }

    #[test]
    fn ensure_home_dir_returns_path_without_creating_when_mode_none() {
        let cfg = AiConfig::default();
        let r = resolve(BackendKind::Qwen, &cfg).unwrap();
        let p = r.ensure_home_dir().unwrap();
        assert_eq!(p.file_name().unwrap().to_str(), Some("qwen"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wrap_bwrap_emits_bwrap_program_and_terminator() {
        let mut cfg = AiConfig::default();
        cfg.sandbox.mode = Some("bwrap".to_string());
        let r = resolve(BackendKind::Claude, &cfg).unwrap();
        let (prog, args) = r.wrap("claude", &["-p".to_string()]);
        assert_eq!(prog, "bwrap");
        // -- の後に program と args が並ぶ
        let sep = args.iter().position(|s| s == "--").expect("-- 区切り");
        assert_eq!(args[sep + 1], "claude");
        assert_eq!(args[sep + 2], "-p");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wrap_bwrap_drops_ssh_auth_sock_by_default() {
        let mut cfg = AiConfig::default();
        cfg.sandbox.mode = Some("bwrap".to_string());
        let r = resolve(BackendKind::Claude, &cfg).unwrap();
        let (_, args) = r.wrap("claude", &[]);
        let pairs: Vec<&String> = args.iter().collect();
        // --unsetenv SSH_AUTH_SOCK が含まれる
        let pos = pairs
            .windows(2)
            .position(|w| w[0] == "--unsetenv" && w[1] == "SSH_AUTH_SOCK");
        assert!(pos.is_some(), "SSH_AUTH_SOCK should be unset by default");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wrap_bwrap_share_net_can_be_disabled() {
        let mut cfg = AiConfig::default();
        cfg.sandbox.mode = Some("bwrap".to_string());
        cfg.sandbox.share_net = Some(false);
        let r = resolve(BackendKind::Claude, &cfg).unwrap();
        let (_, args) = r.wrap("claude", &[]);
        assert!(!args.iter().any(|s| s == "--share-net"));
    }
}
