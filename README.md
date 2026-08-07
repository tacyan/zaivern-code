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

## 🆕 最新リリース: v0.7.0

- 画面全体とファイル単位の2段階ズーム
- 配色テーマを3種類から11種類へ拡充
- TypeScript・Swift・Kotlin・Dart・Zig・TOML・Dockerfile・Terraformなどの構文ハイライト
- 6枚以上のCockpitタイルを読みやすく保つスクロール表示
- 停滞・異常判定の精度改善と指揮官エージェントの可視化
- ターミナル分割を新規エージェント起動と同じ挙動に統一

**品質:** 2,565テスト成功、`cargo fmt --check`差分なし、clippy警告0。

[v0.7.0の詳細](https://github.com/tacyan/zaivern-code/releases/latest) ・ [過去のリリース](https://github.com/tacyan/zaivern-code/releases)

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

プラグイン開発については[プラグインガイド](docs/plugins.md)と[仕様書](docs/PLUGIN_SPEC.md)を参照してください。

## コントリビューション

不具合報告・機能提案・Pull Requestを歓迎します。まず[Issues](https://github.com/tacyan/zaivern-code/issues)で既存の報告を確認してください。

## ライセンス

[Apache License 2.0](LICENSE)

---

<div align="center">

**エージェントは、もう十分に速い。次に速くなるのは、指揮するあなたです。**

</div>
