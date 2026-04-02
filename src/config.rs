use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default = "default_system_prompt")]
    pub system_prompt: String,
}

fn default_system_prompt() -> String {
    "あなたはLinuxサーバ管理の専門家です。SSHセッションの内容を把握しています。".to_string()
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
