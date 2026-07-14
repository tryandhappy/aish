mod claude;
mod cloudflare_workers_ai;
mod codex;
mod common;
mod copilot;
mod cursor;
mod factory;
mod gemini;
mod generic;
mod nvidia_nim;
mod qwen;
mod types;

pub use factory::{auto_detect_backend, check_installed, create_backend, install_guide};
pub use types::{AiBackend, AiError, AiRequest, AiResponse, BackendKind};
