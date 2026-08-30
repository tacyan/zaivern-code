<div align="center">

<img src="assets/Zaivern.png" width="120" alt="Zaivern Code" />

# Zaivern Code

### 複数のコーディングエージェントを、マージ衝突に振り回されずに走らせる。

**2 体から始めて、64 体まで伸ばす。**
Zaivern Code は、重なった編集が**着地する前に**止めます。だからマージ衝突になりません。

Claude Code・Codex・Gemini CLI ほか、すでに入れてある 30 種のエージェント CLI を 1 つの窓で。
単一ネイティブバイナリ —— macOS・Linux・Windows。

[English](README.md) | **日本語** | [简体中文](README.zh-CN.md) | [한국어](README.ko.md) | [Português (Brasil)](README.pt-BR.md) | [Español](README.es.md)

[![Release](https://img.shields.io/github/v/release/tacyan/zaivern-code)](https://github.com/tacyan/zaivern-code/releases/latest)
[![CI](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml/badge.svg)](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)

</div>

**インストールして起動する**

macOS / Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.sh | sh
zai .
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.ps1 | iex
zai .
```

対応する AI コーディング CLI を最低 1 つ、導入してサインインしておく必要があります。
Zaivern Code は手元の CLI を動かすだけで、AI モデルも利用権も同梱しません。

**競合調整（任意）:**

```bash
zai czero init
```

これは現在の Git リポジトリを変更します。
[変更内容の下見と検証 →](#競合調整を有効にする) ·
[手動ダウンロードと検証](SECURITY.md)

<div align="center">

<a href="https://zaivern.com/">
  <img src="assets/zaivern-demo.gif" width="960" alt="Zaivern Code の操縦席 —— 複数のコーディングエージェント CLI を 1 つの窓に並べ、各エージェントの状態を表示している" />
</a>

[**クイックスタート**](#クイックスタート) ·
[**実測**](#実測と限界) ·
[**ドキュメント**](#ドキュメント) ·
[**ダウンロード**](https://github.com/tacyan/zaivern-code/releases/latest) ·
[**公式サイト**](https://zaivern.com/)

</div>

*上の動画は操縦席です。複数のエージェント CLI が 1 つの窓に並んでいるところで、
競合調整の結果は写っていません。そちらは別に測ってあり、すぐ下に載せます。*

## 実証

**64 体・同じリポジトリ・同じ仕事量。** ファイル数 = 書き手 × 6、その半分は
2 体以上が狙います。同じ担当表を 2 回流しました —— 1 回は素の git、
もう 1 回は Zaivern Code の行域台帳を通して。

| | 素の git | Zaivern Code |
|---|---:|---:|
| 衝突したマージ | 64 件中 57 件 | **64 件中 0 件** |
| 人が解く衝突ハンク | 132 | **0** |
| 反映された編集 | 384 件中 384 件 | 384 件中 202 件 |
| 着地前に止めた書き込み | 0 | 182 |

**ゼロは、書き込みを断って買っています。**両側を魔法のように混ぜているのではありません。
計画した 384 件のうち 182 件は、その行を別の生きたエージェントが既に持っていたので
関所で止まりました。うち 14 件は混雑による一時的な拒否で、再試行すれば通り得ます。

**行域が本当に離れていれば、1 件も断りません。**1 つのファイルの 64 個の別々の行域を
64 体が編集すると、**64 件中 64 件**が反映され、拒否 **0**・衝突ハンク **0**。
ファイル単位の所有だと 1 件しか通らず、63 件を断ります。

意味的な衝突は**検出しません**。片方が関数の引数を変え、もう片方が古い呼び方のまま
書いても両方通り、git は綺麗にマージします。

[測り方・規模ごとの数字・関所の遅延・残っている穴 →](docs/conflict-zero.md)

## 問題

エージェントを 1 体動かすのは簡単です。4 体になると、そうではありません。
**同じファイルを触る 2 体でも、もう十分に踏みます。**

- 同じ行を書き換え、それに気づくのはマージのとき。
- どのエージェントが働いていて、詰まっていて、静かに止まっているのかが見えない。
- 見ていないタブで、承認のプロンプトが流れていく。
- 統合が、毎回あなたの仕事になる。

遅いのはエージェントではありません。**エージェント同士の調整**です。

## 解決

Zaivern Code は、どのエージェントがリポジトリのどの部分を安全に編集してよいかを調整します。
衝突をマージのときに見つけるのではなく、**ぶつかる書き込みが着地する前に**捕まえます。
そして、走っているエージェントを見て・操って・立て直す場所を 1 つにまとめます。

```text
Zaivern なし                             Zaivern あり

Agent 1  ─┐                              Agent 1  ─┐
Agent 2  ─┤                              Agent 2  ─┤   ┌─────────────┐
Agent 3  ─┼─→ 同じファイル ─→ マージ衝突  Agent 3  ─┼─→ │   行域の    │ ─→ 綺麗に
   ...   ─┤                                 ...   ─┤   │    台帳     │    統合
Agent 64 ─┘                              Agent 64 ─┘   └─────────────┘
```

## クイックスタート

### 操縦席を起動する

このページの上のワンライナーで導入し、プロジェクトのフォルダで `zai .` を実行します。
そのフォルダで操縦席が開きます —— エージェントのタイル・エディタ・スマホリモート。
`+ Agent` を押し、入れてある CLI を選んで、仕事を渡してください。
**これだけでは競合調整は有効になりません。**それは次の段です。

インストーラは、ダウンロードした書庫をリリースの `checksums.txt` と
**展開する前に**突き合わせ、一致しなければ中止します。
[手動ダウンロード・チェックサム検証・来歴・SBOM →](SECURITY.md)

### 競合調整を有効にする

```bash
zai czero init --dry-run  # 変更の下見
zai czero init            # 台帳と git 連携を入れる
zai czero verify          # 使い捨てのリポジトリで検証する
zai .                     # 操縦席を起動する
```

- **`zai czero init --dry-run`** は予定している変更を見せるだけで、現在の
  リポジトリを変更しません。
- **`zai czero init` は現在の Git リポジトリを変更します。** 行域の台帳を用意し、
  `pre-commit` / `pre-applypatch` / `pre-merge-commit` の git フックを追加し、
  union merge driver を登録し、管理ブロックつきの `.gitattributes` を書いて、
  最後に自己診断します。冪等です。
- **`zai czero verify`** は使い捨てのリポジトリで実際に重なる書き込みとマージを起こし、
  1 つ 1 つが本当に止まるかを確かめます。**現在のリポジトリは変更しません。**
  判定は `verified` / `partial` / `broken` の 3 段で、試せなかった試行がある限り
  「検証済み」とは言いません。
- **`zai czero doctor`** はいまどの段が効いているかを診断し、
  **`zai czero uninstall`** は `init` が入れたものだけを外します。

### 更新

`zai update` は実行するコマンドを見せてから更新します（`--check` は確認のみ、
`--yes` は確認を省略）。エディタが起動していてもいなくても動きます。
消すときは `zai uninstall`。

## 主な機能

Zaivern Code をどれだけ他と隔てるかの順に並べています。1 番目が、この製品がある理由です。

### 1. ファイルと行域の所有を、書き込みの時点で強制する

エージェントは編集の前にファイルか行域を確保します。目印は行番号ではなく、
**周りの内容**です。重なる領域を別の生きたエージェントが既に持っていれば、
git フックがその書き込みを断ります —— マージのときではなく、書き込みのときに。
同じファイルでも行が違えば通るので、ファイル単位のロックのように直列化されません。
[行域の調整の仕組み →](docs/conflict-zero.md)

### 2. 1 画面で、どのエージェントが何をしているか見える

複数の AI CLI を並べて、どれが考えていて・編集していて・実行していて・
あなたの返事を待っているかを一目で。エージェントの追加は 2 クリックで、
コマンドラインを思い出す必要はありません。

### 3. 停滞と終了の検知

Zaivern Code が見るのはピクセルではなく意味的な進捗です。進まなくなったエージェントは
**停滞**として報告し、想定外の終了は通知に出ます。

### 4. 一斉指示と個別指示

1 つの入力欄から走っている全エージェントへ同じ指示を送れます。1 体だけに絞ることもできます。

### 5. 承認

既定は承認必須です。自動 YES はセッションごとの明示的な選択で、権限昇格は必ず人が判断し、
MCP の環境変数の値は 1 度も表示しません。

### 6. スマホからの遠隔操作

進捗の確認・指示・承認・ファイル編集をスマホから。同じ Wi-Fi、
[Tailscale](https://tailscale.com/)、SSH トンネルのいずれでも。

### 7. 内蔵エディタ

Zaivern Code を離れずにコードとエージェントの変更を確認できます。Markdown・画像・
PDF・CSV も。保存前のバッファはクラッシュ後に復元されます。

### 8. コンテキストエンジン — どのエージェントでもトークンを減らす

7,000 行のファイルを素直に読むと約 85k トークンかかります。`zai context read`
は代わりにその**構造**を返すので、同じファイルが約 3.4k トークン (-96%)。
そのうえで `--offset/--limit` で必要な関数だけを取りに行けます。検索・
記号の参照・ディレクトリの地図・JSON・ログも同じ層を通ります。

**Provider 非依存であることを構造で保証**しています。コアには「どの
エージェントが要求したか」で分かれる処理が 1 つも無いので、Claude Code も
Codex も Gemini も同じ挙動になります。追加インストールは不要で、
エージェントへ勝手に入力することも、ファイルを書き換えることもありません
(呼ばれたときにだけ動きます)。

[コンテキストエンジン](docs/context-engine.md)

### 9. AI チーム実行 — SPEC を渡すだけで、開発チームが動く

```sh
zai team run SPEC.md --agents 4
```

Zaivern が SPEC を読み、**いまは決定的な `StaticPlanner`** が Goal と
Definition of Done を起こし、Task Graph を組んで計画を見せます
(LLM に意味を解釈させてはいないので、同じ SPEC からは必ず同じ計画が出ます)。
Planner は差し替えられる境界 (`TeamPlanner`) なので、LLM Planner は同じ
検証済み `TeamPlan` を返す実装として後から入ります:

```text
いま:     SPEC → StaticPlanner (決定的) → 検証済み TeamPlan → Task Graph
これから: SPEC → LLM TeamPlanner        → 同じ TeamPlan     → Task Graph
```

**Start Team** を押すと、計画に必要なぶんだけエージェントを起こし、担当を
配り、実装 → 検証 → レビュー → 修正 → 統合まで進めます。

**「エージェントが完了と言った」では完了になりません。** タスクは
`Running → Validating → Reviewing → Completed` の順にしか進めず、完了報告は
タスク ID / エージェント ID が担当と違う・担当外のファイルを触った・検証
コマンドを実行していない/失敗した・blocker が残っている、のどれかに当たると
却下されます。レビューは**実装したのと別のセッション**が担当します。
エージェントが報告した `validation` は**参考情報として脇に置き**、検証
コマンドは Zaivern 自身が実行します。レビューへ進むのは**実測が通った**
ときだけです。

どのコマンドを走らせるかは、まず SPEC が決めます。「検証」節にコマンドが
書いてあれば**それだけ**を使い、何も足しません。書いていなければ Zaivern が
リポジトリを読みます — `Cargo.toml` なら `cargo fmt --check` と `cargo test`、
`go.mod` なら `go test ./...`、`package.json` なら**実在する** script
(`test` / `lint` / `typecheck` / `check`) だけを lockfile が示す
パッケージマネージャで、pytest はそれを使うと言い切れるときだけ。目印が
1 つも無ければ、**Zaivern は当て推量をしません** — Next.js のリポジトリで
`cargo test` を走らせる代わりに、SPEC へコマンドを書くよう求めます。
解釈できない検証コマンドも黙って捨てません。`npm test && npm run lint` は
「直せるシェル記法の誤り」として返り、何をしても通らない `git push` とは
別の種類で区別されます。

検証コマンドは許可リストで素通しにせず、**危険度で分けます**。パス付きの
実行ファイル (`/tmp/cargo test` / `./cargo test` / `tools/python x.py`) は
実行しません — basename だけで見ると `/tmp/cargo` が起きてしまうからです。
push / merge / deploy / publish / 権限昇格 / 破壊的操作は拒否します。そして
**リポジトリのコードを実行しうるもの** (`cargo test` / `npm test` /
`pytest` / `make` / `node` / `go test`) は、**人が承認するまで 1 行も
走りません** — テスト本体・`build.rs`・`Makefile` はシェルにできることを
何でもできるからです。**ファイルを書き換えるもの**も同じで、`black .` や
`rustfmt src/lib.rs` は承認が要り、`black --check .` や
`rustfmt --check src/lib.rs` は要りません — 決めるのは道具の名前ではなく
旗です。実行体も Zaivern 自身が PATH から解決するので、ワークスペースの中に
置かれた偽の `rustfmt` が本物の代わりに動くことはありません。**外なら安全、
でもありません** — エージェントはあなたと同じ権限で動くので、`~/.local/bin`
にも、Homebrew があなたの所有にした `/opt/homebrew` や `/usr/local` にも
書けます。無承認で走るのは**昇格が要る場所** (`/usr/bin` `/bin` `/sbin`
`C:\Windows` `C:\Program Files`) の実行体だけで、しかも実際に書き換えられると
分かれば区分は下がります (上がることはありません)。前方の信用できない実行体を
飛ばして後方の本物へ落ちることもなく、判定したものと実行するものの間に
シェル (`sh -c` / `cmd /C`) を 1 段も挟みません。実行には時間切れがあり、停止すればプロセスツリーごと
終了し、成功・失敗・時間切れ・停止・起動不可・接続断のどれかで必ず決着します。
**承認が効くのは「その 1 回」だけ**で、コマンド名に対してではありません —
別のタスク、レビュー指摘のあとのやり直し、差し戻し後の再試行では、
検証されるコードが承認したときと別物なので、必ず聞き直します。
承認したものの中身までは隔離しません — Zaivern が保証するのは**何を起動したか**
であって、そのプロセスがその先で何をするかではありません。
push / merge / deploy / 権限昇格 / 破壊的操作は自動実行せず、**画面上の
あなたの判断**になります。

Organization Board には、チームリード・専門チームのレーン・親子のエージェント・
いま各自が何をしているか・Task Graph の進捗・テストとレビューの結果・
そして**いま一番あなたの判断を待っているもの**が出ます。

[AI チーム](docs/team.md)

さらに、プラグインと 6 言語の UI も入っています。
[プラグイン](docs/plugins.md) · [翻訳](docs/translating.md)

## 仕組み

1. **起動** —— 1 つの窓からエージェントを起こす。既に走っているものを繋いでもよい。
2. **確保** —— 編集の前にファイルか行域を、周りの内容を目印にして押さえる。
3. **関所** —— git フックが、重なる書き込みをマージへ届く前に断る。
4. **統合** —— 重ならない変更は、いつもどおり git がマージする。

## 対応エージェント

Claude Code · Codex · Gemini CLI · Cursor Agent · GitHub Copilot CLI ·
**ほか 28 種** —— 起動プリセットは全部で 33 種、加えて ACP で動かせるものが 6 種。

どの組み合わせでも動きます。1 体だけでも構いません。
使っているものが無ければ [追加の要望](https://github.com/tacyan/zaivern-code/issues) をどうぞ。

## なぜ Zaivern か

|  | ターミナル多重化 | 一般的なエージェント画面 | Zaivern Code |
|---|:---:|:---:|:---:|
| 行域の所有 + 書き込み時の拒否 | ❌ | ❌ | ✅ |
| エージェントの状態が判る（思考中 / 待ち / 停滞） | ❌ | まちまち | ✅ |
| 全エージェントを 1 画面で | ❌ | ✅ | ✅ |
| 承認が通知として届く | ❌ | まちまち | ✅ |
| スマホ / 遠隔操作 | ❌ | まちまち | ✅ |
| 単一ネイティブバイナリ・ランタイム不要 | まちまち | まちまち | ✅ |

## 実測と限界

冒頭の 64 体の表は合成リポジトリのものです。**実在のリポジトリ**を複製して
`tools/anyrepo-prove.sh` で 16 体を流すと（zai 0.14.0）:

| リポジトリ | 素の git | Zaivern Code |
|---|---|---|
| zaivern-code（Rust・追跡 259 ファイル） | 26 ファイル / 28 ハンク衝突 | **0 / 0** —— 96/96 反映・拒否 0・ずらし 30 件 |
| hyperframes（TS/HTML・追跡 1,194 ファイル） | 26 / 28 | **0 / 0** —— 96/96 反映・拒否 0・ずらし 32 件 |

断ることだけが答えではありません。確保がぶつかったとき `--shift` は同じ幅が入る
いちばん近い空き行域へずらします。上の 2 行が 1 件も断らずに全部反映できているのはこれが理由です。

### 「衝突ゼロ」が意味すること

- **所有は常に成り立ちます。**「同じ行を 2 人に配らない」は台帳だけで決まり、
  ファイルの中身に依存しません（独立に回した 126 回すべてで `dup_lines = 0`）。
- **綺麗にマージできるかは条件つきです。** 反復的な内容（``` の連続・生成コード・
  同じ行の繰り返し）では、行域が十分に離れていても git が衝突することがあります。
  関所は、保証できないマージを約束する代わりに**その確保を断ります**。
- **意味的な衝突は対象外です。** 防げるのは行の所有の重なりで、引数を変えた側と
  古い呼び方のまま残った別ファイルの組は防げません。
- **離れた作業には元から助けが要りません。** 十分に離れた行域は素の git が
  もとから 0 件でマージします。行域の所有が返しているのは、**ファイル単位の所有が
  壊した並列度**です。比べる相手はそちらです。
- **強制できるのは git が強制できる場所だけです。** `zai lease claim` は非 git の
  フォルダでも成功しますが、そこでは何も止まりません。どのリポジトリ形態
  （worktree・submodule・sparse-checkout・LFS・bare）まで守れているかは
  `zai czero doctor` が出します。

再現はどれもコマンド 1 行: `tools/conflict-bench.sh`、`tools/coedit-bench.sh`、
`tools/anyrepo-prove.sh --repo .`
[測り方の全部と残っている穴 →](docs/conflict-zero.md) ·
[リポジトリの形ごとに何が保証されるか →](docs/czero-repo-shapes.md)

## 対応環境

| 項目 | 内容 |
|---|---|
| OS | macOS arm64/x86_64、Linux x86_64/arm64、Windows x86_64 |
| 配布 | 単一ネイティブバイナリ・ランタイム不要。リリースごとにチェックサム・SBOM・ビルド来歴 |
| AI CLI | 起動プリセット 33 種、加えて ACP で 6 種 |
| テスト | v0.23.0 で 5,005 件。CI で macOS・Linux・Windows の 3 面 |
| ライセンス | Apache-2.0 |

## ドキュメント

| 文書 | 扱っていること |
|---|---|
| [docs/conflict-zero.md](docs/conflict-zero.md) | 「競合ゼロ」が何を主張し何を主張しないか、その裏の実測すべて |
| [docs/context-engine.md](docs/context-engine.md) | コンテキストエンジン: 4 つの畳み方、ワークスペース境界の強制、メトリクス、削減率の比較 |
| [docs/czero-repo-shapes.md](docs/czero-repo-shapes.md) | リポジトリの形ごとに何が保証されるか |
| [docs/idle-cost.md](docs/idle-cost.md) | アイドル CPU とバイナリサイズの測り方 |
| [docs/plugins.md](docs/plugins.md) | プラグインの書き方と[形式の仕様](docs/PLUGIN_SPEC.md) |
| [docs/team.md](docs/team.md) | `zai team`: SPEC がどう Task Graph になるか、何が「完了」の関門か、何を自動実行しないか |
| [docs/README.md](docs/README.md) | 残りの文書の索引（支えている主張ごとの並び） |

[リリースノート](https://github.com/tacyan/zaivern-code/releases) ·
[セキュリティ方針](SECURITY.md) · [貢献の手引き](CONTRIBUTING.md)

## 試す

同じリポジトリで 2 体動かしてみてください:

```bash
zai czero init
zai .
```

2 体を起こして同じファイルへ向け、2 番目の重なる書き込みが**マージ衝突になる前に**
断られるところを見てください。これがこの製品の全部で、1 分ほどで確かめられます。

役に立ったら ⭐ **Star** をいただけると、他の人にも見つけてもらえます。

## コミュニティ

- 調整の抜け道を見つけた？ [Issue を立ててください](https://github.com/tacyan/zaivern-code/issues)。
- まだ対応していないエージェントを使っている？ [追加の要望をどうぞ](https://github.com/tacyan/zaivern-code/issues)。
- 8・16・32・64 体を動かしている？ 数字を教えてください ——
  `tools/conflict-bench.sh` と `tools/anyrepo-prove.sh` が上の表と比べられる結果を出します。

プルリクエストは `main` へどうぞ ——
[CONTRIBUTING.md](CONTRIBUTING.md) にソースからのビルド（Rust 1.88 以上）・
変更の検証・Linux と Windows の確認をローカルで回す方法があります。

## ライセンス

[Apache License 2.0](LICENSE)
