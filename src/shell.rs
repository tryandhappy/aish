use std::io::{self, Write};

use anyhow::Result;
use tokio::sync::mpsc;

use crate::ai::AiManager;
use crate::context::SessionContextManager;
use crate::input::{parse_input, ExplainTarget, UserInput};
use crate::ssh::SshSession;
use crate::ui;

pub struct Shell {
    ssh: SshSession,
    ai_manager: AiManager,
    context: SessionContextManager,
    ssh_target: String,
}

impl Shell {
    pub fn new(ssh: SshSession, ai_manager: AiManager, ssh_target: String) -> Self {
        let context = SessionContextManager::new(&ssh_target);
        Self {
            ssh,
            ai_manager,
            context,
            ssh_target,
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        ui::print_welcome(&self.ssh_target, self.ai_manager.active_kind());

        let mut ssh_rx = self.ssh.start_output_reader()?;

        loop {
            tokio::select! {
                // SSH output
                Some(output) = ssh_rx.recv() => {
                    print!("{}", output);
                    io::stdout().flush().ok();
                }
                // User input (blocking readline in spawn_blocking)
                input_result = self.read_input() => {
                    match input_result {
                        Ok(Some(line)) => {
                            if !self.handle_input(&line, &mut ssh_rx).await? {
                                break;
                            }
                        }
                        Ok(None) => break, // EOF
                        Err(e) => {
                            ui::print_error(&format!("入力エラー: {}", e));
                            break;
                        }
                    }
                }
            }
        }

        ui::print_status("aish を終了します。");
        Ok(())
    }

    async fn read_input(&self) -> Result<Option<String>> {
        let prompt = ui::prompt_string(self.ai_manager.active_provider().name());
        let result = tokio::task::spawn_blocking(move || {
            let mut rl = rustyline::DefaultEditor::new()?;
            match rl.readline(&prompt) {
                Ok(line) => Ok(Some(line)),
                Err(rustyline::error::ReadlineError::Eof) => Ok(None),
                Err(rustyline::error::ReadlineError::Interrupted) => Ok(None),
                Err(e) => Err(anyhow::anyhow!(e)),
            }
        })
        .await??;
        Ok(result)
    }

    async fn handle_input(&mut self, line: &str, ssh_rx: &mut mpsc::Receiver<String>) -> Result<bool> {
        let input = parse_input(line);

        match input {
            UserInput::Quit => return Ok(false),

            UserInput::Help => {
                ui::print_help();
            }

            UserInput::DirectCommand(cmd) => {
                self.ssh.send(&cmd)?;
                self.context.add_command_output(&cmd, "(実行中...)");
                // Wait briefly for output
                self.drain_ssh_output(ssh_rx, 500).await;
            }

            UserInput::SwitchAi(kind) => {
                match self.ai_manager.switch(kind) {
                    Ok(()) => {
                        ui::print_status(&format!(
                            "メインAIを {} に切り替えました。",
                            self.ai_manager.active_provider().name()
                        ));
                    }
                    Err(e) => {
                        ui::print_error(&e.to_string());
                    }
                }
            }

            UserInput::Explain(target) => {
                self.handle_explain(target).await?;
            }

            UserInput::SshConnect(target) => {
                ui::print_status(&format!("SSH接続はすでに {} に確立されています。", self.ssh_target));
                ui::print_status(&format!("別のホストに接続するには: !ssh {}", target));
            }

            UserInput::AiPrompt(prompt) => {
                if prompt.is_empty() {
                    return Ok(true);
                }
                self.handle_ai_prompt(&prompt, ssh_rx).await?;
            }
        }

        Ok(true)
    }

    async fn handle_ai_prompt(&mut self, prompt: &str, ssh_rx: &mut mpsc::Receiver<String>) -> Result<()> {
        // Include recent SSH output in context
        let recent = self.ssh.recent_output(20);
        if !recent.is_empty() {
            self.context.add_command_output("(最近のターミナル出力)", &recent);
        }

        self.context.add_user_prompt(prompt);
        let ctx = self.context.build_context();

        ui::print_status("AIに問い合わせ中...");

        match self.ai_manager.active_provider().send_message(&ctx).await {
            Ok(response) => {
                ui::print_ai_message(self.ai_manager.active_provider().name(), &response.message);
                self.context.add_ai_response(&response.message);

                // Handle suggested commands
                for (i, cmd) in response.suggested_commands.iter().enumerate() {
                    ui::print_suggested_command(&cmd.command, i);
                    if self.confirm_command(&cmd.command)? {
                        self.ssh.send(&cmd.command)?;
                        self.drain_ssh_output(ssh_rx, 1000).await;
                    }
                }
            }
            Err(e) => {
                ui::print_error(&format!("AI応答エラー: {}", e));
            }
        }

        Ok(())
    }

    async fn handle_explain(&mut self, target: ExplainTarget) -> Result<()> {
        let recent = self.ssh.recent_output(30);
        if !recent.is_empty() {
            self.context.add_command_output("(最近のターミナル出力)", &recent);
        }

        let explain_prompt = "直近のターミナル出力とコマンドについて解説してください。";
        self.context.add_user_prompt(explain_prompt);
        let ctx = self.context.build_context();

        ui::print_status("解説を取得中...");

        match target {
            ExplainTarget::Active => {
                match self.ai_manager.active_provider().send_message(&ctx).await {
                    Ok(resp) => {
                        ui::print_ai_message(self.ai_manager.active_provider().name(), &resp.message);
                        self.context.add_ai_response(&resp.message);
                    }
                    Err(e) => ui::print_error(&format!("AI応答エラー: {}", e)),
                }
            }
            ExplainTarget::Specific(kind) => {
                if let Some(provider) = self.ai_manager.get_provider(kind) {
                    match provider.send_message(&ctx).await {
                        Ok(resp) => {
                            ui::print_ai_message(provider.name(), &resp.message);
                            self.context.add_ai_response(&resp.message);
                        }
                        Err(e) => ui::print_error(&format!("AI応答エラー: {}", e)),
                    }
                } else {
                    ui::print_error(&format!("{} のAPIキーが設定されていません。", kind));
                }
            }
            ExplainTarget::All => {
                for kind in self.ai_manager.available_providers() {
                    if let Some(provider) = self.ai_manager.get_provider(kind) {
                        match provider.send_message(&ctx).await {
                            Ok(resp) => {
                                ui::print_ai_message(provider.name(), &resp.message);
                            }
                            Err(e) => {
                                ui::print_error(&format!("{} エラー: {}", provider.name(), e));
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn confirm_command(&self, command: &str) -> Result<bool> {
        print!("  コマンド '{}' を実行しますか? [y/N] ", command);
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        Ok(input.trim().to_lowercase() == "y")
    }

    async fn drain_ssh_output(&self, ssh_rx: &mut mpsc::Receiver<String>, timeout_ms: u64) {
        let deadline = tokio::time::sleep(tokio::time::Duration::from_millis(timeout_ms));
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                Some(output) = ssh_rx.recv() => {
                    print!("{}", output);
                    io::stdout().flush().ok();
                }
                () = &mut deadline => break,
            }
        }
    }
}
