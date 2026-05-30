use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};

pub struct PtyHandler {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    reader: Option<Box<dyn Read + Send>>,
}

impl PtyHandler {
    pub fn spawn_ssh(
        ssh_args: &[String],
        rows: u16,
        cols: u16,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut cmd = CommandBuilder::new("ssh");
        for arg in ssh_args {
            cmd.arg(arg);
        }
        // nested 起動検出用。ssh client (ローカルプロセス) は AISH_PID を継承するが、
        // SSH はデフォルトで環境変数をリモートに転送しないので remote shell 側はクリーン。
        cmd.env("AISH_PID", std::process::id().to_string());
        Self::spawn_command(cmd, rows, cols)
    }

    pub fn spawn_local_shell(rows: u16, cols: u16) -> Result<Self, Box<dyn std::error::Error>> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| {
            if cfg!(windows) {
                "cmd.exe".to_string()
            } else {
                "/bin/bash".to_string()
            }
        });
        let mut cmd = CommandBuilder::new(shell);
        if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(cwd);
        }
        // ローカル子シェル (とその子孫) で aish を再起動した場合に nested と判定するためのマーカー。
        cmd.env("AISH_PID", std::process::id().to_string());
        Self::spawn_command(cmd, rows, cols)
    }

    fn spawn_command(
        cmd: CommandBuilder,
        rows: u16,
        cols: u16,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let child = pair.slave.spawn_command(cmd)?;
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        Ok(Self {
            master: pair.master,
            writer,
            child,
            reader: Some(reader),
        })
    }

    pub fn take_reader(&mut self) -> Box<dyn Read + Send> {
        self.reader.take().expect("reader already taken")
    }

    pub fn write(&mut self, data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        self.writer.write_all(data)?;
        Ok(())
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), Box<dyn std::error::Error>> {
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }

    pub fn is_alive(&mut self) -> bool {
        self.child
            .try_wait()
            .map(|status| status.is_none())
            .unwrap_or(false)
    }
}
