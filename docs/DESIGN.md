# aish 設計書

## アーキテクチャ概要

aish は以下のコンポーネントで構成される。

```
┌─────────────────────────────────────────────────┐
│                   main.rs                       │
│            (CLI引数解析・初期化)                  │
└──────────┬──────────────────────┬───────────────┘
           │                      │
    ┌──────▼──────┐       ┌──────▼──────┐
    │  config.rs  │       │   ssh.rs    │
    │ (設定読込)   │       │ (PTY/SSH)   │
    └──────┬──────┘       └──────┬──────┘
           │                      │
    ┌──────▼──────────────────────▼───────────────┐
    │               shell.rs                      │
    │          (REPL ループ・入力ルーティング)       │
    └──┬────────┬──────────────┬──────────────────┘
       │        │              │
┌──────▼──┐ ┌──▼───┐  ┌──────▼──────┐
│input.rs │ │ui.rs │  │ context.rs  │
│(入力解析)│ │(表示) │  │(会話管理)    │
└─────────┘ └──────┘  └──────┬──────┘
                              │
                    ┌─────────▼─────────┐
                    │    ai/mod.rs      │
                    │  (AiProvider管理)  │
                    └──┬─────┬──────┬──┘
                       │     │      │
              ┌────────▼┐ ┌─▼────┐ ┌▼────────┐
              │claude.rs│ │chatgpt│ │gemini.rs│
              │         │ │.rs   │ │         │
              └─────────┘ └──────┘ └─────────┘
```

## プロジェクト構造

```
src/
├── main.rs           CLIエントリポイント、起動処理
├── config.rs         設定ファイル + 環境変数の読み込み
├── ssh.rs            portable-pty によるSSHセッション管理
├── shell.rs          REPL ループ、入力ルーティング
├── input.rs          ユーザ入力のパース・分類
├── context.rs        AI 用セッションコンテキスト管理
├── ui.rs             ターミナル出力フォーマット・色付け
└── ai/
    ├── mod.rs        AiProvider トレイト、AiManager
    ├── types.rs      共通型 (SessionContext, AiResponse 等)
    ├── claude.rs     Anthropic Messages API 連携
    ├── chatgpt.rs    OpenAI Chat Completions API 連携
    └── gemini.rs     Google Generative Language API 連携
```

## 主要依存クレート

| クレート | 用途 |
|----------|------|
| `tokio` | 非同期ランタイム |
| `reqwest` (rustls-tls) | HTTP クライアント（AI API 呼出） |
| `serde` / `serde_json` / `toml` | シリアライゼーション |
| `clap` | CLI 引数パース |
| `rustyline` | 対話型行入力（ヒストリ、行編集） |
| `portable-pty` | クロスプラットフォーム PTY（SSH サブプロセス） |
| `crossterm` | ターミナル制御（色、スタイル） |
| `anyhow` / `thiserror` | エラーハンドリング |
| `dirs` | ホームディレクトリ取得 |
| `regex` | AI レスポンスからコマンド抽出 |
| `async-trait` | 非同期トレイト |

## 並行処理モデル

3つの並行処理を `tokio::select!` で多重化する。

```
[SSH PTY]          ──blocking read──▶ [std::thread]  ──mpsc channel──▶ [Shell REPL select!]
[User Keyboard]    ──spawn_blocking(rustyline)──────▶ [Shell REPL select!]
[Shell REPL]       ──write to PTY──────────────────▶ [SSH PTY]
[Shell REPL]       ──async reqwest─────────────────▶ [AI API]
```

- **SSH PTY 読み取り**: `portable-pty` のブロッキング Reader を `std::thread` で動かし、`tokio::sync::mpsc` チャネルで async 側に送信
- **ユーザ入力**: `rustyline` はブロッキングなので `tokio::task::spawn_blocking` で実行
- **AI API 呼出**: `reqwest` による非同期 HTTP リクエスト

## 主要な型・トレイト

### AiProvider トレイト (ai/mod.rs)

```rust
#[async_trait]
pub trait AiProvider: Send + Sync {
    async fn send_message(&self, context: &SessionContext) -> Result<AiResponse>;
    fn kind(&self) -> ProviderKind;
    fn name(&self) -> &str;
}
```

全 AI プロバイダがこのトレイトを実装する。

### AiManager (ai/mod.rs)

```rust
pub struct AiManager {
    providers: HashMap<ProviderKind, Box<dyn AiProvider>>,
    active: ProviderKind,
}
```

- 設定済みの全プロバイダを保持
- `switch()` でアクティブ AI を切替
- `active_provider()` で現在の AI を取得

### SessionContext (ai/types.rs)

```rust
pub struct SessionContext {
    pub system_prompt: String,
    pub messages: Vec<ContextMessage>,
}

pub enum ContextMessage {
    User { text: String },
    Assistant { text: String },
    CommandOutput { command: String, output: String },
}
```

- AI に送信するコンテキスト（システムプロンプト + 会話履歴）
- 最大 50 メッセージのスライディングウィンドウ

### UserInput (input.rs)

```rust
pub enum UserInput {
    SwitchAi(ProviderKind),
    Explain(ExplainTarget),
    DirectCommand(String),
    Quit,
    Help,
    SshConnect(String),
    AiPrompt(String),
}
```

ユーザ入力を分類し、shell.rs でディスパッチする。

## データフロー

### AI プロンプトフロー

```
ユーザ入力 ("ディスク容量を確認して")
    │
    ▼
context.add_user_prompt() ─── 直近のSSH出力も追加
    │
    ▼
context.build_context() ─── システムプロンプト + 会話履歴
    │
    ▼
ai_provider.send_message(context)
    │
    ▼
AIレスポンス: "df -h でディスク使用量を確認できます。[COMMAND: df -h]"
    │
    ▼
extract_commands() ─── [COMMAND: ...] マーカーを抽出
    │
    ▼
ユーザ確認: "コマンド 'df -h' を実行しますか? [y/N]"
    │ (y)
    ▼
ssh.send("df -h") ─── SSH セッションで実行
    │
    ▼
出力表示 ─── コンテキストにも記録
```

### 直接コマンドフロー

```
ユーザ入力: "!ls -la"
    │
    ▼
parse_input() → DirectCommand("ls -la")
    │
    ▼
ssh.send("ls -la")
    │
    ▼
PTY出力 → 画面表示 + コンテキスト記録
```

## AI API 連携

### Claude (Anthropic Messages API)

- エンドポイント: `POST https://api.anthropic.com/v1/messages`
- 認証: `x-api-key` ヘッダ
- モデル: `claude-sonnet-4-20250514`

### ChatGPT (OpenAI Chat Completions API)

- エンドポイント: `POST https://api.openai.com/v1/chat/completions`
- 認証: `Authorization: Bearer <key>`
- モデル: `gpt-4o`

### Gemini (Google Generative Language API)

- エンドポイント: `POST https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent?key=<key>`
- 認証: URL パラメータ
- モデル: `gemini-2.0-flash`

## SSH セッション管理

- `portable-pty` クレートで PTY を作成し、`ssh` コマンドをサブプロセスとして起動
- ユーザの既存 SSH 設定（~/.ssh/config, 鍵, ssh-agent）をそのまま利用可能
- PTY 経由のため、リモート側のインタラクティブなプログラム（色、プロンプト等）もサポート
- 出力履歴は最大 200 エントリのリングバッファで保持

## 設定の優先順位

```
環境変数 (ANTHROPIC_API_KEY 等)
    ▼ (優先)
設定ファイル (~/.aish/config.toml)
    ▼ (フォールバック)
デフォルト値
```

## 今後の拡張予定

- AI レスポンスのストリーミング表示（トークン単位で逐次表示）
- AI API の tool/function calling 対応（コマンド提案をより構造化）
- ターミナルリサイズの PTY への伝搬
- SSH 接続断時の自動再接続
- コマンド履歴の永続化（~/.aish/history）
- プラグイン機構による AI プロバイダ追加
