<div align="center">

<img src="assets/Zaivern.png" width="120" alt="Zaivern Code" />

# Zaivern Code

**Claude Code・Codex・Gemini CLI など、いま使っている AI コーディング CLI をひとつの操縦席へ。**<br>
macOS・Windows・Linux で動く、Rust 製の AI 開発コックピットです。

[English](README.md) | [日本語](README.ja.md)

[![Release](https://img.shields.io/github/v/release/tacyan/zaivern-code)](https://github.com/tacyan/zaivern-code/releases/latest)
[![CI](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml/badge.svg)](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)

[**ダウンロード**](https://github.com/tacyan/zaivern-code/releases/latest) ·
[**クイックスタート**](#クイックスタート) ·
[**ドキュメント**](#ドキュメント) ·
[**公式サイト**](https://zaivern.com/)

<a href="https://zaivern.com/">
  <img src="assets/zaivern-demo.gif" width="960" alt="Claude Code・Codex・Gemini CLI などを並べて動かす Zaivern Code の実演" />
</a>

Zaivern Code が役立ちそうなら、⭐ **Star** で開発を応援してください。

</div>

## なぜ Zaivern Code か

AI コーディング CLI を何本も起動するのは簡単ですが、把握し続けるのは大変です。
どのエージェントも別のタブに住み、それぞれの都合で承認を求め、他が何をしているかを
知らないままファイルを書き換えます。

| コックピットが無いとき | Zaivern Code |
|---|---|
| 誰が待っているかタブを巡って探す | 全エージェントを 1 画面で、状態つきで見る |
| 同じ指示を各ツールへ貼り直す | 1 回でフリート全体へ送る／1 体だけを指名する |
| 承認待ちを見落として実行が止まる | 通知とワンクリック承認 |
| 動いている間はデスクを離れられない | スマホから進捗確認と承認 |
| 並列度を上げるほどマージ衝突が増える | 共有台帳で担当行を分ける |

Zaivern Code は AI そのものではなく、AI も同梱しません。すでにインストールして
ログイン済みの CLI を動かす道具です。1 つあれば始められます。

## クイックスタート

**前提** — 対応する AI コーディング CLI を最低 1 つインストールしてログインしてください。
Claude Code・Codex・Gemini CLI を含む 33 種類の起動プリセットを収録しています。
3 つすべてを揃える必要はありません。

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

どちらのインストーラも、取得したアーカイブを**展開する前に**リリースの
`checksums.txt` と SHA-256 を突き合わせます。一致しない場合、あるいは
チェックサム自体を取得できなかった場合は、**展開も実行もせずに中止**します。

スクリプトをシェルへ流し込むのに抵抗がある場合は、
[Releases](https://github.com/tacyan/zaivern-code/releases/latest) から自分の環境向けの
アーカイブを取得し、展開した `zai`（Windows は `zai.exe`）を `PATH` 上に置いてください。
あとはプロジェクトのフォルダで `zai .` を実行します。手で検証する方法・ビルド来歴の
確認・SBOM については [SECURITY.md](SECURITY.md) を参照してください。

画面が開いたら:

1. `+ Agent` を押して、インストール済みの CLI を選ぶ
2. 入力欄へ作業内容を書いて送る
3. 慣れてきたら 2 体目を足す

### 更新する

```bash
zai update            # 最新版を確認し、実行するコマンドを見せてから更新
zai update --check    # 確認するだけ（何も実行しません）
zai update --yes      # 確認を求めずに更新する
```

`zai update` はエディタが起動していなくても使えます。インストール方法
（インストーラ / `cargo install`）を判別して、適切なやり方で更新します。
上のワンライナーを再実行しても同じことができます。

削除は `zai uninstall`（`--dry-run` で消える対象を一覧表示）。削除の対象は
実行ファイル本体と `~/.zaivern` だけで、`PATH` 上の別の `zai` は一覧に出すだけで
消しません。

## 主な機能

### Agent Cockpit

複数の AI CLI をタイル状に並べ、考え中・編集中・実行中・確認待ちのどれかをひと目で
把握できます。33 種類の起動プリセットを内蔵しているので、コマンドラインを思い出さずに
2 クリックでエージェントを足せます。

### ブロードキャスト

ひとつの入力欄から、動いている全エージェントへ同じ指示を送れます。1 体だけを選んで
指示することもできます。同じ訂正をフリート全体へ入れたいときに効きます。

### 状態・承認・通知

権限の確認待ち、停滞、予期しない終了を通知として拾い、ワンクリックで対処できます。
自動承認は既定でオフで、使うときは自分で明示的にオンにします。

### スマホリモート

スマホから進捗確認・指示・承認・ファイル編集ができます。いちばん簡単なのは同じ
Wi-Fi 内での利用で、そうでない場合は SSH トンネル経由になります。

### 競合コーディネーション

これから編集するファイル（あるいは個々の行域）を共有台帳で確保し、ぶつかる書き込みを
git フックが断ります。何を防げて何を防げないかは次の節に書きます。

### 内蔵エディタ

AI が変更した箇所をアプリから離れずに読めます。画像・PDF・CSV・Markdown も開けます。
未保存のまま落ちても本文は失われず、次の起動で書きかけが戻ります。その間にディスク側が
書き換わっていた場合は、黙って上書きせず差分を見せます。

## 競合コーディネーション

これから編集するファイルと行域を、リポジトリごとの共有台帳へ記録し、ぶつかる書き込みを
git フックが断ります。衝突はマージの段階ではなく、起きたその場で表に出ます。

拾えないのは意味的な競合です。片方が関数のシグネチャを変え、もう片方が別のファイルで
古い呼び方のまま書く。マージは綺麗に通り、それでもコードは壊れています。

```console
$ zai czero init      # 台帳・git フック・マージドライバを入れて自己診断
$ zai czero verify    # 使い捨てのリポジトリで実際に競合を起こし、止まるかを確かめる
```

適用範囲・制約・その根拠となる実測は
[docs/conflict-zero.md](docs/conflict-zero.md) にあります。

## 対応環境

| 項目 | 対応内容 |
|---|---|
| OS | macOS arm64/x86_64・Linux x86_64/arm64・Windows x86_64 |
| AI CLI | Claude Code・Codex・Gemini CLI ほか、33 種類の起動プリセット |
| Rust | 1.88 以上 — ソースからビルドするときのみ |
| ライセンス | Apache-2.0 |

Claude Code が実装し、Codex がテストし、Gemini CLI がドキュメントを書く、というのは
よくある構成の一例で、Zaivern Code はその分担を前提にしていません。どの組み合わせでも、
1 体だけでも動きます。

## 安全設計

- 承認必須が既定。自動 YES はセッションごとに明示的に有効化します
- 権限昇格は常に手動承認です
- MCP の環境変数は値を表示せず、設定済みかどうかだけを出します
- セッション破棄時とアプリ終了時に子プロセスを停止し、孤児プロセスを残しません

## ドキュメント

| 文書 | 何が書いてあるか |
|---|---|
| [docs/conflict-zero.md](docs/conflict-zero.md) | 「競合ゼロ」が主張すること・しないことと、その実測 |
| [docs/czero-repo-shapes.md](docs/czero-repo-shapes.md) | リポジトリの形ごとに何が保証されるか |
| [docs/plugins.md](docs/plugins.md) | プラグインの書き方と[仕様書](docs/PLUGIN_SPEC.md) |
| [docs/README.md](docs/README.md) | 他の全文書の索引（何を裏付ける文書かで分類） |

各版のリリースノートは
[Releases ページ](https://github.com/tacyan/zaivern-code/releases)にあります。

## コントリビューション

不具合報告・機能提案・Pull Request を歓迎します。新しく立てる前に
[Issues](https://github.com/tacyan/zaivern-code/issues) で既存の報告を確認し、
[Pull Request](https://github.com/tacyan/zaivern-code/pulls) は `main` へ向けてください。

```bash
git clone https://github.com/tacyan/zaivern-code.git
cd zaivern-code
rustup update stable
cargo run --release -- .
```

変更の検証手順、Linux と Windows の確認を手元で回す方法、このリポジトリの規約は
[CONTRIBUTING.md](CONTRIBUTING.md) にあります。

## ライセンス

[Apache License 2.0](LICENSE)

---

<div align="center">

**エージェントは、もう十分に速い。次に速くなるのは、指揮するあなたです。**

</div>
