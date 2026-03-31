use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use portable_pty::{native_pty_system, Child, CommandBuilder, PtySize};
use tokio::sync::mpsc;

use crate::config::Config;

pub struct SshSession {
    writer: Box<dyn Write + Send>,
    #[allow(dead_code)]
    child: Box<dyn Child + Send + Sync>,
    reader_source: Option<Box<dyn Read + Send>>,
    output_history: Arc<Mutex<Vec<String>>>,
}

impl SshSession {
    pub fn connect(target: &str, config: &Config) -> Result<Self> {
        let pty_system = native_pty_system();
        let pty_pair = pty_system.openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new("ssh");
        if let Some(ref identity) = config.ssh.identity_file {
            cmd.args(["-i", identity]);
        }
        if let Some(ref extra) = config.ssh.extra_args {
            for arg in extra {
                cmd.arg(arg);
            }
        }
        cmd.args(["-o", "StrictHostKeyChecking=accept-new"]);
        cmd.arg(target);

        let child = pty_pair.slave.spawn_command(cmd)?;
        drop(pty_pair.slave);

        let reader = pty_pair.master.try_clone_reader()?;
        let writer = pty_pair.master.take_writer()?;

        Ok(Self {
            writer,
            child,
            reader_source: Some(reader),
            output_history: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Send input to the SSH session
    pub fn send(&mut self, input: &str) -> Result<()> {
        write!(self.writer, "{}\n", input)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Send raw bytes (for things like Ctrl+C)
    #[allow(dead_code)]
    pub fn send_raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Start a background task that reads SSH output and sends it through a channel.
    /// Can only be called once (takes ownership of the reader).
    pub fn start_output_reader(&mut self) -> Result<mpsc::Receiver<String>> {
        let mut reader = self
            .reader_source
            .take()
            .ok_or_else(|| anyhow::anyhow!("Output reader already started"))?;

        let (tx, rx) = mpsc::channel::<String>(256);
        let history = self.output_history.clone();

        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&buf[..n]).to_string();

                        if let Ok(mut h) = history.lock() {
                            h.push(text.clone());
                            let len = h.len();
                            if len > 200 {
                                h.drain(..len - 200);
                            }
                        }

                        if tx.blocking_send(text).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(rx)
    }

    /// Get recent output for AI context
    pub fn recent_output(&self, n_lines: usize) -> String {
        let history = self.output_history.lock().unwrap();
        let start = history.len().saturating_sub(n_lines);
        history[start..].join("")
    }
}
