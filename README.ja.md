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
| 重いエディタがエージェントと機械を奪い合う | 単一のネイティブバイナリと damage 駆動の再描画 |

最後の行は標語ではなく設計上の制約です。Zaivern Code は単一のネイティブバイナリとして
配布され、ブラウザエンジンも Node ランタイムも同梱しません。再描画は damage 駆動で、
常時走るアニメーションループを持ちません。多数の PTY を同時に抱えても、メモリと
レイテンシが現実的な範囲に収まるのはこの構造のおかげです。アイドル時のコストは印象では
なく数値として扱います。`tools/idle-cpu.sh` が自分の機械で測り、素の `sleep` を下限に
置いて、合否の線ではなく CPU 時間の生の増分を出します。その測定で何が分かって何が
分からないかは [docs/idle-cost.md](docs/idle-cost.md) にあります。

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

スクリプトをシェルへ流し込むのに抵抗がある場合は、
[Releases](https://github.com/tacyan/zaivern-code/releases/latest) から自分の環境向けの
アーカイブを取得し、展開した `zai`（Windows は `zai.exe`）を `PATH` 上に置いてください。
あとはプロジェクトのフォルダで `zai .` を実行します。

画面が開いたら:

1. `+ Agent` を押して、インストール済みの CLI を選ぶ
2. 入力欄へ作業内容を書いて送る
3. 慣れてきたら 2 体目を足す

更新は `zai update`（`--check` は確認だけ、`--yes` は確認を省略）、削除は
`zai uninstall`（`--dry-run` で消える対象を一覧表示）。削除の対象は実行ファイル本体と
`~/.zaivern` だけで、`PATH` 上の別の `zai` は一覧に出すだけで消しません。

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

並列で走らせること自体は安いのですが、レビュー時に成果を突き合わせる作業は高くつきます。
Zaivern Code はリポジトリごとに「誰がどのファイル・どの行域を持っているか」の台帳を持ち、
git フックとマージドライバを入れて、**ぶつかる書き込みをその場で止めます**。マージの
段階になって初めて気付く、ということが起きません。

```console
$ zai czero init      # 台帳・git フック・マージドライバ・.gitattributes を入れて自己診断
$ zai czero verify    # 使い捨てのリポジトリで実際に競合を起こし、本当に止まるかを見せる
```

`verify` は設定を読むだけではありません。使い捨てのリポジトリを作って実際に競合を
起こし、各段が止めたかどうかを報告します（対象のリポジトリは 1 バイトも変更しません）。
`zai czero doctor` は段ごとに理由と直し方を出し、`zai czero uninstall` は入れたものだけを
戻します。

**防げるもの** — 同じ台帳を使い、担当行域が安全に離れているエージェント同士の
**git のマージ衝突**。

**防げないもの**

- **意味的な競合。** 片方が関数のシグネチャを変え、もう片方が古い呼び方のまま書く。
  行域は重ならず、マージも綺麗に通り、それでもコードは壊れています。これは止めるのではなく
  検出して見せる側で扱います。
- **反復的な内容での交錯。** 行域が互いに素であることが保証するのは**所有**であって、
  綺麗なマージではありません。周囲の行が繰り返しになっていると、git がハンクを別の位置へ
  合わせて衝突することがあります。
- **フックが届かないリポジトリ。** 非 git のフォルダは確保だけできて何も強制しません。
  submodule の内側・bare・読み取り専用は 4 段の外側です。どれに当たるかは
  `zai czero doctor` が報告します。

実測値・失敗条件・制約の全一覧は [docs/conflict-zero.md](docs/conflict-zero.md) にあります。
書き込みの関門は意図的に **fail-open** です。`zai` が見つからないときや台帳が読めないときは
コミットを通します。止めるのは、実際に検出できた競合のときだけです。

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
| [docs/anyrepo-proof.md](docs/anyrepo-proof.md) | 自分のリポジトリで同じ実験を回す手順 |
| [docs/xplat-bench.md](docs/xplat-bench.md) | macOS と Linux を並べた実測 |
| [docs/idle-cost.md](docs/idle-cost.md) | アイドル時のコストの測り方と、現在の数字 |
| [docs/region-cost.md](docs/region-cost.md) | 行域判定そのものの費用 |
| [docs/guard-edges.md](docs/guard-edges.md) | 書き込みの関所が漏れる形と、その塞ぎ方 |
| [docs/bench-honesty.md](docs/bench-honesty.md) | ベンチが「静かな嘘」をつかないための決まり |
| [docs/workspace-key.md](docs/workspace-key.md) | ワークスペースごとの置き場を決めるキー |
| [docs/plugins.md](docs/plugins.md) · [docs/PLUGIN_SPEC.md](docs/PLUGIN_SPEC.md) | プラグインを書く |

各版のリリースノートは
[Releases ページ](https://github.com/tacyan/zaivern-code/releases)にあります。

## コントリビューション

不具合報告・機能提案・Pull Request を歓迎します。新しく立てる前に
[Issues](https://github.com/tacyan/zaivern-code/issues) で既存の報告を確認し、
[Pull Request](https://github.com/tacyan/zaivern-code/pulls) は `main` へ向けてください。

**ソースからビルド**

```bash
git clone https://github.com/tacyan/zaivern-code.git
cd zaivern-code
rustup update stable
cargo run --release -- .
```

**変更を検証する**

```bash
tools/verify.sh --lint           # 整形・コンパイル・テスト・clippy を一度に
cargo nextest run --profile ci   # CI と同じ全量実行
```

`#[cfg(windows)]` や Linux 限定のコードは macOS のビルドでは一度もコンパイルされません。
どちらも CI を待たずに手元で再現できます。

```bash
tools/linux-test.sh              # Linux のテストを Docker で再現
tools/windows-check.sh           # Windows (MSVC) 向けの型検査
tools/windows-check.sh --build   # 実際に zai.exe を作り、リンクまで確認
```

Windows 側は初回のみ `cargo install cargo-xwin --locked` が必要です。
どちらのスクリプトもホストの `target/` を汚しません。

## ライセンス

[Apache License 2.0](LICENSE)

---

<div align="center">

**エージェントは、もう十分に速い。次に速くなるのは、指揮するあなたです。**

</div>
