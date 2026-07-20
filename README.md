<div align="center">

<img src="assets/Zaivern.png" width="140" alt="Zaivern Code" />

# ⚡ Zaivern Code

**Claude Code・Codex・Gemini CLI を並列で指揮する、Rust製 AI Agent Cockpit。**

これはコードを書くための道具ではありません。<br>
**AIエージェントの群れを従え、開発そのものを指揮するための操縦席**です。

[**日本語**](README.md) | [English](README.en.md)

[![Release](https://img.shields.io/github/v/release/tacyan/zaivern-code)](https://github.com/tacyan/zaivern-code/releases/latest)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)

</div>

---

## あなたのボトルネックは、もう「書く速度」ではない

Claude Code に実装を任せ、Codex にテストを書かせ、Gemini CLI にドキュメントを整えさせる——そんな開発は、もう未来の話ではありません。けれど今、あなたの手元にあるのは、散らばったターミナルのタブだけ。

- どのエージェントが動いていて、どれが止まっているのか、一目では分からない
- 承認待ちのまま 30 分放置されていた Claude Code。気づいた瞬間の、あの脱力感
- 同じ指示を 3 つのタブに 3 回ペーストする、無意味な往復

エージェントは疲れません。文句も言いません。**待たせているのは、いつも人間の側です。**

Zaivern Code は、この「指揮の摩擦」を消すために生まれました。すべてのエージェントを一望し、一言で全軍に命じ、呼ばれたら 1 クリックで応える。あなたはコードを「書く人」から、開発を**「指揮する人」**になります。

---

## 🚀 30秒で操縦席へ

**macOS / Linux** — ビルド済みバイナリを自動取得(無ければソースからビルド):

```bash
curl -fsSL https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.sh | sh
```

**Windows** — PowerShell:

```powershell
irm https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.ps1 | iex
```

インストールが終わったら、開きたいプロジェクトのフォルダで `zai .` と打つだけ——そこがあなたの操縦席になります(`zai [ワークスペースのパス]` でも指定可)。**同じワンライナーをもう一度実行すれば、最新版へ更新されます。** 各 OS のビルド済みバイナリは [**Releases**](https://github.com/tacyan/zaivern-code/releases/latest) から直接ダウンロードもできます(macOS arm64/x86_64・Linux x86_64/arm64・Windows x86_64)。

---

## 操縦席から見える景色

### 🎛 全軍を、一望する — Agent Cockpit

ツールバーの **🎛 Cockpit** を押した瞬間(ショートカットは ⌘⇧C)、走っているすべてのエージェントがグリッドに並びます。各セルは飾りのプレビューではなく、**そのまま打ち込めるライブターミナル**。Claude Code が実装を進め、Codex がテストを直し、Gemini CLI がドキュメントを書く——5体が同時に手を動かす画面をはじめて見たとき、少し鳥肌が立つはずです。

### 📣 一言で、全員に命じる — ブロードキャスト

「テストが通ることを確認して」「その方針で OK、続行」——1つの入力欄から、稼働中の全セッションへ同時送信。タブを渡り歩いて同じ文章をペーストして回る夜は、今日で終わりです。

### 🛡 手綱は、常にあなたの手に — 3つの権限モード

攻めるときは **⚡全自動**:各 CLI の bypass フラグを自動付与し、それでも残る対話プロンプト(初回警告・フォルダ信頼確認・プラン承認など)は画面テキストを検知して自動応答する二段構え。守るときは **🛡承認**:コマンドに紛れ込んだ bypass 系フラグを自動で取り除き、安全側に倒します。プリセットの記述をそのまま尊重する **🤖Agent優先** も。ツールバーからワンクリックで切替、実行中のセッションにも一括送信できます。

**速さと安全は、二者択一ではありません。**

### 🔔 呼ばれたら、必ず気づく — 通知と、相棒の🦀ザイガニ

エージェントが承認を求めた瞬間——ポップアップが出て、効果音が鳴り、セッションの●が黄色に変わり、そして画面の隅を歩くデスクトップペット「**ザイガニ**」が「❗承認待ち」とそわそわし始めます。頭上に浮かぶバブルから **✔承認 / ✖拒否** をワンクリック。成功すれば🎉とジャンプし、失敗すれば💥とバツ目になる。

冷たいログを監視するのではなく、**相棒が肩を叩いてくれる**。エージェントを一秒も待たせない開発は、想像よりずっと気持ちがいいものです。

### 📱 席を立っても、指揮は続く — スマホリモート

トップバーの 📱 から QR コードを読むだけで、同じ Wi-Fi 内のスマホがリモコンになります。ソファから、ベランダから、コーヒーを淹れながら——承認も、指示出しも、ファイル編集も、進捗確認も。**エージェントが働いている間、あなたが机に縛られる理由はもうありません。**(起動ごとのランダムトークン認証・LAN 内のみ)

### 🎤 話すだけで、指示が書ける — マイクボタンひとつの音声入力

**🎤 を押す。それだけ**です。あとは話した内容が、エージェントの入力欄に流れ込み続けます。押しっぱなしのキーも、覚えるショートカットも、別ウィンドウのブラウザも要りません。止めたくなったら、隣の **⏹ を押す**まで動き続けます。

大事なのは、**そこで止まる**こと。音声はよく誤認識します。だから Zaivern は Enter を送りません。入力欄に入った文字を目で見て、直したければ直して、納得してから自分で Enter を押す。**声は速く、送信は慎重に。**

そして **Enter で入力欄が空になっても、録音は続いたまま**です。送った次の瞬間にはもう次の指示を話し始められる——考えるリズムが途切れません。

- 届け先は **🎯 アクティブなエージェント** か **📣 全エージェント**。録音したまま切り替えられます
- 「アクティブ」を選んでおけば、タブを移った先へ自動的についてきます
- 「送信」などの合図キーワードを設定すれば、その言葉で終えたときだけ Enter まで送ります(既定はオフ = 常に手動)
- 言語・エンジンはトップバー 🎤 の隣の ▾ メニューから変更できます
- macOS は OS 内蔵の音声認識(オフライン対応)。他 OS では `voice_command` に好きな認識エンジンを差し込めます

### 📝 そして、最後の一筆は自分の手で — Zed インスパイアのエディタ

AI が 9 割を書く時代でも、最後の 1 割——設計の勘所、名前の選び方、責任を持つ一行——は人間のものです。だから Zaivern は、指揮官の椅子のすぐ横に切れ味のいいペンを置きました。syntect による構文ハイライト、LSP 診断、Git 差分ガター、ファジーパレット(⌘P)、VS Code 同等のファイル操作とスクロール。**書きたくなった瞬間に、書ける。**

---

## なぜ Rust か — コックピットが重くては、話にならない

- **Electron も Node も無し。** egui による GPU 描画のネイティブバイナリ 1 個。起動は一瞬、アイドル時のメモリはブラウザのタブ 1 枚より軽い
- **本物の PTY ターミナル**(portable-pty + vt100)。Claude Code のフルスクリーン TUI がそのまま動く。256色 / TrueColor、ブラケットペースト、スクロールバック対応
- **macOS / Windows / Linux 同一コード。** アプリ終了時に子プロセスは自動 kill(孤児プロセスを残しません)
- 系譜: **Zed の速度 × Cmux の並列エージェント × AGI Cockpit の操縦席UX**

---

## 機能リファレンス

ここから下は、操縦席の計器ひとつひとつの説明書です。

### 📝 エディタ
- syntect による構文ハイライト(Rust / TS / Python / Go / Markdown ほか多数、拡張子から自動判定)
- タブ・行番号ガター・未保存インジケータ(●)・閉じる前の保存確認
- ファイルツリーで VS Code 同等のファイル操作: ➕新規ファイル / 🗂新規フォルダ(インライン入力)、✏名前の変更(開いているタブのパス・言語も自動追従)、🗑削除(確認ダイアログ付き)
- 右クリックメニュー: 開く / 新規作成 / 名前変更 / 削除 / 「パスをエージェントに送信 (@path)」 / フルパスをコピー
- ファイル内検索(⌘F、ヒット件数表示・ヒット行へ中央ジャンプ)
- VS Code 同等のスクロール: 固定ガター・scrollBeyondLastLine・PageUp/PageDown
- ファジー検索コマンドパレット(⌘P でファイル、⌘⇧P でコマンド)
- Git ブランチ表示、日本語 UI フォント自動フォールバック

### 🤖 マルチエージェント
- エージェントプリセットをワンクリック起動(⌘⇧A)し、複数セッションを並列実行
- セッションごとの稼働状態(●/○)・稼働時間・再起動・強制終了
- **29 種の CLI エージェントを内蔵カタログで認識**。Claude Code / Codex / Grok / Cursor / GitHub Copilot / OpenCode / MiMo Code / Amp / OpenClaude / Antigravity / Pi / oh-my-pi / Hermes / Devin / Goose / Auggie / Autohand / Crush / Cline / Command Code / Continue / Droid / Kilo Code / Kimi / Kiro / Mistral Vibe / Qwen Code / Rovo Dev / Aider
- 権限モード(🛡承認 / ⚡全自動)はカタログのエージェントに自動適用される。**プリセット側でフラグを書く必要はない**し、一括承認フラグを持たない Goose / Aider には環境変数の側で適用される
- カタログに無い CLI エージェントも、プリセット登録すればそのまま並列起動できる
- 実行中のセッションには各行の 🛡 ボタン(または「🛡 全切替」)で権限モード切替を送信

### 🔔 通知 + 効果音
- 承認待ち・完了(✅)・失敗(❌ + exit code)をポップアップ + OS 標準効果音で通知(オフ可)
- ウィンドウ非フォーカス時は macOS 通知センター(Linux: notify-send)へも送信

### 🦀 デスクトップペット「ザイガニ」
- まばたき・視線のカーソル追従・お散歩、放置で居眠り→熟睡(💤)、操作で起き抜けホップ
- エージェント連動: 稼働中は「⚙ n」の足踏み(稼働数で高速化)、3体以上でノリノリ(🎵)、承認待ちでそわそわ、成功🎉 / 失敗💥
- 💬 承認バブルから ✔承認 / ✖拒否 / 開く をワンクリック(送信キーは `pet_approve_keys` / `pet_deny_keys` でカスタマイズ可)
- クリックで Cockpit 開閉(承認待ちがあればそのセッションへジャンプ)、ドラッグで移動(自動保存)
- 🎭 見た目 4 種(ブロック / カニ / ネコ / クラウド)+ 好きな画像への差し替え、📏 サイズ 3 段階

### 🔌 プラグイン(自作して、配って、もらう)
シェルさえ書ければ誰でも作れる独自プラグインシステム。`~/.zaivern/plugins/<名前>/` に置く 1 フォルダで、`plugin.toml` に 3 種類を宣言できます。

- **▶ コマンド**: 任意のシェルコマンドを実行し、結果をエディタへ反映
  - `input` = `none` | `selection` | `file`、`output` = `replace` | `insert` | `new_tab` | `notify` | `silent`
  - `langs = ["rust"]` で言語を絞り、`keybind = "cmd+alt+f"` でショートカット起動、`on_save = true` で保存時に自動実行(フォーマッタ向け)
  - 環境変数 `ZV_FILE` / `ZV_LANG` / `ZV_WORKSPACE` / `ZV_PLUGIN_DIR` を参照可能。タイムアウト付きバックグラウンド実行、実行中にバッファを編集した場合は上書きしません
- **🎨 テーマ**: カラーテーマ JSON(VS Code 互換形式・JSONC 可)を同梱。`~/.zaivern/themes/*.json` の単体テーマも自動で並びます
- **✂️ スニペット**: VS Code 互換形式。prefix を入力して Tab で展開(`${1:default}` タブストップ・`$0`・変数対応、日本語安全)

操作は 3 ボタン: **➕ 新規作成**(テンプレート一式を生成)/ **📤 エクスポート**(`.zvplug` を書き出して配布)/ **📦 インストール**(受け取った `.zvplug` / `.zip` を選ぶだけ)。

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

### 🔤 言語サーバー(LSP)
`rust-analyzer` / `typescript-language-server` / `pyright-langserver` / `gopls` が PATH にあれば自動起動し、診断(エラー/警告)を表示。行番号ガターが赤/黄に色付き、ステータスバーに `⛔件数 ⚠件数`。サーバーが無い環境でも通常どおり編集できます。

導入例: `rustup component add rust-analyzer` / `npm i -g typescript-language-server typescript` / `npm i -g pyright` / `go install golang.org/x/tools/gopls@latest`

### ⌨️ 日本語入力(IME)
ターミナル内で日本語がそのまま打てます。変換中の未確定文字はカーソル位置に下線付きでオーバーレイ表示され、確定分だけがエージェントへ送信されます。

### 🌿 Git 行ガター
git リポジトリのファイルは行番号が差分で色分け(緑 = 追加行 / 黄 = 変更行)。ステータスバーにブランチ名 + 変更ファイル数(±N)。

### 📚 マルチフォルダ・ワークスペース
複数のフォルダを同時に開ける。`zai dirA dirB dirC` のように引数を並べるか、コマンドパレットの「フォルダをワークスペースに追加」から後付けする。

- ファイルツリーはルートごとに見出しを立てて並ぶ(1つだけのときは従来通りの見た目)
- ファイル検索は全ルートを横断。**同じ相対パスが複数のルートにある場合だけ**、フォルダ名を前置して区別する
- git はルートごとに実リポジトリを検出(`rev-parse --show-toplevel`)するので、リポジトリのサブディレクトリを開いても差分が正しく出る。同じリポジトリ内の 2 ルートは 1 つの git 状態を共有する
- セッション復元はルートの集合をキーにする。並び順が変わっても同じワークスペースとして復元される

### 🐙 GitHub 連携
`gh` コマンド経由で Pull Request / Issue の一覧、PR の差分閲覧、ブランチ操作。**追加の認証設定は不要**(`gh auth login` 済みならそのまま使える)。`gh` が入っていない環境では機能が安全に無効化される。

差分は追加行 / 削除行を色分けしたインライン diff ビューで読める。

### 🧭 外部 IDE で開く
いま編集しているファイルを、**カーソル行を保ったまま**別のエディタで開く。VS Code / Cursor / Zed / Trae / Kiro / Sublime / JetBrains 各種 / Xcode / Fleet / Neovide / Emacs に対応。

インストール済みの IDE は自動検出する。`code` コマンドが VS Code ではなく別製品のものに置き換わっている環境でも、実体を解決して正しく判別する。CLI が用意されていないアプリには URL スキームで受け渡す。

### 🛰 エージェント監視と再割り当て
複数のエージェントを走らせると、どれかが黙って止まる・同じ失敗を繰り返す・落ちる、といったことが起きる。それを検知して手を打つ層。

- **検知**: 停滞(進捗のない沈黙)、ループ(同じ出力の反復)、エラーの多発、異常終了、承認待ちの放置、出力の暴走。スピナーやカウンタは「進捗ではない」と正しく扱う
- **介入は段階的**: 記録 → 通知 → 自動承認 → 促し → 再起動 → 停止。**再起動と停止は既定で必ず確認を挟む**(作業中の内容が失われるため)。🛡承認モードでは通知より上の介入を自動実行しない
- **再割り当て**: 止まったタスクを別のエージェントへ引き継ぐ。一度失敗したエージェントには同じタスクを振り直さない。**前の担当が停止したと確認できるまで引き渡さない**(二重編集を防ぐため)。試行回数を使い切ったらループせずユーザーへ上げる
- **エージェント間通信**: 相手が待機中のときだけメッセージを届ける(生成中の割り込みは入力を壊すため)。ホップ数上限・流量制限・往復の検出があり、届かなかったメッセージは理由付きで記録される

### 💾 セッション復元
再起動時に前回のタブ・アクティブタブ・パネル状態をワークスペースごとに自動復元(`~/.zaivern/sessions/`)。

### 📱 スマホリモートの詳細
- **できること**: ファイルの閲覧・編集・保存、タブ切替、ワークスペース検索&オープン、エージェントのターミナル閲覧・指示送信・承認操作(Enter / Esc / ^C / ↑ / ↓ / Tab / ⇧Tab / 1 / 2 / 3 / y)、各種コマンド(保存・新規・Cockpit・フォント±・承認モード切替など)
- **仕組み**: 内蔵の極小 HTTP サーバ(ポート 8899、使用中なら 8900〜8919 に自動フォールバック)。依存クレート追加なしの `std::net` のみ
- **セキュリティ**: 起動ごとにランダム生成されるトークンで認証(QR の URL に埋め込み済み)。トークンなしの API アクセスは 401 拒否。LAN 内のみ

---

## インストール(手動)

ワンライナー(冒頭参照)が最速です。`install.sh` は GitHub Releases のビルド済みバイナリを `~/.local/bin/zai` へ配置し、対応バイナリが無い環境では Rust(無ければ rustup ごと自動セットアップ)でソースからビルドします。すでにインストール済みの場合は最新版への**更新**として動作します(古い `zai` が別の場所に残っていれば、そちらも同時に更新します)。

- **ビルド済みバイナリ**: [Releases](https://github.com/tacyan/zaivern-code/releases/latest) から自分の OS のアーカイブを取得し、展開した `zai`(Windows は `zai.exe`)を PATH の通った場所へ
- **ソースから**(要 Rust):

```bash
cargo install --git https://github.com/tacyan/zaivern-code --locked
```

`~/.cargo/bin/zai` に配置されます。

### ビルドと起動

```bash
# 要 Rust 1.88+(rustup update stable)
cargo build --release

# 起動(引数でワークスペースを指定。省略時はカレントディレクトリ)
./target/release/zai ~/dev/my-project

# 複数のフォルダを同時に開く
./target/release/zai ~/dev/frontend ~/dev/backend ~/dev/shared

# ファイルを引数に混ぜるとタブとして開く
./target/release/zai ~/dev/my-project README.md
```

macOS / Windows / Linux で同一コードのままビルドできます(Linux は要 `libgtk-3-dev` 等の rfd 依存)。

---

## キーバインド

| キー | 動作 |
|---|---|
| ⌘⇧C | **Agent Cockpit 切替** |
| ⌘⇧A | **エージェント起動(プリセット1番)** |
| ⌘J または ⌘\` | ターミナル/エージェントパネルの表示切替 |
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
| ⌘B | サイドバー切替 |
| ⌘+ / ⌘- | フォント拡大 / 縮小 |

Windows / Linux では ⌘ を Ctrl に読み替えてください。ターミナル内では Ctrl+C 等の制御キー、矢印、Tab、Esc がそのまま PTY へ送られます(Shift/Option+Enter は改行として送信され、Claude Code の複数行入力に対応)。

`config.toml` の `[keybindings]` で全ショートカットを上書きできます(`save = "cmd+s"` 形式)。action 名: `save` `save_as` `close_tab` `new_file` `palette_files` `palette_commands` `toggle_terminal` `toggle_sidebar` `find` `toggle_cockpit` `new_agent` `font_inc` `font_dec` `toggle_comment` `duplicate_line` `move_line_up` `move_line_down`。修飾キー: `cmd` `ctrl` `shift` `alt`(=`option`)。

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
name = "Gemini CLI"
icon = "✨"
command = "gemini"

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

### 指揮の小ワザ
- ファイルツリーで右クリック →「🤖 パスをエージェントに送信」で `@path ` が入力される(Claude Code のファイル参照記法)
- コマンドパレット →「現在のファイルをエージェントに送信 (@path)」
- Cockpit のブロードキャストで、複数の Claude Code セッションに同じ指示を一斉送信
- 承認待ちはペットの承認バブルから ✔承認 / ✖拒否 をワンクリック、外出先はスマホリモートから

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
├── file_tree.rs     遅延読み込みファイルツリー(複数ルート) + コンテキストメニュー
├── fuzzy.rs         ファジーマッチスコアリング
├── palette.rs       コマンドパレットの状態とアクション定義
├── keybinds.rs      カスタマイズ可能なキーバインド
├── git.rs           git CLI 連携(リポジトリ検出・ブランチ・行単位 diff マーク)
├── github.rs        GitHub 連携(gh CLI 経由・PR/Issue/差分・非同期)
├── diff.rs          unified diff パーサ + インライン diff ビュー
├── ide.rs           外部 IDE への受け渡し(現在行を指定して開く)
├── supervisor.rs    エージェント監視(停滞・ループ・異常終了の検知と段階的介入)
├── coordinator.rs   エージェント間通信とタスク再割り当て
├── lsp.rs           最小 LSP クライアント(stdio JSON-RPC・診断)
├── terminal.rs      PTY セッション + vt100 描画 + 承認プロンプト検知/自動応答
├── agents.rs        セッション管理(起動/再起動/破棄/ブロードキャスト/権限モード適用)
├── remote.rs        スマホリモート(内蔵HTTPサーバ・QRコード・トークン認証)
├── voice.rs         音声入力(止めるまで録音・入力欄へ挿入・エンジン差し替え)
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
- [x] 音声入力(🎤/⏹ だけで完結・止めるまで連続入力・入力欄へ挿入して手動 Enter・届け先/言語/エンジンを個別設定)
- [x] インライン diff ビュー(unified diff のパースと色分け描画)
- [x] マルチフォルダ・ワークスペース(複数のフォルダを同時に開く)
- [x] GitHub 連携(PR / Issue 一覧・PR 差分の閲覧・ブランチ操作)
- [x] エージェント・カタログ(29種の CLI エージェントを権限モードごと自動設定)
- [x] 外部 IDE 連携(カーソル行を保ったまま別エディタで開く)
- [x] エージェント監視(停滞・ループ・異常終了を検知して段階的に介入)
- [x] エージェント間通信とタスク再割り当て
- [x] ターミナル互換性強化(問い合わせ応答・カーソル形状・フォーカス通知・OSC 52)
- [ ] LSP 補完・ホバーの UI(基盤は実装済み、表示は今後)
- [ ] プラグインの文法(TextMate grammar)対応・レジストリ共有
- [ ] スプリットエディタ

## ライセンス
Apache License 2.0 — 詳細は [LICENSE](LICENSE) を参照。

---

<div align="center">

**エージェントは、もう十分に速い。**<br>
**次に速くなるのは、指揮するあなたです。**

</div>
