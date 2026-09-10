use crate::config::ProviderRecipe;
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::fmt;
use std::sync::OnceLock;

pub struct AiRequest<'a> {
    pub terminal_context: &'a str,
    pub user_prompt: &'a str,
}

/// AI が提案コマンドに付ける危険度分類。表示時に文字色へマップされる (`ui::risk_color`)。
/// 判定基準は `common::AI_RESPONSE_SCHEMA` の `risk` description と `build_system_prompt`
/// の応答ルールに **同一文言**で記述する (CLAUDE.md §15.10)。
///
/// - `Green`: サーバに影響を与えない ReadOnly。軽負荷なログ表示・ファイル検索。
/// - `Yellow`: 再起動等の一時的なサービス停止の可能性、または大量ログ/ファイル検索等の高負荷。
/// - `Orange`: サーバ設定の変更 (設定ファイル書き換え・config 変更)。
/// - `Red`: 不可逆 (ファイル削除・DB レコード削除・設定削除)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Risk {
    Green,
    /// 既定 (安全側)。risk 欠落 / 不明値 / 旧形式の裸文字列コマンドはここへ倒す。
    #[default]
    Yellow,
    Orange,
    Red,
}

impl Risk {
    /// 大小無視・前後空白無視で解釈。`green`/`orange`/`red` 以外 (yellow・未知・空) は
    /// 安全側の `Yellow`。**未知値で要素全体の deserialize を失敗させない** ための寛容変換。
    fn from_str_lenient(s: &str) -> Risk {
        match s.trim().to_ascii_lowercase().as_str() {
            "green" => Risk::Green,
            "orange" => Risk::Orange,
            "red" => Risk::Red,
            _ => Risk::Yellow,
        }
    }
}

impl<'de> Deserialize<'de> for Risk {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // null / 欠落は呼び出し側 (ProposedCommand) が default で吸収するので、
        // ここへ来るのは文字列のみ。未知文字列は Yellow。
        let s = String::deserialize(deserializer)?;
        Ok(Risk::from_str_lenient(&s))
    }
}

/// AI が提案する 1 コマンド + その危険度。
///
/// **後方互換**: JSON では「裸文字列」と「object」の両方を受理する (`Deserialize` 参照)。
/// - 裸文字列 `"ls"` → `{ command: "ls", risk: Yellow }`
/// - object `{ "command": "...", "risk": "Green" }`（未知キー = 旧 `explanation` 等は無視）
///
/// **信頼の根幹**: `command` は実行対象の文字列。`risk` は表示専用の metadata (確認画面の
/// 文字色) で、`VettedCommand` や PTY 送信バイトには一切含めない (`conversation::confirm_and_execute`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedCommand {
    pub command: String,
    pub risk: Risk,
}

impl<'de> Deserialize<'de> for ProposedCommand {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PcVisitor;

        impl<'de> Visitor<'de> for PcVisitor {
            type Value = ProposedCommand;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a command string or a {command, risk} object")
            }

            // 旧形式: 裸文字列 → Yellow。
            fn visit_str<E: de::Error>(self, v: &str) -> Result<ProposedCommand, E> {
                Ok(ProposedCommand {
                    command: v.to_string(),
                    risk: Risk::default(),
                })
            }

            fn visit_string<E: de::Error>(self, v: String) -> Result<ProposedCommand, E> {
                Ok(ProposedCommand {
                    command: v,
                    risk: Risk::default(),
                })
            }

            // 新形式: object。command のみ必須、risk は欠落可。未知キー (旧 explanation 等) は無視。
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<ProposedCommand, A::Error> {
                let mut command: Option<String> = None;
                let mut risk: Option<Risk> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "command" => command = Some(map.next_value()?),
                        // null も許容 (None → 既定 Yellow)。文字列は寛容変換。
                        "risk" => {
                            let s: Option<String> = map.next_value()?;
                            risk = Some(s.map(|v| Risk::from_str_lenient(&v)).unwrap_or_default());
                        }
                        _ => {
                            let _: de::IgnoredAny = map.next_value()?;
                        }
                    }
                }
                Ok(ProposedCommand {
                    command: command.ok_or_else(|| de::Error::missing_field("command"))?,
                    risk: risk.unwrap_or_default(),
                })
            }
        }

        deserializer.deserialize_any(PcVisitor)
    }
}

#[derive(Debug, Deserialize)]
pub struct AiResponse {
    pub message: String,
    /// 提案コマンド一覧。各要素は `ProposedCommand` (裸文字列も後方互換で受理)。
    pub commands: Vec<ProposedCommand>,
    /// コマンド実行後、その結果を AI へ自動問い合わせ (follow-up) するか。
    /// AI が「コマンドを教えるだけで出力確認は不要」と判断したら false。
    /// 欠落時は true (従来動作) — フラグを出さないモデル / lossy フォールバックでも
    /// 後方互換で調査ループが壊れない。
    #[serde(default = "default_command_result_followup")]
    pub command_result_followup: bool,
}

fn default_command_result_followup() -> bool {
    true
}

pub trait AiBackend: Send {
    /// バックエンド識別名 (診断・将来のステータス表示用)。
    #[allow(dead_code)]
    fn name(&self) -> &'static str;
    fn send(&mut self, req: &AiRequest) -> Result<AiResponse, AiError>;
    /// 設定から取得できるモデル名 (起動バナー表示用)。
    /// CLI に問い合わせず `extra_args` 等のローカル情報から判定するので、
    /// 取得できない backend は None を返す。
    fn model(&self) -> Option<String> {
        None
    }
    /// 現在の reasoning effort を返す (起動バナー / `/effort` 表示用)。
    fn effort(&self) -> Option<String> {
        None
    }
    /// runtime にモデルを差し替える (`/model <name>` 用)。
    /// 既存セッション (claude の session_id, codex/gemini/qwen の history) は維持する。
    fn set_model(&mut self, _model: Option<&str>) {}
    /// runtime に reasoning effort を差し替える (`/effort <level>` 用)。
    /// 該当 CLI フラグを持たない backend (gemini/qwen) は内部に保存するが
    /// 実際のリクエストには反映されない。
    fn set_effort(&mut self, _effort: Option<&str>) {}
    /// `/model` ピッカーに出す model 候補一覧 (config の static list / 取得コマンド /
    /// backend 組み込み既定を解決済み)。空なら候補なし。ヒント用途なので set はこの一覧に
    /// 縛られない。`/model` 呼び出し時にだけ評価される (取得コマンドはここで実行)。
    fn available_models(&self) -> Vec<String> {
        Vec::new()
    }
    /// `/effort` ピッカーに出す effort 候補一覧。詳細は `available_models` と同じ。
    fn available_efforts(&self) -> Vec<String> {
        Vec::new()
    }
    /// 会話履歴 / セッション ID をリセットする (`/clear` 用)。
    fn clear_history(&mut self) {}
    /// aish 終了時に表示する「このセッションを当該 CLI のインタラクティブモードで再開するための
    /// シェルコマンド例」。session_id を持たない / 永続化されていない場合は None。
    fn resume_command(&self) -> Option<String> {
        None
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum BackendKind {
    Claude,
    Codex,
    Gemini,
    Qwen,
    Cursor,
    Copilot,
    /// Cloudflare Workers AI (REST を curl 経由で叩く native backend)。
    Cloudflare,
    /// NVIDIA NIM (integrate.api.nvidia.com。REST を curl 経由で叩く native backend)。
    Nvidia,
    /// Google Antigravity CLI (`agy`。Gemini CLI 後継。system-prompt-only の native backend)。
    Antigravity,
    /// xAI Grok CLI (`grok`。x.ai/cli。system-prompt-only の native backend)。
    Grok,
    /// Config 駆動 generic CLI backend。`u8` は `[[ai.providers]]` 配列のインデックス。
    /// 実 metadata (name / binary / color / recipe) は `init_generics` で leak された
    /// `GENERIC_REGISTRY` から `generic_at(idx)` 経由で取得する。
    Generic(u8),
}

/// `BackendKind::Generic(u8)` から参照される generic backend metadata。
/// 起動時に一度だけ leak されて process 全期間生存するため、`&'static str` で扱える。
pub struct GenericMeta {
    pub recipe: &'static ProviderRecipe,
    /// `"generic:<name>"` 形式の表示名 (`as_str()` / parse 入力と一致)。leak 済み。
    pub display_name: &'static str,
    /// recipe.binary を leak したもの (`binary()` 経由)。
    pub binary: &'static str,
}

/// プロセス全期間で固定の generic backend テーブル。
/// `init_generics(&[ProviderRecipe])` を 1 度呼んで populate する。
/// 2 回目以降の set() は黙って無視される (テスト等での重複呼び出しに耐える)。
static GENERIC_REGISTRY: OnceLock<Vec<GenericMeta>> = OnceLock::new();

/// native backend 名のみを受け付ける純粋関数 (registry 非依存)。
/// `parse` の 1 段目で使う。
fn parse_native(s: &str) -> Result<BackendKind, ()> {
    match s {
        "claude" => Ok(BackendKind::Claude),
        "codex" => Ok(BackendKind::Codex),
        "gemini" => Ok(BackendKind::Gemini),
        "qwen" => Ok(BackendKind::Qwen),
        "cursor" => Ok(BackendKind::Cursor),
        "copilot" => Ok(BackendKind::Copilot),
        "cloudflare" => Ok(BackendKind::Cloudflare),
        "nvidia" => Ok(BackendKind::Nvidia),
        "antigravity" => Ok(BackendKind::Antigravity),
        "grok" => Ok(BackendKind::Grok),
        _ => Err(()),
    }
}

impl BackendKind {
    /// 起動時に一度だけ呼ぶ。`[[ai.providers]]` 各エントリの recipe / display_name / binary を
    /// `Box::leak` で `&'static str` 化してテーブルに格納する。
    /// 既に初期化済みなら何もしない (テストで複数回呼ばれても安全)。
    pub fn init_generics(providers: &[ProviderRecipe]) {
        let metas: Vec<GenericMeta> = providers
            .iter()
            .map(|p| {
                // display_name は recipe.name そのまま (flat namespace)。
                // native 予約語との衝突は config の resolve_providers (validate_recipes) で先に reject されている。
                let display = Box::leak(p.name.clone().into_boxed_str()) as &'static str;
                let binary = Box::leak(p.binary.clone().into_boxed_str()) as &'static str;
                let recipe = Box::leak(Box::new(p.clone())) as &'static ProviderRecipe;
                GenericMeta {
                    recipe,
                    display_name: display,
                    binary,
                }
            })
            .collect();
        let _ = GENERIC_REGISTRY.set(metas);
    }

    /// `BackendKind::Generic(idx)` の metadata を取得。
    /// init 前 / index 範囲外なら None。
    pub fn generic_meta(self) -> Option<&'static GenericMeta> {
        let BackendKind::Generic(idx) = self else {
            return None;
        };
        GENERIC_REGISTRY.get()?.get(idx as usize)
    }

    /// 文字列を BackendKind に解決する。
    ///
    /// 解決順:
    /// 1. native 6 種 (`"claude"` 等) の固定 match
    /// 2. `GENERIC_REGISTRY` の provider name と完全一致するか線形検索
    ///
    /// generic provider は flat namespace で扱う (prefix 不要)。`validate_recipes` で
    /// native 予約語との衝突は起動時に reject されるので、ここで両ステップが同じ文字列に
    /// マッチすることは無い (= native 優先で曖昧性は無い)。
    pub fn parse(s: &str) -> Result<Self, String> {
        if let Ok(k) = parse_native(s) {
            return Ok(k);
        }
        if let Some(reg) = GENERIC_REGISTRY.get() {
            if let Some(idx) = reg
                .iter()
                .position(|m| m.recipe.name == s)
                .and_then(|i| u8::try_from(i).ok())
            {
                return Ok(BackendKind::Generic(idx));
            }
        }
        // 不一致: 利用可能候補を一覧で示す。
        let mut available: Vec<String> = Self::all_native()
            .iter()
            .map(|k| k.as_str().to_string())
            .collect();
        if let Some(reg) = GENERIC_REGISTRY.get() {
            available.extend(reg.iter().map(|m| m.recipe.name.clone()));
        }
        Err(format!(
            "unknown backend `{s}` (available: {})",
            available.join(", ")
        ))
    }

    /// 表示名 (slash command 入力と round-trip する形式)。
    /// Generic は `"generic:<name>"`、native は `"claude"` 等。
    /// init 未完了の Generic は `"generic:?"` fallback。
    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Claude => "claude",
            BackendKind::Codex => "codex",
            BackendKind::Gemini => "gemini",
            BackendKind::Qwen => "qwen",
            BackendKind::Cursor => "cursor",
            BackendKind::Copilot => "copilot",
            BackendKind::Cloudflare => "cloudflare",
            BackendKind::Nvidia => "nvidia",
            BackendKind::Antigravity => "antigravity",
            BackendKind::Grok => "grok",
            BackendKind::Generic(_) => self.generic_meta().map(|m| m.display_name).unwrap_or("?"),
        }
    }

    /// 実行ファイル名 (`check_installed` / spawn 用)。
    /// Generic は recipe.binary。init 未完了 / 範囲外は `"?"` fallback (spawn は失敗する想定)。
    pub fn binary(self) -> &'static str {
        match self {
            BackendKind::Claude => "claude",
            BackendKind::Codex => "codex",
            BackendKind::Gemini => "gemini",
            BackendKind::Qwen => "qwen",
            BackendKind::Cursor => "cursor-agent",
            BackendKind::Copilot => "copilot",
            // Cloudflare / Nvidia backend は curl をサブプロセスで叩くので「実行ファイル」は curl。
            // check_installed が `curl --version` を見る (= 真の実行時依存)。
            BackendKind::Cloudflare => "curl",
            BackendKind::Nvidia => "curl",
            // agy / grok は実行ファイル名 = 呼び出し名。
            BackendKind::Antigravity => "agy",
            BackendKind::Grok => "grok",
            BackendKind::Generic(_) => self.generic_meta().map(|m| m.binary).unwrap_or("?"),
        }
    }

    /// ring_buffer の sent_marks HashMap キーに使う。
    /// native は固定 0..=9、Generic は `NATIVE_COUNT + idx`。
    pub fn ordinal(self) -> usize {
        match self {
            BackendKind::Claude => 0,
            BackendKind::Codex => 1,
            BackendKind::Gemini => 2,
            BackendKind::Qwen => 3,
            BackendKind::Cursor => 4,
            BackendKind::Copilot => 5,
            BackendKind::Cloudflare => 6,
            BackendKind::Nvidia => 7,
            BackendKind::Antigravity => 8,
            BackendKind::Grok => 9,
            BackendKind::Generic(idx) => Self::NATIVE_COUNT + idx as usize,
        }
    }

    /// native backend の総数 (Generic を含まない)。
    /// 旧 `[T; BackendKind::COUNT]` 固定長配列の値は `ring_buffer` の HashMap 化により不要。
    /// 残存利用は test の網羅性チェックのみ。
    pub const NATIVE_COUNT: usize = 10;

    /// native backend 全種類を列挙。Generic は含まない (init 時のみ既知のため別系統)。
    pub fn all_native() -> [BackendKind; Self::NATIVE_COUNT] {
        [
            BackendKind::Claude,
            BackendKind::Codex,
            BackendKind::Gemini,
            BackendKind::Qwen,
            BackendKind::Cursor,
            BackendKind::Copilot,
            BackendKind::Cloudflare,
            BackendKind::Nvidia,
            BackendKind::Antigravity,
            BackendKind::Grok,
        ]
    }

    /// 現在 registered な generic backend 全種類。init 前は空 Vec。
    pub fn all_generics() -> Vec<BackendKind> {
        GENERIC_REGISTRY
            .get()
            .map(|reg| {
                (0..reg.len())
                    .filter_map(|i| u8::try_from(i).ok())
                    .map(BackendKind::Generic)
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[derive(Debug)]
pub enum AiError {
    Cancelled,
    Spawn(std::io::Error),
    NonZeroExit {
        stderr: String,
    },
    EmptyOutput {
        stderr: String,
    },
    NoJson {
        raw: String,
    },
    ParseFailure {
        raw: String,
        source: serde_json::Error,
    },
    Other(String),
}

impl fmt::Display for AiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AiError::Cancelled => write!(f, "Cancelled"),
            AiError::Spawn(e) => write!(f, "failed to spawn AI CLI: {e}"),
            AiError::NonZeroExit { stderr } => write!(f, "AI CLI failed: {stderr}"),
            AiError::EmptyOutput { stderr } => {
                write!(f, "AI CLI returned empty output. stderr: {stderr}")
            }
            AiError::NoJson { raw } => write!(f, "No JSON found in AI CLI output: {raw}"),
            AiError::ParseFailure { raw, source } => {
                write!(f, "Failed to parse AI CLI output: {source}\nRaw: {raw}")
            }
            AiError::Other(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for AiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AiError::Spawn(e) => Some(e),
            AiError::ParseFailure { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<std::io::Error> for AiError {
    fn from(e: std::io::Error) -> Self {
        AiError::Spawn(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposed_command_deserializes_bare_string_backward_compat() {
        // 旧形式: commands が裸文字列配列 → Yellow で受理 (silent 消失しない)。
        let r: AiResponse =
            serde_json::from_str(r#"{"message":"m","commands":["ls -la","df -h"]}"#).unwrap();
        assert_eq!(r.commands.len(), 2);
        assert_eq!(r.commands[0].command, "ls -la");
        assert_eq!(r.commands[0].risk, Risk::Yellow);
        assert!(r.command_result_followup); // 欠落時 true
    }

    #[test]
    fn proposed_command_deserializes_object_form() {
        // 旧 explanation キーが混ざっていても無視して command/risk を取る (前方互換)。
        let r: AiResponse = serde_json::from_str(
            r#"{"message":"m","commands":[{"command":"rm -rf /tmp/x","explanation":"一時ファイル削除","risk":"Red"}],"command_result_followup":false}"#,
        )
        .unwrap();
        assert_eq!(r.commands[0].command, "rm -rf /tmp/x");
        assert_eq!(r.commands[0].risk, Risk::Red);
        assert!(!r.command_result_followup);
    }

    #[test]
    fn proposed_command_mixed_array_and_lenient_risk() {
        // 裸文字列と object の混在、大小無視・未知 risk→Yellow・risk 欠落→Yellow・null→Yellow。
        let r: AiResponse = serde_json::from_str(
            r#"{"message":"m","commands":[
                "uptime",
                {"command":"cat /etc/hosts","explanation":"閲覧","risk":"green"},
                {"command":"vi /etc/nginx.conf","explanation":"編集","risk":"ORANGE"},
                {"command":"foo","explanation":"e","risk":"weird"},
                {"command":"bar","explanation":"e"},
                {"command":"baz","explanation":"e","risk":null}
            ]}"#,
        )
        .unwrap();
        assert_eq!(r.commands[0].command, "uptime");
        assert_eq!(r.commands[0].risk, Risk::Yellow); // 裸文字列
        assert_eq!(r.commands[1].risk, Risk::Green); // "green"
        assert_eq!(r.commands[2].risk, Risk::Orange); // "ORANGE"
        assert_eq!(r.commands[3].risk, Risk::Yellow); // 未知→安全側
        assert_eq!(r.commands[4].risk, Risk::Yellow); // 欠落→既定
        assert_eq!(r.commands[5].risk, Risk::Yellow); // null→既定
    }

    #[test]
    fn risk_default_is_yellow() {
        assert_eq!(Risk::default(), Risk::Yellow);
    }

    #[test]
    fn parse_known() {
        assert_eq!(BackendKind::parse("claude").unwrap(), BackendKind::Claude);
        assert_eq!(BackendKind::parse("codex").unwrap(), BackendKind::Codex);
        assert_eq!(BackendKind::parse("gemini").unwrap(), BackendKind::Gemini);
        assert_eq!(BackendKind::parse("qwen").unwrap(), BackendKind::Qwen);
        assert_eq!(BackendKind::parse("cursor").unwrap(), BackendKind::Cursor);
        assert_eq!(BackendKind::parse("copilot").unwrap(), BackendKind::Copilot);
        assert_eq!(
            BackendKind::parse("cloudflare").unwrap(),
            BackendKind::Cloudflare
        );
        assert_eq!(BackendKind::parse("nvidia").unwrap(), BackendKind::Nvidia);
        assert_eq!(
            BackendKind::parse("antigravity").unwrap(),
            BackendKind::Antigravity
        );
        assert_eq!(BackendKind::parse("grok").unwrap(), BackendKind::Grok);
    }

    #[test]
    fn parse_unknown() {
        assert!(BackendKind::parse("xyz").is_err());
        assert!(BackendKind::parse("").is_err());
        assert!(BackendKind::parse("Claude").is_err()); // case-sensitive
    }

    #[test]
    fn as_str_roundtrip() {
        for kind in [
            BackendKind::Claude,
            BackendKind::Codex,
            BackendKind::Gemini,
            BackendKind::Qwen,
            BackendKind::Cursor,
            BackendKind::Copilot,
            BackendKind::Cloudflare,
            BackendKind::Nvidia,
            BackendKind::Antigravity,
            BackendKind::Grok,
        ] {
            assert_eq!(BackendKind::parse(kind.as_str()).unwrap(), kind);
        }
    }

    #[test]
    fn binary_overrides_as_str_for_cursor() {
        assert_eq!(BackendKind::Cursor.binary(), "cursor-agent");
        assert_eq!(BackendKind::Claude.binary(), "claude");
        // antigravity は呼び出し名 `antigravity` だが実行ファイルは `agy`。
        assert_eq!(BackendKind::Antigravity.as_str(), "antigravity");
        assert_eq!(BackendKind::Antigravity.binary(), "agy");
        // grok は呼び出し名 = 実行ファイル名。
        assert_eq!(BackendKind::Grok.binary(), "grok");
    }

    #[test]
    fn ordinals_are_unique_and_within_native_range() {
        let mut seen = [false; BackendKind::NATIVE_COUNT];
        for k in BackendKind::all_native() {
            let o = k.ordinal();
            assert!(
                o < BackendKind::NATIVE_COUNT,
                "native ordinal {o} out of range"
            );
            assert!(!seen[o], "duplicate ordinal {o}");
            seen[o] = true;
        }
        assert!(seen.iter().all(|&b| b));
    }

    #[test]
    fn generic_ordinal_starts_after_native() {
        // init 未呼び出しでも ordinal は計算可能 (registry を見ない)。
        assert_eq!(BackendKind::Generic(0).ordinal(), BackendKind::NATIVE_COUNT);
        assert_eq!(
            BackendKind::Generic(7).ordinal(),
            BackendKind::NATIVE_COUNT + 7
        );
    }

    #[test]
    fn parse_unknown_name_returns_err_with_available_list() {
        // 確実に存在しない名前 (UUID 風) なら native / registry のどちらにも hit しない。
        // OnceLock がプロセス共有なので registry が他テストで populate されている可能性に対応。
        let result = BackendKind::parse("nonexistent-xyz-7c3e9b1d-test-only");
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("unknown backend"), "unexpected err: {msg}");
        // メッセージには利用可能な native 名が並んでいる。
        assert!(msg.contains("claude"), "should list claude: {msg}");
    }

    #[test]
    fn parse_native_takes_priority_over_unset_registry() {
        // 既存 native 名は registry の状態に依存せず常に native として解決される。
        assert_eq!(BackendKind::parse("claude").unwrap(), BackendKind::Claude);
        assert_eq!(BackendKind::parse("copilot").unwrap(), BackendKind::Copilot);
    }

    #[test]
    fn cancelled_displays_as_cancelled() {
        assert_eq!(AiError::Cancelled.to_string(), "Cancelled");
    }
}
