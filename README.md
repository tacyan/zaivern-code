<div align="center">

<img src="assets/Zaivern.png" width="120" alt="Zaivern Code" />

# ⚡ Zaivern Code

**Claude Code・Codex・Gemini CLIなど、複数のAIコーディングツールをひとつの画面で動かす。**<br>
macOS・Windows・Linuxで使える、Rust製のAI開発コックピットです。

[**日本語**](README.md) | [English](README.en.md)

[![Release](https://img.shields.io/github/v/release/tacyan/zaivern-code)](https://github.com/tacyan/zaivern-code/releases/latest)
[![CI](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml/badge.svg)](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)

[🌐 **公式サイト**](https://zaivern.com/) ・ [⬇️ **ダウンロード**](https://github.com/tacyan/zaivern-code/releases/latest) ・ [🗒️ **リリース履歴**](https://github.com/tacyan/zaivern-code/releases)

<a href="https://zaivern.com/">
  <img src="assets/zaivern-demo.gif" width="960" alt="Claude Code、Codex、Gemini CLIなどを並列操作するZaivern Codeの実演" />
</a>

</div>

## はじめての方へ

Zaivern CodeはAIそのものではなく、**複数のAIコーディングツールをまとめて操作するアプリ**です。まずはClaude Code・Codex・Gemini CLIなど、使いたいAIツールを1つインストールしてログインしてください。3つすべてを用意する必要はありません。

使い始めるまでの流れは3ステップです。

1. 使いたいAIコーディングツールをインストールしてログイン
2. 下のコマンドでZaivern Codeをインストール
3. プロジェクトのフォルダで`zai .`を実行

## AIエージェントを待たせない開発へ

Claude Codeが実装し、Codexがテストし、Gemini CLIがドキュメントを書く。Zaivern Codeは、散らばったターミナルを**ひとつの操縦席**にまとめます。

| これまで | Zaivern Code |
|---|---|
| エージェントごとにタブを行き来 | 全エージェントを1画面で監視・操作 |
| 同じ指示を何度も貼り付け | 1回の入力で全員へブロードキャスト |
| 承認待ちや停止を見落とす | 状態表示・通知・ワンクリック承認 |
| 作業中はデスクから離れられない | スマホから進捗確認・指示・承認 |

## 🚀 Zaivern Codeをインストール

**macOS / Linux**

```bash
curl -fsSL https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.sh | sh
zai .
```

**Windows PowerShell**

```powershell
irm https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.ps1 | iex
zai .
```

インストーラはお使いのOSに合うアプリを自動で取得します。同じコマンドをもう一度実行すれば最新版へ更新できます。

### 更新する

```bash
zai update            # 最新版を確認し、実行するコマンドを見せてから更新
zai update --check    # 最新かどうかを確認するだけ (何も実行しません)
zai update --yes      # 確認を求めずに更新
```

インストールした場所に合わせて更新手段が選ばれます（`~/.cargo/bin` にあれば `cargo install --force`、それ以外は上のワンライナー）。

### アンインストールする

```bash
zai uninstall --dry-run       # 消えるものと合計サイズを一覧表示（何も消しません）
zai uninstall                 # 一覧を出して y を確認してから削除
zai uninstall --keep-config   # 設定 (config.toml / state.toml) は残す
zai uninstall --yes           # 確認を求めずに削除
```

消えるのは**実行ファイル本体と `~/.zaivern`（設定・セッション記録・端末ログ）だけ**です。OSのアプリ一覧の登録も同時に解除します。PATH上に別の `zai` が残っている場合は、安全のため自動では消さず一覧に表示します。

### 最初の起動後にすること

1. 約2分のガイドツアーで画面の基本操作を確認
2. `+ Agent`を押す
3. インストール済みのAIツールを選ぶ
4. 入力欄へ作業内容を書いて送信

まずは1体だけで試し、慣れてから2体、3体と増やすのがおすすめです。

## できること

### 🎛 Agent Cockpit

複数のAIツールをタイル状に並べ、動いているか、止まっているかをひと目で確認できます。Claude Code、Codex、Gemini CLIを含む29種類の起動設定を収録しています。

### 📣 ブロードキャスト

ひとつの入力欄から、動いている全AIへ同じ指示をまとめて送れます。もちろん、1体だけを選んで指示することもできます。

### 🛡 承認と監視

「この操作を許可しますか？」という確認待ちや、停止・異常終了を検知して通知します。安全のため、自動承認は**初期状態ではオフ**です。

### 📋 フリート管理

各AIが考え中・編集中・実行中・確認中のどこにいるかを一覧表示します。慣れてきたら、同じ課題を複数のAIへ同時に解かせて結果を比較することもできます。

### 📱 スマホリモート

スマホから進捗確認、指示、承認、ファイル編集ができます。まずは同じWi-Fi内で簡単に試せます。

### 📝 コードエディタ

コードを読んだり、AIが変更した箇所を確認したりできるエディタを内蔵しています。画像・PDF・CSV・Markdownなどもアプリ内で開けます。

未保存のまま落ちても本文は失われません（Hot Exit）。次の起動で書きかけの内容がそのまま戻り、その間にディスク側が書き換わっていた場合は黙って上書きせず、差分を見せて選ばせます。

### ⚙ 設定

設定は画面から検索して変更できます（メニューの「ファイル ▸ ユーザー設定 ▸ 設定」／コマンドパレットの「設定を開く」）。説明つきの一覧、既定から変えたものだけを出す絞り込み、1項目ずつ／まとめて既定へ戻す操作があり、書き戻しは該当行だけを差し替えるので手書きのコメントは消えません。GUI に無い設定は同じ画面の「config.toml」ボタンからテキストで編集できます。

## 🆕 最新リリース: v0.8.0

**操作性の穴を22個埋めました。** superset・VS Code・cmux・orca・Zed を調査し、
「これが無いと使えない」欠落から順に潰しています。

**エディタとして成立させた**

- Gitのコミット・push・pull・ハンク単位ステージング・履歴をアプリ内で完結（これまでは必ずターミナルへ落ちる必要がありました）
- `.gitignore`を尊重し、ファイル索引の打ち切りを表示（`node_modules`が索引を食い潰し、⌘Pが無言で壊れていました）
- 未保存の内容を再起動後に復元（Hot Exit）。ディスク側が変わっていたら黙って上書きせず差分を見せます
- マルチカーソルで実際に編集できる（⌘Dは「選ぶ」だけでした）
- 取り消し履歴を自前で持ち、整形やコードアクションも1回の⌘Zで戻せる
- ドラッグ&ドロップでの移動と同名衝突の確認、ごみ箱、ツリー上での⌘Z

**VS Code相当へ**

検索の正規表現・大小区別・単語単位・「前へ」・全ヒット強調 ／ 診断の波線とホバー ／
対応括弧の強調と虹色括弧 ／ 縦ルーラー ／ インデント自動判別 ／
タブのMRU切替・ピン留め・プレビュー ／ ⌘Pの最近使った順・`:123`・`@`シンボル ／
2打鍵ショートカット（⌘K ⌘S）とキーバインド編集画面 ／ 設定画面 ／
問題パネルのワークスペース全体表示 ／ ターミナルのファイルパスとURLをクリックで開く

**AIコックピットとして**

- **追従モード** — エディタが、動いているAIの編集箇所を追いかけます
- **未読カーソル** — 「今どれが自分待ちか」へ1打鍵で移動
- **状態検知の刷新** — 画面の見た目からの推測をやめ、構造化出力とフックを使います。判定の出所も表示します
- **worktree隔離起動と衝突検出** — 2体が同じファイルを触っていたら、後で気付くのではなくその場で警告
- **差分レビューの集中モード** — 「2 / 5」と残件を出し、`]f`/`[f`でファイル間を移動
- **コスト上限とアラート** — セッション/日次の推定コストに上限を置くと、8割で警告、到達で通知。`stop`を選ぶと新規送信を止めて理由を出します（既定は通知だけ。上限が未設定なら1ピクセルも出ません）
- プリセットの⌃1〜⌃9起動、セッションの自動命名、トークン消費と推定コストの表示

**品質:** 3,134テスト、3OS（macOS・Linux・Windows）のCIすべて通過、`cargo fmt --check`差分なし。

[v0.8.0の詳細](https://github.com/tacyan/zaivern-code/releases/latest) ・ [過去のリリース](https://github.com/tacyan/zaivern-code/releases)

## 対応環境

| 項目 | 対応内容 |
|---|---|
| OS | macOS arm64/x86_64・Linux x86_64/arm64・Windows x86_64 |
| AI CLI | Claude Code・Codex・Gemini CLIほか、29種類のプリセット |
| Rust | ソースビルド時のみ1.88以上 |
| ライセンス | Apache-2.0 |

## 安全設計

- 承認必須を既定値にし、自動YESは明示的に有効化
- 権限昇格は常に手動承認
- MCP設定の環境変数は値を表示せず、設定済みかだけを表示
- SSHトンネル使用時はリモートサーバーを`127.0.0.1`のみにバインド
- セッション破棄・終了時に子プロセスを停止し、孤児プロセスを残さない

## よくある質問

### AIツールは同梱されていますか？

いいえ。Claude Code・Codex・Gemini CLIなど、使いたいAIツールを別途インストールしてログインしてください。

### 3種類すべて必要ですか？

いいえ。1種類だけでも使えます。最初は普段使っているAIツール1つで試すのがおすすめです。

### Zaivern Codeは無料ですか？

Zaivern CodeはApache-2.0ライセンスの無料オープンソースソフトウェアです。各AIサービスの利用料金や契約は別途必要です。

### 勝手にコマンドを実行しませんか？

初期状態では承認が必要です。自動承認を使う場合も、自分で明示的にオンへ切り替えます。

## ソースからビルド

```bash
git clone https://github.com/tacyan/zaivern-code.git
cd zaivern-code
rustup update stable
cargo run --release -- .
```

### テスト

```bash
cargo fmt --all --check
cargo nextest run --profile ci
```

### 他OSの検証（macOSからでもローカルで回せます）

`#[cfg(windows)]` や Linux 限定のコードは、macOSのビルドでは一度もコンパイルされません。
CIの往復を待たずに手元で潰せます。

```bash
tools/linux-test.sh              # Linuxのテストを Docker で再現
tools/windows-check.sh           # Windows(MSVC)向けの型検査
tools/windows-check.sh --build   # 実際に zai.exe を作る（リンクまで確認）
```

Windows側は初回のみ `cargo install cargo-xwin --locked` が必要です。
どちらのスクリプトもホストの `target/` は汚しません。

プラグイン開発については[プラグインガイド](docs/plugins.md)と[仕様書](docs/PLUGIN_SPEC.md)を参照してください。

## コントリビューション

不具合報告・機能提案・Pull Requestを歓迎します。まず[Issues](https://github.com/tacyan/zaivern-code/issues)で既存の報告を確認してください。

## ライセンス

[Apache License 2.0](LICENSE)

---

<div align="center">

**エージェントは、もう十分に速い。次に速くなるのは、指揮するあなたです。**

</div>
