#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Local,
    Remote,
    RemoteEnded,
}

impl Mode {
    pub fn accepts_shell_command(&self) -> bool {
        matches!(self, Mode::Local | Mode::Remote)
    }

    pub fn accepts_ai_prompt(&self) -> bool {
        true
    }

    pub fn accepts_exit(&self) -> bool {
        true
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mode::Local => write!(f, "local"),
            Mode::Remote => write!(f, "remote"),
            Mode::RemoteEnded => write!(f, "remote-ended"),
        }
    }
}
