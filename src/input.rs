use crate::ai::ProviderKind;

#[derive(Debug)]
pub enum UserInput {
    /// /claude, /chatgpt, /gemini -- switch active AI
    SwitchAi(ProviderKind),
    /// /? or /? <provider> or /? all -- ask for explanation
    Explain(ExplainTarget),
    /// !command -- execute directly on SSH
    DirectCommand(String),
    /// /quit or /exit
    Quit,
    /// /help
    Help,
    /// ssh user@host -- connect to SSH
    SshConnect(String),
    /// Everything else -- send to AI as a prompt
    AiPrompt(String),
}

#[derive(Debug)]
pub enum ExplainTarget {
    Active,
    Specific(ProviderKind),
    All,
}

pub fn parse_input(line: &str) -> UserInput {
    let trimmed = line.trim();

    if trimmed.is_empty() {
        return UserInput::AiPrompt(String::new());
    }

    // !command -- execute directly
    if let Some(cmd) = trimmed.strip_prefix('!') {
        return UserInput::DirectCommand(cmd.trim().to_string());
    }

    // /? -- explain
    if let Some(rest) = trimmed.strip_prefix("/?") {
        let arg = rest.trim();
        return match arg {
            "" => UserInput::Explain(ExplainTarget::Active),
            "all" => UserInput::Explain(ExplainTarget::All),
            other => match ProviderKind::from_name(other) {
                Some(kind) => UserInput::Explain(ExplainTarget::Specific(kind)),
                None => UserInput::Explain(ExplainTarget::Active),
            },
        };
    }

    // Slash commands
    match trimmed {
        "/claude" => UserInput::SwitchAi(ProviderKind::Claude),
        "/chatgpt" => UserInput::SwitchAi(ProviderKind::ChatGpt),
        "/gemini" => UserInput::SwitchAi(ProviderKind::Gemini),
        "/quit" | "/exit" => UserInput::Quit,
        "/help" => UserInput::Help,
        _ => {
            // ssh user@host
            if trimmed.starts_with("ssh ") {
                let target = trimmed[4..].trim().to_string();
                if !target.is_empty() {
                    return UserInput::SshConnect(target);
                }
            }
            UserInput::AiPrompt(trimmed.to_string())
        }
    }
}
