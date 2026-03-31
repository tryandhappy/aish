use crate::ai::types::{ContextMessage, SessionContext};

const MAX_MESSAGES: usize = 50;

pub struct SessionContextManager {
    messages: Vec<ContextMessage>,
    ssh_target: String,
}

impl SessionContextManager {
    pub fn new(ssh_target: &str) -> Self {
        Self {
            messages: Vec::new(),
            ssh_target: ssh_target.to_string(),
        }
    }

    pub fn add_user_prompt(&mut self, text: &str) {
        self.messages.push(ContextMessage::User {
            text: text.to_string(),
        });
        self.trim();
    }

    pub fn add_ai_response(&mut self, text: &str) {
        self.messages.push(ContextMessage::Assistant {
            text: text.to_string(),
        });
        self.trim();
    }

    pub fn add_command_output(&mut self, command: &str, output: &str) {
        self.messages.push(ContextMessage::CommandOutput {
            command: command.to_string(),
            output: output.to_string(),
        });
        self.trim();
    }

    pub fn build_context(&self) -> SessionContext {
        SessionContext {
            system_prompt: self.build_system_prompt(),
            messages: self.messages.clone(),
        }
    }

    fn build_system_prompt(&self) -> String {
        format!(
            "あなたはSSH経由で {} に接続しているユーザーを支援するAIアシスタントです。\n\
             ターミナルの出力を読み取り、ユーザーのコマンド操作やトラブルシューティングを手助けします。\n\
             \n\
             サーバー上でコマンドを実行したい場合は、以下の形式で提案してください：\n\
             [COMMAND: <コマンド>]\n\
             \n\
             ユーザーが確認した後にのみコマンドが実行されます。\n\
             回答は簡潔に、わかりやすく日本語で答えてください。\n\
             /? で聞かれた場合は、直近のターミナル出力とコマンドの解説をしてください。",
            self.ssh_target
        )
    }

    fn trim(&mut self) {
        if self.messages.len() > MAX_MESSAGES {
            let excess = self.messages.len() - MAX_MESSAGES;
            self.messages.drain(..excess);
        }
    }
}
