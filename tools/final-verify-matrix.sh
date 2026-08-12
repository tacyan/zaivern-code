#!/usr/bin/env sh
# 「どのリポジトリでも競合が起きない」を **反証しに行く** ための総当たり駆動。
#
# `tools/anyrepo-prove.sh` を 1 回だけ回しても「その条件では反証できなかった」
# しか言えない。主張は「どのリポジトリでも」なので、
#
#   リポジトリ × 書き手数 × 乱数種 × 重なり × 交渉の有無
#
# を振って、**1 件でも `dup_lines > 0` / `conflict_files > 0` が出るか**を探す。
#
# ## anyrepo-prove.sh の判定をそのまま信じない
#
# **かつて `verdict_of()` は `conflict_files` しか見ておらず `dup_lines` を
# 見ていなかった**。「2 人が同じ行を書いたが、たまたま git が衝突と呼ばなかった」
# 場合に向こうは `proved` と言っていた。これは直したが (`C2`)、
# **ここでの独立な計数はやめない**。判定器と計数器を同じ人が書いている以上、
# 片方が壊れたときにもう片方が気付けなければ意味がないためである。
#
# したがってこの駆動は 2 つを別々に出す:
#
#   * 反証 — `zaivern` 段で `dup_lines > 0` か `conflict_files > 0`
#   * **判定器の欠陥** — 反証があるのに向こうが `proved` と言った
#
# 「断られて 0」と「全員書けて 0」も区別する (`applied` / `planned`)。
#
# ## 使い方
#
#   tools/final-verify-matrix.sh --repos <パス,パス,...> \
#       [--writers 8,16,32,64] [--seeds 1,2,3] [--overlaps 0.0,0.5,1.0] \
#       [--shift both|on|off] [--out <ディレクトリ>] [--picks N] [--timeout 秒]
#
#   --shift both  交渉あり / なし の両方を回す (既定)
#
# 出力: `<out>/<repo>__w<N>__s<seed>__o<overlap>__<shift>.json` と、
#       最後に反証の一覧を stderr へ。
#
# 終了コード: 0 = 反証は 1 件も出なかった / 1 = 反証が出た / 2 = 使い方の誤り
set -eu
# Windows (Git Bash / PowerShell) の既定コードページは UTF-8 ではないので、
# Python が日本語を stdout へ書いた瞬間に
# `UnicodeEncodeError: 'charmap' codec can't encode characters` で落ちる
# (CI の probe (windows-latest) が実際にこれで赤くなった)。
# **どの OS でも同じ出力になるよう UTF-8 を明示する。** 既に設定されていれば尊重する。
export PYTHONUTF8="${PYTHONUTF8:-1}"
export PYTHONIOENCODING="${PYTHONIOENCODING:-utf-8}"

# shellcheck disable=SC1007
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

repos=""
writers="8,16"
seeds="20260811"
overlaps="0.5"
shiftmode="both"
out=""
picks=6
timeout_s=180
extra=""

while [ $# -gt 0 ]; do
    case "$1" in
    --repos)    repos=${2:-}; shift 2 ;;
    --writers)  writers=${2:-}; shift 2 ;;
    --seeds)    seeds=${2:-}; shift 2 ;;
    --overlaps) overlaps=${2:-}; shift 2 ;;
    --shift)    shiftmode=${2:-}; shift 2 ;;
    --out)      out=${2:-}; shift 2 ;;
    --picks)    picks=${2:-}; shift 2 ;;
    --timeout)  timeout_s=${2:-}; shift 2 ;;
    --extra)    extra=${2:-}; shift 2 ;;
    -h|--help)  sed -n '2,33p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "知らない引数: $1" >&2; exit 2 ;;
    esac
done

[ -n "$repos" ] || { echo "--repos が要ります" >&2; exit 2; }
[ -n "$out" ] || out=$(mktemp -d)
mkdir -p "$out"

echo "== 出力先: $out" >&2

case "$shiftmode" in
both) shifts="on off" ;;
on)   shifts="on" ;;
off)  shifts="off" ;;
*) echo "--shift は both/on/off" >&2; exit 2 ;;
esac

IFS_SAVE=$IFS
n=0
for repo in $(printf '%s' "$repos" | tr ',' ' '); do
    rname=$(basename "$repo")
    for w in $(printf '%s' "$writers" | tr ',' ' '); do
        for sd in $(printf '%s' "$seeds" | tr ',' ' '); do
            for ov in $(printf '%s' "$overlaps" | tr ',' ' '); do
                for sh_ in $shifts; do
                    tag="${rname}__w${w}__s${sd}__o${ov}__${sh_}"
                    jf="$out/$tag.json"
                    lf="$out/$tag.log"
                    tf="$out/$tag.trace.jsonl"
                    [ -s "$jf" ] && { echo "   skip (既にある): $tag" >&2; continue; }
                    sflag=""
                    [ "$sh_" = "off" ] && sflag="--no-shift"
                    n=$((n + 1))
                    echo "-- [$n] $tag" >&2
                    # shellcheck disable=SC2086
                    sh "$root/tools/anyrepo-prove.sh" \
                        --repo "$repo" --writers "$w" --seed "$sd" \
                        --overlap "$ov" --picks "$picks" \
                        --timeout "$timeout_s" --trace "$tf" \
                        $sflag $extra --json >"$jf" 2>"$lf" || true
                    if [ ! -s "$jf" ]; then
                        echo "   !! JSON が空。log の末尾:" >&2
                        tail -5 "$lf" >&2 || true
                    fi
                done
            done
        done
    done
done
IFS=$IFS_SAVE

echo "== 集計 ($n 回)" >&2
python3 - "$out" <<'PY'
import json, os, sys
d = sys.argv[1]
rows, bad, broken = [], [], []
for fn in sorted(os.listdir(d)):
    if not fn.endswith(".json"):
        continue
    p = os.path.join(d, fn)
    try:
        o = json.load(open(p, encoding="utf-8"))
    except Exception as e:
        broken.append((fn, "JSON を読めません: %s" % e))
        continue
    for res in o.get("results", [o]):
        v = res.get("verdict")
        for r in res.get("runs", []):
            for stage in ("baseline", "zaivern"):
                s = r.get(stage)
                if not s:
                    continue
                rows.append(dict(file=fn, verdict=v, writers=r["writers"], stage=stage,
                                 planned=s["planned"], applied=s["applied"],
                                 dup=s["dup_lines"], conf=s["conflict_files"],
                                 hunks=s.get("hunks", 0),
                                 refused=s.get("claim_refused", 0),
                                 timeouts=s.get("timeouts", 0),
                                 detail=s.get("dup_detail", [])))
                if stage == "zaivern" and (s["dup_lines"] > 0 or s["conflict_files"] > 0):
                    bad.append(rows[-1])
                    if v == "proved":
                        # **判定器の欠陥。** 反証が出ているのに「証明できた」と
                        # 言うのは、測り方ではなく判定の側が壊れている
                        broken.append((fn, "判定器の欠陥: dup=%d conf=%d なのに proved"
                                       % (s["dup_lines"], s["conflict_files"])))
        if v not in ("proved",):
            broken.append((fn, "%s: %s" % (v, "; ".join(res.get("reasons", []))[:300])))

print("%-52s %-8s %4s %-8s %6s %6s %5s %5s %5s" %
      ("file", "verdict", "N", "stage", "plan", "appl", "dup", "conf", "to"))
for r in rows:
    print("%-52s %-8s %4d %-8s %6d %6d %5d %5d %5d" %
          (r["file"][:52], r["verdict"], r["writers"], r["stage"],
           r["planned"], r["applied"], r["dup"], r["conf"], r["timeouts"]))
print()
print("== 反証 (zaivern 段で dup_lines>0 か conflict_files>0): %d 件" % len(bad))
for r in bad:
    print("   %s N=%d dup=%d conf=%d  %s" %
          (r["file"], r["writers"], r["dup"], r["conf"], r["detail"][:6]))
print("== proved 以外 / 壊れた出力: %d 件" % len(broken))
for f, why in broken:
    print("   %s: %s" % (f, why))
sys.exit(1 if bad else 0)
PY
