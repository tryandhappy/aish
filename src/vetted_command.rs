/// 制御文字 (改行/CR/ESC/NUL/TAB/その他 C0/DEL/C1) を含まないことが検証済みの
/// AI 提案コマンド。
///
/// 「画面で承認した物 = サーバで実行される物」(信頼の根幹) を型で運ぶための newtype。
/// 検証は `vet()` だけが行い、文字列は一切変形しない (`as_str()` は入力と同一スライスを
/// 返す)。`PtyHandler::send_approved_command` はこの型しか受け付けないため、
/// AI 提案経路で未検証文字列が PTY に届くコードパスは型レベルで存在しない。
///
/// 背景: 正当な単一行シェルコマンドに制御文字は不要。含まれている場合、`\r` で行頭復帰・
/// `\x1b[2K` で行消去して確認画面の見た目を送信バイトとズラす偽装や、`\r` が bash に
/// Enter として届いて 1 承認で複数コマンドが実行される事故が可能になるため、承認 UI に
/// 載せる前に拒否する。ユーザ手入力経路 (Line / passthrough raw) には適用しない
/// (ユーザが自分で打った物の検閲は逆に透明性を壊す)。
pub struct VettedCommand<'a>(&'a str);

impl<'a> VettedCommand<'a> {
    /// コマンドを検証する。制御文字を含む場合は Err に元文字列をそのまま返す
    /// (拒否メッセージの可視化表示用)。
    pub fn vet(cmd: &'a str) -> Result<Self, &'a str> {
        if cmd.chars().any(|c| c.is_control()) {
            Err(cmd)
        } else {
            Ok(Self(cmd))
        }
    }

    /// 検証済みコマンド文字列。vet() に渡した入力と同一のスライス (無変形)。
    pub fn as_str(&self) -> &'a str {
        self.0
    }
}

impl std::fmt::Display for VettedCommand<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vet_accepts_clean_commands() {
        assert!(VettedCommand::vet("ls -la").is_ok());
        assert!(VettedCommand::vet("echo 'hello world'").is_ok());
        // 非 ASCII の通常文字は制御文字ではないので通る。
        assert!(VettedCommand::vet("echo 'こんにちは'").is_ok());
    }

    #[test]
    fn vet_rejects_smuggling() {
        // CR で行頭復帰して見た目を偽装するパターン
        assert!(VettedCommand::vet("git status\rrm -rf ~").is_err());
        // CR + 行消去エスケープで確認画面を上書きするパターン
        assert!(VettedCommand::vet("git status\r\x1b[2Krm -rf /tmp/x").is_err());
        assert!(VettedCommand::vet("a\tb").is_err());
        assert!(VettedCommand::vet("a\nb").is_err());
        assert!(VettedCommand::vet("a\0b").is_err());
        assert!(VettedCommand::vet("echo \x1b[0m").is_err());
    }

    #[test]
    fn vet_err_returns_original_for_display() {
        let raw = "bad\rcmd";
        assert_eq!(VettedCommand::vet(raw).err(), Some(raw));
    }

    #[test]
    fn as_str_is_identity_slice() {
        // 「承認した文字列 = 送る文字列」: vet は変形せず、同一スライスを返す。
        let raw = "df -h";
        let vetted = VettedCommand::vet(raw).unwrap();
        assert!(std::ptr::eq(vetted.as_str(), raw));
    }
}
