mod claude;
mod codex;
mod common;
mod copilot;
mod cursor;
mod factory;
mod gemini;
mod generic;
mod qwen;
mod types;

pub use factory::{check_installed, create_backend};
pub use types::{AiBackend, AiError, AiRequest, BackendKind};
