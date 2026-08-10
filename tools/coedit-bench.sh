#!/usr/bin/env sh
# 「**同じファイルでも、違う行なら 2 人が同時に書ける**」を実測するハーネス。
#
# ## なぜ別に要るのか (tools/conflict-zero-bench.sh との違い)
#
# 既存のハーネスは **ファイルを N 個** 置いて「誰がどのファイルを取るか」を測る。
# そこではファイル単位の所有が自然に効くので、**行域オーナーシップの価値は
# 1 ミリも見えない**。この製品が新しく主張したいのは
#
#   > 同じファイルの違う行なら、2 人が同時に書ける
#
# なので、測る土俵は逆にしないといけない。**ファイルは 1 個だけ**置く。
# ファイル単位の所有だと並列度が構造的に 1 に潰れる状況で、
# 行域オーナーシップが何を取り戻すのかを数字にする。
#
# ## 中核の問い
#
#   1. N 体が「同じファイルの違う行」を同時に書いたとき、衝突は本当に 0 か
#   2. ファイル単位の所有と比べて、**拒否された書き込みは何回減ったのか**
#   3. **完了した書き込み**は保存されているか (0 件は「書かせない」でも買える。
#      総作業量を測らないと「衝突 0」の主張は片手落ちになる)
#
# ## 段 (--mode)
#
#   a     素の git。所有の仕組みを 1 つも通さない。**対照群**
#   b     ファイル単位の所有 (`zai lease claim <file>`)。従来モード
#   c     行域オーナーシップ (`zai lease claim '<file>#L10-40'`)。出荷物での実測。
#         **出荷物が行域を理解していなければ skip して「未測定」と出す**
#   cref  行域オーナーシップの**参照ゲート**。`src/region.rs` の
#         `spans_too_close` (gap < band なら近すぎる) をハーネス内で再実装したもの。
#         出荷物と独立に「行域所有で到達できる上限」を出す。
#         docs/conflict-zero.md 3.5 が参照マージドライバでやったのと同じ作法
#
# ## 並べ方 (--layout)
#
#   disjoint  安全帯 (band) 以上離した行域を配る。**衝突は出ないはず**
#   crowded   わざと域を近づける・重ねる。**衝突が出ることの裏取り**と、
#             安全帯がどれだけ保守的か (git なら通ったのに断った数) を測る
#
# ## 使い方
#
#   tools/coedit-bench.sh                                   既定 (8 体 / 800 行 / 全段 / 両方の並べ方)
#   tools/coedit-bench.sh --agents 8 --lines 500 --mode all
#   tools/coedit-bench.sh --agents 1,4,8,16,32,64 --lines 2000
#   tools/coedit-bench.sh --agents 64 --lines 2000 --layout disjoint
#   tools/coedit-bench.sh --mode a,cref --json
#   tools/coedit-bench.sh --seed 12345                      乱数の種 (既定 20260810)
#   tools/coedit-bench.sh --band 3                          安全帯 (既定は region::SAFE_BAND と同じ 3)
#   tools/coedit-bench.sh --keep                            一時リポジトリを残す
#
# 環境変数 ZAIVERN_BIN で使う zai を明示できます (無くても a / cref は必ず出ます)。
#
# ## 終了コード
#
#   0  保護のある段 (b / c / cref) が 1 ハンクも衝突を残さなかった。
#      あるいはその段が skip された (理由は必ず表示される)
#   1  保護のある段に衝突が残った。**主張が壊れている**
#   2  引数の指定ミス、または行数が足りず行域を配れない
#   3  前提が無い (git / awk が見つからない)
#
# ## 副作用を持たない作り
#
#   * 一時リポジトリは `mktemp -d` (= `$TMPDIR` 由来)。パスを直書きしない
#   * `HOME` を段ごとの一時ディレクトリへ差し替えるので、**本物の `~/.zaivern` と
#     `~/.gitconfig` には一切触らない**
#   * **cargo を呼ばない。** 既にある `zai` を使うだけで、無ければ段を skip する。
#     ホストの `target/` は 1 バイトも触らない
#   * **python を呼ばない。** POSIX sh + awk だけで完結する
#     (Windows の git 同梱 sh でも走る。`[[` / `local` / 配列を使わない)
#   * 後始末は trap。`--keep` を付けたときだけ残す
set -eu

agents=8
lines=800
modes=all
layouts=both
seed=20260810
band=3
json=0
keep=0

usage() {
    cat <<'EOS'
使い方: tools/coedit-bench.sh [オプション]

  --agents N[,N...]   同時に走らせる体数。カンマ区切りで掃引できる (既定 8)
  --lines N           合成ファイルの行数 (既定 800)
  --mode a|b|c|cref|all
                      段。カンマ区切り可 (既定 all = a,b,c,cref)
  --layout disjoint|crowded|both
                      行域の並べ方 (既定 both)
  --seed N            乱数の種。同じ種なら同じ担当表になる (既定 20260810)
  --band N            安全帯の行数。region::SAFE_BAND と揃える (既定 3)
  --json              JSON を stdout へ、表を stderr へ
  --keep              一時リポジトリを消さない
  -h, --help          これ

終了コード: 0=保護段に衝突なし / 1=保護段に衝突あり / 2=引数か行数の誤り / 3=前提不足
EOS
}

while [ $# -gt 0 ]; do
    case "$1" in
    --agents)
        [ $# -ge 2 ] || { echo "--agents に値がありません" >&2; exit 2; }
        agents=$2; shift 2 ;;
    --lines)
        [ $# -ge 2 ] || { echo "--lines に値がありません" >&2; exit 2; }
        lines=$2; shift 2 ;;
    --mode)
        [ $# -ge 2 ] || { echo "--mode に値がありません" >&2; exit 2; }
        modes=$2; shift 2 ;;
    --layout)
        [ $# -ge 2 ] || { echo "--layout に値がありません" >&2; exit 2; }
        layouts=$2; shift 2 ;;
    --seed)
        [ $# -ge 2 ] || { echo "--seed に値がありません" >&2; exit 2; }
        seed=$2; shift 2 ;;
    --band)
        [ $# -ge 2 ] || { echo "--band に値がありません" >&2; exit 2; }
        band=$2; shift 2 ;;
    --json) json=1; shift ;;
    --keep) keep=1; shift ;;
    -h | --help) usage; exit 0 ;;
    *)
        echo "知らない引数です: $1" >&2
        usage >&2
        exit 2 ;;
    esac
done

# ── 引数の検算 (数字でないものを黙って 0 として扱わない) ─────────────
num_ok() {
    case "$1" in
    '' | *[!0-9]*) return 1 ;;
    *) return 0 ;;
    esac
}
for v in $(printf '%s' "$agents" | tr ',' ' '); do
    num_ok "$v" && [ "$v" -ge 1 ] || { echo "--agents は 1 以上の整数です: $v" >&2; exit 2; }
done
num_ok "$lines" && [ "$lines" -ge 1 ] || { echo "--lines は 1 以上の整数です: $lines" >&2; exit 2; }
num_ok "$seed" || { echo "--seed は整数です: $seed" >&2; exit 2; }
num_ok "$band" || { echo "--band は 0 以上の整数です: $band" >&2; exit 2; }

case "$modes" in all) modes="a,b,c,cref" ;; esac
for m in $(printf '%s' "$modes" | tr ',' ' '); do
    case "$m" in
    a | b | c | cref) ;;
    *) echo "--mode は a / b / c / cref のいずれかです: $m" >&2; exit 2 ;;
    esac
done
case "$layouts" in
both) layouts="disjoint,crowded" ;;
disjoint | crowded) ;;
*) echo "--layout は disjoint / crowded / both です: $layouts" >&2; exit 2 ;;
esac

# ── 前提 ──────────────────────────────────────────────────────────
for need in git awk; do
    command -v "$need" >/dev/null 2>&1 || {
        echo "$need が見つかりません。この計測には $need が要ります。" >&2
        exit 3
    }
done

# ── ミリ秒時計。無ければ綺麗に粗くなる ────────────────────────────
# `date +%s%N` は GNU coreutils だけ。BSD (macOS) は末尾に literal な "N" を返す。
# 巨大な整数を POSIX sh の算術に入れない (桁溢れの挙動が実装依存) ので、
# 文字列として末尾 6 桁を落とす。
_probe=$(date +%s%N 2>/dev/null || echo x)
case "$_probe" in
'' | *[!0-9]*) clock=none ;;
*) clock='date' ;;
esac
if [ "$clock" = none ] && command -v perl >/dev/null 2>&1; then
    if perl -MTime::HiRes=time -e 'printf "%.0f", time()*1000' >/dev/null 2>&1; then
        clock=perl
    fi
fi
now_ms() {
    case "$clock" in
    date)
        _n=$(date +%s%N)
        printf '%s\n' "${_n%??????}" ;;
    perl)
        perl -MTime::HiRes=time -e 'printf "%.0f\n", time()*1000' ;;
    *)
        # 秒精度しか無い環境。時間の列は「粗い」と明記して出す
        printf '%s000\n' "$(date +%s)" ;;
    esac
}

# ── git のバージョン (merge-tree --write-tree は 2.38 から) ─────────
git_version=$(git --version 2>/dev/null | awk '{print $3}')
has_merge_tree=$(printf '%s\n' "$git_version" | awk -F. '{
    maj = $1 + 0; min = $2 + 0
    print (maj > 2 || (maj == 2 && min >= 38)) ? 1 : 0
}')
[ -n "$has_merge_tree" ] || has_merge_tree=0

# ── 使う zai を探す。**絶対にビルドしない** ────────────────────────
# 古い zai は知らないサブコマンドを GUI 起動として扱って固まるので、
# `zai help` の中身に載っているものだけを叩く (既存ハーネスと同じ作法)。
# shellcheck disable=SC1007  # `CDPATH= cd` は「その cd にだけ空の CDPATH を渡す」正しい書き方
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-}
[ -n "$target_dir" ] || target_dir="$root/target"

zai=""
zai_help_text=""
for cand in \
    "${ZAIVERN_BIN:-}" \
    "$target_dir/debug/zai" "$target_dir/debug/zai.exe" \
    "$target_dir/release/zai" "$target_dir/release/zai.exe" \
    "$(command -v zai 2>/dev/null || true)"; do
    [ -n "$cand" ] || continue
    [ -x "$cand" ] || continue
    if zai_help_text=$("$cand" help 2>/dev/null </dev/null); then
        zai=$cand
        break
    fi
done
has_lease=0
if [ -n "$zai" ] && printf '%s\n' "$zai_help_text" | grep -q 'zai lease'; then
    has_lease=1
fi

# ── 使い捨ての作業場 ──────────────────────────────────────────────
work=$(mktemp -d 2>/dev/null || mktemp -d -t coeditbench)
# shellcheck disable=SC2329  # trap から呼ばれる
cleanup() {
    if [ "$keep" = 1 ]; then
        echo "== 一時リポジトリを残しました: $work" >&2
    else
        rm -rf "$work"
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

export GIT_CONFIG_NOSYSTEM=1
export GIT_TERMINAL_PROMPT=0
export GIT_AUTHOR_NAME=coedit-bench
export GIT_AUTHOR_EMAIL=coedit-bench@example.invalid
export GIT_COMMITTER_NAME=coedit-bench
export GIT_COMMITTER_EMAIL=coedit-bench@example.invalid
export GIT_ADVICE=0
# 本物の ~/.zaivern と ~/.gitconfig に触らせない (段ごとに更に差し替える)
mkdir -p "$work/home"
HOME="$work/home"
USERPROFILE="$work/home"
export HOME USERPROFILE

# ── 出荷物が「行域」を理解しているかの能力検査 ─────────────────────
#
# **ここを省くと静かな嘘になる。** 0.13.0 の `zai lease claim` は
# `a.rs#L1-10` を**ただの文字列**として受け取るので、
#   * `a.rs#L1-10` と `a.rs#L5-15` (重なっている) が **両方通る**
#   * `a.rs` を誰かが持っていても `a.rs#L1-10` が通る
# つまり保護が 1 つも無いのに「全員書けて衝突 0」という綺麗な数字が出る。
# 重なった 2 つを投げて**断られること**を確かめてから段 c を実行する。
region_aware=0
region_skip_reason="zai が見つかりません"
if [ "$has_lease" = 1 ]; then
    region_skip_reason=""
    probe="$work/probe"
    mkdir -p "$probe/repo" "$probe/home"
    probe_rc=0
    (
        HOME="$probe/home"
        USERPROFILE="$probe/home"
        export HOME USERPROFILE
        cd "$probe/repo"
        git init -q -b main . >/dev/null 2>&1
        printf 'x\n' >a.rs
        git add -A >/dev/null 2>&1
        git commit -qm probe >/dev/null 2>&1
        "$zai" lease enable --dir "$probe/repo" >/dev/null 2>&1 || exit 20
        # 1 本目は通らないといけない
        "$zai" lease claim 'a.rs#L1-10' --agent probe1 --dir "$probe/repo" >/dev/null 2>&1 || exit 21
        # 重なっている 2 本目は**断られないといけない**
        if "$zai" lease claim 'a.rs#L5-15' --agent probe2 --dir "$probe/repo" >/dev/null 2>&1; then
            exit 22
        fi
        # 安全帯より離れた 3 本目は通らないといけない
        "$zai" lease claim 'a.rs#L100-110' --agent probe3 --dir "$probe/repo" >/dev/null 2>&1 || exit 23
        exit 0
    ) || probe_rc=$?
    case "$probe_rc" in
    0) region_aware=1 ;;
    20) region_skip_reason="zai lease enable が失敗しました" ;;
    21) region_skip_reason="zai lease claim が行域指定を受け付けません" ;;
    22) region_skip_reason="重なった 2 つの行域が両方通りました (行域を理解していない = 保護がありません)" ;;
    23) region_skip_reason="離れた行域を断りました (行域の判定が壊れています)" ;;
    *) region_skip_reason="能力検査そのものが失敗しました" ;;
    esac
fi

# ── 担当表を作る ──────────────────────────────────────────────────
#
# disjoint: ファイルを N 等分し、各枠の中に「後ろへ band 行の空きを残した」行域を置く。
#           隣との隙間は必ず band 行以上 (= region::spans_too_close が false)。
# crowded : 幅 band+3 の行域を stride 2 で並べる。隣同士は**重なり**、
#           3 つ先とは「重なってはいないが band 行未満しか離れていない」組になる。
#           前者は git でも衝突し、後者は git なら通るが安全帯は断る。
plan_regions() {
    awk -v n="$1" -v total="$2" -v band="$3" -v layout="$4" -v seed="$5" 'BEGIN {
        srand(seed)
        if (layout == "disjoint") {
            slot = int(total / n)
            for (i = 1; i <= n; i++) {
                slot_start = (i - 1) * slot + 1
                avail = slot - band          # 枠の末尾に band 行の空きを残す
                if (avail < 1) avail = 1
                rl = int(avail / 2)
                if (rl < 1) rl = 1
                maxjit = avail - rl
                if (maxjit < 0) maxjit = 0
                jit = int(rand() * (maxjit + 1))
                s = slot_start + jit
                e = s + rl - 1
                if (e > total) e = total
                printf "%d %d %d\n", i, s, e
            }
        } else {
            rl = band + 3
            stride = 2
            base = int((total - (n - 1) * stride - rl) / 2)
            if (base < 1) base = 1
            for (i = 1; i <= n; i++) {
                s = base + (i - 1) * stride
                e = s + rl - 1
                if (e > total) e = total
                if (s > total) s = total
                printf "%d %d %d\n", i, s, e
            }
        }
    }'
}

# 担当表そのものが「一撃マージできる」条件を満たしているか (region::is_disjoint 相当)。
# 出力: "近すぎる組の数"
count_close_pairs() {
    awk -v band="$band" '
        { s[NR] = $2; e[NR] = $3; n = NR }
        END {
            c = 0
            for (i = 1; i <= n; i++)
                for (j = i + 1; j <= n; j++) {
                    if (s[i] <= s[j]) { lo_e = e[i]; hi_s = s[j] } else { lo_e = e[j]; hi_s = s[i] }
                    gap = hi_s - lo_e - 1
                    if (gap < band) c++
                }
            print c
        }' "$1"
}

# ── 参照ゲート (段 cref) ──────────────────────────────────────────
# `src/region.rs` の spans_too_close をそのまま写したもの。
# 台帳は 1 本のテキストファイル。mkdir で相互排他する (並列の体が本当に競る)。
ref_claim() { # $1=台帳 $2=path $3=start $4=end -> 0 で確保 / 1 で拒否
    _tries=0
    while ! mkdir "$1.lock" 2>/dev/null; do
        _tries=$((_tries + 1))
        [ "$_tries" -lt 200000 ] || return 1
    done
    if awk -v p="$2" -v s="$3" -v e="$4" -v band="$band" '
            $1 == p {
                if (s <= $2) { lo_e = e; hi_s = $2 } else { lo_e = $3; hi_s = s }
                gap = hi_s - lo_e - 1
                if (gap < band) { bad = 1; exit }
            }
            END { exit(bad ? 1 : 0) }' "$1"; then
        printf '%s %s %s\n' "$2" "$3" "$4" >>"$1"
        rmdir "$1.lock"
        return 0
    fi
    rmdir "$1.lock"
    return 1
}

# ── 統計 (p50 / p95 / max / mean)。1 行 1 数値のファイルを食う ──────
pct() {
    awk '
        { v[n++] = $1 + 0; sum += $1 + 0 }
        END {
            if (n == 0) { print "0 0 0 0"; exit }
            for (i = 0; i < n; i++)
                for (j = i + 1; j < n; j++)
                    if (v[j] < v[i]) { t = v[i]; v[i] = v[j]; v[j] = t }
            p50 = v[int(n * 0.50)] ; if (int(n * 0.50) >= n) p50 = v[n-1]
            p95 = v[int(n * 0.95)] ; if (int(n * 0.95) >= n) p95 = v[n-1]
            printf "%d %d %d %d\n", p50, p95, v[n-1], sum / n
        }' "$1"
}

# ── 1 段を走らせる ────────────────────────────────────────────────
# 結果はグローバルへ置く (POSIX sh に戻り値の構造体は無い)。
run_stage() { # $1=mode $2=layout $3=n
    st_mode=$1
    st_layout=$2
    st_n=$3
    st_status=ok
    st_reason=""
    st_planned=$st_n
    st_done=0
    st_denied=0
    st_confl_br=0
    st_confl_files=0
    st_hunks=0
    st_confl_lines=0
    st_human=0
    st_survived=0
    st_edit_ms=0
    st_merge_ms=0
    st_gate_p50=0; st_gate_p95=0; st_gate_max=0; st_gate_mean=0
    st_merge_p50=0; st_merge_p95=0; st_merge_max=0; st_merge_mean=0
    st_pred_checked=0
    st_pred_hit=0
    st_close_pairs=0

    if [ "$st_mode" = b ] && [ "$has_lease" != 1 ]; then
        st_status=skip
        st_reason="zai lease が見つかりません"
        return 0
    fi
    if [ "$st_mode" = c ] && [ "$region_aware" != 1 ]; then
        st_status=skip
        st_reason=$region_skip_reason
        return 0
    fi

    stage="$work/$st_mode-$st_layout-$st_n"
    repo="$stage/repo"
    mkdir -p "$repo" "$stage/res" "$stage/home"

    # 段ごとに HOME を作り直す。前の段のリース台帳を引きずらない
    HOME="$stage/home"
    USERPROFILE="$stage/home"
    export HOME USERPROFILE

    # 1 個だけの合成ファイル。ここが肝 —— ファイル単位の所有だと並列度 1 に潰れる
    mkdir -p "$repo/src"
    awk -v n="$lines" 'BEGIN { for (i = 1; i <= n; i++) printf "let value_%06d = %d;\n", i, i }' \
        >"$repo/src/wide.rs"
    if ! (
        cd "$repo"
        git init -q -b main . >/dev/null 2>&1 ||
            { git init -q . && git symbolic-ref HEAD refs/heads/main; }
        git add -A
        git commit -qm "coedit-bench: 合成ファイル $lines 行"
    ) >/dev/null 2>&1; then
        st_status=skip
        st_reason="合成リポジトリを作れませんでした"
        return 0
    fi

    plan_regions "$st_n" "$lines" "$band" "$st_layout" "$seed" >"$stage/plan"
    st_close_pairs=$(count_close_pairs "$stage/plan")

    if [ "$st_mode" = b ] || [ "$st_mode" = c ]; then
        if ! "$zai" lease enable --dir "$repo" >/dev/null 2>&1; then
            st_status=skip
            st_reason="zai lease enable が失敗しました"
            return 0
        fi
    fi
    : >"$stage/ledger"

    # 体ごとの隔離 worktree を先に作る (worktree の生成そのものは計測対象外)
    i=1
    while [ "$i" -le "$st_n" ]; do
        git -C "$repo" worktree add -q -b "w$i" "$stage/wt$i" main >/dev/null 2>&1
        i=$((i + 1))
    done

    # ── 書き込みフェーズ (ここだけ本当に並列で走らせる) ────────────
    t0=$(now_ms)
    while read -r idx rstart rend; do
        (
            agent=$(printf 'a%03d' "$idx")
            wt="$stage/wt$idx"
            res="$stage/res/$idx"
            g0=$(now_ms)
            granted=1
            case "$st_mode" in
            a) granted=1 ;;
            b) "$zai" lease claim 'src/wide.rs' --agent "$agent" --dir "$repo" >/dev/null 2>&1 || granted=0 ;;
            c) "$zai" lease claim "src/wide.rs#L$rstart-$rend" --agent "$agent" --dir "$repo" >/dev/null 2>&1 || granted=0 ;;
            cref) ref_claim "$stage/ledger" 'src/wide.rs' "$rstart" "$rend" || granted=0 ;;
            esac
            g1=$(now_ms)
            if [ "$granted" = 1 ]; then
                if awk -v s="$rstart" -v e="$rend" -v a="$idx" '{
                    if (NR >= s && NR <= e)
                        printf "let value_%06d = %d; // a%03d\n", NR, NR * 1000 + a, a
                    else print
                }' "$wt/src/wide.rs" >"$wt/src/wide.new"; then
                    mv "$wt/src/wide.new" "$wt/src/wide.rs"
                fi
                # ref のロック競合で稀に失敗するので数回だけ粘る (長い sleep は書かない)
                tries=0
                while [ "$tries" -lt 5 ]; do
                    if git -C "$wt" commit -qam "$agent: L$rstart-$rend" >/dev/null 2>&1; then
                        break
                    fi
                    tries=$((tries + 1))
                done
            fi
            printf '%s %s\n' "$granted" "$((g1 - g0))" >"$res"
        ) &
    done <"$stage/plan"
    wait || true
    t1=$(now_ms)
    st_edit_ms=$((t1 - t0))

    : >"$stage/gate_ms"
    i=1
    while [ "$i" -le "$st_n" ]; do
        if [ -f "$stage/res/$i" ]; then
            read -r g_ok g_ms <"$stage/res/$i" || { g_ok=0; g_ms=0; }
        else
            g_ok=0; g_ms=0
        fi
        if [ "$g_ok" = 1 ]; then
            st_done=$((st_done + 1))
        else
            st_denied=$((st_denied + 1))
        fi
        [ "$st_mode" = a ] || printf '%s\n' "$g_ms" >>"$stage/gate_ms"
        i=$((i + 1))
    done
    # shellcheck disable=SC2046  # pct は空白区切りの 4 数値を返す。分割させたい
    set -- $(pct "$stage/gate_ms")
    st_gate_p50=$1; st_gate_p95=$2; st_gate_max=$3; st_gate_mean=$4

    # ── 統合フェーズ (作成順に直列マージ。衝突したら abort して次へ) ──
    git -C "$repo" worktree add -q -b integ "$stage/integ" main >/dev/null 2>&1
    integ="$stage/integ"
    : >"$stage/merge_ms"
    t0=$(now_ms)
    i=1
    while [ "$i" -le "$st_n" ]; do
        nc=$(git -C "$repo" rev-list --count "main..w$i" 2>/dev/null || echo 0)
        if [ "$nc" -gt 0 ]; then
            if [ "$has_merge_tree" = 1 ]; then
                head_sha=$(git -C "$integ" rev-parse HEAD)
                if git -C "$repo" merge-tree --write-tree "$head_sha" "w$i" >/dev/null 2>&1; then
                    pred=clean
                else
                    pred=conflict
                fi
                st_pred_checked=$((st_pred_checked + 1))
            else
                pred=unknown
            fi
            m0=$(now_ms)
            if git -C "$integ" merge --no-edit -q "w$i" >/dev/null 2>&1; then
                actual=clean
            else
                actual=conflict
                st_confl_br=$((st_confl_br + 1))
                st_human=$((st_human + 1))
                git -C "$integ" diff --name-only --diff-filter=U >"$stage/unmerged" 2>/dev/null || : >"$stage/unmerged"
                nf=$(awk 'END { print NR + 0 }' "$stage/unmerged")
                st_confl_files=$((st_confl_files + nf))
                if [ "$nf" -gt 0 ]; then
                    # shellcheck disable=SC2046  # 衝突ファイル名の並びを awk の引数へ展開したい
                    set -- $(cd "$integ" && awk '
                        /^<<<<<<< / { h++; ins = 1; next }
                        /^=======$/ { if (ins) next }
                        /^>>>>>>> / { ins = 0; next }
                        { if (ins) l++ }
                        END { printf "%d %d\n", h + 0, l + 0 }' $(cat "$stage/unmerged"))
                    st_hunks=$((st_hunks + $1))
                    st_confl_lines=$((st_confl_lines + $2))
                fi
                git -C "$integ" merge --abort >/dev/null 2>&1 || true
            fi
            m1=$(now_ms)
            printf '%s\n' "$((m1 - m0))" >>"$stage/merge_ms"
            if [ "$pred" != unknown ] && [ "$pred" = "$actual" ]; then
                st_pred_hit=$((st_pred_hit + 1))
            fi
        fi
        i=$((i + 1))
    done
    t1=$(now_ms)
    st_merge_ms=$((t1 - t0))
    # shellcheck disable=SC2046
    set -- $(pct "$stage/merge_ms")
    st_merge_p50=$1; st_merge_p95=$2; st_merge_max=$3; st_merge_mean=$4

    # ── 総作業量は保存されたか ──────────────────────────────────
    # 統合後のファイルに、何体ぶんの印が残っているかを数える。
    # 「拒否も衝突も 0」でも、統合後に消えていたら意味が無い
    st_survived=$(grep -o '// a[0-9][0-9][0-9]' "$integ/src/wide.rs" 2>/dev/null |
        sort -u | awk 'END { print NR + 0 }')
    [ -n "$st_survived" ] || st_survived=0

    HOME="$work/home"
    USERPROFILE="$work/home"
    export HOME USERPROFILE
    return 0
}

# ── 実行 ──────────────────────────────────────────────────────────
rows="$work/rows"
: >"$rows"
fail=0

mode_label() {
    case "$1" in
    a) echo "A 素の git" ;;
    b) echo "B ファイル所有" ;;
    c) echo "C 行域(出荷物)" ;;
    cref) echo "C 行域(参照)" ;;
    esac
}

for lay in $(printf '%s' "$layouts" | tr ',' ' '); do
    for n in $(printf '%s' "$agents" | tr ',' ' '); do
        # 行域を配れるかを先に検算する。配れないまま走らせて「衝突 0」を
        # 出すのが一番たちが悪い
        if [ "$lay" = disjoint ]; then
            need=$((n * (band + 2)))
            if [ "$lines" -lt "$need" ]; then
                echo "行数が足りません: --agents $n --band $band には --lines $need 以上が要ります (いまは $lines)" >&2
                exit 2
            fi
        fi
        for m in $(printf '%s' "$modes" | tr ',' ' '); do
            run_stage "$m" "$lay" "$n"
            printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
                "$lay" "$m" "$n" "$st_status" "$st_planned" "$st_done" "$st_denied" \
                "$st_confl_br" "$st_confl_files" "$st_hunks" "$st_confl_lines" "$st_human" \
                "$st_survived" "$st_edit_ms" "$st_merge_ms" \
                "$st_gate_p50" "$st_gate_p95" "$st_gate_max" "$st_gate_mean" \
                "$st_merge_p50" "$st_merge_p95" "$st_merge_max" "$st_merge_mean" \
                "${st_close_pairs:-0}" "$st_pred_checked" "$st_pred_hit" >>"$rows"
            if [ "$st_status" = skip ]; then
                printf 'skip\t%s\t%s\t%s\t%s\n' "$lay" "$m" "$n" "$st_reason" >>"$work/skips"
            elif [ "$m" != a ] && [ "$st_hunks" -gt 0 ]; then
                fail=1
            fi
        done
    done
done

# ── 報告 ──────────────────────────────────────────────────────────
out=/dev/stdout
[ "$json" = 0 ] || out=/dev/stderr

{
    echo "== coedit-bench: 同じファイルの違う行を N 体で同時に書く =="
    echo "   行数: $lines / 安全帯: $band 行 / 種: $seed / 体数: $agents"
    echo "   git: $git_version (merge-tree --write-tree: $([ "$has_merge_tree" = 1 ] && echo あり || echo 'なし — 予測の検算は skip'))"
    echo "   zai: ${zai:-見つかりません}"
    if [ "$region_aware" = 1 ]; then
        echo "   行域対応: あり (段 C を出荷物で実測します)"
    else
        echo "   行域対応: **なし** — 段 C は未測定。理由: $region_skip_reason"
    fi
    [ "$clock" != none ] || echo "   ** 時計が秒精度しかありません。ms 列は粗い値です **"
    echo
    printf '%-9s %-15s %4s %6s %5s %5s %5s %5s %6s %6s %6s %5s %6s %8s %8s\n' \
        layout 段 体数 状態 計画 完了 拒否 衝突枝 衝突F ハンク 衝突行 人手 生存 編集ms 統合ms
    while IFS='	' read -r r_lay r_mode r_n r_st r_plan r_done r_deny r_br r_cf r_hunk r_cl r_hum r_surv r_ems r_mms r_gp50 r_gp95 r_gmax r_gmean r_mp50 r_mp95 r_mmax r_mmean r_close r_pc r_ph; do
        if [ "$r_st" = skip ]; then
            printf '%-9s %-15s %4s %6s %s\n' "$r_lay" "$(mode_label "$r_mode")" "$r_n" skip "(未測定)"
        else
            printf '%-9s %-15s %4s %6s %5s %5s %5s %5s %6s %6s %6s %5s %6s %8s %8s\n' \
                "$r_lay" "$(mode_label "$r_mode")" "$r_n" ok \
                "$r_plan" "$r_done" "$r_deny" "$r_br" "$r_cf" "$r_hunk" "$r_cl" "$r_hum" "$r_surv" "$r_ems" "$r_mms"
        fi
    done <"$rows"
    echo
    echo "-- ゲート待ち / 統合 1 回 (ms, p50/p95/max/mean)。max と mean の乖離が停止のサイン --"
    printf '%-9s %-15s %4s %22s %22s %10s %12s\n' layout 段 体数 ゲート 統合1回 近すぎる組 予測的中
    while IFS='	' read -r r_lay r_mode r_n r_st r_plan r_done r_deny r_br r_cf r_hunk r_cl r_hum r_surv r_ems r_mms r_gp50 r_gp95 r_gmax r_gmean r_mp50 r_mp95 r_mmax r_mmean r_close r_pc r_ph; do
        if [ "$r_st" = skip ]; then continue; fi
        printf '%-9s %-15s %4s %22s %22s %10s %12s\n' \
            "$r_lay" "$(mode_label "$r_mode")" "$r_n" \
            "$r_gp50/$r_gp95/$r_gmax/$r_gmean" "$r_mp50/$r_mp95/$r_mmax/$r_mmean" "$r_close" "$r_ph/$r_pc"
    done <"$rows"
    if [ -f "$work/skips" ]; then
        echo
        echo "-- skip した段 (黙って飛ばさない) --"
        while IFS='	' read -r _s s_lay s_mode s_n s_reason; do
            printf '   %s / %s / %s 体: %s\n' "$s_lay" "$(mode_label "$s_mode")" "$s_n" "$s_reason"
        done <"$work/skips"
    fi
    echo
    if [ "$fail" = 0 ]; then
        echo "== 保護のある段 (B / C / Cref) に衝突ハンクは 1 つも残りませんでした =="
    else
        echo "== 保護のある段に衝突が残りました。主張が壊れています =="
    fi
} >"$out"

if [ "$json" = 1 ]; then
    awk -F'\t' -v lines="$lines" -v band="$band" -v seed="$seed" \
        -v gitver="$git_version" -v zaipath="${zai:-}" -v ra="$region_aware" \
        -v rr="$region_skip_reason" -v clk="$clock" -v fail="$fail" '
        BEGIN {
            printf "{\n  \"lines\": %d,\n  \"band\": %d,\n  \"seed\": %d,\n", lines, band, seed
            printf "  \"git\": \"%s\",\n  \"zai\": \"%s\",\n", gitver, zaipath
            printf "  \"region_aware\": %s,\n  \"region_skip_reason\": \"%s\",\n", (ra == 1 ? "true" : "false"), rr
            printf "  \"clock\": \"%s\",\n  \"claim_holds\": %s,\n  \"stages\": [\n", clk, (fail == 0 ? "true" : "false")
        }
        {
            if (NR > 1) printf ",\n"
            printf "    {\"layout\": \"%s\", \"mode\": \"%s\", \"agents\": %d, \"status\": \"%s\"", $1, $2, $3, $4
            if ($4 != "skip")
                printf ", \"planned\": %d, \"completed\": %d, \"denied\": %d, \"conflicted_branches\": %d, \"conflicted_files\": %d, \"hunks\": %d, \"conflict_lines\": %d, \"human_touches\": %d, \"survived_agents\": %d, \"edit_ms\": %d, \"merge_ms\": %d, \"gate_ms\": {\"p50\": %d, \"p95\": %d, \"max\": %d, \"mean\": %d}, \"merge_one_ms\": {\"p50\": %d, \"p95\": %d, \"max\": %d, \"mean\": %d}, \"too_close_pairs\": %d, \"predict_checked\": %d, \"predict_hit\": %d", $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26
            printf "}"
        }
        END { printf "\n  ]\n}\n" }' "$rows"
fi

exit "$fail"
