use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default = "default_system_prompt")]
    pub system_prompt: String,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default)]
    pub display: DisplayConfig,
    #[serde(default)]
    pub log: LogConfig,
    #[serde(default)]
    pub ai: AiConfig,
}

fn default_language() -> String {
    "Japanese".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct AiConfig {
    #[serde(default = "default_backend")]
    pub backend: String,
    /// 全バックエンド共通のモデル名。空なら指定なし。`--model` が優先。
    /// 解決後は選択されたバックエンドの extra_args に `--model <name>` を注入する。
    #[serde(default)]
    pub model: String,
    /// reasoning effort レベル。空なら指定なし。`--effort` が優先。
    /// claude → `--effort <level>`、codex → `-c model_reasoning_effort=<level>` に変換。
    /// gemini / qwen は該当 CLI フラグが無いので無視される。
    #[serde(default)]
    pub effort: String,
    /// 空なら Config.system_prompt にフォールバック (Config::load 内でマージ)
    #[serde(default)]
    pub system_prompt: String,
    /// 空なら Config.language にフォールバック
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub claude: ClaudeBackendConfig,
    #[serde(default)]
    pub codex: GenericBackendConfig,
    #[serde(default)]
    pub gemini: GenericBackendConfig,
    #[serde(default)]
    pub qwen: GenericBackendConfig,
    #[serde(default)]
    pub cursor: CursorBackendConfig,
    #[serde(default)]
    pub copilot: CopilotBackendConfig,
    /// Google Antigravity CLI (`agy`) backend 設定 (extra_args + `/model` `/effort` 候補)。
    #[serde(default)]
    pub antigravity: GenericBackendConfig,
    /// xAI Grok CLI (`grok`) backend 設定 (extra_args + `/model` 候補。effort フラグ無し)。
    #[serde(default)]
    pub grok: GenericBackendConfig,
    /// Cloudflare Workers AI backend 設定。account/token は環境変数のみ (config 不可) なので
    /// ここは `/model` 候補リスト (OptionLists) のみ。TOML セクション名はハイフン形
    /// `[ai.cloudflare-workers-ai]` (呼び出し名 `cloudflare` とは住み分け)。
    #[serde(default, rename = "cloudflare-workers-ai")]
    pub cloudflare_workers_ai: CloudflareWorkersAiBackendConfig,
    /// NVIDIA NIM backend 設定。API key は環境変数のみ (config 不可) なので
    /// ここは `/model` 候補リスト (OptionLists) のみ。TOML セクション名はハイフン形
    /// `[ai.nvidia-nim]` (呼び出し名 `nvidia` とは住み分け)。
    #[serde(default, rename = "nvidia-nim")]
    pub nvidia_nim: NvidiaNimBackendConfig,
    /// `[[ai.providers]]` 配列。ユーザが書いた **上書き / 追加** エントリ。
    /// 同名の組み込みデフォルト recipe があれば「書いたフィールドだけ」上書きし、
    /// 無ければ新規 generic backend として追加する (`resolve_providers`)。
    /// presence 判定のため各フィールドは `Option` (= `ProviderOverride`)。
    #[serde(default)]
    pub providers: Vec<ProviderOverride>,
    /// 組み込みデフォルト recipe + `providers` をマージ・検証した最終 recipe 一覧。
    /// `Config::load` (or `resolve_providers`) で確定し、`init_generics` に渡る。
    /// TOML には現れない (serde skip)。配列インデックスは `BackendKind::Generic(u8)`
    /// に詰める都合上 0..=255 まで。
    #[serde(skip)]
    pub resolved_providers: Vec<ProviderRecipe>,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            backend: default_backend(),
            model: String::new(),
            effort: String::new(),
            system_prompt: String::new(),
            language: String::new(),
            claude: ClaudeBackendConfig::default(),
            codex: GenericBackendConfig::default(),
            gemini: GenericBackendConfig::default(),
            qwen: GenericBackendConfig::default(),
            cursor: CursorBackendConfig::default(),
            copilot: CopilotBackendConfig::default(),
            antigravity: GenericBackendConfig::default(),
            grok: GenericBackendConfig::default(),
            cloudflare_workers_ai: CloudflareWorkersAiBackendConfig::default(),
            nvidia_nim: NvidiaNimBackendConfig::default(),
            providers: Vec::new(),
            resolved_providers: Vec::new(),
        }
    }
}

fn default_backend() -> String {
    "claude".to_string()
}

/// `/model` `/effort` の対話ピッカーに出す候補リスト設定。各 backend 設定に
/// `#[serde(flatten)]` で埋め込む。候補解決の優先順位は
/// **static list (`*s`) > 取得コマンド (`*_command`) > backend 組み込み既定**
/// (`crate::ai::common::resolve_option_list`)。ヒント用途なので検証はしない。
#[derive(Debug, Deserialize, Default, Clone)]
pub struct OptionLists {
    /// model 候補 (static)。空なら `models_command` / 組み込み既定にフォールバック。
    #[serde(default)]
    pub models: Vec<String>,
    /// model 候補を動的取得するコマンド (ローカルで `sh -c`、stdout を 1 行 1 候補で解釈)。
    #[serde(default)]
    pub models_command: String,
    /// effort 候補 (static)。空なら `efforts_command` / 組み込み既定にフォールバック。
    #[serde(default)]
    pub efforts: Vec<String>,
    /// effort 候補を動的取得するコマンド。
    #[serde(default)]
    pub efforts_command: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ClaudeBackendConfig {
    #[serde(default = "default_disallowed_tools")]
    pub disallowed_tools: String,
    /// true のとき mandatory baseline (Bash/Edit/Write の常時 deny) の強制を解除し、
    /// `disallowed_tools` を verbatim で claude に渡す。AI 自身に shell 実行 / ファイル
    /// 編集を許す危険設定なので、明示的に opt-in した上級者向け。既定 (false) では
    /// `disallowed_tools` を空にしても Bash/Edit/Write は必ず deny される。
    #[serde(default)]
    pub allow_unsafe_tools: bool,
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(flatten)]
    pub options: OptionLists,
}

impl Default for ClaudeBackendConfig {
    fn default() -> Self {
        Self {
            disallowed_tools: default_disallowed_tools(),
            allow_unsafe_tools: false,
            extra_args: Vec::new(),
            options: OptionLists::default(),
        }
    }
}

fn default_disallowed_tools() -> String {
    "Bash,Edit,Write,Read".to_string()
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct GenericBackendConfig {
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(flatten)]
    pub options: OptionLists,
}

/// Cloudflare Workers AI backend 設定。
/// account_id / api_token は環境変数 (`CLOUDFLARE_ACCOUNT_ID` / `CLOUDFLARE_API_TOKEN`) 専用で
/// config には置かない (config.toml は平文なのでトークン漏洩を避ける)。よって `/model`
/// 候補リスト (OptionLists) のみを持つ。
#[derive(Debug, Deserialize, Default, Clone)]
pub struct CloudflareWorkersAiBackendConfig {
    #[serde(flatten)]
    pub options: OptionLists,
}

/// NVIDIA NIM 用設定。API key は環境変数 `NVIDIA_API_KEY` のみ (config 不可)。
#[derive(Debug, Deserialize, Default, Clone)]
pub struct NvidiaNimBackendConfig {
    #[serde(flatten)]
    pub options: OptionLists,
}

/// cursor-agent 用設定。
/// - `mode`: `--mode <value>` の値 (`"plan"` / `"ask"`)。aish 用途では `"plan"` (read-only / propose-only) が推奨。
/// - `sandbox`: `--sandbox <value>` の値 (`"enabled"` / `"disabled"`)。defense-in-depth。
/// - `extra_args`: その他追加引数 (`--sandbox` 等を直接書いてもよい; 重複時は CLI の挙動次第)。
///
/// なお `--trust` は cursor-agent の headless モードで必須なので config からは指定不可で常に付与される。
#[derive(Debug, Deserialize, Clone)]
pub struct CursorBackendConfig {
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// `--mode` に渡す値。空 / 未指定なら何も渡さない (= 通常モード)。aish では `"plan"` 推奨。
    #[serde(default = "default_cursor_mode")]
    pub mode: String,
    /// `--sandbox` に渡す値。空 / 未指定なら何も渡さない。
    #[serde(default)]
    pub sandbox: String,
    #[serde(flatten)]
    pub options: OptionLists,
}

impl Default for CursorBackendConfig {
    fn default() -> Self {
        Self {
            extra_args: Vec::new(),
            mode: default_cursor_mode(),
            sandbox: String::new(),
            options: OptionLists::default(),
        }
    }
}

fn default_cursor_mode() -> String {
    // plan = read-only / propose-only。aish の「提案のみ」セマンティクスと合致する安全側既定。
    "plan".to_string()
}

/// copilot CLI 用設定。
/// - `mode`: `--mode <value>` の値 (`"plan"` / `"interactive"` / `"autopilot"`)。aish 用途では `"plan"` 推奨。
/// - `extra_args`: その他追加引数。
///
/// なお以下は cursor と同様 aish が常に自動付与するので config から指定できない:
/// - `--output-format json` (JSONL 出力)
/// - `--allow-all-tools` (非対話モードで必須)
/// - `--deny-tool=shell` / `--deny-tool=write` (信頼の根幹: shell 実行と書き込みを完全拒否)
/// - `--no-ask-user` (会話は aish が仕切る)
#[derive(Debug, Deserialize, Clone)]
pub struct CopilotBackendConfig {
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// `--mode` に渡す値。空 / 未指定なら何も渡さない (= interactive 既定)。aish では `"plan"` 推奨。
    #[serde(default = "default_copilot_mode")]
    pub mode: String,
    #[serde(flatten)]
    pub options: OptionLists,
}

impl Default for CopilotBackendConfig {
    fn default() -> Self {
        Self {
            extra_args: Vec::new(),
            mode: default_copilot_mode(),
            options: OptionLists::default(),
        }
    }
}

fn default_copilot_mode() -> String {
    "plan".to_string()
}

/// Config 駆動 generic CLI backend の解決済みレシピ (内部表現)。
/// 組み込みデフォルト (`builtin_providers`) と、ユーザの `[[ai.providers]]`
/// (`ProviderOverride`) をマージした結果がこの型 (`resolve_providers`)。
///
/// 必須フィールド: `name`, `binary`。それ以外は default あり。
/// `parse` / `prompt_delivery` は文字列 enum 形式で、不正値は起動時に検出する
/// (`validate_recipes` で実装)。
#[derive(Debug, Deserialize, Clone)]
pub struct ProviderRecipe {
    /// `/ai generic:<name>` で参照される識別子。providers 内で一意。
    pub name: String,
    /// 実行ファイル名 (PATH 検索) または絶対パス。
    pub binary: String,
    /// 固定引数。aish が動的引数 (`--model`, `--resume <sid>` 等) を後ろに追加する。
    #[serde(default)]
    pub args: Vec<String>,
    /// 子プロセスに追加する環境変数 (TOML では `env = { KEY = "value" }`)。
    /// CLI フラグで表現できない設定 (例: opencode の `OPENCODE_CONFIG_CONTENT`) 用。
    /// 上書き時は `args` と同じく丸ごと置換 (キー単位マージはしない)。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// `"stdin"` (推奨) | `"arg"` (最後に positional 追加) | `"flag"` (`prompt_flag` の値として渡す)。
    #[serde(default = "default_prompt_delivery")]
    pub prompt_delivery: String,
    /// `prompt_delivery = "flag"` のときの prompt フラグ名 (例 `"-p"`)。
    #[serde(default)]
    pub prompt_flag: String,
    /// `"lossy"` | `"extract_json"` | `"jsonl"`。
    #[serde(default = "default_parse")]
    pub parse: String,
    /// `parse = "jsonl"` のとき、最終応答テキストを取り出す `"type:dot.path"` 形式の指定。
    /// 例: `"assistant.message:data.content"`
    #[serde(default)]
    pub jsonl_content_path: String,
    /// `parse = "jsonl"` のとき、session_id を取り出す `"type:dot.path"` 形式。
    /// 例: `"result:sessionId"`
    #[serde(default)]
    pub jsonl_session_path: String,
    /// `parse = "extract_json"` のとき、抽出した JSON 内の session_id フィールド名 (top-level key)。
    /// 空なら native resume を使わず内部 history fallback になる。
    #[serde(default)]
    pub session_id_path: String,
    /// session_id 捕獲時の resume 引数名 (例 `"--resume"`)。
    /// 空なら native resume なし。
    #[serde(default)]
    pub resume_flag: String,
    /// model 指定引数名 (例 `"--model"` / `"-m"`)。空なら model 指定を渡さない。
    #[serde(default)]
    pub model_flag: String,
    /// reasoning effort 指定引数名 (例 `"--effort"`)。空なら effort は保存のみで反映しない。
    #[serde(default)]
    pub effort_flag: String,
    /// `/ai/<name>` ラベル / banner の 256-color。
    #[serde(default = "default_provider_color")]
    pub color: u8,
    /// `true`: system prompt を初回プロンプト先頭に焼き込む (resume 後は再送しない)。
    /// `false`: 毎回先頭に system prompt + history を再送する (gemini/qwen 互換)。
    #[serde(default = "default_true")]
    pub system_prompt_inline: bool,
    /// `session_id_path` が空のとき (= native resume 無し)、内部 history で保持するターン数。
    #[serde(default = "default_history_turns")]
    pub history_turns: usize,
    /// `/model` `/effort` ピッカーの候補リスト (model/effort それぞれ static + 取得コマンド)。
    #[serde(flatten)]
    pub options: OptionLists,
}

fn default_prompt_delivery() -> String {
    "stdin".to_string()
}

fn default_parse() -> String {
    "lossy".to_string()
}

fn default_provider_color() -> u8 {
    // claude 既定色と同じ orange。provider 設定で上書き想定。
    208
}

fn default_true() -> bool {
    true
}

fn default_history_turns() -> usize {
    8
}

impl ProviderRecipe {
    /// `name` / `binary` 以外を既定値で埋めた recipe を作る。
    /// 組み込みデフォルト recipe の記述と、新規 provider の生成に使う
    /// (どちらも `ProviderOverride::apply_to` で必要フィールドを上書きする想定)。
    fn with_defaults(name: impl Into<String>, binary: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            binary: binary.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            prompt_delivery: default_prompt_delivery(),
            prompt_flag: String::new(),
            parse: default_parse(),
            jsonl_content_path: String::new(),
            jsonl_session_path: String::new(),
            session_id_path: String::new(),
            resume_flag: String::new(),
            model_flag: String::new(),
            effort_flag: String::new(),
            color: default_provider_color(),
            system_prompt_inline: true,
            history_turns: default_history_turns(),
            options: OptionLists::default(),
        }
    }
}

/// `[[ai.providers]]` の 1 エントリ。組み込みデフォルト recipe への **フィールド単位
/// 上書き** を表現するため、`name` 以外は全て `Option`。
///
/// - `name` が組み込みデフォルトと一致 → 指定された (`Some`) フィールドだけ上書き。
/// - 一致しない → 新規 generic backend (このとき `binary` 必須)。
///
/// `Vec` フィールド (`args`) は「丸ごと置換」で、Vec 内の部分マージはしない。
#[derive(Debug, Deserialize, Clone)]
pub struct ProviderOverride {
    pub name: String,
    #[serde(default)]
    pub binary: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub prompt_delivery: Option<String>,
    #[serde(default)]
    pub prompt_flag: Option<String>,
    #[serde(default)]
    pub parse: Option<String>,
    #[serde(default)]
    pub jsonl_content_path: Option<String>,
    #[serde(default)]
    pub jsonl_session_path: Option<String>,
    #[serde(default)]
    pub session_id_path: Option<String>,
    #[serde(default)]
    pub resume_flag: Option<String>,
    #[serde(default)]
    pub model_flag: Option<String>,
    #[serde(default)]
    pub effort_flag: Option<String>,
    #[serde(default)]
    pub color: Option<u8>,
    #[serde(default)]
    pub system_prompt_inline: Option<bool>,
    #[serde(default)]
    pub history_turns: Option<usize>,
    #[serde(default)]
    pub models: Option<Vec<String>>,
    #[serde(default)]
    pub models_command: Option<String>,
    #[serde(default)]
    pub efforts: Option<Vec<String>>,
    #[serde(default)]
    pub efforts_command: Option<String>,
}

impl ProviderOverride {
    /// `Some` のフィールドだけを recipe に反映する (フィールド単位マージ)。
    fn apply_to(&self, r: &mut ProviderRecipe) {
        if let Some(v) = &self.binary {
            r.binary = v.clone();
        }
        if let Some(v) = &self.args {
            r.args = v.clone();
        }
        if let Some(v) = &self.env {
            r.env = v.clone();
        }
        if let Some(v) = &self.prompt_delivery {
            r.prompt_delivery = v.clone();
        }
        if let Some(v) = &self.prompt_flag {
            r.prompt_flag = v.clone();
        }
        if let Some(v) = &self.parse {
            r.parse = v.clone();
        }
        if let Some(v) = &self.jsonl_content_path {
            r.jsonl_content_path = v.clone();
        }
        if let Some(v) = &self.jsonl_session_path {
            r.jsonl_session_path = v.clone();
        }
        if let Some(v) = &self.session_id_path {
            r.session_id_path = v.clone();
        }
        if let Some(v) = &self.resume_flag {
            r.resume_flag = v.clone();
        }
        if let Some(v) = &self.model_flag {
            r.model_flag = v.clone();
        }
        if let Some(v) = &self.effort_flag {
            r.effort_flag = v.clone();
        }
        if let Some(v) = self.color {
            r.color = v;
        }
        if let Some(v) = self.system_prompt_inline {
            r.system_prompt_inline = v;
        }
        if let Some(v) = self.history_turns {
            r.history_turns = v;
        }
        if let Some(v) = &self.models {
            r.options.models = v.clone();
        }
        if let Some(v) = &self.models_command {
            r.options.models_command = v.clone();
        }
        if let Some(v) = &self.efforts {
            r.options.efforts = v.clone();
        }
        if let Some(v) = &self.efforts_command {
            r.options.efforts_command = v.clone();
        }
    }
}

/// バイナリに同梱する組み込みデフォルト recipe 一覧。
///
/// ユーザは `/ai <name>` で zero-config に呼び出せ、`[[ai.providers]]` で
/// フィールド単位に上書きできる。**追加・更新はこの関数 1 箇所に集約する**。
///
/// 信頼の根幹: ここに載せる recipe は aish が著者として read-only / plan 相当の
/// 安全フラグを `args` に焼き込む。**read-only を強制できない CLI は載せない**
/// (AI が承認 UI を迂回してサーバを変更し得るため)。
pub fn builtin_providers() -> Vec<ProviderRecipe> {
    vec![
        // Kimi CLI (MoonshotAI/kimi-cli)。
        // `--plan` で read-only tools に制限 (cursor の `--mode plan` 相当の安全側既定)、
        // `--quiet` で非対話 + 最終メッセージのみ text 出力 → lossy parse で
        // `{message, commands}` を抽出する。
        // NOTE: prompt 渡し方 (stdin/positional) と最終出力フォーマットは kimi-cli
        // インストール環境での実機検証が必要。挙動が異なる場合は config で上書き可能。
        ProviderRecipe {
            args: vec!["--plan".to_string(), "--quiet".to_string()],
            model_flag: "--model".to_string(),
            color: 99, // purple 寄り。native とは別色。
            ..ProviderRecipe::with_defaults("kimi", "kimi")
        },
        // OpenCode (anomalyco/opencode, 旧 sst/opencode)。
        // read-only 強制は CLI フラグでは不可のため、`OPENCODE_CONFIG_CONTENT`
        // (ユーザ config より後にマージされるインライン config) で edit/bash を deny した
        // 専用 agent `aish` を定義して `--agent aish` で使う。deny されたツールは
        // ツールセット自体から除去される (実機検証済み、v1.17.13)。
        // `task`/`todowrite` も無効化 (プロジェクト側 config の緩い agent への迂回防止)。
        // read / webfetch / websearch は許可 (非 Claude 系は web 調査を許す既存方針)。
        // built-in `plan` agent は ask ベースで headless ハングするため使わない。
        // `--auto` (auto-approve) は絶対に付けない。
        ProviderRecipe {
            args: vec![
                "run".to_string(),
                "--agent".to_string(),
                "aish".to_string(),
            ],
            env: BTreeMap::from([(
                "OPENCODE_CONFIG_CONTENT".to_string(),
                r#"{"permission":{"edit":"deny","bash":"deny"},"agent":{"aish":{"mode":"primary","permission":{"edit":"deny","bash":"deny"},"tools":{"task":false,"todowrite":false}}}}"#
                    .to_string(),
            )]),
            prompt_delivery: "arg".to_string(), // `opencode run [message..]` の positional
            model_flag: "--model".to_string(),  // provider/model 形式
            effort_flag: "--variant".to_string(), // provider 依存の reasoning effort
            color: 37, // teal 系。kimi (99) / 既定 (208) と区別。
            options: OptionLists {
                models_command: "opencode models".to_string(),
                ..OptionLists::default()
            },
            ..ProviderRecipe::with_defaults("opencode", "opencode")
        },
    ]
}

impl AiConfig {
    /// 組み込みデフォルト recipe (`builtin_providers`) に `providers` の上書き / 追加を
    /// マージし、検証済みの最終 recipe 一覧を返す。`Config::load` が呼ぶ。
    pub fn resolve_providers(&self) -> Result<Vec<ProviderRecipe>, String> {
        self.resolve_with_builtins(builtin_providers())
    }

    /// `resolve_providers` の本体。テスト用に builtins を差し込めるよう分離。
    ///
    /// マージ規則:
    /// - `providers` の各エントリ name が builtins に在れば「指定フィールドだけ」上書き。
    /// - 無ければ新規 recipe (このとき `binary` 必須)。
    fn resolve_with_builtins(
        &self,
        builtins: Vec<ProviderRecipe>,
    ) -> Result<Vec<ProviderRecipe>, String> {
        let mut resolved = builtins;
        for ov in &self.providers {
            if ov.name.is_empty() {
                return Err("[[ai.providers]] entry has empty `name`".to_string());
            }
            if let Some(existing) = resolved.iter_mut().find(|r| r.name == ov.name) {
                // 既存 (組み込み or 先行エントリ) へフィールド単位上書き。
                ov.apply_to(existing);
            } else {
                // 新規 provider。binary は必須。
                let binary = ov.binary.clone().filter(|b| !b.is_empty()).ok_or_else(|| {
                    format!(
                        "[[ai.providers]] `{}`: new provider requires non-empty `binary`",
                        ov.name
                    )
                })?;
                let mut recipe = ProviderRecipe::with_defaults(ov.name.clone(), binary);
                ov.apply_to(&mut recipe);
                resolved.push(recipe);
            }
        }
        validate_recipes(&resolved)?;
        Ok(resolved)
    }
}

/// マージ後の最終 recipe 一覧を検証する。
/// - 個数 <= 256 (BackendKind::Generic(u8) が u8 を埋めるため)
/// - name の一意性 / 非空
/// - binary 非空
/// - name が native 予約語 (claude/codex/gemini/qwen/cursor/copilot) と衝突しないこと
/// - parse / prompt_delivery の値が許可リスト内
fn validate_recipes(recipes: &[ProviderRecipe]) -> Result<(), String> {
    if recipes.len() > 256 {
        return Err(format!(
            "[[ai.providers]] entries exceed 256 (got {})",
            recipes.len()
        ));
    }
    // native 予約語は `BackendKind::all_native()` から導出して二重定義を避ける。
    // 新しい native backend を追加したら自動で予約語にも入る。
    let reserved: std::collections::HashSet<&str> = crate::ai::BackendKind::all_native()
        .iter()
        .map(|k| k.as_str())
        .collect();
    let mut seen = std::collections::HashSet::new();
    for p in recipes {
        if p.name.is_empty() {
            return Err("[[ai.providers]] entry has empty `name`".to_string());
        }
        if p.binary.is_empty() {
            return Err(format!("[[ai.providers]] `{}` has empty `binary`", p.name));
        }
        if reserved.contains(p.name.as_str()) {
            return Err(format!(
                "[[ai.providers]] `{}`: provider name collides with built-in backend",
                p.name
            ));
        }
        if !seen.insert(p.name.clone()) {
            return Err(format!("[[ai.providers]] has duplicate name `{}`", p.name));
        }
        if !matches!(p.parse.as_str(), "lossy" | "extract_json" | "jsonl") {
            return Err(format!(
                "[[ai.providers]] `{}`: parse=`{}` is not one of: lossy, extract_json, jsonl",
                p.name, p.parse
            ));
        }
        if !matches!(p.prompt_delivery.as_str(), "stdin" | "arg" | "flag") {
            return Err(format!(
                "[[ai.providers]] `{}`: prompt_delivery=`{}` is not one of: stdin, arg, flag",
                p.name, p.prompt_delivery
            ));
        }
        if p.prompt_delivery == "flag" && p.prompt_flag.is_empty() {
            return Err(format!(
                "[[ai.providers]] `{}`: prompt_delivery=\"flag\" requires non-empty `prompt_flag`",
                p.name
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize, Clone)]
pub struct LogConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_log_path")]
    pub path: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: default_log_path(),
        }
    }
}

fn default_log_path() -> String {
    "~/.aish/logs/claude-code.log".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct DisplayConfig {
    #[serde(default = "default_shell_prefix_label")]
    pub shell_prefix_label: String,
    /// 旧ステータスバー / 旧バナー用の色。現状はバナーがバックエンド色を使うため未参照。
    /// 既存ユーザの設定ファイルが parse エラーにならないよう field は残す。
    #[allow(dead_code)]
    #[serde(default = "default_header_color")]
    pub header_color: String,
    #[serde(default = "default_prompt_label")]
    pub prompt_label: String,
    #[serde(default = "default_prompt_color")]
    pub prompt_color: String,
    #[serde(default = "default_thinking_message")]
    pub thinking_message: String,
    #[serde(default = "default_thinking_color")]
    pub thinking_color: String,
    #[serde(default = "default_ai_color")]
    pub ai_color: String,
    #[serde(default)]
    pub input_color: String,
    #[serde(default = "default_confirm_color")]
    pub confirm_color: String,
    #[serde(default)]
    pub term_fg_color: String,
    #[serde(default)]
    pub term_bg_color: String,
    #[serde(default = "default_term_cursor_color")]
    pub term_cursor_color: String,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            shell_prefix_label: default_shell_prefix_label(),
            header_color: default_header_color(),
            prompt_label: default_prompt_label(),
            prompt_color: default_prompt_color(),
            thinking_message: default_thinking_message(),
            thinking_color: default_thinking_color(),
            ai_color: default_ai_color(),
            input_color: String::new(),
            confirm_color: default_confirm_color(),
            term_fg_color: String::new(),
            term_bg_color: String::new(),
            term_cursor_color: default_term_cursor_color(),
        }
    }
}

fn default_system_prompt() -> String {
    "あなたはLinuxサーバ管理の専門家です。SSHセッションの内容を把握しています。".to_string()
}

fn default_shell_prefix_label() -> String {
    "[aish]".to_string()
}

fn default_header_color() -> String {
    "\x1b[38;5;208m".to_string()
}

fn default_prompt_label() -> String {
    "[aish]".to_string()
}

fn default_prompt_color() -> String {
    "\x1b[38;5;208;48;2;50;35;20m".to_string()
}

fn default_thinking_message() -> String {
    "Thinking...".to_string()
}

fn default_thinking_color() -> String {
    "\x1b[38;5;208m".to_string()
}

fn default_ai_color() -> String {
    "\x1b[38;5;216m".to_string()
}

fn default_confirm_color() -> String {
    "\x1b[38;5;228;48;5;239m".to_string()
}

fn default_term_cursor_color() -> String {
    "#ff8800".to_string()
}

impl Config {
    /// 設定をロードする。
    /// `config_path` が `Some` (ユーザが `--config` で明示) の場合、
    /// ファイル不在・読み取り失敗・パース失敗はエラーとして返す。
    /// `None` (デフォルトパス) の場合は読み取り/パース失敗時に警告を出して既定値で続行する。
    pub fn load(config_path: Option<&str>) -> Result<Self, String> {
        let (path, explicit) = match config_path {
            Some(p) => (PathBuf::from(p), true),
            None => {
                let mut p = dirs::home_dir().unwrap_or_default();
                p.push(".aish");
                p.push("config.toml");
                (p, false)
            }
        };

        // ファイルが無い / 読めない / parse 失敗でも、組み込みデフォルト recipe は
        // 必ず利用可能にするため、最後に必ず resolve_providers を通す。
        let mut config = if !path.exists() {
            if explicit {
                return Err(format!("Config file not found: {}", path.display()));
            }
            Config::default()
        } else {
            match fs::read_to_string(&path) {
                Ok(content) => match toml::from_str::<Config>(&content) {
                    Ok(mut config) => {
                        config.merge_ai_fallbacks();
                        config
                    }
                    Err(e) => {
                        if explicit {
                            return Err(format!(
                                "Failed to parse config file {}: {}",
                                path.display(),
                                e
                            ));
                        }
                        eprintln!("Warning: Failed to parse config file: {e}");
                        Config::default()
                    }
                },
                Err(e) => {
                    if explicit {
                        return Err(format!(
                            "Failed to read config file {}: {}",
                            path.display(),
                            e
                        ));
                    }
                    eprintln!("Warning: Failed to read config file: {e}");
                    Config::default()
                }
            }
        };

        // 組み込みデフォルト + ユーザ上書きをマージ・検証。
        // 失敗は常にエラー (explicit 不問)。recipe 不整合のまま起動すると runtime に
        // 意味不明な失敗を起こすため。
        config.ai.resolved_providers = config
            .ai
            .resolve_providers()
            .map_err(|e| format!("Invalid [[ai.providers]] in {}: {}", path.display(), e))?;
        Ok(config)
    }

    /// `[ai]` セクションが空のフィールドはトップレベル値で埋める。
    /// 既存ユーザの `system_prompt` / `language` を `[ai]` 不在でも引き継ぐための後方互換処理。
    fn merge_ai_fallbacks(&mut self) {
        if self.ai.system_prompt.is_empty() {
            self.ai.system_prompt = self.system_prompt.clone();
        }
        if self.ai.language.is_empty() {
            self.ai.language = self.language.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_fallback_picks_top_level_when_ai_section_missing() {
        let toml_str = r#"
system_prompt = "top-level prompt"
language = "English"
"#;
        let mut config: Config = toml::from_str(toml_str).unwrap();
        config.merge_ai_fallbacks();
        assert_eq!(config.ai.system_prompt, "top-level prompt");
        assert_eq!(config.ai.language, "English");
        assert_eq!(config.ai.backend, "claude");
    }

    #[test]
    fn ai_section_overrides_top_level() {
        let toml_str = r#"
system_prompt = "top-level prompt"
language = "English"

[ai]
backend = "codex"
system_prompt = "ai-section prompt"
language = "French"
"#;
        let mut config: Config = toml::from_str(toml_str).unwrap();
        config.merge_ai_fallbacks();
        assert_eq!(config.ai.system_prompt, "ai-section prompt");
        assert_eq!(config.ai.language, "French");
        assert_eq!(config.ai.backend, "codex");
    }

    #[test]
    fn ai_partial_override_falls_back_per_field() {
        let toml_str = r#"
system_prompt = "top-level prompt"
language = "English"

[ai]
backend = "gemini"
language = "Japanese"
"#;
        let mut config: Config = toml::from_str(toml_str).unwrap();
        config.merge_ai_fallbacks();
        // system_prompt was empty in [ai] so falls back
        assert_eq!(config.ai.system_prompt, "top-level prompt");
        assert_eq!(config.ai.language, "Japanese");
        assert_eq!(config.ai.backend, "gemini");
    }

    #[test]
    fn claude_disallowed_tools_default() {
        let cfg = ClaudeBackendConfig::default();
        assert_eq!(cfg.disallowed_tools, "Bash,Edit,Write,Read");
        assert!(cfg.extra_args.is_empty());
    }

    fn ai_from_toml(toml_str: &str) -> AiConfig {
        let config: Config = toml::from_str(toml_str).unwrap();
        config.ai
    }

    #[test]
    fn providers_default_resolves_to_builtins() {
        let cfg = AiConfig::default();
        assert!(cfg.providers.is_empty());
        let resolved = cfg.resolve_providers().expect("builtins must be valid");
        let names: Vec<String> = resolved.iter().map(|r| r.name.clone()).collect();
        let builtin_names: Vec<String> = builtin_providers().into_iter().map(|r| r.name).collect();
        assert_eq!(names, builtin_names);
    }

    #[test]
    fn builtin_providers_are_valid() {
        // 同梱 recipe 自体が検証 (重複 / 予約語 / parse 値 等) を通ること。
        validate_recipes(&builtin_providers()).unwrap();
    }

    #[test]
    fn builtin_kimi_keeps_plan_flag() {
        // 信頼の根幹: kimi 同梱 recipe の read-only 安全フラグ --plan が残っていること。
        let builtins = builtin_providers();
        let kimi = builtins.iter().find(|r| r.name == "kimi").unwrap();
        assert!(kimi.args.iter().any(|a| a == "--plan"));
    }

    #[test]
    fn builtin_opencode_enforces_readonly_config() {
        // 信頼の根幹: opencode 同梱 recipe の read-only 強制 (env 注入 + 専用 agent)。
        let builtins = builtin_providers();
        let oc = builtins.iter().find(|r| r.name == "opencode").unwrap();
        // --auto (auto-approve) と --agent plan (headless ハング) は絶対に使わない。
        assert!(!oc.args.iter().any(|a| a == "--auto"));
        let agent_pos = oc.args.iter().position(|a| a == "--agent").unwrap();
        assert_eq!(oc.args[agent_pos + 1], "aish", "--agent plan に変えない");
        // OPENCODE_CONFIG_CONTENT が valid JSON で deny 構成を含む。
        let content = oc.env.get("OPENCODE_CONFIG_CONTENT").expect("env 注入");
        let v: serde_json::Value = serde_json::from_str(content).expect("valid JSON");
        assert_eq!(v["permission"]["edit"], "deny");
        assert_eq!(v["permission"]["bash"], "deny");
        assert_eq!(v["agent"]["aish"]["permission"]["edit"], "deny");
        assert_eq!(v["agent"]["aish"]["permission"]["bash"], "deny");
        assert_eq!(v["agent"]["aish"]["tools"]["task"], false);
        assert_eq!(v["agent"]["aish"]["tools"]["todowrite"], false);
    }

    #[test]
    fn new_provider_parses_minimal() {
        let ai = ai_from_toml(
            r#"
[[ai.providers]]
name = "ollama-llama"
binary = "ollama"
args = ["run", "llama3.2"]
"#,
        );
        let resolved = ai.resolve_providers().unwrap();
        let p = resolved.iter().find(|r| r.name == "ollama-llama").unwrap();
        assert_eq!(p.binary, "ollama");
        assert_eq!(p.args, vec!["run".to_string(), "llama3.2".to_string()]);
        // 未指定フィールドは既定値で埋まる。
        assert_eq!(p.prompt_delivery, "stdin");
        assert_eq!(p.parse, "lossy");
        assert!(p.system_prompt_inline);
        assert_eq!(p.history_turns, 8);
        assert_eq!(p.color, 208);
    }

    #[test]
    fn new_provider_parses_full() {
        let ai = ai_from_toml(
            r#"
[[ai.providers]]
name = "fancy"
binary = "fancy-cli"
args = ["chat"]
env = { FANCY_MODE = "safe", FANCY_TOKEN_FILE = "~/.fancy" }
prompt_delivery = "flag"
prompt_flag = "-p"
parse = "jsonl"
jsonl_content_path = "assistant.message:data.content"
jsonl_session_path = "result:sessionId"
session_id_path = "session_id"
resume_flag = "--resume"
model_flag = "--model"
effort_flag = "--effort"
color = 42
system_prompt_inline = false
history_turns = 4
"#,
        );
        let resolved = ai.resolve_providers().unwrap();
        let p = resolved.iter().find(|r| r.name == "fancy").unwrap();
        assert_eq!(p.prompt_delivery, "flag");
        assert_eq!(p.prompt_flag, "-p");
        assert_eq!(p.parse, "jsonl");
        assert_eq!(p.resume_flag, "--resume");
        assert_eq!(p.color, 42);
        assert!(!p.system_prompt_inline);
        assert_eq!(p.history_turns, 4);
        assert_eq!(p.env.get("FANCY_MODE").map(String::as_str), Some("safe"));
        assert_eq!(p.env.len(), 2);
    }

    #[test]
    fn override_env_replaces_wholesale() {
        // env は args と同じく丸ごと置換 (キー単位マージはしない)。
        let builtins = vec![ProviderRecipe {
            env: BTreeMap::from([
                ("KEEP".to_string(), "old".to_string()),
                ("DROP".to_string(), "old".to_string()),
            ]),
            ..ProviderRecipe::with_defaults("demo", "demo-cli")
        }];
        let ai = ai_from_toml(
            r#"
[[ai.providers]]
name = "demo"
env = { KEEP = "new" }
"#,
        );
        let resolved = ai.resolve_with_builtins(builtins).unwrap();
        let p = &resolved[0];
        assert_eq!(p.env.get("KEEP").map(String::as_str), Some("new"));
        assert!(!p.env.contains_key("DROP"), "丸ごと置換なので消える");
    }

    #[test]
    fn override_merges_single_field_onto_builtin() {
        // 組み込み "demo" の model_flag だけ上書き。他フィールドは builtin のまま残る。
        let builtins = vec![ProviderRecipe {
            args: vec!["--plan".into()],
            model_flag: "--model".into(),
            color: 99,
            ..ProviderRecipe::with_defaults("demo", "demo-cli")
        }];
        let ai = ai_from_toml(
            r#"
[[ai.providers]]
name = "demo"
model_flag = "-m"
"#,
        );
        let resolved = ai.resolve_with_builtins(builtins).unwrap();
        assert_eq!(resolved.len(), 1, "置換マージなので新規追加されない");
        let p = &resolved[0];
        assert_eq!(p.model_flag, "-m"); // 上書きされた
        assert_eq!(p.binary, "demo-cli"); // builtin のまま
        assert_eq!(p.args, vec!["--plan".to_string()]); // builtin のまま
        assert_eq!(p.color, 99); // builtin のまま
    }

    #[test]
    fn override_unknown_name_adds_new_provider() {
        let builtins = vec![ProviderRecipe::with_defaults("demo", "demo-cli")];
        let ai = ai_from_toml(
            r#"
[[ai.providers]]
name = "extra"
binary = "extra-cli"
"#,
        );
        let resolved = ai.resolve_with_builtins(builtins).unwrap();
        assert_eq!(resolved.len(), 2);
        assert!(resolved
            .iter()
            .any(|r| r.name == "extra" && r.binary == "extra-cli"));
    }

    #[test]
    fn new_provider_without_binary_errs() {
        let ai = ai_from_toml(
            r#"
[[ai.providers]]
name = "extra"
model_flag = "-m"
"#,
        );
        let err = ai.resolve_providers().unwrap_err();
        assert!(err.contains("binary"), "got: {err}");
    }

    #[test]
    fn resolve_rejects_reserved_native_name() {
        let ai = ai_from_toml(
            r#"
[[ai.providers]]
name = "claude"
binary = "alt-claude"
"#,
        );
        let err = ai.resolve_providers().unwrap_err();
        assert!(err.contains("built-in"), "got: {err}");
    }

    #[test]
    fn resolve_rejects_bad_parse_value() {
        let ai = ai_from_toml(
            r#"
[[ai.providers]]
name = "a"
binary = "x"
parse = "yaml"
"#,
        );
        let err = ai.resolve_providers().unwrap_err();
        assert!(err.contains("parse"));
    }

    #[test]
    fn resolve_rejects_flag_delivery_without_flag_name() {
        let ai = ai_from_toml(
            r#"
[[ai.providers]]
name = "a"
binary = "x"
prompt_delivery = "flag"
"#,
        );
        let err = ai.resolve_providers().unwrap_err();
        assert!(err.contains("prompt_flag"));
    }

    #[test]
    fn duplicate_user_names_merge_last_wins() {
        // 同名ユーザエントリは後勝ちのフィールドマージ (エラーにしない)。
        let ai = ai_from_toml(
            r#"
[[ai.providers]]
name = "a"
binary = "x"
model_flag = "-m"

[[ai.providers]]
name = "a"
binary = "y"
"#,
        );
        let resolved = ai.resolve_providers().unwrap();
        let dups: Vec<_> = resolved.iter().filter(|r| r.name == "a").collect();
        assert_eq!(dups.len(), 1);
        assert_eq!(dups[0].binary, "y"); // 後のエントリで上書き
        assert_eq!(dups[0].model_flag, "-m"); // 先のエントリの値は残る
    }

    #[test]
    fn native_option_lists_parse() {
        // `[ai.claude]` に flatten された models/efforts/取得コマンドが読めること。
        let toml_str = r#"
[ai.claude]
models = ["claude-opus-4-8", "claude-sonnet-4-6"]
efforts = ["low", "high"]
models_command = "echo hi"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let o = &config.ai.claude.options;
        assert_eq!(o.models, vec!["claude-opus-4-8", "claude-sonnet-4-6"]);
        assert_eq!(o.efforts, vec!["low", "high"]);
        assert_eq!(o.models_command, "echo hi");
        assert!(o.efforts_command.is_empty());
    }

    #[test]
    fn generic_override_models_only_keeps_builtin_efforts() {
        // 組み込み "demo" の efforts はそのままに、models だけ上書きできる。
        let mut builtin = ProviderRecipe::with_defaults("demo", "demo-cli");
        builtin.options.efforts = vec!["low".into(), "high".into()];
        let ai = ai_from_toml(
            r#"
[[ai.providers]]
name = "demo"
models = ["m1", "m2"]
"#,
        );
        let resolved = ai.resolve_with_builtins(vec![builtin]).unwrap();
        let p = &resolved[0];
        assert_eq!(p.options.models, vec!["m1", "m2"]); // 上書き
        assert_eq!(p.options.efforts, vec!["low", "high"]); // builtin のまま
    }
}
