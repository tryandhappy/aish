use crossterm::style::{Color, Stylize};

use crate::ai::ProviderKind;

pub fn print_welcome(target: &str, ai: ProviderKind) {
    println!(
        "{}",
        "╔══════════════════════════════════════════╗"
            .with(Color::Cyan)
    );
    println!(
        "{}",
        "║            aish - SSH + AI               ║"
            .with(Color::Cyan)
    );
    println!(
        "{}",
        "╚══════════════════════════════════════════╝"
            .with(Color::Cyan)
    );
    println!(
        "  接続先: {}  |  AI: {}",
        target.with(Color::Green),
        ai.to_string().with(Color::Yellow)
    );
    println!(
        "  {} でヘルプ表示",
        "/help".with(Color::DarkGrey)
    );
    println!();
}

pub fn print_help() {
    println!("{}", "=== aish コマンド一覧 ===".with(Color::Cyan));
    println!(
        "  {}        SSH上でコマンドを直接実行",
        "!<command>".with(Color::Green)
    );
    println!(
        "  {}          テキストをAIに送信",
        "<prompt>".with(Color::Green)
    );
    println!(
        "  {}          メインAIをClaudeに切替",
        "/claude".with(Color::Green)
    );
    println!(
        "  {}         メインAIをChatGPTに切替",
        "/chatgpt".with(Color::Green)
    );
    println!(
        "  {}          メインAIをGeminiに切替",
        "/gemini".with(Color::Green)
    );
    println!(
        "  {}               現在のAIに解説を依頼",
        "/?".with(Color::Green)
    );
    println!(
        "  {}         指定AIに解説を依頼",
        "/? <ai>".with(Color::Green)
    );
    println!(
        "  {}          全AIに解説を依頼",
        "/? all".with(Color::Green)
    );
    println!(
        "  {}            終了",
        "/quit".with(Color::Green)
    );
    println!(
        "  {}            ヘルプ表示",
        "/help".with(Color::Green)
    );
    println!();
}

pub fn print_ai_message(provider_name: &str, message: &str) {
    let header = format!("[{}]", provider_name);
    println!("{} {}", header.with(Color::Yellow), message);
}

pub fn print_suggested_command(command: &str, index: usize) {
    println!(
        "  {} {}",
        format!("提案コマンド {}:", index + 1).with(Color::Magenta),
        command.with(Color::White)
    );
}

pub fn print_status(msg: &str) {
    println!("{}", msg.with(Color::DarkGrey));
}

pub fn print_error(msg: &str) {
    println!("{} {}", "[エラー]".with(Color::Red), msg);
}

pub fn prompt_string(ai_name: &str) -> String {
    format!("aish({})> ", ai_name)
}
