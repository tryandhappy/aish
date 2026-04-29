use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default = "default_system_prompt")]
    pub system_prompt: String,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default)]
    pub display: DisplayConfig,
    #[serde(default)]
    pub log: LogConfig,
}

fn default_language() -> String {
    "Japanese".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct LogConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_log_path")]
    pub path: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: default_log_path(),
        }
    }
}

fn default_log_path() -> String {
    "~/.aish/logs/claude-code.log".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct DisplayConfig {
    #[serde(default = "default_shell_prefix_label")]
    pub shell_prefix_label: String,
    #[serde(default = "default_header_color")]
    pub header_color: String,
    #[serde(default = "default_prompt_label")]
    pub prompt_label: String,
    #[serde(default = "default_prompt_color")]
    pub prompt_color: String,
    #[serde(default = "default_thinking_message")]
    pub thinking_message: String,
    #[serde(default = "default_thinking_color")]
    pub thinking_color: String,
    #[serde(default = "default_ai_color")]
    pub ai_color: String,
    #[serde(default)]
    pub input_color: String,
    #[serde(default = "default_confirm_color")]
    pub confirm_color: String,
    #[serde(default)]
    pub term_fg_color: String,
    #[serde(default)]
    pub term_bg_color: String,
    #[serde(default = "default_term_cursor_color")]
    pub term_cursor_color: String,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            shell_prefix_label: default_shell_prefix_label(),
            header_color: default_header_color(),
            prompt_label: default_prompt_label(),
            prompt_color: default_prompt_color(),
            thinking_message: default_thinking_message(),
            thinking_color: default_thinking_color(),
            ai_color: default_ai_color(),
            input_color: String::new(),
            confirm_color: default_confirm_color(),
            term_fg_color: String::new(),
            term_bg_color: String::new(),
            term_cursor_color: default_term_cursor_color(),
        }
    }
}

fn default_system_prompt() -> String {
    "あなたはLinuxサーバ管理の専門家です。SSHセッションの内容を把握しています。".to_string()
}

fn default_shell_prefix_label() -> String {
    "[aish]".to_string()
}

fn default_header_color() -> String {
    "\x1b[38;5;208m".to_string()
}

fn default_prompt_label() -> String {
    "[aish]".to_string()
}

fn default_prompt_color() -> String {
    "\x1b[38;5;208;48;2;50;35;20m".to_string()
}

fn default_thinking_message() -> String {
    "Thinking...".to_string()
}

fn default_thinking_color() -> String {
    "\x1b[38;5;208m".to_string()
}

fn default_ai_color() -> String {
    "\x1b[38;5;216m".to_string()
}

fn default_confirm_color() -> String {
    "\x1b[38;5;228;48;5;239m".to_string()
}

fn default_term_cursor_color() -> String {
    "#ff8800".to_string()
}

impl Config {
    /// 設定をロードする。
    /// `config_path` が `Some` (ユーザが `--aish-config` で明示) の場合、
    /// ファイル不在・読み取り失敗・パース失敗はエラーとして返す。
    /// `None` (デフォルトパス) の場合は読み取り/パース失敗時に警告を出して既定値で続行する。
    pub fn load(config_path: Option<&str>) -> Result<Self, String> {
        let (path, explicit) = match config_path {
            Some(p) => (PathBuf::from(p), true),
            None => {
                let mut p = dirs::home_dir().unwrap_or_default();
                p.push(".aish");
                p.push("config.toml");
                (p, false)
            }
        };

        if !path.exists() {
            if explicit {
                return Err(format!("Config file not found: {}", path.display()));
            }
            return Ok(Config::default());
        }

        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                if explicit {
                    return Err(format!(
                        "Failed to read config file {}: {}",
                        path.display(),
                        e
                    ));
                }
                eprintln!("Warning: Failed to read config file: {e}");
                return Ok(Config::default());
            }
        };

        match toml::from_str(&content) {
            Ok(config) => Ok(config),
            Err(e) => {
                if explicit {
                    Err(format!(
                        "Failed to parse config file {}: {}",
                        path.display(),
                        e
                    ))
                } else {
                    eprintln!("Warning: Failed to parse config file: {e}");
                    Ok(Config::default())
                }
            }
        }
    }
}
