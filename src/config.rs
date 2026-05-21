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
    #[serde(default)]
    pub ai: AiConfig,
}

fn default_language() -> String {
    "Japanese".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct AiConfig {
    #[serde(default = "default_backend")]
    pub backend: String,
    /// 全バックエンド共通のモデル名。空なら指定なし。`--model` が優先。
    /// 解決後は選択されたバックエンドの extra_args に `--model <name>` を注入する。
    #[serde(default)]
    pub model: String,
    /// reasoning effort レベル。空なら指定なし。`--effort` が優先。
    /// claude → `--effort <level>`、codex → `-c model_reasoning_effort=<level>` に変換。
    /// gemini / qwen は該当 CLI フラグが無いので無視される。
    #[serde(default)]
    pub effort: String,
    /// 空なら Config.system_prompt にフォールバック (Config::load 内でマージ)
    #[serde(default)]
    pub system_prompt: String,
    /// 空なら Config.language にフォールバック
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub claude: ClaudeBackendConfig,
    #[serde(default)]
    pub codex: GenericBackendConfig,
    #[serde(default)]
    pub gemini: GenericBackendConfig,
    #[serde(default)]
    pub qwen: GenericBackendConfig,
    #[serde(default)]
    pub cursor: CursorBackendConfig,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            backend: default_backend(),
            model: String::new(),
            effort: String::new(),
            system_prompt: String::new(),
            language: String::new(),
            claude: ClaudeBackendConfig::default(),
            codex: GenericBackendConfig::default(),
            gemini: GenericBackendConfig::default(),
            qwen: GenericBackendConfig::default(),
            cursor: CursorBackendConfig::default(),
        }
    }
}

fn default_backend() -> String {
    "claude".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct ClaudeBackendConfig {
    #[serde(default = "default_disallowed_tools")]
    pub disallowed_tools: String,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

impl Default for ClaudeBackendConfig {
    fn default() -> Self {
        Self {
            disallowed_tools: default_disallowed_tools(),
            extra_args: Vec::new(),
        }
    }
}

fn default_disallowed_tools() -> String {
    "Bash,Edit,Write,Read".to_string()
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct GenericBackendConfig {
    #[serde(default)]
    pub extra_args: Vec<String>,
}

/// cursor-agent 用設定。`sandbox` は cursor-agent の `--sandbox <mode>` 値を
/// そのまま渡す (`on` / `off` 等)。`extra_args` に `--sandbox` を直接書いてもよいが、
/// 専用フィールドの方が typo しにくいので両方サポートする (extra_args が後勝ち)。
/// 未指定なら cursor-agent 既定に任せる。
#[derive(Debug, Deserialize, Default, Clone)]
pub struct CursorBackendConfig {
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// `--sandbox` に渡す値。空 / 未指定なら何も渡さない。
    #[serde(default)]
    pub sandbox: String,
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
    /// 旧ステータスバー / 旧バナー用の色。現状はバナーがバックエンド色を使うため未参照。
    /// 既存ユーザの設定ファイルが parse エラーにならないよう field は残す。
    #[allow(dead_code)]
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
    /// `config_path` が `Some` (ユーザが `--config` で明示) の場合、
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

        match toml::from_str::<Config>(&content) {
            Ok(mut config) => {
                config.merge_ai_fallbacks();
                Ok(config)
            }
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

    /// `[ai]` セクションが空のフィールドはトップレベル値で埋める。
    /// 既存ユーザの `system_prompt` / `language` を `[ai]` 不在でも引き継ぐための後方互換処理。
    fn merge_ai_fallbacks(&mut self) {
        if self.ai.system_prompt.is_empty() {
            self.ai.system_prompt = self.system_prompt.clone();
        }
        if self.ai.language.is_empty() {
            self.ai.language = self.language.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_fallback_picks_top_level_when_ai_section_missing() {
        let toml_str = r#"
system_prompt = "top-level prompt"
language = "English"
"#;
        let mut config: Config = toml::from_str(toml_str).unwrap();
        config.merge_ai_fallbacks();
        assert_eq!(config.ai.system_prompt, "top-level prompt");
        assert_eq!(config.ai.language, "English");
        assert_eq!(config.ai.backend, "claude");
    }

    #[test]
    fn ai_section_overrides_top_level() {
        let toml_str = r#"
system_prompt = "top-level prompt"
language = "English"

[ai]
backend = "codex"
system_prompt = "ai-section prompt"
language = "French"
"#;
        let mut config: Config = toml::from_str(toml_str).unwrap();
        config.merge_ai_fallbacks();
        assert_eq!(config.ai.system_prompt, "ai-section prompt");
        assert_eq!(config.ai.language, "French");
        assert_eq!(config.ai.backend, "codex");
    }

    #[test]
    fn ai_partial_override_falls_back_per_field() {
        let toml_str = r#"
system_prompt = "top-level prompt"
language = "English"

[ai]
backend = "gemini"
language = "Japanese"
"#;
        let mut config: Config = toml::from_str(toml_str).unwrap();
        config.merge_ai_fallbacks();
        // system_prompt was empty in [ai] so falls back
        assert_eq!(config.ai.system_prompt, "top-level prompt");
        assert_eq!(config.ai.language, "Japanese");
        assert_eq!(config.ai.backend, "gemini");
    }

    #[test]
    fn claude_disallowed_tools_default() {
        let cfg = ClaudeBackendConfig::default();
        assert_eq!(cfg.disallowed_tools, "Bash,Edit,Write,Read");
        assert!(cfg.extra_args.is_empty());
    }
}
