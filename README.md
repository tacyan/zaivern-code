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
| **並列で走らせるほどマージ衝突が増える** | **同じファイルでも行が違えば衝突しない** |

並列エージェントの本当のコストは実行時間ではなく、**レビュー時の衝突解決**です。
1つのファイルに64体をぶつける実測で、素のgitは**48枝が衝突して960行・48回の手作業**を
要しました。Zaivern Codeは同じ条件で**衝突0・手作業0**、しかも**64体全員が書けます**
（[数字と線引き](docs/conflict-zero.md)）。

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

### 🧩 競合ゼロ — 同じファイルでも、違う行なら同時に書ける

**この製品の中核です。** 担当を*行の単位*で持つので、`src/app.rs` のような
大きなファイルを何人でも分け合えます。埋まっていたら近くの空きへ自動でずらすので、
断られることもありません。詳細は[v0.14.0の節](#-v0140-で入ったもの--同じファイルでも競合しない)へ。

```console
$ zai czero init      # 台帳・gitフック・マージドライバを一度に入れて自己診断
$ zai czero verify    # 実際に競合を起こして、本当に止まるか実演する
```

### 🎛 Agent Cockpit

複数のAIツールをタイル状に並べ、動いているか、止まっているかをひと目で確認できます。Claude Code、Codex、Gemini CLIを含む33種類の起動設定を収録しています。

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

## 🆕 v0.14.0 で入ったもの — 同じファイルでも競合しない

**🧩 行域オーナーシップ** — これまで並列エージェントを守る方法は「誰かが持っている
ファイルには書かせない」でした。安全ですが、**大きなファイルを 1 人が持つと他の全員が
そこへ 1 バイトも書けません**。v0.14.0 からは**行の単位**で持ちます。
`zai lease claim 'src/app.rs#L1200-1260'`。

1 ファイルへ 64 体をぶつけた実測（`tools/coedit-bench.sh --agents 64 --lines 2000`）:

| 守り方 | 書けた担当 | 断られた | マージ衝突 | 人の手 |
|---|---:|---:|---:|---:|
| 守らない（素の git） | 64 | 0 | **48 枝 / 960 行** | **48 回** |
| ファイル単位で持つ（〜v0.13） | **1** | 63 | 0 | 0 |
| **行の単位で持つ（v0.14）** | **64** | 0 | **0** | **0** |

**衝突ゼロ自体は v0.13 でも達成できていました。v0.14 が買うのは並列度です** —
64 体中 1 体しか書けなかったものが、全員書けるようになります。

**🔀 断らずに、ずらす** — 要求した行が埋まっていても、近くの空いている行を
**自動で割り当てます**（`zai lease claim --shift`）。わざと担当を重ねた条件での実測:

| 守り方 | 完了 | 拒否 | 衝突枝 | 衝突行 | 人の手 |
|---|---:|---:|---:|---:|---:|
| 守らない（素の git） | 64 | 0 | **48** | **960** | **48** |
| ファイル単位で持つ | 1 | 63 | 0 | 0 | 0 |
| 行の単位で持つ | 11 | 53 | 0 | 0 | 0 |
| **ずらす（v0.14）** | **64** | **0** | **0** | **0** | **0** |

代償は**要求からどれだけ離れた場所を渡されたか**で、実測は p50 129 行 / p95 253 行 /
max 281 行、ファイルの外へ出たものは 0 件でした。ずらしてよいと明示した要求だけが
対象です（行域は行番号ではなく*そこにある内容*に紐づくので、既定は必ず「ずらさない」）。
どこまで動かしてよいかは設定 `negotiate.max_shift` と `--max-shift <行>` で決められ、
**無制限が既定になることはありません**。

> **正直に**: この「拒否 0」は 64 体では毎回は出ません。同じ条件を 6 回まわすと
> 4 回は 64/64 ですが、macOS で 58/64、Linux で 47/64 が各 1 回出ました。
> 32 体以下では 1 度も出ていません。詳細は
> [docs/conflict-zero.md §5-1](docs/conflict-zero.md)。

**🤝 エージェント同士が裏で認識し合う** — Erlang のプロセスと同じ仕組みで、
それぞれが身元とメールボックスを持ち、互いを監視します。身元には「起動した瞬間」が
入っているので、**OS が PID を使い回しても別人を自分だと誤認しません**。
そして **担当を持ったまま落ちたエージェントの担当は自動で解放されます** —
Erlang の "let it crash" をそのまま持ち込んだ部分で、人が掃除する必要がありません。

**🔒 一撃マージの証明** — 複数ブランチの変更行域が離れていれば、`git merge` は
**必ず**衝突しません。実際の git で網羅的に確かめて**見逃し 0 件**。証明が立てば
N 本を人手ゼロで統合します。作業ツリーを一度も触らず、最後に参照を 1 回だけ動かすので、
途中で失敗しても中途半端な統合が残りません。

**🧬 どのリポジトリでも、一覧への追記が衝突しない** — `.gitignore`・`CHANGELOG.md`・
`package.json` の依存・`import` 宣言のような「両方の行を残すだけ」の衝突を、
**中身を見て**自動判定して解決します。目印を書く必要はありません。マーカ無しの検証で
**人が読む衝突行が 80% 減り、誤って自動解決した件数は 0**。一覧と判断できないファイルでは
**素の git と 1 バイトも変わりません**。

**🔍 誰がどこを持っているか** — 同じファイルを複数人が持っている状態を横帯で見せ、
近すぎる組だけを赤枠にして「あと何行空ければ一撃で通るか」を出します。
コマンドパレットの「競合ゼロ点検」から。

**🚦 どのリポジトリでも 1 コマンド** — `zai czero init` で台帳・git フック・
マージドライバ・`.gitattributes` が一度に入り、**入れた直後に自己診断まで走ります**。

```console
$ zai czero verify
🔬 実証 — 守られています (実際に競合を起こして確かめました)
  ✅ 同じファイルでも、離れた行なら 2 人が同時に持てる
  ✅ 他人が保有するファイルへの書き込みを台帳が断る
  ✅ 一覧への両側追記を merge driver が解決する
  ✅ 他人が保有するファイルの git commit が実際に止まる
  ✅ 一覧への両側追記が、実際の git merge で自動解決する
```

`verify` は**設定を読むだけではありません**。使い捨ての一時リポジトリで
**実際に競合を起こして、本当に止まるかを実演します**（対象リポジトリは 1 バイトも
汚しません）。「入れたのに効いていない」が構造的に起こらないようにするためです。
`zai czero doctor` は段ごとに理由と直し方を、`zai czero uninstall` は入れたものだけを
綺麗に戻します（他人が書いた `.gitattributes` の行や既存のフックは無傷）。

**🧪 あなたのリポジトリで証明できます** — 合成のベンチではなく、**手元の実在の
リポジトリを複製して**同じ実験を回せます（元のリポジトリは 1 バイトも汚しません）。

```console
$ tools/anyrepo-prove.sh --repo . --writers 8
```

| リポジトリ | 書き手 | 素の git | Zaivern Code あり |
|---|---:|---|---|
| zaivern-code（Rust・追跡 259 ファイル） | 8 | **9 ファイル / 11 ハンク**衝突 | **0 / 0**（48/48 成立・拒否 0） |
| zaivern-code | 16 | **26 / 28** | **0 / 0**（96/96・30 件ずらし・拒否 0） |
| hyperframes（TS/HTML・追跡 1194 ファイル） | 16 | **26 / 28** | **0 / 0**（96/96・32 件ずらし・拒否 0） |
| 全員が同じファイルを触る合成 | 32 | **118 / 147** | **0 / 0**（192/192・171 件ずらし） |

**素の git は言語にもファイル数にも依らずに壊れます**（Rust の 259 ファイルでも
TS/HTML の 1194 ファイルでも同じ 26/28）。**乱数の種を 12 通り振り直しても
二重書き込み 0 / 衝突 0 / 拒否 0**。同じ条件で保護を外すと**毎回 4〜9 行**の
二重書き込みが出ます。

**🐧 macOS と Linux で結果が一致します** — 同じハーネスを Docker の Linux でも
回して並べました（`tools/xplat-bench.sh`）。**落とす指標（衝突ハンク数・二重書き込み・
完了件数）は全段で完全に一致**します。壁時計だけは macOS が 6〜10 倍遅く、原因は
ファイルシステムではなく**プロセス起動の値段**でした（書き込みだけの編集フェーズは
逆に macOS のほうが速い）。**時間の数字は OS をまたいで持ち込めません。**

**🪫 アイドル時はほぼ 0** — 設計原則の 1 つが「アイドル時のコストはゼロ」です。
実測（`tools/idle-cpu.sh`）で、暖機 5 秒 → アイドル 30 秒が **+0.290 秒（1 コアの
0.97%）**、暖機 70 秒 → アイドル 60 秒が **+0.190 秒（0.32%）**。基準線として置いた
`sleep` は +0.000 秒です。自走する唯一の再描画源はペットのアニメーション
（`src/pet.rs:444`。80ms から 20 秒で 160ms へ鈍り、60 秒で完全に止まります）。

**📐 どのリポジトリ形態で何が保証されるか** — `zai czero doctor` が形を検出して、
できること／できないことを言います。

| | 形 |
|---|---|
| ✅ | 素の作業ツリー・linked worktree・LFS（`merge=lfs` あり） |
| ⚠ | submodule を抱える（**submodule の中は素通り**・個別に init が要る）・sparse-checkout（cone 外の `.gitattributes` は効かない）・shallow（一撃統合が縮退）・既存のフックフレームワーク（共存はするが、向こうが書き直すと関所が消える） |
| ❌ | 非 git（フックが入らず**他のプロセスは 1 つも止まりません**）・bare・読み取り専用・**LFS と union の重なり** |

**⚠️ できていないこと（伏せません）**

- **64 体規模の `--shift` は取りこぼす回があります。** 上の「拒否 0」の注記のとおりです。
- **Windows は実行時の挙動と GUI が未検証です。** クロスコンパイルはコンパイルと
  リンクまでしか担保しません。実在リポジトリでの証明は macOS で、OS 比較は
  macOS と Docker の Linux です。**Windows で走らせた数字は 1 つもありません。**
- **`zai lease claim` は非 git のフォルダでも成功します。** 台帳はできますが
  フックが入らないので**他のプロセスは止まりません**（警告は必ず出しますが、
  終了コードは 0 のままです）。

数字と線引き（**効かない条件も含めて**）は [docs/conflict-zero.md](docs/conflict-zero.md) にあります。
掘り下げた実測は用途ごとに分かれています。

| 文書 | 何が書いてあるか |
|---|---|
| [docs/anyrepo-proof.md](docs/anyrepo-proof.md) | 実在のリポジトリでの証明と、**間欠的な二重配布の真因** |
| [docs/xplat-bench.md](docs/xplat-bench.md) | macOS と Linux を並べた実測 |
| [docs/idle-cost.md](docs/idle-cost.md) | アイドル時のコストの測り方と数字 |
| [docs/region-cost.md](docs/region-cost.md) | 行域判定そのものの費用（ハーネスを差し引く） |
| [docs/czero-repo-shapes.md](docs/czero-repo-shapes.md) | リポジトリ形態ごとの保証 |
| [docs/guard-edges.md](docs/guard-edges.md) | 書き込みの関所が漏れる形と、その塞ぎ方 |
| [docs/workspace-key.md](docs/workspace-key.md) | 台帳の置き場を決めるキーと、旧キーの引き取り |
| [docs/bench-honesty.md](docs/bench-honesty.md) | ベンチが「静かな嘘」をつかないための決まり |

## 対応環境

| 項目 | 対応内容 |
|---|---|
| OS | macOS arm64/x86_64・Linux x86_64/arm64・Windows x86_64 |
| AI CLI | Claude Code・Codex・Gemini CLIほか、33種類のプリセット |
| Rust | ソースビルド時のみ1.88以上 |
| ライセンス | Apache-2.0 |

## 安全設計

- 承認必須を既定値にし、自動YESは明示的に有効化
- 権限昇格は常に手動承認
- MCP設定の環境変数は値を表示せず、設定済みかだけを表示
- SSHトンネル使用時はリモートサーバーを`127.0.0.1`のみにバインド
- セッション破棄・終了時に子プロセスを停止し、孤児プロセスを残さない
- 書き込みの関門は**fail-open**。`zai`が見つからない・台帳が壊れているときは通します
  （「ツールを消したらコミットできない」は許されません）。止めるのは**本物の競合**のときだけです

## よくある質問

### AIツールは同梱されていますか？

いいえ。Claude Code・Codex・Gemini CLIなど、使いたいAIツールを別途インストールしてログインしてください。

### 3種類すべて必要ですか？

いいえ。1種類だけでも使えます。最初は普段使っているAIツール1つで試すのがおすすめです。

### Zaivern Codeは無料ですか？

Zaivern CodeはApache-2.0ライセンスの無料オープンソースソフトウェアです。各AIサービスの利用料金や契約は別途必要です。

### 勝手にコマンドを実行しませんか？

初期状態では承認が必要です。自動承認を使う場合も、自分で明示的にオンへ切り替えます。

### 「競合ゼロ」は本当に0件ですか？

**保護のある状態では0件です**が、条件を正直に書きます。担当が互いに`SAFE_BAND`（3行）以上
離れていれば、gitの三方向マージは**構造的に**衝突を出しません。1ファイルに64体を
ぶつける実測で、衝突ハンク0・手作業0を確認しています。実在のリポジトリでも、
乱数の種を12通り振り直して**毎回0**でした。

3行という幅は**パッチ適用（`git apply`）を通す前提**の値です。三方向マージ
（`git merge`）だけを保証すればよい場面では**1行離れていれば足りる**ことを、
4つのdiffアルゴリズム（myers / minimal / patience / histogram）すべてで確かめました
（`region::MERGE_ONLY_BAND`）。「diffの既定の文脈が3行だからハンクが畳まれる」は
誤りで、3行は*表示*の話です。

**できないこと**もあります。ファイルが違うのに噛み合わない変更（片方が引数を変え、
もう片方が古い呼び方のまま書く）は原理的に防げません。これは検出して画面に出す側で
扱います。数字と線引きは[docs/conflict-zero.md](docs/conflict-zero.md)にあります。

### 既存のリポジトリでも使えますか？

`zai czero init`で入ります。既存のgitフック（husky / lefthook / pre-commit framework）は
壊さず、先に呼んで終了コードを尊重します。`.gitattributes`に人が書いた行も残します。
`zai czero uninstall`で入れたものだけを戻せます。

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

### 主張を自分で確かめる

主張を支えるハーネスはすべてリポジトリに入っています。**どれもホストの `target/` を
汚さず、本物の `~/.zaivern` にも触りません**（一時ディレクトリへ `HOME` ごと退避します）。

```bash
tools/anyrepo-prove.sh --repo . --writers 8   # 自分のリポジトリで「競合ゼロ」を証明する
tools/coedit-bench.sh --agents 64 --lines 2000 --mode all   # 行域オーナーシップ
tools/conflict-zero-bench.sh --writers 8      # ベースラインとガードの比較
tools/xplat-bench.sh                          # macOS と Linux を並べる
tools/idle-cpu.sh --baseline                  # アイドル時のCPU（sleepを下限として並べる）
tools/region-cost.sh                          # 行域判定そのものの費用
tools/verify.sh --lint                        # 整形＋コンパイル＋テスト＋clippy
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
