#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Local,
    Remote,
}

impl Mode {
    pub fn accepts_shell_command(&self) -> bool {
        true
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mode::Local => write!(f, "local"),
            Mode::Remote => write!(f, "remote"),
        }
    }
}
