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

### 64 体の `c+` (交渉あり) は **OS 差ではなく実行ごとのブレ**

`c+` (`zai lease claim --shift`) の完了数だけは走らせるたびに揺れた。
crowded / 64 体で 6 回測った内訳:

| 測定 | macOS | Linux |
| --- | --- | --- |
| 掃引 (8,16,32,64) | **58/64** (6 件拒否) | 64/64 |
| 追試 1 | 64/64 | **47/64** (17 件拒否) |
| 追試 2 | 64/64 | 64/64 |

**片方の OS の問題ではない。両方で出る。** 同じ担当表を参照ゲート
(`c+ref`, `src/region.rs` の `spans_too_close` をハーネス内で再実装したもの) に
掛けると **6 回とも 64/64** なので、**「拒否 0 は原理的に到達可能で、出荷物の
`--shift` がときどき取りこぼしている」**と読める。README / コミットの
「crowded 64 体で拒否 53 → 0」は**平均的には正しいが、毎回そうなるとは限らない**。

これは CLAUDE.md の「固定の待ち予算は N が増えれば必ず破綻する」と同じ形の
現象に見える (32 体以下では 1 度も出ない)。**追いかけるべき実装側の宿題**として
残す。ハーネス側では `denied` を落とす指標に入れていないので、赤にはならない。

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
