use crate::vetted_command::VettedCommand;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// 子 ConPTY (conhost) が win32-input-mode (`\x1b[?9001h`) を要求しているか。
/// Windows では conhost が起動直後にほぼ必ず要求する。これが立った後は、
/// **素の ESC (0x1b) / 単独 `\r` を PTY に書いても入力として届かなくなる**ことがある:
/// パススルーが転送するユーザキーストローク (win32-input-mode シーケンス) を conhost の
/// VT 入力パーサが一度でも処理すると、以後の生 ESC / 単独 CR は握りつぶされる
/// (2026-08 ConPTY 実測 — SPEC.md § 15.13。文字列 + `\r` のコマンド送信は影響なし)。
/// そのため kill_line / refresh_prompt の合成キーは、このフラグが立っていれば
/// win32-input-mode シーケンスにエンコードして送る (完全なシーケンスは常に確実に届く)。
static WIN32_INPUT_MODE: AtomicBool = AtomicBool::new(false);
/// チャンク境界で `\x1b[?9001h` が分断された場合に備えた持ち越しバッファ (直近 8 byte)。
static SCAN_CARRY: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// PTY 出力チャンクから win32-input-mode の set/reset (`\x1b[?9001h` / `\x1b[?9001l`) を
/// 検出してフラグを更新する。全 PTY 出力の choke point (`pty_drain::drain_pty`) から呼ぶ。
pub fn note_pty_output(chunk: &[u8]) {
    let mut carry = SCAN_CARRY.lock().unwrap();
    if let Some(mode) = scan_win32_input_mode(&mut carry, chunk) {
        WIN32_INPUT_MODE.store(mode, Ordering::Relaxed);
    }
}

/// carry + chunk を走査し、最後に現れた `\x1b[?9001h`/`l` の状態を返す (純関数)。
/// carry は次回の走査用に「結合列の末尾 8 byte」へ更新される (トークン長 9 byte なので
/// 完全一致が carry 内で二重検出されることはない)。
fn scan_win32_input_mode(carry: &mut Vec<u8>, chunk: &[u8]) -> Option<bool> {
    const SET: &[u8] = b"\x1b[?9001h";
    const RESET: &[u8] = b"\x1b[?9001l";
    let mut combined = std::mem::take(carry);
    combined.extend_from_slice(chunk);
    let mut result = None;
    for i in 0..combined.len().saturating_sub(SET.len() - 1) {
        let w = &combined[i..i + SET.len()];
        if w == SET {
            result = Some(true);
        } else if w == RESET {
            result = Some(false);
        }
    }
    let keep = combined.len().saturating_sub(8);
    combined.drain(..keep);
    *carry = combined;
    result
}

/// win32-input-mode のキー 1 打鍵 (down + up) をエンコードする
/// (`\x1b[Vk;Sc;Uc;Kd;Cs;Rc_`。conhost の VT 入力パーサが INPUT_RECORD に復号する)。
fn win32_key_bytes(vk: u16, sc: u16, uc: u16) -> Vec<u8> {
    format!("\x1b[{vk};{sc};{uc};1;0;1_\x1b[{vk};{sc};{uc};0;0;1_").into_bytes()
}

/// 打ちかけ消去キーのバイト列 (win32-input-mode 有効時は ESC をエンコード)。純関数。
fn kill_line_bytes_for(win32_mode: bool) -> Vec<u8> {
    if cfg!(windows) && win32_mode {
        win32_key_bytes(27, 1, 27) // VK_ESCAPE / scancode 1
    } else {
        PtyHandler::KILL_LINE_BYTES.to_vec()
    }
}

/// Enter キーのバイト列 (win32-input-mode 有効時は CR をエンコード)。純関数。
/// 対象は refresh_prompt / 空 Enter 注入等の「単独 Enter」のみ。コマンド文字列に続く
/// Enter (`send_approved_command`) は文字列直後の `\r` が実測で常に届くため素のまま
/// (§ 15.13。変える必要が出たら実測してから)。
fn enter_bytes_for(win32_mode: bool) -> Vec<u8> {
    if cfg!(windows) && win32_mode {
        win32_key_bytes(13, 28, 13) // VK_RETURN / scancode 28
    } else {
        vec![PtyHandler::ENTER_BYTE]
    }
}

pub struct PtyHandler {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    reader: Option<Box<dyn Read + Send>>,
}

/// シェルのパス/名前が PowerShell 系 (powershell / pwsh) かを判定する。
/// 大小無視・`.exe` 有無・フルパス/ベース名のいずれも受理する
/// (例: `powershell.exe`, `pwsh`, `C:\Program Files\PowerShell\7\pwsh.exe`)。
fn is_powershell_shell(shell: &str) -> bool {
    // `/` と `\` の両方でベース名を取る (std::path は Unix で `\` を区切らないため、
    // Windows フルパスも判定できるよう手動分割する)。末尾 `.exe` は大小無視で剥がす。
    let base = shell.rsplit(['/', '\\']).next().unwrap_or(shell);
    let lower = base.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    stem == "powershell" || stem == "pwsh"
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
                // cmd.exe でなく PowerShell を既定にする: 既定プロンプト `PS C:\> ` は
                // 末尾空白があり PromptSniffer で無修正検出できる (cmd の `C:\>` は
                // 末尾空白が無く、cfg!(windows) 特例が要る。§ prompt_sniffer)。
                "powershell.exe".to_string()
            } else {
                "/bin/bash".to_string()
            }
        });
        let mut cmd = CommandBuilder::new(shell.clone());
        // PowerShell 系シェルは -NoLogo で起動ロゴ/著作権/更新通知を抑止する:
        // 付けないと子 powershell.exe が自分の起動バナーを再出力し、その行数ぶん
        // ビューポートがスクロールして aish の起動バナーが画面外へ押し出される (§ 15.8/15.13)。
        // プロンプト形 (`PS C:\...> `) は変わらないので PromptSniffer には影響しない。
        if is_powershell_shell(&shell) {
            cmd.arg("-NoLogo");
        }
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
        crate::debug_pty_write(data);
        self.writer.write_all(data)?;
        Ok(())
    }

    /// シェルの打ちかけ行を消去するバイト列。
    /// - Unix: bash readline の Ctrl+A + Ctrl+K (0x01,0x0b)。SIGINT を発火させないため
    ///   Ctrl+C (0x03) は使わない (vim/top 等の子プロセスを意図せず kill しないため)。
    ///   打ちかけは bash の kill-ring に退避するので Ctrl+Y で復元可能。
    /// - Windows: ESC (0x1b)。PSReadLine (Windows edit mode) では Ctrl+A=SelectAll 等で
    ///   kill-line にならず、**打ちかけ未消去のまま改行 = 未承認 submit の trust リスク**
    ///   になるため。ESC は cmd.exe / PSReadLine 共通の「入力行クリア」。
    ///   (Windows 実機での実効性検証はチェックリスト項目 — SPEC.md § 15.13)
    ///
    /// どちらも emacs 行編集以外 (vim 等の TUI、zsh vi モード) に届くとリテラルが残る /
    /// モード遷移する既知の穏当な劣化モード。
    const KILL_LINE_BYTES: &'static [u8] = if cfg!(windows) {
        &[0x1b]
    } else {
        &[0x01, 0x0b]
    };

    /// シェルへ送る「Enter」1 byte。
    /// - Unix: `\n` (LF)。bash 等の line discipline は LF を行確定として扱う。
    /// - Windows: `\r` (CR)。PSReadLine (win32-input-mode 経由) は bare `\n` を
    ///   Enter でなく複数行編集の改行挿入として扱い、`>> ` 継続プロンプトのまま
    ///   固まる (空行の `refresh_prompt` だけでなく、コマンド文字列 + `\n` の
    ///   `send_approved_command` でも同様。2026-07 実機報告 + portable_pty で
    ///   PowerShell に直接バイト列を送って再現・確認済み)。`\r` は即座に確定・実行される。
    const ENTER_BYTE: u8 = if cfg!(windows) { b'\r' } else { b'\n' };

    /// シェルの打ちかけ行を消去する。打ちかけが無ければ no-op。
    /// Windows で子 conhost が win32-input-mode を要求済みなら ESC を
    /// win32-input-mode シーケンスで送る (素の 0x1b はパススルー転送済みの
    /// キーイベント列の後だと握りつぶされるため — § 15.13)。
    pub fn kill_line(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let bytes = kill_line_bytes_for(WIN32_INPUT_MODE.load(Ordering::Relaxed));
        self.write(&bytes)
    }

    /// 打ちかけ消去 + 改行でシェルプロンプトを再表示させる。
    /// 消去せず改行だけ送ると、ユーザが Enter していない打ちかけコマンドを
    /// 勝手に実行してしまう (信頼の根幹: 承認していないコマンドを実行しない)。
    /// 2 送信を分離できないようメソッドに固定する。
    /// エラー処理は旧コード互換: 消去側の write エラーは無視し、改行側の Result を
    /// 返す (呼び出し側の `let _` / `?` 使い分けを保つため。消去側を `?` にしないこと)。
    pub fn refresh_prompt(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let win32 = WIN32_INPUT_MODE.load(Ordering::Relaxed);
        let _ = self.write(&kill_line_bytes_for(win32));
        // Windows (win32-input-mode 無効の fallback のみ): ESC と Enter (\r) が
        // 子 ConPTY に 1 バーストで届くと、VT 入力パーサが `ESC \r` を Alt+Enter として
        // 解釈し PSReadLine が両方を無視する (2026-08 実測)。書き込みを時間で分離し、
        // ESC を単独キーとして確定させてから Enter を送る。win32-input-mode 有効時は
        // 完全なシーケンス単位で届くため曖昧さが無く、分離は不要 (§ 15.13)。
        if cfg!(windows) && !win32 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        self.write(&enter_bytes_for(win32))
    }

    /// 空 Enter 1 打鍵を送る (ローカルシェル限定の ConPTY 座標合わせ注入用。
    /// 入力行が空なので何も実行されず履歴にも残らない)。win32-input-mode 有効時は
    /// エンコードして送る (kill_line と同じ理由)。リモートには送らないこと。
    pub fn send_empty_enter(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let bytes = enter_bytes_for(WIN32_INPUT_MODE.load(Ordering::Relaxed));
        self.write(&bytes)
    }

    /// ユーザが画面で承認した AI 提案コマンド + 改行を PTY に送る。
    /// `VettedCommand` (制御文字フリーが検証済みの型) しか受け付けないため、
    /// 未検証文字列が AI 提案経路から PTY に届くコードパスは型レベルで存在しない。
    /// コマンドはラップ・変形せずそのまま送る (信頼の根幹)。
    pub fn send_approved_command(
        &mut self,
        cmd: &VettedCommand<'_>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut bytes = cmd.as_str().as_bytes().to_vec();
        bytes.push(Self::ENTER_BYTE);
        self.write(&bytes)
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

#[cfg(test)]
mod tests {
    use super::*;
    // mpsc / Duration は下記 cfg(unix) 限定の実 PTY スモークだけで使う。
    // windows ターゲットでは当該ヘルパーが cfg で消えるので import も unix 限定にする
    // (さもないと windows clippy cross-check が unused import で落ちる)。
    #[cfg(unix)]
    use std::sync::mpsc;
    #[cfg(unix)]
    use std::time::Duration;

    #[test]
    fn kill_line_bytes_per_platform() {
        // 打ちかけ消去バイト列の固定: unix=Ctrl+A+Ctrl+K / windows=ESC。
        // (windows-latest ランナーでは windows 値が検証される)
        if cfg!(windows) {
            assert_eq!(PtyHandler::KILL_LINE_BYTES, &[0x1b]);
        } else {
            assert_eq!(PtyHandler::KILL_LINE_BYTES, &[0x01, 0x0b]);
        }
    }

    #[test]
    fn enter_byte_per_platform() {
        // Enter バイトの固定: unix=LF / windows=CR。
        // (windows-latest ランナーでは windows 値が検証される)
        if cfg!(windows) {
            assert_eq!(PtyHandler::ENTER_BYTE, b'\r');
        } else {
            assert_eq!(PtyHandler::ENTER_BYTE, b'\n');
        }
    }

    #[test]
    fn scan_win32_input_mode_detects_set_and_reset() {
        let mut carry = Vec::new();
        assert_eq!(scan_win32_input_mode(&mut carry, b"plain output"), None);
        assert_eq!(
            scan_win32_input_mode(&mut carry, b"\x1b[?9001h\x1b[?1004h"),
            Some(true)
        );
        assert_eq!(
            scan_win32_input_mode(&mut carry, b"\x1b[?9001l"),
            Some(false)
        );
        // set → reset が同一チャンクに来たら最後の状態を返す
        let mut carry2 = Vec::new();
        assert_eq!(
            scan_win32_input_mode(&mut carry2, b"\x1b[?9001h...\x1b[?9001l"),
            Some(false)
        );
    }

    #[test]
    fn scan_win32_input_mode_handles_chunk_split() {
        // トークンがチャンク境界で分断されても carry 経由で検出する
        let mut carry = Vec::new();
        assert_eq!(scan_win32_input_mode(&mut carry, b"xxx\x1b[?90"), None);
        assert_eq!(scan_win32_input_mode(&mut carry, b"01h yyy"), Some(true));
    }

    #[test]
    fn synthetic_key_bytes_per_mode() {
        // win32-input-mode 無効時は従来バイト列 (全プラットフォーム)
        assert_eq!(kill_line_bytes_for(false), PtyHandler::KILL_LINE_BYTES);
        assert_eq!(enter_bytes_for(false), vec![PtyHandler::ENTER_BYTE]);
        if cfg!(windows) {
            // 有効時は down+up の win32 シーケンス (素の ESC/CR は conhost に
            // 握りつぶされ得るため — § 15.13)
            assert_eq!(
                kill_line_bytes_for(true),
                b"\x1b[27;1;27;1;0;1_\x1b[27;1;27;0;0;1_".to_vec()
            );
            assert_eq!(
                enter_bytes_for(true),
                b"\x1b[13;28;13;1;0;1_\x1b[13;28;13;0;0;1_".to_vec()
            );
        } else {
            // Unix では win32-input-mode 自体が来ないが、来ても素のまま
            assert_eq!(kill_line_bytes_for(true), PtyHandler::KILL_LINE_BYTES);
            assert_eq!(enter_bytes_for(true), vec![PtyHandler::ENTER_BYTE]);
        }
    }

    #[test]
    fn is_powershell_shell_detects_variants() {
        // 大小無視・.exe 有無・フルパスいずれも PowerShell 系と判定する。
        for s in [
            "powershell.exe",
            "powershell",
            "PowerShell.exe",
            "pwsh",
            "pwsh.exe",
            r"C:\Program Files\PowerShell\7\pwsh.exe",
        ] {
            assert!(is_powershell_shell(s), "{s} should be powershell");
        }
        // cmd / bash 等には -NoLogo を付けない。
        for s in ["cmd.exe", "cmd", "/bin/bash", "bash", "zsh", "powershelly"] {
            assert!(!is_powershell_shell(s), "{s} should not be powershell");
        }
    }

    /// 実 PTY で /bin/sh を起動するスモーク (cfg(unix)。CI ランナーで動作)。
    /// reader は別スレッド + タイムアウトで、期待文字列が現れるまで読む。
    #[cfg(unix)]
    fn read_until(
        rx: &mpsc::Receiver<Vec<u8>>,
        needle: &str,
        timeout: Duration,
    ) -> Result<String, String> {
        let mut acc = String::new();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remain = deadline.saturating_duration_since(std::time::Instant::now());
            if remain.is_zero() {
                return Err(format!("timeout waiting for {needle:?}. got: {acc}"));
            }
            match rx.recv_timeout(remain) {
                Ok(chunk) => {
                    acc.push_str(&String::from_utf8_lossy(&chunk));
                    if acc.contains(needle) {
                        return Ok(acc);
                    }
                }
                Err(_) => return Err(format!("EOF/timeout waiting for {needle:?}. got: {acc}")),
            }
        }
    }

    #[cfg(unix)]
    fn spawn_sh() -> (PtyHandler, mpsc::Receiver<Vec<u8>>) {
        // SHELL 環境変数に依存しないよう明示 (テストの決定性)。
        std::env::set_var("SHELL", "/bin/sh");
        let mut pty = PtyHandler::spawn_local_shell(24, 80).expect("spawn sh");
        let mut reader = pty.take_reader();
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match std::io::Read::read(&mut reader, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        (pty, rx)
    }

    #[test]
    #[cfg(unix)]
    fn approved_command_roundtrip_and_aish_pid_env() {
        let (mut pty, rx) = spawn_sh();
        assert!(pty.is_alive());

        // 承認済みコマンド送信 → 出力が返る。
        let cmd = crate::vetted_command::VettedCommand::vet("echo aish-pty-$((20+22))")
            .expect("clean command");
        pty.send_approved_command(&cmd).unwrap();
        read_until(&rx, "aish-pty-42", Duration::from_secs(10)).unwrap();

        // 子シェル環境に nested 検出用 AISH_PID が注入されている。
        // needle は展開後の実値にする (入力エコー行の `$AISH_PID` に誤マッチしないため)。
        let cmd2 = crate::vetted_command::VettedCommand::vet("echo pid=$AISH_PID").unwrap();
        pty.send_approved_command(&cmd2).unwrap();
        let expect = format!("pid={}", std::process::id());
        read_until(&rx, &expect, Duration::from_secs(10)).unwrap();

        // resize はエラーにならない。
        pty.resize(30, 100).unwrap();

        // exit で終了し is_alive が false になる。
        pty.write(b"exit\n").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while pty.is_alive() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!pty.is_alive(), "shell did not exit");
    }
}
