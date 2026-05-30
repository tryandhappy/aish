use crate::config::ProviderRecipe;
use serde::Deserialize;
use std::fmt;
use std::sync::OnceLock;

pub struct AiRequest<'a> {
    pub terminal_context: &'a str,
    pub user_prompt: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct AiResponse {
    pub message: String,
    pub commands: Vec<String>,
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
                // native 予約語との衝突は config::validate_providers で先に reject されている。
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
    /// generic provider は flat namespace で扱う (prefix 不要)。`validate_providers` で
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
            BackendKind::Generic(_) => self.generic_meta().map(|m| m.binary).unwrap_or("?"),
        }
    }

    /// ring_buffer の sent_marks HashMap キーに使う。
    /// native は固定 0..=5、Generic は `6 + idx`。
    pub fn ordinal(self) -> usize {
        match self {
            BackendKind::Claude => 0,
            BackendKind::Codex => 1,
            BackendKind::Gemini => 2,
            BackendKind::Qwen => 3,
            BackendKind::Cursor => 4,
            BackendKind::Copilot => 5,
            BackendKind::Generic(idx) => 6 + idx as usize,
        }
    }

    /// native backend の総数 (Generic を含まない)。
    /// 旧 `[T; BackendKind::COUNT]` 固定長配列の値は `ring_buffer` の HashMap 化により不要。
    /// 残存利用は test の網羅性チェックのみ。
    pub const NATIVE_COUNT: usize = 6;

    /// native backend 全種類を列挙。Generic は含まない (init 時のみ既知のため別系統)。
    pub fn all_native() -> [BackendKind; Self::NATIVE_COUNT] {
        [
            BackendKind::Claude,
            BackendKind::Codex,
            BackendKind::Gemini,
            BackendKind::Qwen,
            BackendKind::Cursor,
            BackendKind::Copilot,
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
    fn parse_known() {
        assert_eq!(BackendKind::parse("claude").unwrap(), BackendKind::Claude);
        assert_eq!(BackendKind::parse("codex").unwrap(), BackendKind::Codex);
        assert_eq!(BackendKind::parse("gemini").unwrap(), BackendKind::Gemini);
        assert_eq!(BackendKind::parse("qwen").unwrap(), BackendKind::Qwen);
        assert_eq!(BackendKind::parse("cursor").unwrap(), BackendKind::Cursor);
        assert_eq!(BackendKind::parse("copilot").unwrap(), BackendKind::Copilot);
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
        ] {
            assert_eq!(BackendKind::parse(kind.as_str()).unwrap(), kind);
        }
    }

    #[test]
    fn binary_overrides_as_str_for_cursor() {
        assert_eq!(BackendKind::Cursor.binary(), "cursor-agent");
        assert_eq!(BackendKind::Claude.binary(), "claude");
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
        assert_eq!(BackendKind::Generic(0).ordinal(), 6);
        assert_eq!(BackendKind::Generic(7).ordinal(), 13);
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
