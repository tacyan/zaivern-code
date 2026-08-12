#!/usr/bin/env sh
# リポジトリの**形ごと**に「競合ゼロ」が成り立つかを、その場で作って実測する。
#
# ## なぜ tools/anyrepo-prove.sh --shapes と別に要るのか
#
# `anyrepo-prove.sh --shapes` は形を作って**台帳とゲート**を通す。こちらは
# `zai czero` が言う **4 段の守りそのもの**を形ごとに測る。測るのは 3 つで、
# どれも「czero doctor が何と言ったか」ではなく**実際に起きたこと**である。
#
#   G1 関所   他人が保有する行域へ書いて `git commit` すると**本当に止まるか**
#   G2 union  一覧への両側追記が**実際の `git merge` で**衝突なしに解決するか
#   G3 一撃   `git merge-tree --write-tree` が HEAD から切った 2 本で通るか
#
# **doctor の ✅ を成功と数えない。** 判定関数が主張している保証そのものを
# 見ているかを確かめるのがこのハーネスの役目なので、doctor の答えは
# 「実測と食い違っていないか」の照合にだけ使う (docs/conflict-zero.md §3.11.5)。
#
# ## 対象
#
#   plain          素の作業ツリー
#   linked-wt      git worktree add で切ったツリー
#   submodule      親 + 入れ子の submodule (--recurse-submodules の検査)
#   sparse-cone    sparse-checkout (cone mode) — cone の外へ両側追記
#   sparse-nocone  sparse-checkout (no-cone) — 頂点の .gitattributes も作業ツリーに無い
#   shallow        depth=1 の clone
#   lfs            merge=lfs 付きの .gitattributes
#   lfs-unsafe     filter=lfs だけ手書き (union が当たると壊れる組)
#   hooksframework 既存の core.hooksPath (husky 相当)
#   nongit         git 管理でないフォルダ
#   bare           bare リポジトリ
#   readonly       読み取り専用チェックアウト
#
# ## 決まり
#
#   * `ZAIVERN_HOME` を一時ディレクトリへ向ける。**実 ~/.zaivern に触らない。**
#   * `HOME` / `GIT_CONFIG_GLOBAL` も一時領域へ。利用者の ~/.gitconfig を読まない
#     (署名の強制・core.hooksPath の上書きは、検証を偽陰性にする)。
#   * `--seed` で決定的。乱数を使う箇所は種から起こす。
#   * cargo を 1 度も呼ばない。既にある zai を使うだけ。
#
# ## 使い方
#
#   tools/repo-shapes-prove.sh
#   tools/repo-shapes-prove.sh --seed 20260812
#   tools/repo-shapes-prove.sh --only submodule,sparse-nocone
#   tools/repo-shapes-prove.sh --keep          作った形を残す
#   tools/repo-shapes-prove.sh --json
#
# 環境変数 ZAIVERN_BIN で使う zai を明示できます。
#
# 終了コード: 0 = 形ごとの実測が期待表と一致 / 1 = 食い違った / 2 = 使い方の誤り
set -eu

# shellcheck disable=SC1007
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

seed=20260812
only=""
keep=0
as_json=0

usage() {
    [ -n "${1:-}" ] && printf '%s\n\n' "$1" >&2
    sed -n '2,48p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --seed) seed=${2:?--seed に値が要ります}; shift 2 ;;
        --only) only=${2:?--only に値が要ります}; shift 2 ;;
        --keep) keep=1; shift ;;
        --json) as_json=1; shift ;;
        -h|--help) usage ;;
        *) usage "知らない引数です: $1" ;;
    esac
done

# ── zai を決める (版と mtime を照合する) ────────────────────────────────
zai=${ZAIVERN_BIN:-}
if [ -z "$zai" ]; then
    for cand in "$root/target/debug/zai" "$root/target/release/zai"; do
        [ -x "$cand" ] && zai=$cand && break
    done
fi
if [ -z "$zai" ] || [ ! -x "$zai" ]; then
    printf 'zai が見つかりません。先に: cargo build --bin zai\n' >&2
    printf '(ZAIVERN_BIN で明示もできます)\n' >&2
    exit 2
fi
# **版が同じまま中身だけ古いバイナリ**がいちばん質が悪い (CLAUDE.md)。
# src/ の最新更新より古ければ、実装したのに直っていない嘘の結果が出る。
newest_src=$(find "$root/src" -name '*.rs' -newer "$zai" -print 2>/dev/null | head -1 || true)
if [ -n "$newest_src" ]; then
    printf '[skip] %s は src/ より古いです (%s が新しい)\n' "$zai" "$newest_src" >&2
    printf '       直し方: cargo build --bin zai\n' >&2
    exit 2
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/zaivern-shapes-XXXXXX")
cleanup() { [ "$keep" = 1 ] || rm -rf "$work"; }
trap cleanup EXIT INT TERM

# **実 ~/.zaivern と ~/.gitconfig に触らない。**
ZAIVERN_HOME="$work/zaivern-home"; export ZAIVERN_HOME
HOME="$work/home"; export HOME
GIT_CONFIG_GLOBAL="$work/gitconfig"; export GIT_CONFIG_GLOBAL
GIT_CONFIG_SYSTEM=/dev/null; export GIT_CONFIG_SYSTEM
mkdir -p "$ZAIVERN_HOME" "$HOME"
cat > "$GIT_CONFIG_GLOBAL" <<'GC'
[user]
	name = zaivern shapes
	email = shapes@example.invalid
[init]
	defaultBranch = main
[commit]
	gpgsign = false
[gc]
	auto = 0
[protocol "file"]
	allow = always
GC

g() { dir=$1; shift; git -C "$dir" "$@"; }
gq() { dir=$1; shift; git -C "$dir" "$@" >/dev/null 2>&1; }

# 種から決まる本文 (乱数は使わず、種を混ぜて決定的に作る)。
body() { i=1; while [ "$i" -le 24 ]; do printf 'l%02d s%s uniq%02d\n' "$i" "$seed" "$i"; i=$((i+1)); done; }

seed_repo() {
    d=$1
    mkdir -p "$d"
    gq "$d" init
    body > "$d/list.txt"
    mkdir -p "$d/in" "$d/out"
    body > "$d/in/a.txt"
    body > "$d/out/b.txt"
    gq "$d" add -A
    gq "$d" commit -m base --no-verify
}

# ═══════════════════════════════════════════════════════════════════════
#  実測 — 「doctor が何と言ったか」ではなく「何が起きたか」
# ═══════════════════════════════════════════════════════════════════════

# いまの枝名。**submodule は既定で detached HEAD** なので、その場合は
# 測るための枝をこちらで作る (`git merge` は detached では使えない)。
branch_of() {
    d=$1
    b=$(g "$d" symbolic-ref --short HEAD 2>/dev/null) || b=""
    if [ -z "$b" ]; then
        b=zshapes-base
        gq "$d" checkout -B "$b" || return 1
    fi
    printf '%s' "$b"
}

# G3: HEAD から切った 2 本で merge-tree が通るか。
probe_mergetree() {
    d=$1
    base=$(branch_of "$d") || { echo na; return; }
    gq "$d" checkout -B zp1 "$base" || { echo na; return; }
    # **`git add -A` を使わない。** 追跡されていない `.gitattributes`
    # (czero init が置いたもの) まで巻き込んで枝へコミットしてしまい、
    # `checkout` で戻った瞬間に作業ツリーから消える。**測る道具が
    # 測る対象を壊す**典型なので、必ずパスを明示する。
    echo one > "$d/zp1.txt"; gq "$d" add -- zp1.txt; gq "$d" commit -m zp1 --no-verify
    gq "$d" checkout "$base"; gq "$d" checkout -B zp2 "$base"
    echo two > "$d/zp2.txt"; gq "$d" add -- zp2.txt; gq "$d" commit -m zp2 --no-verify
    if gq "$d" merge-tree --write-tree zp1 zp2; then echo yes; else echo no; fi
    gq "$d" checkout "$base"
    gq "$d" reset --hard "$base"
}

# G2: 一覧への両側追記が実 git merge で解決するか。`target` は追記するファイル。
probe_union() {
    d=$1; target=$2
    base=$(branch_of "$d") || { echo na; return; }
    gq "$d" checkout -B zu-theirs "$base" || { echo na; return; }
    # 作業ツリーに無いファイル (sparse の cone 外) にも追記できるよう、
    # **blob を作って index 経由で書く**。ここを普通の書き込みにすると
    # sparse の形では測れない (それが測りたい当のものなので)。
    add_line() {
        # **`$(…)` は末尾の改行を落とす。** `printf '%s%s\n'` にすると
        # 追記が最終行の**続き**になり、両側が同じ行を書き換える形になる。
        # union driver は「別々の行の追記」しか解決しないので、
        # そのままでは**ハーネスが自分で作った衝突**を測ってしまう
        # (実際にこれで sparse だけ「union が効かない」と出た)。
        cur=$(g "$d" show "HEAD:$target" 2>/dev/null || true)
        printf '%s\n%s\n' "$cur" "$1" > "$work/blob"
        oid=$(g "$d" hash-object -w --path "$target" -- "$work/blob")
        g "$d" update-index --add --cacheinfo "100644,$oid,$target" >/dev/null
        gq "$d" commit -m "$1" --no-verify
    }
    add_line "ZU-THEIRS"
    gq "$d" checkout -B zu-ours "$base"
    add_line "ZU-OURS"
    if gq "$d" merge --no-edit zu-theirs; then
        txt=$(g "$d" show "HEAD:$target" 2>/dev/null || true)
        case "$txt" in
            *"<<<<<<<"*) echo marker ;;
            *ZU-THEIRS*) case "$txt" in *ZU-OURS*) echo yes ;; *) echo lost ;; esac ;;
            *) echo lost ;;
        esac
    else
        gq "$d" merge --abort || true
        echo no
    fi
    gq "$d" checkout "$base"
    gq "$d" reset --hard "$base"
}

# G1: 他人が保有する行域へ書いて commit すると本当に止まるか。
#
# **「他人」を本物にする。** `guard::holder_is_me` は台帳に載った持ち主の cwd が
# 同じツリー (かその配下) なら自分と見なすので、同じツリーから確保しても
# 止まらない — それが正しい設計である。だから連結 worktree を 1 本足して
# **そちらから確保する**。並列エージェントの実際の形と同じになる。
probe_gate() {
    d=$1
    other="$d/../zgate-other-$(basename "$d")"
    gq "$d" worktree add -b "zgate-$(basename "$d")" "$other" HEAD || { echo na; return; }
    # **行域ではなくファイル全体を確保する。** 行域にすると、形ごとに
    # list.txt の長さが違う (shallow は履歴を足したぶん長い) ので、
    # 追記した行が確保域から 3 行以上離れて**正しく通ってしまい**、
    # 「関所が効いていない」と読める嘘の赤が出る (実際に出した)。
    if ! (cd "$other" && "$zai" lease claim 'list.txt' --agent shapes-other) >/dev/null 2>&1; then
        gq "$d" worktree remove --force "$other" || true
        echo na; return
    fi
    printf 'INTRUDER\n' >> "$d/list.txt"
    gq "$d" add -- list.txt
    # フックが居れば commit は失敗する。--no-verify は付けない (関所を測るので)。
    # **パイプを挟まない** — `| head` を挟むと `$?` はそちらのものになる。
    if git -C "$d" commit -m intrude >/dev/null 2>&1; then out=no; else out=yes; fi
    gq "$d" reset --hard HEAD
    (cd "$other" && "$zai" lease release --agent shapes-other) >/dev/null 2>&1 || true
    gq "$d" worktree remove --force "$other" || true
    echo "$out"
}

doctor_mark() {
    d=$1
    "$zai" czero doctor --repo "$d" --json 2>/dev/null \
        | python3 -c 'import json,sys
try: j=json.load(sys.stdin)
except Exception: print("na"); raise SystemExit
rows=[f for f in j.get("findings",[]) if f.get("stage")=="shape"]
order={"ok":0,"warn":1,"bad":2}
print(max((r.get("mark","na") for r in rows), key=lambda m: order.get(m,-1)) if rows else "na")'
}

rows=""
record() { rows="$rows$1|$2|$3|$4|$5
"; }

want() { case ",$only," in ,,) return 0 ;; *",$1,"*) return 0 ;; *) return 1 ;; esac; }

# ═══════════════════════════════════════════════════════════════════════
#  形を作って測る
# ═══════════════════════════════════════════════════════════════════════

run_shape() {
    name=$1; d=$2; target=${3:-list.txt}
    want "$name" || return 0
    mark=$(doctor_mark "$d")
    gate=$(probe_gate "$d" 2>/dev/null || echo na)
    union=$(probe_union "$d" "$target" 2>/dev/null || echo na)
    mt=$(probe_mergetree "$d" 2>/dev/null || echo na)
    record "$name" "$mark" "$gate" "$union" "$mt"
}

S="$work/shapes"; mkdir -p "$S"

if want plain; then
    seed_repo "$S/plain"
    "$zai" czero init --repo "$S/plain" >/dev/null 2>&1 || true
    run_shape plain "$S/plain"
fi

if want linked-wt; then
    seed_repo "$S/wt-src"
    "$zai" czero init --repo "$S/wt-src" >/dev/null 2>&1 || true
    gq "$S/wt-src" worktree add -b lw "$S/linked-wt" HEAD
    run_shape linked-wt "$S/linked-wt"
fi

if want submodule; then
    seed_repo "$S/sub-leaf"
    seed_repo "$S/sub-mid"
    gq "$S/sub-mid" submodule add -- "$S/sub-leaf" leaf
    gq "$S/sub-mid" commit -m "add leaf" --no-verify
    seed_repo "$S/sub-parent"
    gq "$S/sub-parent" submodule add -- "$S/sub-mid" mid
    gq "$S/sub-parent" commit -m "add mid" --no-verify
    gq "$S/sub-parent" submodule update --init --recursive
    # **--recurse-submodules で入れる。** 親だけへ入れても届かないのが要点。
    "$zai" czero init --repo "$S/sub-parent" --recurse-submodules >/dev/null 2>&1 || true
    run_shape submodule "$S/sub-parent"
    # 入れ子の一番奥まで関所が入ったか (ここが ⚠ → ✅ の本体)。
    want submodule && {
        deep="$S/sub-parent/mid/leaf"
        mark=$(doctor_mark "$deep")
        gate=$(probe_gate "$deep" 2>/dev/null || echo na)
        union=$(probe_union "$deep" list.txt 2>/dev/null || echo na)
        mt=$(probe_mergetree "$deep" 2>/dev/null || echo na)
        record "submodule/深部" "$mark" "$gate" "$union" "$mt"
    }
fi

if want sparse-cone; then
    seed_repo "$S/sparse-cone"
    "$zai" czero init --repo "$S/sparse-cone" >/dev/null 2>&1 || true
    gq "$S/sparse-cone" sparse-checkout set in
    run_shape sparse-cone "$S/sparse-cone" out/b.txt
fi

if want sparse-nocone; then
    seed_repo "$S/sparse-nocone"
    "$zai" czero init --repo "$S/sparse-nocone" >/dev/null 2>&1 || true
    gq "$S/sparse-nocone" sparse-checkout set --no-cone '/in/'
    run_shape sparse-nocone "$S/sparse-nocone" out/b.txt
fi

if want shallow; then
    seed_repo "$S/shallow-src"
    i=0; while [ "$i" -lt 6 ]; do
        printf 'extra %s %s\n' "$i" "$seed" >> "$S/shallow-src/list.txt"
        gq "$S/shallow-src" commit -am "e$i" --no-verify; i=$((i+1))
    done
    gq "$S" clone --quiet --depth 1 "file://$S/shallow-src" shallow
    "$zai" czero init --repo "$S/shallow" >/dev/null 2>&1 || true
    run_shape shallow "$S/shallow"
fi

if want lfs; then
    seed_repo "$S/lfs"
    printf '*.bin filter=lfs diff=lfs merge=lfs -text\n' > "$S/lfs/.gitattributes"
    printf 'version https://git-lfs.github.com/spec/v1\noid sha256:%s\nsize 1\n' \
        "0000000000000000000000000000000000000000000000000000000000000000" > "$S/lfs/big.bin"
    gq "$S/lfs" add -A; gq "$S/lfs" commit -m lfs --no-verify
    "$zai" czero init --repo "$S/lfs" >/dev/null 2>&1 || true
    run_shape lfs "$S/lfs"
fi

if want lfs-unsafe; then
    seed_repo "$S/lfs-unsafe"
    printf '*.txt filter=lfs\n' > "$S/lfs-unsafe/.gitattributes"
    gq "$S/lfs-unsafe" add -A; gq "$S/lfs-unsafe" commit -m lfs-unsafe --no-verify
    "$zai" czero init --repo "$S/lfs-unsafe" >/dev/null 2>&1 || true
    # **測るのは「union を当てなかったこと」。** union はポインタ行を連結する
    # ので、`filter=lfs` が付いたパターンへ当てた時点で LFS が壊れる。
    applied=$(git -C "$S/lfs-unsafe" check-attr merge -- list.txt 2>/dev/null | sed 's/.*: //')
    case "$applied" in
        zaivern-union*) avoided=no ;;
        *) avoided=yes ;;
    esac
    want lfs-unsafe && record "lfs-unsafe" "$(doctor_mark "$S/lfs-unsafe")" "avoided=$avoided" skip skip
fi

if want hooksframework; then
    seed_repo "$S/husky"
    mkdir -p "$S/husky/.husky"
    printf '#!/bin/sh\nexit 0\n' > "$S/husky/.husky/pre-commit"
    chmod +x "$S/husky/.husky/pre-commit"
    gq "$S/husky" config --local core.hooksPath .husky
    "$zai" czero init --repo "$S/husky" >/dev/null 2>&1 || true
    run_shape hooksframework "$S/husky"
fi

# ── 入らない形: 「静かに壊れず、はっきり断る」ことを測る ────────────────
refusal() {
    name=$1; d=$2
    want "$name" || return 0
    if "$zai" czero init --repo "$d" >"$work/o" 2>&1; then init_rc=0; else init_rc=$?; fi
    # **出力を切り詰めない。** 非 git / bare は 1 行目で断るが、読み取り専用は
    # 段を全部出したあとの自己検査で理由を言うので、先頭だけ見ると取り逃す。
    said=$(tr '\n' ' ' < "$work/o")
    # doctor は**エラーにせず説明する**こと (診断まで死ぬと直し方が判らない)。
    if "$zai" czero doctor --repo "$d" >/dev/null 2>&1; then doc_rc=0; else doc_rc=$?; fi
    # 台帳だけが黙って動いてしまわないこと。
    if "$zai" lease claim --dir "$d" --path list.txt --lines 1-3 \
            --agent shapes-x --session shapes-x >/dev/null 2>&1; then claim=ok; else claim=refused; fi
    reason=na
    case "$said" in
        *bare*) reason=bare ;;
        *"git リポジトリではありません"*) reason=nongit ;;
        *"書けない場所があります"*) reason=readonly ;;
        *) case "$said" in *[!\ ]*) reason=other ;; esac ;;
    esac
    record "$name" "init=$init_rc($reason)" "doctor=$doc_rc" "claim=$claim" "-"
}

if want nongit; then
    mkdir -p "$S/nongit"; body > "$S/nongit/list.txt"
    refusal nongit "$S/nongit"
    # --git-init を明示すれば通ること (❌ → ✅ の逃げ道)。
    mkdir -p "$S/nongit2"; body > "$S/nongit2/list.txt"
    if "$zai" czero init --repo "$S/nongit2" --git-init >/dev/null 2>&1; then r=0; else r=$?; fi
    gitified=no; [ -d "$S/nongit2/.git" ] && gitified=yes
    record "nongit --git-init" "init=$r" "gitified=$gitified" \
        "$(doctor_mark "$S/nongit2")" "-"
fi

if want bare; then
    seed_repo "$S/bare-src"
    gq "$S" clone --quiet --bare "$S/bare-src" bare.git
    refusal bare "$S/bare.git"
fi

if want readonly; then
    seed_repo "$S/readonly"
    chmod -R a-w "$S/readonly" 2>/dev/null || true
    refusal readonly "$S/readonly"
    chmod -R u+w "$S/readonly" 2>/dev/null || true
fi

# ═══════════════════════════════════════════════════════════════════════
#  期待表との照合 — **実測が表と食い違ったら落とす**
# ═══════════════════════════════════════════════════════════════════════
#
# 「doctor が緑」ではなく「G1/G2/G3 が期待どおり」を見る。
# na = その形では測れない (前提が無い)。skip = わざと測っていない。
expected() {
    case "$1" in
        plain)             echo "ok|yes|yes|yes" ;;
        linked-wt)         echo "ok|yes|yes|yes" ;;
        submodule)         echo "warn|yes|yes|yes" ;;
        "submodule/深部")  echo "warn|yes|yes|yes" ;;
        sparse-cone)       echo "ok|yes|yes|yes" ;;
        sparse-nocone)     echo "ok|yes|yes|yes" ;;
        shallow)           echo "ok|yes|yes|yes" ;;
        lfs)               echo "ok|yes|yes|yes" ;;
        # czero は `filter=lfs` が付いたパターンを**避ける**ので、union は
        # 当たらない = 壊れる組が存在しない。だから形態の段は ✅ が正しい
        # (❌ になるのは、人が既に union を当ててしまっているときだけ)。
        lfs-unsafe)        echo "ok|avoided=yes|skip|skip" ;;
        hooksframework)    echo "warn|yes|yes|yes" ;;
        *) echo "" ;;
    esac
}

fails=0
printf '\n== リポジトリの形ごとの実測 (seed=%s, git %s)\n' "$seed" "$(git --version | awk '{print $3}')"
printf '%-18s %-22s %-12s %-12s %-10s %s\n' 形 doctor G1関所 G2union G3一撃 判定
printf -- '---------------------------------------------------------------------------------------\n'
printf '%s' "$rows" | while IFS='|' read -r name mark gate union mt; do
    [ -z "$name" ] && continue
    exp=$(expected "$name")
    got="$mark|$gate|$union|$mt"
    if [ -z "$exp" ]; then
        verdict="(表なし)"
    elif [ "$exp" = "$got" ]; then
        verdict="一致"
    else
        verdict="食い違い 期待=$exp"
        echo x >> "$work/fail"
    fi
    printf '%-18s %-22s %-12s %-12s %-10s %s\n' "$name" "$mark" "$gate" "$union" "$mt" "$verdict"
done
[ -f "$work/fail" ] && fails=$(wc -l < "$work/fail" | tr -d ' ')

printf -- '---------------------------------------------------------------------------------------\n'
printf 'G1 関所 = 他人の行域へ書いた commit が止まったか / G2 = 両側追記が実 git merge で解決したか\n'
printf 'G3 一撃 = HEAD から切った 2 本で merge-tree --write-tree が通ったか\n'

if [ "$as_json" = 1 ]; then
    printf '{"seed":%s,"rows":[' "$seed"
    first=1
    printf '%s' "$rows" | while IFS='|' read -r name mark gate union mt; do
        [ -z "$name" ] && continue
        [ "$first" = 1 ] || printf ','
        first=0
        printf '{"shape":"%s","doctor":"%s","gate":"%s","union":"%s","merge_tree":"%s"}' \
            "$name" "$mark" "$gate" "$union" "$mt"
    done
    printf ']}\n'
fi

if [ "$fails" != 0 ]; then
    printf '\n%s 件が期待表と食い違いました。\n' "$fails" >&2
    exit 1
fi
printf '\n全ての形で実測が期待表と一致しました。\n'
[ "$keep" = 1 ] && printf '作った形: %s\n' "$S"
exit 0
