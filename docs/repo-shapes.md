# リポジトリの形ごとの競合ゼロ — 実測で ⚠ / ❌ を動かした記録

計測環境: macOS 26.5.2 (darwin 25.5.0) / **git 2.47.1** / 2026-08-12。
harness: `tools/repo-shapes-prove.sh --seed 20260812` (決定的)。

この文書は [docs/czero-repo-shapes.md](czero-repo-shapes.md) の対応表のうち、
**⚠ と ❌ を実測で洗い直した結果**である。動かせたものは根拠を、
動かせなかったものは**なぜ原理的に無理か**を測定付きで残す。

## 0. 何を「保証」と数えたか

**`czero doctor` の ✅ を成功と数えない。** 判定関数が主張している保証そのものを
見ているかを確かめるのがこの検証の目的なので (`docs/conflict-zero.md` §3.11.5)、
doctor の答えは「実測と食い違っていないか」の照合にしか使わない。
実際に測るのは 3 つで、どれも**起きたこと**である。

| 記号 | 測るもの | 測り方 |
| --- | --- | --- |
| **G1 関所** | 他人が保有する行域へ書いた `git commit` が**本当に止まるか** | 連結 worktree を 1 本足して**そちらから**確保し、本体で commit する |
| **G2 union** | 一覧への両側追記が**実際の `git merge`** で衝突なしに解決するか | 両側に別々の行を足して `git merge`。本文に衝突マーカが無く、両方の行が残ること |
| **G3 一撃** | `git merge-tree --write-tree` が HEAD から切った 2 本で通るか | 局所枝を 2 本切って merge-tree |

G1 で連結 worktree を使うのは、`guard::holder_is_me` が
**台帳に載った持ち主の cwd が同じツリー (かその配下) なら自分と見なす**ためである。
同じツリーから確保しても止まらないのは正しい設計なので、
「他人」を本物にしないと**関所が効いていないのに緑**という嘘の結果が出る。

## 1. before / after

| 形 | before | after | 根拠 |
| --- | --- | --- | --- |
| 素の作業ツリー | ✅ | ✅ | G1/G2/G3 すべて yes |
| linked worktree | ✅ | ✅ | 同上 |
| **submodule** | **⚠ 素通り** | **✅** | `--recurse-submodules` を実装。**入れ子 2 段目まで** G1/G2/G3 すべて yes |
| **sparse-checkout** | **⚠ cone 外に効かない** | **✅** | 前提が誤りだった。cone / no-cone の**両方**で cone の外に G2 = yes |
| **shallow** | **⚠ 一撃統合が縮退** | **✅ (範囲を限って)** | 落ちるのは「真の共通祖先が graft より下」の組だけ。coedit が混ぜる形は depth=1 でも通る |
| git-lfs (`merge=lfs` あり) | ✅ | ✅ | G1/G2/G3 すべて yes |
| git-lfs (`filter=lfs` だけ手書き) | ❌ | **✅ (回避を確認)** | czero は当てない (`avoided=yes`)。壊れる組が**存在しない** |
| 既存フックフレームワーク | ⚠ | ⚠ (据え置き) | 共存はする。向こうの再インストールで消えるのは変わらない |
| **非 git** | **❌** | **⚠ → 明示すれば ✅** | `--git-init` を実装。既定では 1 バイトも書かず、`lease claim` も**断る** |
| **bare** | ❌ | ❌ (はっきり断る) | 原理的に不可。断り方を確認 |
| **読み取り専用** | ❌ | ❌ (はっきり断る) | 原理的に不可。断り方を確認 |

実測 (`tools/repo-shapes-prove.sh`, seed=20260812, git 2.47.1):

```
形                doctor        G1関所       G2union   G3一撃  判定
plain             ok            yes          yes       yes     一致
linked-wt         ok            yes          yes       yes     一致
submodule         warn          yes          yes       yes     一致
submodule/深部    warn          yes          yes       yes     一致
sparse-cone       ok            yes          yes       yes     一致
sparse-nocone     ok            yes          yes       yes     一致
shallow           ok            yes          yes       yes     一致
lfs               ok            yes          yes       yes     一致
lfs-unsafe        ok            avoided=yes  skip      skip    一致
hooksframework    warn          yes          yes       yes     一致
nongit            init=3(nongit)  doctor=1   claim=refused
nongit --git-init init=0         gitified=yes  ok
bare              init=3(bare)    doctor=1   claim=refused
readonly          init=1(readonly) doctor=1  claim=refused
```

## 2. submodule — ⚠ → ✅

### 何が問題だったか

submodule は**別リポジトリ**である。独自の `.git/config`、独自のフック置き場
(`.git/modules/<名>/hooks`) を持つので、親で `czero init` を打っても
**1 バイトも届かない**。従来の案内は「各 submodule で
`zai czero init --repo <パス>` を打て」だったが、入れ子があると現実的でない。

### 直したもの

`zai czero init --recurse-submodules` / `zai czero doctor --recurse-submodules`。
`.gitmodules` を読んで**入れ子まで深さ優先で辿り**、初期化済みの submodule
全部へ同じ導入をする (`czero_init::submodule_repos` / `init_all` / `doctor_all`)。

`git submodule foreach --recursive` を使わなかった理由が 3 つある:

1. submodule ごとに**シェルを起こす**ので、Windows では引数の綴りが
   cmd の再解析とずれる (CLAUDE.md「`cmd /C` に押し込まない」の同型)。
2. 未初期化の submodule を**黙って飛ばす**ので、「届いていない場所」が
   出力に出ない。
3. 終了コードが 1 つに畳まれるので、どの submodule で失敗したか判らない。

`.gitmodules` を自分で読めば**決定的**で、git の呼び出しも 0 回で済む。

### 未初期化の submodule は ❌ にする

`git submodule update --init` を打っていない submodule は**空のディレクトリ**で、
導入できない。ここを黙って飛ばすと、**あとで checkout した瞬間にそこだけ
素通り**になる。飛ばした理由を段として残し、形態の段を ❌ にする。

### ついでに塞いだ穴 — `.gitmodules` は追跡ファイルである

`.gitmodules` の中身は「このリポジトリを clone した誰か」が決められる。
`path = ../../../evil` と書かれていると、`--recurse-submodules` が
**リポジトリの外へフックを書き込む**。導入は書き込みを伴うので、
頂点の外へ出る綴りは辿らないことにした (fail-closed。
`lease::rel_within` / `guard::check_path` と同じ立場)。輪も深さも止める。

**字句で `..` を畳んでから比べる**のが要点である。`canonicalize` は実在しない
要素があると失敗するので未初期化の submodule には使えず、畳まずに比べると
`root/a/b/../../../evil` が前半だけ一致して `starts_with(root)` を素通りする。
一方で**畳む前に正規形へ寄せる**ことも要る — 生の綴りから辿ると、実在する子は
`/private/var/…` に、実在しない子は `/var/…` になって**同じ木の中で綴りが
2 種類**でき、頂点判定が未初期化のものだけを落とす (実際に落ちた)。

## 3. sparse-checkout — ⚠ → ✅ (**前提が誤りだった**)

### 従来の記述

> cone の外にある `.gitattributes` は作業ツリーに無く、`czero init` が書くのも
> 頂点の 1 枚だけ。**cone を後から広げると、そこだけ union が当たらない**。

### 実測は逆だった

**git は作業ツリーに無い `.gitattributes` を index から読む。**

| モード | `.gitattributes` は作業ツリーに | cone 外への check-attr | cone 外の実マージ (G2) |
| --- | --- | --- | --- |
| full | あり | `zaivern-union-auto` | clean |
| cone (`set in`) | あり (cone mode は頂点のファイルを必ず出す) | `zaivern-union-auto` | **clean** |
| **no-cone** (`set --no-cone /in/`) | **無し** | `zaivern-union-auto` | **clean** |

no-cone の行が決定的である。**頂点の `.gitattributes` が作業ツリーに
1 バイトも無い**状態でも、cone の外のパスへ union driver が当たり、
両側追記が衝突なしに解決した。つまり「作業ツリーに無いから効かない」は成り立たない。

`trial_sparse_union` としてプロダクト側の `czero verify` にも入れたので、
これ以降は主張が壊れたら実証が落ちる。sparse は
「実証が触れていない形態」の一覧からも外した。

### 試して**採らなかった**案 — `$GIT_DIR/info/attributes`

当初「cone 外へ届かせるなら `info/attributes` へ書けばよい」と考えて測った。
確かに全パスへ届く。**しかしこれは採ってはいけない**:

```
# in-tree .gitattributes:  root.md merge=zaivern-union
# info/attributes:         root.md merge=from-info
$ git check-attr merge -- root.md
root.md: merge: from-info      ← info/attributes が勝つ
```

`$GIT_DIR/info/attributes` は**最高優先度**で、in-tree の `.gitattributes` を
上書きする。つまり利用者が意図して書いた `merge=lfs` や `merge=ours` を
**黙って潰す**。union が LFS のポインタ行を連結すれば、その時点で
LFS オブジェクトが壊れる — ❌ に分類してある事故を、こちらから作りに行くことになる。
届かせる必要が無いと判った以上、採る理由が 1 つも無い。**撤回した。**

## 4. shallow — ⚠ → ✅ (範囲を限って)

### 落ちる条件を測った

`git merge-tree --write-tree` が落ちるのは
**「2 つの ref の真の共通祖先が graft 点より下にある」ときだけ**で、
そのとき git は `fatal: refusing to merge unrelated histories` (rc=128) を返す。

| 組 | depth=1 の clone で | 備考 |
| --- | --- | --- |
| **HEAD から切った局所枝どうし** | **rc=0 (通る)** | **coedit が混ぜるのはこれ**。共通祖先は HEAD 自身 = 必ず手元にある |
| graft より古い分岐点を持つ ref どうし | rc=128 | `git merge-base` も rc=1 (共通祖先が無い) |

つまり「shallow だと一撃統合が縮退する」は、**この製品の使い方では起こらない**。
`coedit` が作るのは常にいまの HEAD から切った枝なので、depth=1 でも通る。

`trial_shallow_merge_tree` は**境界の両側**を測る。通る側だけを見せると
「shallow は大丈夫」という別の嘘になるので、落ちる組が**同じ試行の中で
ちゃんと落ちる**ことも確かめ、落ちなければ「境界を再現できていない」として
試行そのものを失敗にする。

### 落ちる組に当たったときの直し方

| 案 | 結果 |
| --- | --- |
| `git fetch --deepen=<数>` を繰り返す | **有効。** 実測で 16 段深めたところで共通祖先が現れ、merge-tree が rc=0 になった (このとき `.git/shallow` 自体が消えた) |
| `git fetch --unshallow` | 有効 (全部持ってくる) |
| **`--allow-unrelated-histories` を付けて続行** | **採らない。** rc=1 で `CONFLICT (add/add)` を返す — 共通祖先を空ツリーと見なすので、**両側が持っている全ファイルが偽の衝突になる**。縮退ではなく**誤った答え**なので、自動の代替経路にしてはいけない |

自動で `--deepen` を撃つのは採らなかった。**ネットワークアクセスを
黙って起こすことになる**うえ、落ちる組はこの製品の経路では発生しないため
費用に見合わない。診断の文面へ直し方を書くに留めた。

## 5. 非 git — ❌ → 明示すれば ✅

### いちばん誤解される形だった

- `zai lease claim` は**成功する** (台帳はカレントを鍵にできる)
- `zai czero init` は**失敗する** (作業ツリーが要る)

つまり「台帳に参加したエージェントだけは互いを避けるが、強制は 1 つも無い」。
人間の `git commit` も、台帳を知らないエージェントも 1 つも止まらない。

### 実測: 静かには壊れていない

`Roots::rooted` の判定が既に効いていて、**`zai lease claim` は非 git で
断る** (`claim=refused`)。`czero init` は終了コード 3 で、bare と非 git を
**区別した**理由を出す。`doctor` はエラーにせず説明する (終了コード 1)。
ここは「静かに壊れる」形ではなかった。

### 足したもの: `zai czero init --git-init`

フックが入らないのは git が無いからなので、**git 化すれば全部入る**。
ただし黙って `git init` すると勝手にリポジトリを生やすことになるので、
**フラグを打った人だけが通れる道**にした。`--dry-run` では 1 バイトも書かない。
bare では断る (git 化しても作業ツリーは生えない)。

実測: `--git-init` の後、フック 3 種と merge driver と台帳が入り、
形態の段は ✅ になる。

**案内が輪にならないようにした。** `git init` した直後は追跡ファイルが 0 件で、
union を当てる先が無い。ここで従来どおり「もう一度 `zai czero init`」と出すと
**何度打っても同じ画面に戻る**ので、この場合だけ ⚠ にして
「ファイルを 1 度コミットしてから」と言う。

### 検討して採らなかった代替

| 案 | 費用対効果 |
| --- | --- |
| ファイル監視で書き込みを見張る | **強制にならない。** 検出は書かれた**後**なので、2 人が同じ行を書いたことを「後で発見」するだけになる。これは製品が否定している当のもの (競合実装への立ち位置: 衝突を後で発見させない) |
| `git` / エディタのラッパーを PATH へ差す | 経路が 1 つ増えるだけで、ラッパーを通らない書き込み (別シェル・別ツール・エージェントの直接 I/O) は素通り。**「全部止まる」と言えないものを入口に据えると、保証の意味が薄まる** |
| **`--git-init` で git 化する** | **採用。** 1 コマンドで ❌ → ✅ に動き、実装は `git init` 1 回。強制の仕組みを 1 つも増やさない |

## 6. bare / 読み取り専用 — ❌ (原理的に無理)

### bare

作業ツリーが無いので `.gitattributes` を置く場所が無く、`pre-commit` も
発火しない (誰もそこでコミットしない)。**bare 自体を守る意味が無い。**

実測: `init` は終了コード 3 で
「bare リポジトリなので作業ツリーがありません … clone した作業ツリー側で
実行してください」と出す。`doctor` は動く。`lease claim` は断る。

### 読み取り専用

書けないので入らない。実測: 形態の段が ❌ で
「書けない場所があります: git ディレクトリ … / 作業ツリー … —
読み取り専用のチェックアウトでは czero init が失敗します (診断だけは動きます)」。
直し方まで出る。`lease claim` も断る。

**検出の限界は変わっていない**: git ディレクトリは実際に一時ファイルを作って
消すので確実だが、作業ツリー側は `permissions().readonly()` しか見ない
(作業ツリーへ 1 バイトも書かないため)。したがって「誰も書けない」形は拾えるが、
**他人の所有物 (0755) は拾えない** — その場合は `init` の失敗として出る。

## 7. ハーネス自身が作った嘘 (3 件)

CLAUDE.md の「ハーネスは自分で壊した結果を測ることがある」の実例が 3 つ出た。
**どれも最初は製品の不具合に見えた。**

| 見えた症状 | 真因 |
| --- | --- |
| sparse だけ union が効かない (G2=no) | `cur=$(git show …)` が**末尾の改行を落とす**ので、`printf '%s%s\n'` が追記を最終行の**続き**にしていた。両側が同じ行を書き換える形になり、union driver の担当外の衝突を測っていた |
| shallow だけ関所が効かない (G1=no) | 確保が `list.txt#L1-24` 固定だったが、shallow の `list.txt` は履歴を足したぶん長い (31 行)。追記が確保域から 3 行以上離れ、**正しく通っていた**。ファイル全体の確保に変えた |
| `.gitattributes` が消える | `probe_mergetree` の `git add -A` が、追跡されていない `.gitattributes` (czero init が置いたもの) まで枝へコミットし、`checkout` で戻った瞬間に作業ツリーから消していた。**測る道具が測る対象を壊していた** |

3 件目は「測っていない」だけでなく**対象を汚していた**ので、
`git add -A` を使わずパスを明示する形に直した。

## 8. 残っている限界 (伏せない)

1. **既存フックフレームワークの再インストールは防げない。** `pre-commit install` /
   `husky install` はフック本体を書き直すので、こちらの関所が黙って消える。
   ⚠ のまま。入れ直したら `zai czero doctor`。
2. **shallow の「落ちる組」は対象リポジトリの履歴でしか試せない。**
   実証は使い捨ての shallow clone で「HEAD から切った枝は通る」ところまで。
   `untested_notes` にそう出す。
3. **submodule は ⚠ 表示のまま。** `--recurse-submodules` を打てば ✅ になるが、
   打っていない状態を ✅ とは言えない。「打てば届く」を案内する ⚠ である。
4. **測ったのは macOS / git 2.47.1 の 1 点。** git の属性解決 (index からの読み出し)
   と merge-tree の祖先要求は古い git で挙動が違いうる。
   Linux / Windows での再測は未実施。
5. **`--git-init` は「その場所を git にしてよいか」を人に決めさせているだけ**で、
   非 git のまま強制する方法を見つけたわけではない。
