/// 改行 (`\n`) と TAB (`\t`) 以外の制御文字 (CR/ESC/NUL/その他 C0/DEL/C1) を
/// 含まないことが検証済みの AI 提案コマンド。
///
/// 「画面で承認した物 = サーバで実行される物」(信頼の根幹) を型で運ぶための newtype。
/// 検証は `vet()` だけが行い、文字列は一切変形しない (`as_str()` は入力と同一スライスを
/// 返す)。`PtyHandler::send_approved_command` はこの型しか受け付けないため、
/// AI 提案経路で未検証文字列が PTY に届くコードパスは型レベルで存在しない。
///
/// 許可する制御文字とその根拠:
/// - `\n` (改行): heredoc / 複数行スクリプトを 1 提案として送るために必要。表示側
///   (`ui::print_ai_commands` / `print_single_confirm_prompt`) が `\n` を**実際の改行 +
///   字下げ**で全行描画するので「画面で見た全行 = 送信する全行」が保たれる (隠れた行は
///   作れない)。送信時は無変形でそのまま PTY に流れ、bash readline が各行を行として処理する。
/// - `\t` (TAB): heredoc / スクリプトのタブ字下げをそのまま通すために許可。TAB はカーソル
///   を次のタブストップへ進めるだけで、上移動・行消去はできないため確認画面の偽装に使えない。
///
/// 拒否し続ける制御文字とその根拠: `\r` で行頭復帰・`\x1b[2K` で行消去して確認画面の見た目を
/// 送信バイトとズラす偽装や、`\r` が bash に Enter として届いて承認外のコマンドが実行される
/// 事故が可能になるため、`\n`/`\t` 以外の制御文字を含むコマンドは承認 UI に載せる前に拒否する。
/// ユーザ手入力経路 (Line / passthrough raw) には適用しない (ユーザが自分で打った物の検閲は
/// 逆に透明性を壊す)。**この許可集合 (`\n`/`\t`) は表示側の「字下げ literal で描く / それ以外の
/// 制御文字は caret 化する」境界と一致させること** (`ui::visualize_command_segment`)。一致が
/// 崩れると「許可したが caret 表示される」または「拒否対象が literal で混ざる」ズレが生じる。
pub struct VettedCommand<'a>(&'a str);

impl<'a> VettedCommand<'a> {
    /// コマンドを検証する。`\n`/`\t` 以外の制御文字を含む場合は Err に元文字列を
    /// そのまま返す (拒否メッセージの可視化表示用)。
    pub fn vet(cmd: &'a str) -> Result<Self, &'a str> {
        if cmd
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
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
    fn vet_accepts_newline_and_tab() {
        // heredoc (改行を含む 1 コマンド) は許可する。
        assert!(VettedCommand::vet("cat > f << 'EOF'\nhello\nEOF").is_ok());
        // 複数行スクリプトのタブ字下げも許可する。
        assert!(VettedCommand::vet("if true; then\n\techo hi\nfi").is_ok());
        assert!(VettedCommand::vet("a\nb").is_ok());
        assert!(VettedCommand::vet("a\tb").is_ok());
    }

    #[test]
    fn vet_rejects_smuggling() {
        // CR で行頭復帰して見た目を偽装するパターン (改行を許可しても CR は拒否)
        assert!(VettedCommand::vet("git status\rrm -rf ~").is_err());
        // CR + 行消去エスケープで確認画面を上書きするパターン
        assert!(VettedCommand::vet("git status\r\x1b[2Krm -rf /tmp/x").is_err());
        // 改行を許可しても、行内に CR を紛れ込ませた多行コマンドは拒否する。
        assert!(VettedCommand::vet("echo a\nfoo\rrm -rf /\nEOF").is_err());
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
