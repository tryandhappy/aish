use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default = "default_system_prompt")]
    pub system_prompt: String,
    #[serde(default)]
    pub display: DisplayConfig,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct DisplayConfig {
    #[serde(default = "default_prompt_label")]
    pub prompt_label: String,
    #[serde(default = "default_prompt_foreground")]
    pub prompt_foreground: String,
    #[serde(default)]
    pub prompt_background: String,
    #[serde(default = "default_thinking_message")]
    pub thinking_message: String,
    #[serde(default = "default_thinking_foreground")]
    pub thinking_foreground: String,
    #[serde(default)]
    pub thinking_background: String,
    #[serde(default)]
    pub ai_foreground: String,
    #[serde(default = "default_ai_background")]
    pub ai_background: String,
    #[serde(default = "default_input_background")]
    pub input_background: String,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            prompt_label: default_prompt_label(),
            prompt_foreground: default_prompt_foreground(),
            prompt_background: String::new(),
            thinking_message: default_thinking_message(),
            thinking_foreground: default_thinking_foreground(),
            thinking_background: String::new(),
            ai_foreground: String::new(),
            ai_background: default_ai_background(),
            input_background: default_input_background(),
        }
    }
}

fn default_system_prompt() -> String {
    "あなたはLinuxサーバ管理の専門家です。SSHセッションの内容を把握しています。".to_string()
}

fn default_prompt_label() -> String {
    "[aish]".to_string()
}

fn default_prompt_foreground() -> String {
    "\x1b[36m".to_string()
}

fn default_thinking_message() -> String {
    "Thinking...".to_string()
}

fn default_thinking_foreground() -> String {
    "\x1b[38;5;208m".to_string()
}

fn default_ai_background() -> String {
    "\x1b[48;5;238m".to_string()
}

fn default_input_background() -> String {
    "\x1b[43m".to_string()
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
