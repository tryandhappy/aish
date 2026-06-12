use crate::vetted_command::VettedCommand;
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

    /// bash readline の打ちかけ行を消去する (Ctrl+A + Ctrl+K = 0x01,0x0b)。
    /// SIGINT を発火させないため Ctrl+C (0x03) は使わない (vim/top 等の子プロセスを
    /// 意図せず kill しないため)。打ちかけが無ければ no-op。打ちかけは bash の
    /// kill-ring に退避するので Ctrl+Y で復元可能。emacs 行編集以外 (vim 等の TUI、
    /// zsh vi モード) に届くと ^A^K がリテラルで残る既知の穏当な劣化モード。
    pub fn kill_line(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.write(&[0x01, 0x0b])
    }

    /// 打ちかけ消去 + 改行でシェルプロンプトを再表示させる。
    /// 消去せず `\n` だけ送ると、ユーザが Enter していない打ちかけコマンドを
    /// 勝手に実行してしまう (信頼の根幹: 承認していないコマンドを実行しない)。
    /// 2 送信を分離できないようメソッドに固定する。
    /// エラー処理は旧コード互換: 消去側の write エラーは無視し、改行側の Result を
    /// 返す (呼び出し側の `let _` / `?` 使い分けを保つため。消去側を `?` にしないこと)。
    pub fn refresh_prompt(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let _ = self.write(&[0x01, 0x0b]);
        self.write(b"\n")
    }

    /// ユーザが画面で承認した AI 提案コマンド + 改行を PTY に送る。
    /// `VettedCommand` (制御文字フリーが検証済みの型) しか受け付けないため、
    /// 未検証文字列が AI 提案経路から PTY に届くコードパスは型レベルで存在しない。
    /// コマンドはラップ・変形せずそのまま送る (信頼の根幹)。
    pub fn send_approved_command(
        &mut self,
        cmd: &VettedCommand<'_>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.write(format!("{}\n", cmd.as_str()).as_bytes())
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
