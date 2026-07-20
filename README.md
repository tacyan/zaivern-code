# ⚡ Zaivern Code

**Rust製 AI-Native コードエディタ** — Zed の速度 × Cmux の並列エージェント × AGI Cockpit の操縦席UX

Claude Code などの CLI エージェントを「本物の PTY ターミナル」として複数同時に走らせながらコードを編集できる、クロスプラットフォーム(macOS / Windows / Linux)のデスクトップエディタです。GUI は egui による GPU 描画で、依存ランタイム(Electron 等)はありません。

---

## 主な機能

### 📝 エディタ(Zed インスパイア)
- **syntect による構文ハイライト**(Rust / TS / Python / Go / Markdown ほか多数、拡張子から自動判定)
- タブ・行番号ガター・未保存インジケータ(●)・閉じる前の保存確認
- ファイルツリーで **VS Code 同等のファイル操作**: ➕新規ファイル / 🗂新規フォルダ(ツリー内インライン入力、Enter確定・Escキャンセル)、✏名前の変更(開いているタブのパス・言語も自動追従)、🗑削除(確認ダイアログ付き。未保存の変更があるタブは内容を保持)
- 右クリックメニュー: エディタで開く / 新規作成 / 名前変更 / 削除 / 「パスをエージェントに送信 (@path)」 / フルパスをコピー(ファイル・フォルダ両対応)
- ファイル内検索(⌘F、ヒット件数表示・次へジャンプ — ヒット行は画面中央にジャンプ)
- **VS Code 同等のスクロール**: 行番号ガターは水平スクロールで動かない固定表示、最終行を越えてスクロールできる余白(scrollBeyondLastLine)、PageUp/PageDown で1画面分のカーソル移動+スクロール
- ファジー検索コマンドパレット(⌘P でファイル、⌘⇧P でコマンド)
- Git ブランチ表示、日本語 UI フォント自動フォールバック

### 🤖 マルチエージェント(Cmux インスパイア)
- **portable-pty + vt100 による本物のターミナル**。Claude Code のようなフルスクリーン TUI がそのまま動作
- エージェントプリセットをワンクリック起動(⌘⇧A)し、**複数セッションを並列実行**
- セッションごとの稼働状態(●/○)・稼働時間・再起動・強制終了
- 256色 / TrueColor、ブラケットペースト、スクロールバック(マウスホイール)対応

### 🎛 Agent Cockpit(AGI Cockpit インスパイア)
- 全エージェントセッションを **グリッドで一覧表示**(⌘⇧C)。各セルは直接操作可能なライブターミナル
- **📣 ブロードキャスト**: 1つの入力欄から全稼働セッションへ同じプロンプトを一斉送信
- セルから即フォーカス・再起動・終了

### 🛡⚡🤖 権限モード(3モード、ツールバーでワンクリック切替)
`claude` / `codex` / `agy` で始まるコマンドのプリセットに自動適用されます(プリセット側でフラグを書く必要なし)。

- **🛡 承認**(デフォルト): 操作のたびにユーザーの許可が必要。コマンドに紛れた bypass 系フラグ(`--dangerously-skip-permissions`、`--permission-mode bypassPermissions` 等)は**自動除去**して安全側に倒します
- **⚡ 全自動**: すべて自動 YES。各 CLI の bypass フラグを自動付与(claude/agy: `--dangerously-skip-permissions`、codex: `--dangerously-bypass-approvals-and-sandbox`)。さらに bypass 起動でも残る対話プロンプト(初回 Bypass 警告・フォルダ信頼確認・プラン承認など)は**画面テキストを検知して自動応答**する二段構え
- **🤖 Agent優先**: プリセットのコマンドに書かれたフラグをそのまま尊重。「(全自動)」プリセットと通常プリセットを使い分けたい場合はこれ
- 切替は次に起動するセッションから適用。現在のモードはステータスバーに常時表示
- **実行中のセッション**には各行の 🛡 ボタン(または「🛡 全切替」)で権限モード切替を送信できます

### 🔔 通知 + 効果音
- エージェントが**承認待ち**になると右下にポップアップ + セッションの●が黄色に変化
- エージェントの**終了**を成功(✅)/失敗(❌ + exit code)で通知
- 承認待ち・完了・エラーを **OS 標準の効果音**でお知らせ(macOS: Ping / Glass / Basso。オフ可)
- テーマ切替・モード切替などの操作も全てトースト通知
- ウィンドウが非フォーカスのときは **macOS 通知センター**(Linux: notify-send)へも送信

### 🦀 デスクトップペット「ザイガニ」(clawd-on-desk インスパイア)
- 画面右下をうろうろ歩くペット。まばたき・視線のカーソル追従・お散歩、放置で居眠り→熟睡(💤)、操作すると起き抜けのびっくりホップ
- **エージェントに連動してリアクション**: 稼働中は「⚙ n」の足踏み(稼働数に応じて高速化)、3体以上でノリノリ(🎵)、**承認待ちで「❗承認待ち」とそわそわ**、成功で🎉ジャンプ、失敗で💥バツ目
- **💬 承認バブル**: 承認待ちになるとペットの頭上にカードが浮かび、**✔承認 / ✖拒否 / 開く** をワンクリックで実行(送信キーは `pet_approve_keys` / `pet_deny_keys` でカスタマイズ可)
- クリックで Cockpit 開閉(承認待ちがあればそのセッションへジャンプ)、ダブルクリックでご機嫌、連打で怒る(ぷるぷる)、ドラッグで好きな位置へ移動(自動保存)
- **🎭 見た目 4種**(ブロック / カニ / ネコ / クラウド)+ 好きな**画像への差し替え**、📏 サイズ 3段階(小/中/大)
- ツールバーの 🐾 メニューで表示・見た目・サイズ・散歩・居眠り・効果音・承認バブルを個別に切替。設定は自動保存

### 🔌 プラグイン(自作して、配って、もらう)
VS Code 拡張(Node ランタイム前提)は Zaivern では動かないため、代わりに**シェルさえ書ければ誰でも作れる独自プラグインシステム**を搭載しています。サイドバーの「🔌 プラグイン」タブ、またはコマンドパレットから操作します。

プラグインは `~/.zaivern/plugins/<名前>/` に置く 1 フォルダで、`plugin.toml` に次の 3 種類を宣言できます。

- **▶ コマンド**: 任意のシェルコマンドを実行し、結果をエディタへ反映
  - `input` = `none` | `selection`(選択範囲を stdin へ)| `file`(ファイル全体を stdin へ)
  - `output` = `replace`(置換)| `insert`(カーソル位置へ挿入)| `new_tab` | `notify` | `silent`
  - `langs = ["rust"]` で言語を絞り、`keybind = "cmd+alt+f"` でショートカット起動、`on_save = true` で**保存時に自動実行**(フォーマッタ向け。整形後は自動で再保存)
  - 環境変数 `ZV_FILE` / `ZV_LANG` / `ZV_WORKSPACE` / `ZV_PLUGIN_DIR` を参照可能。タイムアウト付きでバックグラウンド実行され、実行中にバッファを編集した場合は上書きしません
- **🎨 テーマ**: カラーテーマ JSON(VS Code 互換形式・JSONC 可)を同梱。🎨 メニューとプラグインタブから適用。`~/.zaivern/themes/*.json` に置いた単体テーマも自動で並びます
- **✂️ スニペット**: スニペット JSON(VS Code 互換形式)を同梱。対応言語の編集中に **prefix を入力して Tab** で展開(`${1:default}` タブストップ・`$0`・変数対応、日本語安全)

使い方は 3 ボタン:

- **➕ 新規作成**: 名前を入れるとサンプル入りのテンプレート一式を生成し、`plugin.toml` をそのまま開きます。編集したら ⟳(再スキャン)で即反映
- **📤 エクスポート**: `<名前>-<バージョン>.zvplug`(実体は ZIP)をワークスペースに書き出し。これを配るだけで共有できます
- **📦 インストール**: 受け取った `.zvplug` / `.zip` を選ぶだけ。アンインストールは 🗑

```toml
# plugin.toml の例: 保存時に JSON を自動整形
[plugin]
name = "json-fmt"
version = "0.1.0"
description = "保存時に JSON を整形"

[[command]]
title = "JSON を整形"
run = "python3 -m json.tool"
input = "file"
output = "replace"
langs = ["json"]
on_save = true
keybind = "cmd+alt+f"
```

- **🔤 言語サーバー(LSP)**: `rust-analyzer` / `typescript-language-server` / `pyright-langserver` / `gopls` が PATH にあれば自動起動し、**診断(エラー/警告)を表示**します。行番号ガターが赤/黄に色付き、ステータスバーに `⛔件数 ⚠件数`。サーバーが無い環境でも通常どおり編集できます
  - 導入例: `rustup component add rust-analyzer` / `npm i -g typescript-language-server typescript` / `npm i -g pyright` / `go install golang.org/x/tools/gopls@latest`

### ⌨️ 日本語入力(IME)
- ターミナル内で日本語がそのまま打てます。変換中の未確定文字はカーソル位置に下線付きでオーバーレイ表示され、確定分だけが Claude Code へ送信されます

### 🌿 Git 行ガター
- git リポジトリのファイルは行番号が差分で色分けされます: **緑 = 追加行 / 黄 = 変更行**
- ステータスバーにブランチ名 + 変更ファイル数(±N)を表示

### ⌨️ キーバインドのカスタマイズ
- `config.toml` の `[keybindings]` で全ショートカットを上書き可能(`save = "cmd+s"` 形式)
- action 名: `save` `save_as` `close_tab` `new_file` `palette_files` `palette_commands` `toggle_terminal` `toggle_sidebar` `find` `toggle_cockpit` `new_agent` `font_inc` `font_dec` `toggle_comment` `duplicate_line` `move_line_up` `move_line_down`
- 修飾キー: `cmd` `ctrl` `shift` `alt`(=`option`)、キー: 英数字 / `f1`-`f12` / `up` `down` `left` `right` / `enter` `tab` `escape` `space` / `backtick` `plus` `minus` `slash` `comma` `period`

### 💾 セッション復元
- 再起動時に前回開いていたタブ・アクティブタブ・パネル状態をワークスペースごとに自動復元(`~/.zaivern/sessions/`)

### 📱 スマホリモート
PC で Zaivern Code を起動中、**同じ Wi-Fi(LAN)内のスマホからブラウザで操作**できます。

- **使い方**: トップバーの 📱 ボタン → 表示される QR コードをスマホで読み取るだけ(URL コピーも可)。起動ログにも接続 URL が出ます
- **できること**: 開いているファイルの閲覧・編集・保存、タブ切替、ワークスペースのファイル検索&オープン、エージェント(Claude Code 等)のターミナル画面閲覧・指示送信・承認操作(Enter / Esc / ^C / ↑ / ↓ / Tab / ⇧Tab 権限切替 / 1 / 2 / 3 / y ボタン)、各種コマンド(保存・新規・ターミナル・Cockpit・フォント±・承認モード切替など)
- **仕組み**: 内蔵の極小 HTTP サーバ(ポート 8899、使用中なら 8900〜8919 に自動フォールバック)。依存クレート追加なしの `std::net` のみ
- **セキュリティ**: 起動ごとにランダム生成されるトークンで認証(QR の URL に埋め込み済み)。トークンなしの API アクセスは 401 拒否。LAN 内のみ

---

## インストール

ワンライナー(Rust が無ければ rustup ごと自動セットアップ):

```bash
curl -fsSL https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.sh | sh
```

Rust 導入済みなら直接:

```bash
cargo install --git https://github.com/tacyan/zaivern-code --locked
```

いずれも `~/.cargo/bin/zai` に配置されます。起動は `zai [ワークスペースのパス]`。

---

## ビルドと起動

```bash
# 要 Rust 1.88+(rustup update stable)
cargo build --release

# 起動(引数でワークスペースを指定。省略時はカレントディレクトリ)
./target/release/zai ~/dev/my-project
```

macOS / Windows / Linux で同一コードのままビルドできます(Linux は要 `libgtk-3-dev` 等の rfd 依存)。

---

## キーバインド

| キー | 動作 |
|---|---|
| ⌘P (Ctrl+P) | ファイルをファジー検索して開く |
| ⌘⇧P | コマンドパレット(`>` プレフィックス) |
| ⌘S / ⌘⇧S | 保存 / 名前を付けて保存 |
| ⌘N / ⌘W | 新規ファイル / タブを閉じる |
| ⌘F | ファイル内検索 |
| ⌘/ | 行コメントのトグル |
| ⌘⇧D | 行を複製 |
| ⌥↑ / ⌥↓ | 行を上下に移動 |
| PageUp / PageDown | 1画面分のカーソル移動 + スクロール |
| Enter | 自動インデント(直前行の字下げ + `{ ( [ :` 後は追加字下げ) |
| ⌘J または ⌘\` | ターミナル/エージェントパネルの表示切替 |
| ⌘⇧A | エージェント起動(プリセット1番) |
| ⌘⇧C | Agent Cockpit 切替 |
| ⌘B | サイドバー切替 |
| ⌘+ / ⌘- | フォント拡大 / 縮小 |

Windows / Linux では ⌘ を Ctrl に読み替えてください。ターミナル内では Ctrl+C 等の制御キー、矢印、Tab、Esc がそのまま PTY へ送られます(Shift/Option+Enter は改行として送信され、Claude Code の複数行入力に対応)。

---

## カスタマイズ — `~/.zaivern/config.toml`

初回起動時に自動生成されます。編集後はコマンドパレットの **「設定を再読み込み」** で即反映(パレットから **「設定 config.toml を開く」** で直接編集も可能)。

```toml
# テーマ: "zaivern-dark" | "zaivern-midnight" | "zaivern-light"
# または VS Code 互換テーマJSONへのフルパス
theme = "zaivern-dark"
editor_font_size = 15.0
terminal_font_size = 13.0
show_hidden_files = true

# 既定の権限モード (claude / codex / agy に自動適用)
#   "ask"   = 毎回ユーザー承認が必要(安全・デフォルト)
#   "auto"  = すべて自動YES(各CLIの bypass フラグを自動付与)
#   "agent" = Agent欄優先(プリセットのコマンドに書かれたフラグをそのまま使う)
approval_mode = "ask"

# デスクトップペット 🦀
show_pet = true
# pet_variant = "blocky"   # 見た目: "blocky" | "crab" | "cat" | "cloud"
# pet_scale = 1.0          # 大きさ: 0.75=小 / 1.0=中 / 1.4=大
# pet_free_roam = true     # うろうろ散歩
# pet_sleep = true         # 無操作で睡眠
# pet_sounds = true        # 効果音
# pet_bubbles = true       # 承認バブル
# pet_approve_keys = "\r"    # 承認時にPTYへ送るキー (Enter)
# pet_deny_keys = "\u001B"   # 拒否時にPTYへ送るキー (ESC)

# ── AIエージェントのプリセット(いくつでも追加可能)──
[[agents]]
name = "Claude Code"
icon = "🤖"
command = "claude"

[[agents]]
name = "Claude Code (全自動)"
icon = "⚡"
command = "claude --dangerously-skip-permissions"

[[agents]]
name = "Codex"
icon = "🧠"
command = "codex"

[[agents]]
name = "Codex (全自動)"
icon = "⚡"
command = "codex --dangerously-bypass-approvals-and-sandbox"

[[agents]]
name = "Antigravity"
icon = "🚀"
command = "agy"

[[agents]]
name = "Shell"
icon = "🖥"
command = ""          # 空文字はログインシェル

# [[agents]]
# name = "Claude (Opus 明示)"
# icon = "🧠"
# command = "claude --model claude-opus-4-8"
# env = { MAX_THINKING_TOKENS = "31999" }
```

- `command` はログインシェル(`$SHELL -lc`)経由で実行されるため、PATH や alias がそのまま使えます。
- `env` でプリセット固有の環境変数を注入できます(モデル指定・APIキー切替などに)。
- `cwd = "~/some/dir"` で作業ディレクトリを固定できます(省略時はワークスペース)。
- **プロジェクトごとの上書き**: ワークスペース直下に `.zaivern.toml` を置くと、テーマ・フォント・承認モード・追加エージェントをプロジェクト単位で設定できます。
- **UI での選択は `~/.zaivern/state.toml` に自動保存**されます(テーマ・承認モード・ペット関連)。手書きの config.toml を汚しません。「設定を再読み込み」実行時は config.toml が優先されます。

### Claude Code との連携ワザ
- ファイルツリーで右クリック →「🤖 パスをエージェントに送信」で `@path ` が入力される(Claude Code のファイル参照記法)
- コマンドパレット →「現在のファイルをエージェントに送信 (@path)」
- Cockpit のブロードキャストで、複数の Claude Code セッションに同じ指示を一斉送信
- 承認待ちはペットの承認バブルから **✔承認 / ✖拒否** をワンクリック、外出先はスマホリモートから

---

## アーキテクチャ

```
src/
├── main.rs          エントリポイント(eframe ブートストラップ)
├── app.rs           アプリ状態・レイアウト・ショートカット・パレット統合
├── theme.rs         3テーマ(Dark / Midnight / Light)と egui スタイル適用
├── theme_json.rs    カラーテーマ JSON(VS Code 互換形式)の取り込み
├── config.rs        ~/.zaivern/config.toml の読み込み・自動生成・プロジェクト上書き
├── editor.rs        バッファ・タブ管理
├── editor_ops.rs    テキスト編集操作の純関数(マルチバイト安全)
├── highlight.rs     syntect → egui LayoutJob 変換(ハッシュキャッシュ付き)
├── snippets.rs      VS Code 互換スニペットの解析・Tab 展開
├── file_tree.rs     遅延読み込みファイルツリー + コンテキストメニュー
├── fuzzy.rs         ファジーマッチスコアリング
├── palette.rs       コマンドパレットの状態とアクション定義
├── keybinds.rs      カスタマイズ可能なキーバインド
├── git.rs           git CLI 連携(ブランチ・行単位 diff マーク)
├── lsp.rs           最小 LSP クライアント(stdio JSON-RPC・診断)
├── terminal.rs      PTY セッション + vt100 描画 + 承認プロンプト検知/自動応答
├── agents.rs        セッション管理(起動/再起動/破棄/ブロードキャスト/権限モード適用)
├── remote.rs        スマホリモート(内蔵HTTPサーバ・QRコード・トークン認証)
├── session.rs       ワークスペースごとのセッション復元
├── notify.rs        OS ネイティブ通知
├── sound.rs         効果音(OS 標準サウンドを fire-and-forget 再生)
├── plugins.rs       独自プラグインシステム(コマンド/テーマ/スニペット/.zvplug)
├── pet.rs           デスクトップペット本体(状態機械 + 描画)
├── pet_variants.rs  ペットの見た目バリアント(カニ/ネコ/クラウド)
└── pet_bubble.rs    承認バブル(✔承認/✖拒否 カード)
```

- ターミナルは PTY 読み取りスレッド → `vt100::Parser`(Mutex)→ 毎フレームセル描画、という構成。ウィンドウリサイズに追従して PTY もリサイズされます。
- アプリ終了・セッション破棄時に子プロセスは自動 kill されます(孤児プロセスを残しません)。

## ロードマップ
- [x] キーバインドの config.toml カスタマイズ
- [x] Git 差分ガター(行番号色分け)
- [x] OS ネイティブ通知
- [x] セッション復元(タブ・パネル状態)
- [x] LSP 連携(診断表示 — rust-analyzer / tsserver / pyright / gopls)
- [x] 独自プラグインシステム(コマンド実行・保存時フック・テーマ・スニペット・.zvplug 配布)
- [x] スマホリモート(LAN 内ブラウザからの閲覧・編集・エージェント操作)
- [x] VS Code 同等のスクロール(固定ガター・scrollBeyondLastLine・PageUp/PageDown)
- [x] 権限モード 3種(🛡承認 / ⚡全自動 / 🤖Agent優先)+ 実行中セッションへの一括切替
- [x] ペット強化(見た目4種・画像差し替え・サイズ・睡眠/散歩・効果音・承認バブルからの承認/拒否)
- [ ] LSP 補完・ホバーの UI(基盤は実装済み、表示は今後)
- [ ] プラグインの文法(TextMate grammar)対応・レジストリ共有
- [ ] インライン diff ビュー
- [ ] スプリットエディタ

## ライセンス
Apache License 2.0 — 詳細は [LICENSE](LICENSE) を参照。
