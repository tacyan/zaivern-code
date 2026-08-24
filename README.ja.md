<div align="center">

<img src="assets/Zaivern.png" width="120" alt="Zaivern Code" />

# Zaivern Code

### AI エージェント 64 体。リポジトリ 1 つ。マージ衝突ゼロ。

**並列で走る AI コーディングエージェントの調整層。**

Claude Code・Codex・Gemini CLI などのコーディングエージェントを、
同じリポジトリの上で —— マージ衝突に振り回されずに走らせる。

[English](README.md) | **日本語** | [简体中文](README.zh-CN.md) | [한국어](README.ko.md) | [Português (Brasil)](README.pt-BR.md) | [Español](README.es.md)

[![Release](https://img.shields.io/github/v/release/tacyan/zaivern-code)](https://github.com/tacyan/zaivern-code/releases/latest)
[![CI](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml/badge.svg)](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)

<!-- TODO: 15〜20 秒のベンチ動画へ差し替える:
     1 つのリポジトリに 64 体 / 素の git は 132 ハンク / Zaivern は 0。
     下の GIF は操縦席の絵で、調整の結果は写っていない。 -->
<a href="https://zaivern.com/">
  <img src="assets/zaivern-demo.gif" width="960" alt="Claude Code・Codex・Gemini CLI などを並べて動かす Zaivern Code の実演" />
</a>

| 書き手 64 体 · 同じリポジトリ · 同じ仕事量 | 素の git | Zaivern Code |
|---|---:|---:|
| 衝突したマージ | 64 件中 57 件 | **0** |
| 衝突ハンク | 132 | **0** |

[測り方・引き換えにしたもの・限界を読む →](docs/conflict-zero.md)

[**クイックスタート**](#クイックスタート) ·
[**実測**](#実測) ·
[**ドキュメント**](#ドキュメント) ·
[**ダウンロード**](https://github.com/tacyan/zaivern-code/releases/latest) ·
[**公式サイト**](https://zaivern.com/)

</div>

## 問題

エージェントを 1 体動かすのは簡単です。4 体になると、そうではありません。
同じファイルを触る 2 体でも、もう十分に踏みます。

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
Zaivern なし                              Zaivern あり

エージェント 1  ─┐                        エージェント 1  ─┐
エージェント 2  ─┤                        エージェント 2  ─┤   ┌─────────────┐
エージェント 3  ─┼─→ 同じファイル ─→ 衝突  エージェント 3  ─┼─→ │  行域の台帳  │ ─→ そのまま
     ...        ─┤                             ...        ─┤   │             │    統合できる
エージェント 64 ─┘                        エージェント 64 ─┘   └─────────────┘

衝突ハンク 132                            衝突ハンク 0
```

**64 体も要りません。** 同じファイルを触る 2 体で十分に踏みます。2 体から始めて、64 体まで。

## クイックスタート

まず、対応している AI コーディング CLI を 1 つ入れてサインインしてください ——
Zaivern Code には **33 個**の起動プリセットが同梱されていますが、始めるには 1 つで足ります。

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

あとは画面で `+ Agent` を押し、手元にある CLI を選んで仕事を投げるだけ。

リポジトリで衝突調整を有効にする:

```bash
zai czero init      # 台帳・git フック・マージドライバを導入して自己診断する
zai czero verify    # 使い捨てのリポジトリで本物の衝突を作り、止まることを確かめる
```

インストーラは公開されている `checksums.txt` と**展開する前に**照合し、合わなければ中止します。
[手動ダウンロード・チェックサム検証・来歴・SBOM →](SECURITY.md)

### 更新

```bash
zai update            # 新しい版を確認し、コマンドを見せてから更新する
zai update --check    # 見るだけ。何も変えない
zai update --yes      # 確認を飛ばして更新する
```

エディタが動いていてもいなくても使えます。削除は `zai uninstall`。

## 主な機能

### 1. マージ衝突に振り回されずに並列で走らせる

エージェントは編集の前に、ファイルまたは行域を確保します。生きている別のエージェントが
その領域を持っていれば、git フックがぶつかる書き込みを断ります —— マージのときではなく、
**書き込みのその瞬間に**。

行域を離して配った 64 体の実測では、**64 体**全部が書けて衝突ハンクは **0**。
ファイル単位のリースなら、通っていたのはちょうど 1 体です。
[行域の調整の仕組み →](docs/conflict-zero.md)

### 2. 並列エージェントの管理

複数の AI CLI を並べて、どれが考えていて、書いていて、走っていて、あなたを待っているのかを
一目で見られます。エージェントを足すのは、コマンドラインを思い出す作業ではなく 2 クリックです。

### 3. 健康状態と停滞の検知

Zaivern が見るのは画面のピクセルではなく**意味のある進捗**です。進捗が止まった
エージェントは**停滞**として報告され、予期しない終了は通知として出ます。

### 4. 一斉指示

1 つの入力欄から、走っている全エージェントへ同じ指示を送れます。1 体だけを狙うこともできます。

### 5. 承認

既定は承認必須です。自動 YES はセッションごとの明示的な opt-in、権限昇格は必ず人が答え、
MCP の環境変数は値を表示しません。

### 6. スマホからの遠隔操作

進捗の確認、指示、承認、ファイルの編集をスマホからできます。同じ Wi-Fi、
[Tailscale](https://tailscale.com/)、あるいは SSH トンネルのいずれでも。

### 7. 内蔵エディタ

アプリを離れずにコードとエージェントの変更を確認できます（Markdown・画像・PDF・CSV も）。
未保存の内容はクラッシュのあとに復元されます。

このほかにプラグイン機構と、6 言語の UI が入っています。
[プラグイン](docs/plugins.md) · [翻訳](docs/translating.md)

## 仕組み

1. **起動** —— 1 つの窓からエージェントを起こす（走っているものに繋ぐこともできます）。
2. **確保** —— 編集の前に、ファイルまたは行域を、周囲の内容に錨づけして確保する。
3. **門** —— 重なる書き込みを、マージへ届く前に git フックが断る。
4. **統合** —— 重なっていない変更は、いつもどおり git がマージする。

[技術的な詳細 →](docs/conflict-zero.md) ·
[どの保証がどの形のリポジトリで成り立つか →](docs/czero-repo-shapes.md)

## 対応エージェント

Claude Code · Codex · Gemini CLI · Cursor Agent · GitHub Copilot CLI ·
**ほか 28 個** —— 起動プリセットは全部で 33 個、加えて ACP 経由で駆動できるものが 6 個。

Zaivern Code は AI モデルではなく、モデルを同梱もしません。あなたがすでに入れて
サインインした CLI を動かします。どんな組み合わせでも、1 体だけでも構いません。
使っているものが無い？
[対応の要望を出してください](https://github.com/tacyan/zaivern-code/issues)。

## なぜ Zaivern か

|  | 端末マルチプレクサ | 汎用エージェント盤 | Zaivern Code |
|---|:---:|:---:|:---:|
| 複数のエージェントを同時に走らせる | ✅ | ✅ | ✅ |
| 全部を 1 画面で見る | ❌ | ✅ | ✅ |
| 状態が分かる（思考中 / 待ち / 停滞） | ❌ | まちまち | ✅ |
| 行域の所有 + 書き込み時の拒否 | ❌ | ❌ | ✅ |
| 承認が通知として出る | ❌ | まちまち | ✅ |
| スマホ・遠隔からの操作 | ❌ | まちまち | ✅ |
| 単一のネイティブバイナリ（実行環境不要） | まちまち | まちまち | ✅ |

## 実測

**64 体・同じリポジトリ・同じ仕事量**（ファイル数 = 書き手 × 6、ファイルの重なり 50%）:

| | 素の git | Zaivern Code |
|---|---:|---:|
| 衝突したマージ | 64 件中 57 件 | **0** |
| 衝突ハンク | 132 | **0** |

ゼロは書き込みを断って買っている数字です。計画された 384 件のうち書けたのは 202 件で、
残りは門で止まりました。行域が実際に離れている場合は、64 体すべてが書けて拒否は 0 件です。

**このリポジトリ自身へ 16 体を同時に**（zai 0.14.0）: 素の git は
**衝突 26 ファイル / 28 ハンク**。台帳を挟むと **0 / 0** で、**96 件の編集が全部成立**
しました（拒否 0・うち 30 件は空いている行域へずらして確保）。

### 「衝突ゼロ」が意味すること

- Zaivern は、重なる書き込みをマージ衝突にする代わりに**断る**ことがあります。
  衝突は 0 件ですが、通った量は 0 ではありません。
- 防ぐのは行の所有の重なりです。**意味的な衝突は検知しません** ——
  片方が関数の型を変え、もう片方が古い呼び出しを残しても、マージはきれいに通ります。
- 十分に離れた行域は、もともと助けが要りません。素の git がそのまま衝突なしでマージします。
  行域の所有は、ファイル単位のリースが壊す並列性を返しているだけです。

[測り方の全体・規模別の数字・門の遅延・限界 →](docs/conflict-zero.md)

## 対応環境

| 項目 | 対応 |
|---|---|
| OS | macOS arm64/x86_64、Linux x86_64/arm64、Windows x86_64 |
| AI CLI | 起動プリセット 33 個、加えて ACP 経由が 6 個 |
| テスト | 4,985 件。CI で macOS・Linux・Windows の 3 面 |
| ライセンス | Apache-2.0 |

## ドキュメント

| ドキュメント | 内容 |
|---|---|
| [docs/conflict-zero.md](docs/conflict-zero.md) | 「衝突ゼロ」が主張すること・しないことと、裏づけの実測すべて |
| [docs/czero-repo-shapes.md](docs/czero-repo-shapes.md) | どの保証が、どの形のリポジトリで成り立つか |
| [docs/plugins.md](docs/plugins.md) | プラグインの書き方と[形式仕様](docs/PLUGIN_SPEC.md) |
| [docs/README.md](docs/README.md) | 他の全ドキュメントの索引（裏づける主張ごとに分類） |

[アイドル CPU とバイナリサイズの実測 →](docs/idle-cost.md) ·
[リリースノート](https://github.com/tacyan/zaivern-code/releases)

## 試す

並列のコーディングエージェントが日常にあるなら、次の多エージェント作業で
Zaivern Code を回してみてください —— リポジトリで `zai czero init` を実行し、
同じファイルに 2 体を向けて、2 つ目の書き込みが**まずいマージにならずに断られる**のを
見るところからで十分です。

## コミュニティ

- 調整の抜けを見つけた？ [Issue を立ててください](https://github.com/tacyan/zaivern-code/issues)。
- 未対応のコーディングエージェントを使っている？ [対応の要望を出してください](https://github.com/tacyan/zaivern-code/issues)。
- 8・16・32・64 体で回している？ 実測を共有してください —— `tools/conflict-bench.sh` と
  `tools/anyrepo-prove.sh` が、上の表と比較できる数字を出します。
- Zaivern Code で何か作った？ 構成を見せてください。

プルリクエストは `main` へどうぞ —— ソースからのビルド（Rust 1.88+）、変更の検証、
Linux / Windows の確認を手元で回す方法は [CONTRIBUTING.md](CONTRIBUTING.md) にあります。

Zaivern Code が役に立ったら、⭐ **Star** が他の人に見つけてもらう助けになります。

## ライセンス

[Apache License 2.0](LICENSE)
