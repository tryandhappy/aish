use regex::Regex;

/// The context sent to AI providers
#[derive(Clone, Debug)]
pub struct SessionContext {
    pub system_prompt: String,
    pub messages: Vec<ContextMessage>,
}

#[derive(Clone, Debug)]
pub enum ContextMessage {
    User { text: String },
    Assistant { text: String },
    CommandOutput { command: String, output: String },
}

/// AI response with optional command suggestions
#[derive(Debug)]
pub struct AiResponse {
    pub message: String,
    pub suggested_commands: Vec<SuggestedCommand>,
}

#[derive(Debug)]
pub struct SuggestedCommand {
    pub command: String,
    pub explanation: String,
}

/// Extract [COMMAND: ...] markers from AI response text
pub fn extract_commands(response_text: &str) -> (String, Vec<SuggestedCommand>) {
    let re = Regex::new(r"\[COMMAND:\s*(.+?)\]").unwrap();
    let mut commands = Vec::new();

    let cleaned = re.replace_all(response_text, |caps: &regex::Captures| {
        commands.push(SuggestedCommand {
            command: caps[1].trim().to_string(),
            explanation: String::new(),
        });
        format!(">>> {}", caps[1].trim())
    });

    (cleaned.to_string(), commands)
}
