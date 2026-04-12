# 設計判断メモ

## テーマカラー
- aishのテーマカラーはオレンジ (256色: 208)
- shell_prefix_color は薄めのオレンジ (216)、背景なし
- prompt_color はオレンジ (208) + ダークブラウン背景 (rgb(50,35,20))
- ai_color はオレンジ (208)、背景なし
- confirm_color はペールイエロー (228) + グレー背景 (239)
- header_color はグレー (245) — 起動バナー用
- thinking_color はオレンジ (208)

## 色設定の方針
- foreground/background を分離せず、1つの color フィールドに統合する
- ANSI エスケープは `\x1b[38;5;208;48;5;238m` のように1つにまとめて書ける
- 旧フィールド名の後方互換 (serde alias) は不要。廃止するときはソース・設定・ドキュメントから完全に消す

## shell_prefix と prompt の分離
- shell_prefix_label / shell_prefix_color: PS1プレフィックス（シェルプロンプトの先頭）
- prompt_label / prompt_color: aishプロンプト (Ctrl+/) のラベル
- 起動バナーとターミナルタイトルは shell_prefix 系を使用

## 確認プロンプト (Execute? Y/n)
- ESC / Ctrl+C で n と同じ動作（キャンセル）
- confirm_color で独立した色設定

## ログ機能
- Claude Code CLIのコマンドとレスポンスを記録
- デフォルトパス: ~/.aish/logs/claude-code.log
- デフォルトは OFF (config.toml の [log] enabled = false)
- ログにラベル ([command], [stdout]) は不要 — 見ればわかる
- stderr のみ [stderr] ラベル付き

## 言語設定
- config.toml の language フィールドで AI 応答言語を指定
- デフォルトは "Japanese"
- システムプロンプトに "Respond in {language}." を自動付与

## ローカルモード
- PTY起動時にカレントディレクトリをaish実行時のディレクトリに設定 (cmd.cwd)

## io::stdin() の注意
- io::stdin() は BufReader を内包しており、poll() と併用するとデータ喪失する
- read_line_raw_loop_from では ManuallyDrop<File::from_raw_fd(0)> で BufReader をバイパス

## 依存クレートの方針
- 不要な依存は追加しない (例: chrono の代わりに libc + 標準ライブラリでタイムスタンプ生成)
