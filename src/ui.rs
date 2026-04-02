use std::io::{self, Read, Write};

const AI_COLOR_START: &str = "\x1b[46m";
const AI_COLOR_END: &str = "\x1b[0m";

pub fn print_ai_message(message: &str) {
    print!("{}{}{}\n", AI_COLOR_START, message, AI_COLOR_END);
    io::stdout().flush().ok();
}

pub fn print_ai_commands(commands: &[String]) {
    if commands.is_empty() {
        return;
    }
    print!(
        "{}Proposed commands:{}\n",
        AI_COLOR_START, AI_COLOR_END
    );
    for (i, cmd) in commands.iter().enumerate() {
        print!(
        "{}  {}: {}{}\n",
            AI_COLOR_START,
            i + 1,
            cmd,
            AI_COLOR_END
        );
    }
    io::stdout().flush().ok();
}

pub fn confirm_execution(commands: &[String]) -> bool {
    if commands.is_empty() {
        return false;
    }
    print_ai_commands(commands);
    print!("Execute? (Y/n) ");
    io::stdout().flush().ok();

    let mut buf = [0u8; 1];
    match io::stdin().read(&mut buf) {
        Ok(1) => {
            match buf[0] {
                b'\n' | b'Y' | b'y' => true,
                0x1b => {  // ESC
                    false
                }
                b'N' | b'n' => {
                    // consume trailing newline
                    let _ = io::stdin().read(&mut buf);
                    false
                }
                _ => {
                    let _ = io::stdin().read(&mut buf);
                    false
                }
            }
        }
        _ => false,
    }
}

pub enum UserInput {
    AiPrompt(String),
    ShellCommand(String),
    Exit,
}

pub fn parse_input(input: &str) -> UserInput {
    let trimmed = input.trim();

    if trimmed.eq_ignore_ascii_case("exit") {
        return UserInput::Exit;
    }

    if let Some(prompt) = trimmed.strip_prefix("@ai") {
        return UserInput::AiPrompt(prompt.trim().to_string());
    }

    if let Some(prompt) = trimmed.strip_prefix('?') {
        return UserInput::AiPrompt(prompt.trim().to_string());
    }

    UserInput::ShellCommand(input.to_string())
}

pub fn read_line() -> Option<String> {
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(0) => None, // EOF
        Ok(_) => Some(line.trim_end_matches('\n').trim_end_matches('\r').to_string()),
        Err(_) => None,
    }
}
