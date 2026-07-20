# プラグイン基盤 v2 — 実装仕様（内部設計メモ）

この文書は実装者向けの確定仕様である。既存 v1 マニフェストとの後方互換を必ず保つこと。

## 0. 互換性の絶対条件

- 既存の `plugin.toml`（`[plugin]` / `[[command]]` / `[[theme]]` / `[[snippet]]`）は **無改造でそのまま動く**こと。
- 既存の `CmdInput` / `CmdOutput` / `Plugin` / `PluginCommand` の意味を変えない。フィールド追加のみ。
- `zai` および `zai <dir>` の起動挙動を変えない（GUI 起動のまま）。

## 1. マニフェスト v2

```toml
[plugin]
name = "example"          # 既存: [a-z0-9_-]{1,64}
version = "0.1.0"
author = ""
description = ""
api = 2                   # 追加: 省略時 1

[[command]]
id = "fmt"                # 追加: 安定ID。省略時は title から slug 生成
title = "整形"
icon = "✨"
run = "..."
input = "none" | "selection" | "file"
output = "replace" | "insert" | "new_tab" | "notify" | "silent" | "agent_prompt" | "panel" | "actions"
langs = ["rust"]
keybind = "cmd+alt+f"
on_save = true            # 既存互換
timeout_secs = 30
panel = "tasks"           # output="panel" のとき出力先パネルID

[[hook]]                  # 追加
event = "startup" | "file_open" | "file_save" | "agent_finish" | "agent_attention" | "git_change" | "interval"
run = "..."
interval_secs = 60        # event="interval" のときのみ必須
output = "silent" | "notify" | "actions" | "panel"
panel = "tasks"
timeout_secs = 30

[[panel]]                 # 追加: サイドバーに独自パネルを追加
id = "tasks"
title = "タスク"
icon = "📋"
run = ""                  # 空可。空ならアクション経由でのみ更新される
refresh = "manual" | "on_open" | "interval"
interval_secs = 30
format = "text" | "markdown"

[[setting]]               # 追加: プラグイン設定
key = "token"
type = "string" | "bool" | "int"
default = ""
label = "APIトークン"
secret = false            # true ならUIでマスク表示

[[theme]]                 # 既存
label = "..."
path = "themes/x.json"

[[snippet]]               # 既存
language = "rust"
path = "snippets/rust.json"
```

### 検証規則
- `on_save = true` は従来どおり `input="file"` + `output="replace"` を要求。
- `output="panel"` は `panel` が既存パネルIDを指すこと。
- `event="interval"` は `interval_secs >= 5`。
- 不正値はプラグイン全体を落とさず `Plugin.error` に格納（既存挙動を踏襲）。

## 2. アクションプロトコル（プラグイン → アプリ）

`output = "actions"` のとき、stdout を **JSON Lines** として解釈する。1行1アクション。
解釈できない行は無視し、警告としてログに残す（プラグインを落とさない）。

```json
{"action":"open_file","path":"src/main.rs","line":42}
{"action":"notify","message":"完了","level":"info"}
{"action":"insert_text","text":"..."}
{"action":"replace_buffer","text":"..."}
{"action":"new_tab","title":"結果","text":"..."}
{"action":"agent_prompt","agent":"claude","text":"...","submit":false}
{"action":"run_terminal","command":"cargo test","cwd":"."}
{"action":"open_url","url":"https://example.com"}
{"action":"set_panel","panel":"tasks","text":"..."}
{"action":"set_status","text":"..."}
{"action":"refresh_files"}
{"action":"set_setting","key":"token","value":"..."}
```

`level` は `info` | `warn` | `error`（省略時 `info`）。
`submit` が false ならエージェント入力欄に差し込むだけで送信しない（既定 false）。

## 3. 環境変数（プラグインプロセスへ渡す）

既存: `ZV_FILE` `ZV_LANG` `ZV_WORKSPACE` `ZV_PLUGIN_DIR`

追加:
- `ZV_API` = `2`
- `ZV_BIN` = 実行中の `zai` バイナリ絶対パス（CLI 折り返し呼び出し用）
- `ZV_PLUGIN_DATA` = `~/.zaivern/plugin-data/<name>/`（永続データ置き場。自動作成）
- `ZV_SELECTION` = 選択テキスト（無選択なら空）
- `ZV_LINE` / `ZV_COLUMN` = カーソル位置（1始まり）
- `ZV_AGENT` = アクティブなエージェント名（無ければ空）
- `ZV_EVENT` = フック起動時のイベント名（コマンド起動時は空）
- `ZV_GIT_BRANCH` = 現在のブランチ名（git 管理外なら空）
- `ZV_CFG_<KEY大文字>` = `[[setting]]` の現在値

## 4. 設定の永続化（config.toml）

```toml
[plugins]
disabled = ["example"]        # 無効化リスト（未記載＝有効）

[plugins.settings.example]
token = "xxx"
```

- 無効なプラグインはコマンド・フック・パネル・キーバインドを一切登録しない。テーマ／スニペットも読み込まない。
- 一覧UIには残り、再有効化できること。

## 5. バンドル標準プラグイン

- 実体は `assets/plugins/<name>/` 配下（マニフェストとシェルスクリプト）。
- ビルド時に `include_str!` で埋め込み、初回起動時に `~/.zaivern/plugins/<name>/` へ展開する。
- 展開済み判定は `~/.zaivern/plugins/<name>/.bundled` に書いたバージョン文字列で行う。
  バンドル版のほうが新しい場合のみ再展開する（ユーザーが編集したファイルを毎回潰さない）。
- 標準プラグインは無効化できるが、アンインストールは無効化として扱う（次回起動で復活してよい）。
- シェルスクリプトは展開時に実行権限を付与する。

## 6. CLI 制御チャネル

`zai` は既定で GUI を起動する。**既知のサブコマンド名が第1引数に来たときだけ** CLI として動作する。
それ以外（パス・存在しない語）は従来どおりワークスペース指定として扱う。

```
zai open <file> [--line N]
zai notify <message> [--level info|warn|error]
zai prompt <text> [--agent NAME] [--submit]
zai run <command...>
zai panel <panel-id> <text>
zai status <text>
zai state                       # 実行中インスタンスの状態を JSON で出力
zai plugin list
zai plugin new <name>
zai plugin enable <name>
zai plugin disable <name>
zai --help | -h
zai --version | -V
```

### 接続方式
実行中インスタンスは起動時に `~/.zaivern/instance.json` を書く:

```json
{"port":8900,"token":"dc3143dcc1","workspace":"/path","pid":12345}
```

CLI はこれを読み、既存のローカル HTTP サーバへリクエストを送る。
- インスタンスが無い／`pid` が死んでいる場合は、標準エラーへ日本語で明示して終了コード 1。
- ファイルは終了時に削除する。

## 7. 用語・文言の制約

- コード・コメント・ドキュメント・UI 文言に、他社製品名や由来を示す記述を一切書かない。
- UI 文言はすべて日本語。既存のトーンに合わせる。
