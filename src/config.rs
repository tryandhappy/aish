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
    #[serde(default = "default_shell_prefix_color")]
    pub shell_prefix_color: String,
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
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            shell_prefix_label: default_shell_prefix_label(),
            shell_prefix_color: default_shell_prefix_color(),
            header_color: default_header_color(),
            prompt_label: default_prompt_label(),
            prompt_color: default_prompt_color(),
            thinking_message: default_thinking_message(),
            thinking_color: default_thinking_color(),
            ai_color: default_ai_color(),
            input_color: String::new(),
            confirm_color: default_confirm_color(),
        }
    }
}

fn default_system_prompt() -> String {
    "あなたはLinuxサーバ管理の専門家です。SSHセッションの内容を把握しています。".to_string()
}

fn default_shell_prefix_label() -> String {
    "[aish]".to_string()
}

fn default_shell_prefix_color() -> String {
    "\x1b[38;5;216m".to_string()
}

fn default_header_color() -> String {
    "\x1b[38;5;245m".to_string()
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

impl Config {
    pub fn load(config_path: Option<&str>) -> Self {
        let path = match config_path {
            Some(p) => PathBuf::from(p),
            None => {
                let mut p = dirs::home_dir().unwrap_or_default();
                p.push(".aish");
                p.push("config.toml");
                p
            }
        };

        if path.exists() {
            match fs::read_to_string(&path) {
                Ok(content) => match toml::from_str(&content) {
                    Ok(config) => return config,
                    Err(e) => {
                        eprintln!("Warning: Failed to parse config file: {}", e);
                    }
                },
                Err(e) => {
                    eprintln!("Warning: Failed to read config file: {}", e);
                }
            }
        }

        Config::default()
    }
}
