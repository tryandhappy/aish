mod ai;
mod config;
mod context;
mod input;
mod shell;
mod ssh;
mod ui;

use anyhow::Result;
use clap::Parser;

use ai::AiManager;
use config::Config;
use shell::Shell;
use ssh::SshSession;

#[derive(Parser)]
#[command(name = "aish", version, about = "SSH + AI アシスタント CLI")]
struct Cli {
    /// SSH接続先 (例: user@host, user@host:port)
    target: String,

    /// 初期AI (claude, chatgpt, gemini)
    #[arg(long, default_value = "claude")]
    ai: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load()?;

    // Override default AI if specified
    let mut config = config;
    if cli.ai != "claude" {
        config.default_ai = Some(cli.ai);
    }

    let ai_manager = AiManager::new(&config)?;

    ui::print_status(&format!("SSH接続中: {} ...", cli.target));
    let ssh_session = SshSession::connect(&cli.target, &config)?;

    let mut shell = Shell::new(ssh_session, ai_manager, cli.target);
    shell.run().await?;

    Ok(())
}
