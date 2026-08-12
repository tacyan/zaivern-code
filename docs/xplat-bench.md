# OS をまたいで「競合ゼロ」を測り直す (xplat-bench)

## なぜ要ったのか

`docs/conflict-zero.md` と `docs/conflict-bench.md` の数字は、**全部 macOS で
しか測っていなかった**。だがこの機能が寄りかかっているのは言語機能ではなく、
**ファイルシステムの挙動**である。

* `create_new` / `rename` のアトミック性
* 削除の見え方 (Windows は *delete pending* を経て `ACCESS_DENIED (os error 5)` を返す)
* ロックの取り合いで何が返るか (`lease::lock_contended`)

OS が変われば数字も、場合によっては**結論も**変わりうる。片方の OS の数字を
「実測」と書くのは、測っていない側について黙って嘘をつくことになる。

## 使い方

```sh
tools/xplat-bench.sh                        # 既定 (8,16 体)
tools/xplat-bench.sh --writers 8,16,32,64   # 掃引する
tools/xplat-bench.sh --json                 # JSON は stdout、表は stderr
tools/xplat-bench.sh --bench cz             # conflict-zero だけ
tools/xplat-bench.sh --host-only            # Docker を使わない
tools/xplat-bench.sh --linux-fs overlay     # コンテナ側を overlayfs で測る
```

やることは 4 つだけ。

1. ホスト側で `zai` を用意する (無ければビルドする)
2. **同じソースから** Linux コンテナ内でも `zai` をビルドする
3. 両側で `tools/conflict-zero-bench.sh --json` と `tools/coedit-bench.sh --json` を
   **同じ引数・同じ種**で走らせる
4. 並べた表と JSON を出す

Docker が無い / 動いていないときは、**理由を名指しして skip し、ホスト側だけで
続行する**。黙って飛ばさない。

## 何で合否を決め、何を決めないか

**ここを間違えると必ず嘘の赤が出る。** CLAUDE.md の「絶対時間で性能テストの線を
引かない」と同じ理由で、時間とスケジューリングに依存する指標は**情報として
出すだけ**にしてある。

| | 指標 | 扱い |
| --- | --- | --- |
| ✗ 落とす | 衝突ハンク / 2 人以上が書いたファイル / 衝突したマージ / 範囲外配分 / 契約違反 / 近すぎる配分 / **下位ベンチ自身の終了コード** | OS が違っても同じでなければおかしい |
| △ 出すだけ | 書けた件数 / 断った件数 / 壁時計 / ゲートの p50・p95・max / ずらした距離 | OS 差として当然ありうる |

この線引きが正しかったことは実測で裏が取れた。**「断った件数」を落とす側に
入れていたら、この版は赤になっていた** (下の「64 体の `c+`」を参照)。

段が片側でしか動かなかった場合 (zai の能力差など) は `△ 片側のみ` として出し、
**落とさない**。ただし「同じものを測った」と読めてしまうので、両側の
サブコマンド一覧の差を表の前に必ず出す。

## 実測 (2026-08-11)

測った相手:

| | ホスト | コンテナ |
| --- | --- | --- |
| OS | Darwin x86_64 (macOS) | Linux x86_64 (Debian 13 / rust:1.90-slim + git,python3) |
| 作業場の FS | apfs | ext4 (Docker の名前付きボリューム) |
| `zai` | `target/debug/zai` 0.14.0 | `/target/debug/zai` 0.14.0 |
| サブコマンド | 22 個 | **完全に同じ 22 個** |

引数: `--writers 8,16,32,64`・`--overlap 0.5`・`--seed 20260810`・
conflict-zero は 48 ファイル (64 体のときだけ 64)・coedit は 800/800/1024/2048 行。
所要 38 分 (両側・両ベンチ・4 サイズ)。

### conflict-zero-bench — 落とす指標は **20 段すべてで完全一致**

値は `macOS / Linux`。

| 体数 | 段 | 書けた | 断った | 重複file | ハンク | 衝突マージ |
| --- | --- | --- | --- | --- | --- | --- |
| 8 | baseline | 48 / 48 | 0 / 0 | 5 / 5 | 7 / 7 | 4/8 / 4/8 |
| 8 | guard | 27 / 27 | 21 / 21 | **0 / 0** | **0 / 0** | 0/8 / 0/8 |
| 8 | train | 48 / 48 | 0 / 0 | 5 / 5 | 7 / 7 | 3/8 / 3/8 |
| 8 | union | 48 / 48 | 0 / 0 | 5 / 5 | 7 / 7 | 4/8 / 4/8 |
| 16 | baseline | 48 / 48 | 0 / 0 | 3 / 3 | 11 / 11 | 7/16 / 7/16 |
| 16 | guard | 26 / 26 | 22 / 22 | **0 / 0** | **0 / 0** | 0/16 / 0/16 |
| 32 | baseline | 32 / 32 | 0 / 0 | 1 / 1 | 12 / 12 | 12/32 / 12/32 |
| 32 | guard | 16 / 16 | 16 / 16 | **0 / 0** | **0 / 0** | 0/32 / 0/32 |
| 64 | baseline | 64 / 64 | 0 / 0 | 1 / 1 | 27 / 27 | 27/64 / 27/64 |
| 64 | guard | 32 / 32 | 32 / 32 | **0 / 0** | **0 / 0** | 0/64 / 0/64 |
| 64 | union | 64 / 64 | 0 / 0 | 1 / 1 | 27 / 27 | 27/64 / 27/64 |

(train / train(素朴順) / union の全行も同じく一致。全 20 段で差 0。)

**結論: 競合ゼロの主張は OS に依存しない。** ゲートは macOS でも Linux でも
64 体まで「2 人以上が書いたファイル 0・衝突ハンク 0」を保つ。しかも
`denied` の数まで 4 サイズ全部で 1 件も違わなかった (21/21・22/22・16/16・32/32)。

### 壁時計 — **macOS は Linux の 6〜10 倍遅い**

値は `macOS / Linux` と倍率。

| 体数 | 段 | マージ〜解消 | 倍率 | 段の全体 | 倍率 |
| --- | --- | --- | --- | --- | --- |
| 8 | baseline | 2.4s / 340ms | 7.0x | 6.1s / 954ms | 6.3x |
| 16 | baseline | 4.1s / 588ms | 7.0x | 10.8s / 1.9s | 5.8x |
| 32 | baseline | 8.3s / 1.2s | 6.7x | 22.3s / 3.4s | 6.5x |
| 64 | baseline | 16.0s / 2.6s | 6.1x | 49.9s / 7.2s | 6.9x |
| 64 | guard | 6.0s / 1.0s | 5.9x | 64.1s / 6.0s | **10.7x** |
| 64 | union | 35.5s / 4.0s | 8.8x | 115.3s / 11.5s | 10.1x |

ゲート 1 回の待ち (`zai hook` = `crate::lease::gate`):

| 体数 | p50 | p95 | max |
| --- | --- | --- | --- |
| 8 | 46ms / 33ms | 82ms / 107ms | 93ms / 117ms |
| 16 | 91ms / 66ms | 286ms / 148ms | 403ms / 154ms |
| 32 | 152ms / 56ms | 267ms / 86ms | 300ms / 109ms |
| 64 | 82ms / 56ms | 199ms / 110ms | 217ms / 138ms |

**仮説 (未検証)**: 差の出どころはファイルシステムそのものより
**プロセス起動 (fork/exec) の値段**である可能性が高い。根拠は倍率の分布で、
差が最も大きいのは git を大量に起こす「マージ〜解消」と「段の全体」(6〜10 倍)
なのに対し、**書き込みだけの「編集」フェーズは逆に macOS のほうが速い**
(64 体 baseline で 51ms / 172ms)。ファイルシステムの生の書き込みが遅いなら
編集フェーズも遅くなるはずで、そうなっていない。macOS は
`posix_spawn` + dyld のライブラリ解決 + コード署名の検証が 1 プロセスごとに
乗るので、**git を数百回起こす段だけが伸びる**という形と一致する。
確かめるには「git の呼び出し回数」と「1 回あたりの所要」を分けて数える必要が
あり、それはこのハーネスの担当ではない。

### coedit-bench (行域オーナーシップ) — 落とす指標は **64 行すべてで一致**

`disjoint` は全段・全サイズで `完了 = 体数 / 衝突ハンク 0 / 範囲外 0`、
`crowded` は素の git (段 a) だけが衝突ハンクを出し (8→6 / 16→12 / 32→24 / 64→48)、
保護のある段は**両 OS とも 0**。ここも差は 1 件も無い。

### 64 体の `c+` (交渉あり) — 0.15.0 で塞いだ。**Linux は要再測定**

`c+` (`zai lease claim --shift`) の完了数だけが 0.14.0 で揺れた。
**原因は競合状態でも OS 差でもなく、ずらし上限の固定値**である。
既定 `negotiate.max_shift = 200 行`に対し crowded な計画は 204〜234 行の
ずらしを要求していた (空きは 1868 行あった)。0.15.0 の
`lease::shift_ceiling` が上限を混雑ぶんだけ広げて塞いだ。
`tools/coedit-bench.sh --agents 64 --lines 2000 --layout crowded` を
6 回まわして **6 回とも 64/0** (参照ゲートと完全一致)。

> **【要再測定】直した後の 6 回は macOS でしか測っていない。**
> この文書の Linux 側の 64 体 `c+` の数字は**すべて修正前のもの**なので、
> `tools/xplat-bench.sh` で取り直すまで OS 比較としては使えない。

参照ゲート (`c+ref`) は 0.14.0 の時点でも **6 回とも 64/64** だった。
つまり「拒否 0」は当時から原理的に到達可能で、落ちていたのは出荷物だけである。

#### 当時こう書いていた (反証済み・履歴)

> **OS 差ではなく実行ごとのブレ。** crowded / 64 体で 6 回測ると、
> 掃引 macOS **58/64** (6 件拒否) / Linux 64/64、追試 1 macOS 64/64 /
> Linux **47/64** (17 件拒否)、追試 2 は両方 64/64。
> 32 体以下では 1 度も出ない。

**これは否定された。** 後日 6 回測り直したところ、6 回とも
`51/13 · 54/10 · 53/11 · 55/9 · 54/10 · 54/10` で、**64/64 は 1 度も出なかった**。
「4 回は 64/64」も「稀」も再現しない。詳細は
[docs/conflict-zero.md](conflict-zero.md) §6-1。

## 測り方の細かいところ

* **ホストの `target/` を 1 バイトも触らない。** コンテナの `CARGO_TARGET_DIR` は
  ワークツリーごとの名前付きボリューム (`tools/linux-test.sh` と同じ命名)。
* **`rust:*-slim` には python3 が無い。** `conflict-zero-bench.sh` は python3 が
  無いと 1 行も走らないので、git と python3 を足した派生イメージを 1 度だけ作る
  (`zaivern-xb-<cksum>`)。
* **コンテナ側の作業場は既定で無名ボリューム (`/bench`)。** 何もしないと
  `mktemp -d` はコンテナの書き込み層 = **overlayfs** に落ちる。overlayfs は
  copy-up を挟むので、ロックの取り合いを測る土俵としては素直ではない。
  `--linux-fs overlay` で意図的に overlayfs 側も測れる。**この既定は実測で
  決めた**: 同じ conflict-zero を overlayfs で測り直すと、落とす指標
  (重複ファイル / ハンク) は 10 段すべて ext4 と完全に同じまま、
  マージ〜解消だけが **1.1〜1.9 倍**遅くなった (8 体 baseline 590ms / 340ms、
  16 体 baseline 1120ms / 588ms、8 体 union 1412ms / 827ms)。
  既定を overlayfs にしていたら、**Linux 側の数字にこの税が黙って乗る**。
* **GNU の `stat -f -c %T` は ext4 を "ext2/ext3" と呼ぶ** (magic が同じ)。
  実名が要るので `/proc/mounts` を先に見る。
* **両側で同じ `zai` を使わせる。** `ZAIVERN_BIN` を明示的に渡す。これをしないと
  下位ベンチが**別々のバイナリ**を拾う (下記「見つけた地雷」)。

## 見つけた地雷

1. **`conflict-zero-bench.sh` と `coedit-bench.sh` は `zai` の探索順が逆。**
   前者は `release` → `debug`、後者は `debug` → `release`。この環境には
   0.12.0 の古い `release` と 0.14.0 の `debug` が両方あったので、
   **同じセッションで 2 つのベンチが別のバイナリを測る**状態だった。
   `xplat-bench.sh` は `Cargo.toml` の版と `--version` を照合したうえで
   `ZAIVERN_BIN` を明示的に渡してこれを潰している。
2. **`conflict-zero-bench.sh` には事前の能力検査が無い。** `zai help` に
   `zai hook` / `zai lease` という文字列があるかを見るだけで、
   **ゲートが本当に 1 件でも止められるのかを測る前に確かめていない**。
   壊れたゲートが検出されるのは「重なりがあるのに衝突が残った」という
   *事後*の判定だけなので、`--overlap 0.0` で走らせると素通りする。
   `coedit-bench.sh` のほうは `lease claim` を 3 回撃つ事前検査 (exit 20〜23) を
   持っていて、こちらが手本。**xplat-bench は両側の版とサブコマンド一覧を
   毎回出して差を先に見せる**が、ゲートの実効性検査そのものは
   `conflict-zero-bench.sh` の担当なので直していない。
3. **`rust:*-slim` に python3 が無い**ことに気付かず `conflict-zero-bench.sh` を
   コンテナで走らせると、「python3 が見つかりません」で即死する。

## Windows

**実行時の挙動はここでは担保できない。** `tools/windows-check.sh` は
`cargo xwin check` / `clippy` であって、**コンパイルが通ることしか言わない**。
Windows の *delete pending* (`ACCESS_DENIED`) は `lease::lock_contended` が
握っている実行時の分岐なので、**走らせないと 1 バイトも検証されない**。
Windows の数字を取るには CI の `windows-latest` でこのベンチを回すしかない。

今回 `tools/windows-check.sh --clippy` を実際に走らせた結果は
**緑 (rc=0 / 警告 0 件 / 4 分 40 秒)**。つまり「Windows 向けにコンパイルは通る」
ところまでは確定したが、**それ以上は何も言えない**。

`.github/workflows/` は共有面なので、ここでは案だけを残す (統合担当が直列に入れる)。

### CI に足すべきジョブの案 (ワークフローは共有面なので触っていない)

> **【この案は 2026-08-12 に実装された。以下は当時の案の記録である。】**
> 実際に入ったのは `.github/workflows/xplat.yml` で、案とは形が違う
> (`zai` のビルドが要らない probe を毎 push・3 OS で回し、
> `zai` が要るものだけを schedule / 手動へ回した)。
> 現在の内容は下の「追測 (2026-08-12)」を見ること。

`windows-latest` の runner には git・python3 (`python`) が最初から入っているので、
**コンテナは要らない**。ホスト側だけを走らせればよい。

```yaml
  xplat-windows:
    # 毎 push で回すには重い (zai のビルドが要る)。週次 + 手動を想定。
    if: github.event_name == 'schedule' || github.event_name == 'workflow_dispatch'
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2      # CLAUDE.md の「CI は最小構成 + rust-cache 必須」
      - run: cargo build --bin zai
      - name: 競合ゼロを Windows で測る
        shell: bash                        # git 同梱の bash。POSIX sh で書いてある
        env:
          ZAIVERN_BIN: target/debug/zai.exe
        run: tools/xplat-bench.sh --host-only --writers 8,16,32,64 --json > xplat-windows.json
      - uses: actions/upload-artifact@v4
        with: { name: xplat-windows, path: xplat-windows.json }
```

* `--host-only` にするのは、Windows の runner で Linux コンテナを起こせないため
  (Windows コンテナは別物で、rust イメージも別)。
* **ここでしか出ない不具合**が本命: *delete pending* で `create_new` が
  `ACCESS_DENIED (os error 5)` を返す経路 (`lease::lock_contended`) は、
  64 体が取り合ったときにだけ踏む。macOS / Linux では**一度も通らない**。
* 出た JSON はこのドキュメントの表と同じ形なので、macOS / Linux の JSON と
  並べれば 3 つ目の列になる。突き合わせは `xplat-bench.sh` の判定と同じ規則
  (落とす指標だけを比べる) で行うこと。

---

# 追測 (2026-08-12) — git の版の軸を埋める

上の §「実測 (2026-08-11)」は **OS の軸** (macOS / Linux) を埋めたが、
**git の版はどちらも 2.47.x の 1 点**だった。ところが中核の判定は git の版で
はっきり変わる。埋めたのがこの節である。

再現: `tools/git-matrix-prove.sh` / `tools/git-portability-probe.sh` /
CI の `.github/workflows/xplat.yml`。

## なぜ OS ではなく git だけを振るのか

ディストリを替えると glibc も FS も python も一緒に変わり、
**「git の版のせい」と言えなくなる**。そこで土台イメージを
`rust:1.90-slim` (Debian trixie) に固定し、**git だけをソースから入れ替えた**。

裏取りとして、素の `debian:<コードネーム>-slim` に apt で入る git でも同じ
プローブを回した。**ソースから作った 2.30.2 と、bullseye の apt が入れる
2.30.2 は 1 項目も違わない** — 作り方の違いが結果に混ざっていないことの確認。

## 素の git の挙動 (`tools/git-portability-probe.sh`)

| 測った場所 | OS | git | `merge-tree --write-tree` | `git merge` の既定戦略 | FS が大小を畳む | `lease.rs` の `cfg!` | CRLF のマージ | 判定 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| ホスト | macOS 26 (darwin 25.5.0) | **2.47.1** | あり | ort | **はい** | 畳む | 3 通りとも clean | ok |
| コンテナ (trixie) | Linux 6.10 | **2.47.3** | あり | ort | いいえ | 畳まない | 3 通りとも clean | ok |
| コンテナ (trixie + 源) | Linux 6.10 | **2.30.2** | **なし** | **recursive** | いいえ | 畳まない | 3 通りとも clean | ok |
| コンテナ (bookworm) | Linux 6.10 | 2.39.5 | あり | ort | いいえ | 畳まない | 3 通りとも clean | ok |
| コンテナ (bullseye) | Linux 6.10 | 2.30.2 | **なし** | **recursive** | いいえ | 畳まない | 3 通りとも clean | ok |

「CRLF のマージ」は `core.autocrlf` を **未設定 / true / input** の 3 通りで
回した結果。CRLF のファイルへ互いに素な 2 つの編集を入れて `git merge` する
だけの、道具を 1 つも挟まない測定である
(CLAUDE.md の「ハーネスは自分で壊した結果を測ることがある」の裏取り —
`anyrepo-prove.sh` が CRLF を LF へ書き換えていた件は**道具側の欠陥**で、
git は 5 つの環境すべてで CRLF のまま綺麗にマージする)。

### 分かった差は 2 つ。どちらも **2.34 / 2.38 の境**で出る

1. **`git merge-tree --write-tree` は 2.38 未満に無い。**
   `src/conflict.rs` は版番号ではなく usage の中身で判定しているので
   (`merge_tree_probe_argv` / `conflict.rs:825-830`)、**予想と実測は 5 環境
   すべてで一致した**。無い側では行範囲だけの判定へ正しく縮退し、
   `region::` の三方向テストは理由付きで skip される。**壊れない。**
2. **`git merge` の既定戦略は 2.34 未満で `recursive`** (= diff は
   `diff.algorithm` 既定の myers)。2.34 以降は ort で histogram 固定。

## 2 番目の差が効く場所 — §3.16 の反証表は **git 2.34 以降の話**だった

`docs/conflict-zero.md` §3.16 は「帯を満たしていても反復本文では衝突する」
という穴を `A={17} B={5,13,25}` で示している。**これを git の版で振り直した**
(`tools/merge-band-probe.sh --mode bracket`、周期 6 の 600 行):

| git | 戦略 | unique | fences | blank | generated | repeat |
| --- | --- | --- | --- | --- | --- | --- |
| 2.47.1 (macOS) / 2.47.3 (Linux) | ort (histogram) | clean | **衝突** | clean | **衝突** | **衝突** |
| **2.30.2 (Linux)** | **recursive (myers)** | clean | clean | clean | clean | clean |

**古い git のほうが通る。** §3.16.2 が挙げている原因
(「ort は diff-algorithm を histogram に固定している」) と完全に整合する。

これは主張が崩れたのではなく、**主張の適用範囲が測れた**ということである:

* §3.16 の失敗表は **git ≥ 2.34 でだけ再現する**。
  「素の git ならどこでも起きる」とは読まないこと。
* **直した後のモデル (帯 + 交錯/錨 + 昇順) の判定は両方の版で同一**で、
  ort で衝突する 3 つを **2.30.2 でもきちんと「断」にしている**。
  つまり古い git では**過剰に断っている** (fail-closed) だけで、
  「通してよいと言ったのに衝突した組」は **両方の版で 0**。安全側は保たれている。

## 帯そのもの (`--mode pairs`) は版に依らない

`region::MERGE_ONLY_BAND` の根拠になった測定を 4 環境で回した結果は
**完全に同一**。5 種類の本文 (unique / fences / blank / generated / repeat)
すべてで `gap=1` は衝突、`gap>=2` は clean。

> **`gap` の定義が 2 つある。混ぜないこと。**
> このハーネスの `gap` は**行番号の差** (行 100 と 100+gap) で、
> `region::SAFE_BAND` / `MERGE_ONLY_BAND` の `gap`
> (`region::spans_too_close`) は**間に挟まる未変更行の数**である。
> ハーネスの `gap=2` = 未変更行 1 行 = `MERGE_ONLY_BAND = 1`。
> つまり下の表は `MERGE_ONLY_BAND = 1` と**矛盾していない**、
> どころかそれを 4 環境で再確認したものである
> (`src/region.rs:45-62` の下限表を参照)。

| git | gap=1 | gap=2 | gap=3 | gap=4 | gap=6 | gap=8 |
| --- | --- | --- | --- | --- | --- | --- |
| 2.47.1 (macOS) | 衝突 ×5 | clean ×5 | clean ×5 | clean ×5 | clean ×5 | clean ×5 |
| 2.47.3 (Linux) | 衝突 ×5 | clean ×5 | clean ×5 | clean ×5 | clean ×5 | clean ×5 |
| 2.39.5 (Linux) | 衝突 ×5 | clean ×5 | clean ×5 | clean ×5 | clean ×5 | clean ×5 |
| 2.30.2 (Linux) | 衝突 ×5 | clean ×5 | clean ×5 | clean ×5 | clean ×5 | clean ×5 |

## リポジトリの形ごとの可否 — macOS と Linux で **差 0**

`docs/anyrepo-proof.md` の「形ごとの可否」の表は **macOS / git 2.47.1 の
1 点**でしか測っていなかった。`tools/anyrepo-prove.sh --shapes --writers 4
--overlap 1.0` を Linux でも回した結果:

| 形 | macOS 2.47.1 | Linux 2.47.3 | Linux **2.30.2** |
| --- | --- | --- | --- |
| 普通のリポジトリ / bare / 連結 worktree / shallow / 1 コミット / detached HEAD / 未コミットあり / sparse-checkout / submodule / LFS / 巨大 (4200 件) | `proved`・素の git 6 ファイル 7 ハンク → **0 / 0**・重複行 0・予約 24/24 成立・拒否 0 | **同左 (全項目一致)** | **同左 (全項目一致)** |
| コミットが 0 件 / 非 git ディレクトリ | `skip` (理由付き) | 同左 | 同左 |

**13 形 × 3 環境 = 39 通りで、落とす指標が 1 つも違わなかった。**
`merge-tree --write-tree` の無い 2.30.2 でも `skip` は増えず、全部 `proved`
のまま — つまり `--shapes` の経路は `merge-tree` に寄りかかっていない。

`docs/anyrepo-proof.md:182` の「(git 2.47.1 / macOS)」は
**「macOS / Linux・git 2.30.2〜2.47.3 で同じ」**へ広げてよい (統合担当へ)。

## 残った穴 — 大文字小文字の畳みは **FS ではなく `cfg!` で決めている**

プローブが見つけた唯一の構造的な問題。`src/lease.rs:240` は

```rust
cfg!(any(windows, target_os = "macos"))
```

つまり**コンパイル時の OS** で「台帳のパス鍵を小文字へ畳むか」を決めている。
実 FS の挙動ではない。食い違うと 2 方向あり、片方は穴になる:

| `cfg!` | 実 FS | 何が起きるか |
| --- | --- | --- |
| 畳む | 畳まない | 本当は別物の `Foo.rs` / `foo.rs` を 1 つと見なす。**過剰に断るだけ (fail-closed)**。case-sensitive の APFS / WSL の per-directory case sensitivity で起こる |
| **畳まない** | **畳む** | **同じファイルに 2 つの鍵ができる。**2 人が「互いに素」な行域を同じ実ファイルへ持てる = 競合ゼロが破れる。Linux で case-insensitive なマウント (ext4 の casefold / CIFS / exFAT) を使うと起こる |

今回測った 5 環境では **1 つも食い違わなかった** (macOS = 畳む / 畳む、
Linux = 畳まない / 畳まない) ので、**現に壊れてはいない**。
ただし「どのリポジトリでも」を名乗る以上、置き場が変われば踏む。
`tools/git-portability-probe.sh` は食い違いを検出し、**穴の開く向きのときだけ
判定を赤にする** (G5)。CI の `probe` ジョブが 3 OS で毎週これを見る。

判定の前に、**`src/lease.rs` の式そのものをソースから読んで照合する**
(`read_fold_cfg`)。プローブ側に `Windows または macOS` と書き写すと、
実装が変わった日に**プローブだけが黙って古くなる**ためである
(CLAUDE.md の「ソースを読む回帰テストは改行を正規化する」に従い、
CRLF のチェックアウトでも外れないようにしてある)。式が変わっていたら
G5 は**落とさずに「式が変わっている」と出して降りる** — 写経した想定で
赤にするのは嘘の赤だからである。

直し方の案 (実装は統合担当へ。`src/` は触っていない):
`src/lease.rs:240` の `fold_case` を、`cfg!` ではなく
**台帳のルートで実際に 1 度だけ試した結果** (`Foo` を作って `foo` で開けるか)
から決める。純関数の `normalize_path_on(raw, win_sep, fold_case)`
(`src/lease.rs:250`) は既に `fold_case` を引数で受け取っているので、
**呼び出し側の 1 行だけ**で済む。表で固定しているテスト
(`src/lease.rs:6958` 三つの OS の正規化規則) は `fold_case` を直接渡す形なので
影響を受けない。

なお `src/lease.rs:287` は Unicode の `to_lowercase()`、
`src/history.rs:302` は ASCII 限定の `make_ascii_lowercase()` を使っており、
**非 ASCII のパスでは台帳の鍵とワークスペース鍵の畳み方が違う**。
穴ではない (どちらも一貫して同じ側で使われる) が、揃えておくのが安全。

## Windows — CI のジョブを実際に足した

上の「### CI に足すべきジョブの案」は案のままだったので、
**`.github/workflows/xplat.yml` として入れた**。案との違い:

* **`zai` のビルドが要らない `probe` を毎 push・3 OS で回す。**
  素の git と python3 だけで動くので 1 ジョブ 1 分前後。
  `xplat-bench.sh --host-only` は `zai` のフルビルドが要り、
  3 OS 分では 5 分予算を確実に割るため、そちらは `shapes` として
  **schedule と手動起動に限った**。
* **git の版の軸も CI で回す** (`git-matrix` ジョブ、
  `debian:{bullseye,bookworm,trixie}-slim` のコンテナ = git 2.30 / 2.39 / 2.47)。
  ここも cargo を 1 度も呼ばない。
* `test.yml` とは別ワークフローなので、**既存のジョブは 1 秒も遅くならない**。

### Windows について、まだ言えないこと

`tools/windows-check.sh --wine` で Windows バイナリのテストを wine 上で
実行できるが、**wine は実機 Windows ではない**。とくに *delete pending*
(最後のハンドルが閉じるまで削除が中間状態に留まり、その間の `create_new` が
`ACCESS_DENIED (os error 5)` を返す) は wine が忠実に再現しない。
`lease::lock_contended` のこの分岐を本当に踏ませられるのは
**CI の `windows-latest` だけ**である。

`.github/workflows/test.yml` の `windows-lease` ジョブが
`cargo test --bin zai lease::` を実機 Windows で回しているので、
**単体の経路はすでに CI で毎回検証されている**。
未検証のまま残っているのは、`tools/anyrepo-prove.sh --shapes` のような
**シェルのハーネスを Windows の git-bash で通すこと**で、
これが `xplat.yml` の `shapes` ジョブ (schedule / 手動) の担当である。

### wine で実際に走らせて出たもの (2026-08-12)

`tools/windows-check.sh --wine lease:: region:: conflict:: czero_init::` の実測。
**226 件成功 / 43 件失敗**。43 件の内訳は 2 種類で、**片方は環境由来、
もう片方は実装の非対称**である。混ぜてはいけない。

#### (a) 環境由来 — 40 件。**regression ではない**

`features::czero_init::imp::tests::*` の 40 件は、wine のイメージ
(debian + wine) に **git が入っていない**ために落ちている。Windows バイナリ
から見える `git.exe` が存在しないので、git を起こすテストは
**スキップではなく FAILED になる**。

`tools/linux-test.sh` は「git が無いと*無言でスキップ*される」を警告していたが、
wine 側はもっと質が悪い — **環境由来の赤が本物の赤を埋める**。
`tools/windows-check.sh` に `warn_no_git_in_wine` を足して、
走らせる前に何が赤くなるかを名指しするようにした。

#### (b) 実装の非対称 — 3 件。**こちらが本題**

```
thread 'lease::span_tests::六十四体が同じファイルの違う行を同時に取れる'
  panicked at src/lease.rs:7797:
  台帳が壊れた: 台帳を差し替えられません: Access denied. (os error 5)
```

64 スレッドが取り合ったときにだけ出る。**取り合いの無いテストは
36/39 が緑**なので、wine の `rename` が壊れているのではなく
**delete pending の経路にだけ入っている**。

コードの非対称はこうなっている:

| 操作 | 場所 | Windows の `ACCESS_DENIED (os error 5)` を… |
| --- | --- | --- |
| ロックの取得 (`create_new`) | `lock_contended` (`src/lease.rs:2981-2987`) | **取り合いとして待つ** (`cfg!(windows) && PermissionDenied`) |
| 台帳の差し替え (`rename`) | `write_store` (`src/lease.rs:2807-2819`) | **待たない。`台帳を差し替えられません: {e}` で即失敗** |

さらにその文字列は `is_lock_busy` (`src/lease.rs:2781-2783`) が見る
`LOCK_BUSY` 接頭辞 (`src/lease.rs:200`) を持たないので、
**呼び出し側の再試行にも乗らない**。`Err(e) => panic!("台帳が壊れた")` へ落ちる。

CLAUDE.md はまさにこの現象を「Windows はファイル削除が *delete pending* を
経る … 64 体がロックを奪い合うと必ず踏み、**いちばん混んでいるとき =
いちばん衝突しやすいときにだけ**台帳が使えなくなる」と書いている。
**その対策がロック取得側にしか入っていない。**

##### 正直に言えないこと

* **実機 Windows で今これが起きているとは言えない。** CI の
  `windows (lease/instances)` ジョブは `cargo test --bin zai lease::` を
  windows-latest で回していて、直近の main は **緑**である。
* wine の NT ファイル意味論は実機と同じではない。`MoveFileEx` の
  置換が「開かれている相手」に対して何を返すかは実装差が出やすい箇所。
* したがってこれは「**実機でも踏みうる非対称が構造として残っている**」
  という指摘であって、「実機が壊れている」という報告ではない。

##### 直し方の案 (`src/` は触っていない。統合担当へ)

`src/lease.rs:2815` の `std::fs::rename` を、`acquire_lock_in` と同じ
**上限付きの再試行**で包む。判定は既にある `lock_contended`
(`src/lease.rs:2981`) をそのまま使えば、Windows でだけ待って
unix では即失敗するという既存の方針と揃う。
再試行しても駄目だったときだけ `LOCK_BUSY` 接頭辞を付けて返せば、
`is_lock_busy` を通る呼び出し側の再試行 (`with_store_retry`) にも乗る。

3 件目 (`六十四体が同時に確保してもbusyは出ず勝者は一つ` が
`busy-deny 62 件 / 2929ms`) は、CLAUDE.md の
「固定の待ち予算は N が増えれば必ず破綻する」がそのまま出たもの。
wine ではロック操作 1 回が実機より桁で遅いので `LOCK_WAIT_MS` を使い切る。
**進捗が観測できる限り待ちを延ばす**という既存の方針を、
この経路にも適用するのが筋。
