//! minibuffer (Ctrl+/ の AI プロンプト入力) の ↑↓ 履歴を `~/.aish/history` に永続化する。
//!
//! 信頼の根幹には非関与: PTY への書き込みはゼロで、ローカルファイル IO のみ。
//! 失敗 (読めない・書けない・パーミッション) は全て silent に無視する best-effort 機能
//! (`ai::common::write_log` と同方針)。
//!
//! フォーマットの不変条件 (§ 15.16): 1 行 1 エントリのプレーンテキスト。プロンプトは
//! 複数行 (Alt+Enter / bracketed paste) を含みうるので、`encode_entry` で
//! `\` → `\\` / 改行 → `\n` (2 文字) / `\r` → `\r` にエスケープしてから 1 行に畳む。
//! `decode_entry` は逆変換で、**決して失敗しない** (未知のエスケープは文字どおり残す)。
//! このエンコード規則は本ファイルの純関数だけが持ち、他所で書き換えない。

use crate::config::HistoryConfig;
use std::path::PathBuf;
use std::sync::OnceLock;

/// 解決済みの履歴ファイルパス。`None` = 履歴無効 (設定 off / パス解決不能)。
/// `init` が一度だけセットする。
static HISTORY_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// 1 エントリを 1 行に畳むためエスケープする。改行と `\` を安全化する。
pub fn encode_entry(entry: &str) -> String {
    let mut out = String::with_capacity(entry.len());
    for c in entry.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

/// `encode_entry` の逆変換。未知のエスケープは文字どおり残し、決して失敗しない。
pub fn decode_entry(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                // 未知のエスケープはバックスラッシュごと文字どおり残す。
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// ファイル内容をパースし、末尾 `max` 件だけを (古い順のまま) 返す。空行はスキップ。
pub fn parse_history(content: &str, max: usize) -> Vec<String> {
    let all: Vec<String> = content
        .lines()
        .filter(|l| !l.is_empty())
        .map(decode_entry)
        .collect();
    if max == 0 || all.len() <= max {
        all
    } else {
        all[all.len() - max..].to_vec()
    }
}

/// エントリ列を圧縮書き出し用のファイル内容にシリアライズする (末尾改行付き)。
pub fn serialize_history(entries: &[String]) -> String {
    let mut out = String::new();
    for e in entries {
        out.push_str(&encode_entry(e));
        out.push('\n');
    }
    out
}

/// ファイルの行数が上限の 2 倍を超えたら圧縮 (rewrite) する、という判定。
/// 起動時のみ行うので閾値は緩めにして rewrite 頻度を抑える。
pub fn needs_compaction(lines: usize, max: usize) -> bool {
    max > 0 && lines > max.saturating_mul(2)
}

/// 起動時に一度だけ呼ぶ。履歴を読み込んで (末尾 max 件) 返し、以後の `append` 用に
/// パスを記録する。無効時は空 Vec を返しパスを `None` にする。
///
/// `--config <path>` には連動しない (履歴パスは `[history].path` で独立指定。
/// `[log].path` と同じ方針)。ファイル読み込み失敗は空 Vec で silent。
pub fn init(cfg: &HistoryConfig) -> Vec<String> {
    if !cfg.enabled {
        let _ = HISTORY_PATH.set(None);
        return Vec::new();
    }
    let path = PathBuf::from(crate::ai::expand_tilde(&cfg.path));
    let content = std::fs::read(&path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let entries = parse_history(&content, cfg.max_entries);

    // ファイルが上限の 2 倍を超えていたら、起動時に一度だけ末尾 max 件へ圧縮する。
    // read → rename の窓で他プロセスの追記 1 件が落ちうるが、履歴の best-effort
    // 性質上許容する (§ 15.16)。
    let line_count = content.lines().filter(|l| !l.is_empty()).count();
    if needs_compaction(line_count, cfg.max_entries) {
        compact(&path, &entries);
    }

    let _ = HISTORY_PATH.set(Some(path));
    entries
}

/// 履歴を末尾 max 件に圧縮する: 同一ディレクトリに tmp を書いて rename で原子置換
/// (`update.rs` の原子置換と同パターン)。失敗は silent。
fn compact(path: &PathBuf, entries: &[String]) {
    let Some(parent) = path.parent() else { return };
    let _ = std::fs::create_dir_all(parent);
    let tmp = path.with_extension("tmp");
    if write_file_private(&tmp, serialize_history(entries).as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// 1 エントリを追記する。パス未設定 (無効) なら no-op。
/// 行全体を 1 回の `write` で書き、複数 aish プロセスの追記が行内で混ざるのを防ぐ。
/// 失敗は全て silent。
pub fn append(entry: &str) {
    let Some(Some(path)) = HISTORY_PATH.get() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut line = encode_entry(entry);
    line.push('\n');
    append_file_private(path, line.as_bytes());
}

/// 新規作成時のみ Unix パーミッション 0600 でファイルを作る (append)。
/// プロンプトにパスワード等が入りうるため。既存ファイルの perms は触らない (bash 同様)。
fn append_file_private(path: &PathBuf, bytes: &[u8]) {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(path) {
        let _ = f.write_all(bytes);
    }
}

/// 新規作成 (truncate) で 0600 のファイルを書く。圧縮の tmp 用。
fn write_file_private(path: &PathBuf, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip_plain() {
        let s = "ls -la /var/log";
        assert_eq!(decode_entry(&encode_entry(s)), s);
    }

    #[test]
    fn encode_decode_roundtrip_multiline() {
        let s = "cat <<EOF\nhello\nworld\nEOF";
        let enc = encode_entry(s);
        assert!(!enc.contains('\n'), "encoded entry must be single line");
        assert_eq!(decode_entry(&enc), s);
    }

    #[test]
    fn encode_decode_roundtrip_literal_backslash() {
        // ユーザが実際に "\\n" という 2 文字 (バックスラッシュ + n) を打った場合も
        // 改行と混同せず復元できること。
        let s = "echo 'a\\nb'";
        let enc = encode_entry(s);
        assert_eq!(decode_entry(&enc), s);
        // バックスラッシュ + 改行 の組み合わせ。
        let s2 = "line1\\\nline2";
        assert_eq!(decode_entry(&encode_entry(s2)), s2);
    }

    #[test]
    fn encode_decode_roundtrip_cr() {
        let s = "a\rb";
        assert_eq!(decode_entry(&encode_entry(s)), s);
    }

    #[test]
    fn decode_never_fails_on_unknown_escape() {
        // 未知のエスケープや末尾の裸バックスラッシュでも panic せず文字どおり残す。
        assert_eq!(decode_entry("a\\zb"), "a\\zb");
        assert_eq!(decode_entry("trailing\\"), "trailing\\");
    }

    #[test]
    fn parse_history_skips_blank_and_keeps_tail() {
        let content = "one\n\ntwo\nthree\n";
        assert_eq!(parse_history(content, 10), vec!["one", "two", "three"]);
        assert_eq!(parse_history(content, 2), vec!["two", "three"]);
    }

    #[test]
    fn parse_history_max_zero_returns_all() {
        let content = "a\nb\nc\n";
        assert_eq!(parse_history(content, 0), vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_history_decodes_multiline_entries() {
        let content = format!("{}\n", encode_entry("a\nb"));
        assert_eq!(parse_history(&content, 10), vec!["a\nb"]);
    }

    #[test]
    fn serialize_roundtrips_through_parse() {
        let entries = vec!["ls".to_string(), "cat <<E\nx\nE".to_string()];
        let content = serialize_history(&entries);
        assert_eq!(parse_history(&content, 10), entries);
    }

    #[test]
    fn needs_compaction_boundary() {
        assert!(!needs_compaction(2000, 1000)); // 丁度 2x は圧縮しない
        assert!(needs_compaction(2001, 1000));
        assert!(!needs_compaction(0, 1000));
        assert!(!needs_compaction(5, 0)); // max=0 は無制限扱い
    }

    #[cfg(unix)]
    #[test]
    fn append_then_read_roundtrip_with_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("aish-hist-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("history");
        let _ = std::fs::remove_file(&path);

        // HISTORY_PATH は OnceLock で 1 度しかセットできないため、init を通さず
        // append の下請け (append_file_private) を直接検証する。
        append_file_private(&path, format!("{}\n", encode_entry("first cmd")).as_bytes());
        append_file_private(
            &path,
            format!("{}\n", encode_entry("multi\nline")).as_bytes(),
        );

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            parse_history(&content, 10),
            vec!["first cmd", "multi\nline"]
        );

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "history file must be created 0600");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
