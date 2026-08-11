#!/usr/bin/env sh
# 「行域を安全帯ぶん離して配れば git は衝突しない」という**モデルそのもの**を
# 反証しに行く。zaivern を 1 度も呼ばない — 素の git だけで測る。
#
# ## 何が分かったか (このスクリプトが見つけた穴)
#
# `region::SAFE_BAND` (3) と `region::MERGE_ONLY_BAND` (1) は
# **2 本の組**で「何行離せば git が衝突しないか」を測って決めた値で、
# **その主張は今も正しい** (周期 1〜12 のどんな反復本文でも、1 行ずつの
# 変更 2 つは間隔 1 行で綺麗に通る。`--mode pairs` が毎回測り直す)。
#
# 崩れたのは**そこから先の推論**のほうで、
#
#     「全部の組が帯を満たす」⇒「全部まとめてマージしても綺麗に通る」
#
# は成り立たない。`git merge` の既定戦略 ort は **diff アルゴリズムを
# histogram に固定している** (`man git-merge`: "ort specifically uses
# diff-algorithm=histogram")。histogram は本文が反復的だと*同じ側の複数の
# 変更*を 1 つの巨大なハンクへ畳むので、
#
#   * **交錯** — 片方の担当がもう片方を上下から挟んでいる
#   * **順番**  — 直列マージで `ours` に積み上がった変更が、これから混ぜる枝を挟む
#
# のどちらかが起きた瞬間、帯を何行取っていても衝突しうる。
#
# ## 直した後のモデル (このスクリプトが今検査しているもの)
#
#   1. 帯を満たす (従来どおり)
#   2. **かつ** 交錯していないか、交錯するなら**隣り合う他人の域の間に
#      「ファイル内で唯一の行」(錨) が 1 本以上ある**
#   3. **かつ** 混ぜる順が行番号の昇順 (`coedit::merge_order`)
#
# 3 つ揃って初めて「一撃でマージできる」と言ってよい。
#
# ## 使い方
#
#   tools/merge-band-probe.sh              全部
#   tools/merge-band-probe.sh --mode bracket
#   tools/merge-band-probe.sh --mode order    順番だけを振る
#
# 終了コード: 0 = 反証なし / 1 = 「直した後のモデルが通したのに衝突した」組があった
set -eu

mode=all
case "${1:-}" in
--mode) mode=${2:-all} ;;
-h | --help)
    sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
esac

python3 - "$mode" <<'PY'
import collections
import os
import subprocess
import sys
import tempfile

mode = sys.argv[1]
BAND = 3  # region::SAFE_BAND


def sh(*a, cwd=None):
    return subprocess.run(a, cwd=cwd, capture_output=True, text=True)


def content(kind, n=600):
    L = []
    for i in range(n):
        if kind == "fences":
            L.append(["```", "code line", "```", "", "---", ""][i % 6])
        elif kind == "repeat":
            L.append("SAME LINE")
        elif kind == "blank":
            L.append("" if i % 3 else "para %d" % (i // 3))
        elif kind == "generated":
            L.append(["// AUTO-GENERATED. DO NOT EDIT.", "#[derive(Debug)]",
                      "pub struct S {", "    pub a: u32,", "}", ""][i % 6])
        else:
            L.append("unique line %d" % i)
    return "\n".join(L) + "\ntail\n"


KINDS = ("unique", "fences", "blank", "generated", "repeat")


# ── 直した後のモデル (region::anchor_lines / interleave_safe と同じ判定) ──

def anchors(text):
    """ファイル内でちょうど 1 回しか出てこない行の 1 始まり行番号。"""
    L = text.split("\n")
    c = collections.Counter(L)
    return {i + 1 for i, l in enumerate(L) if c[l] == 1}


def model_ok(text, a, b, band=BAND):
    """帯 + 交錯 (錨で緩める) の判定。a / b は 1 行ずつの担当行の一覧。"""
    for x in a:
        for y in b:
            if abs(x - y) - 1 < band:
                return False
    # 交錯していなければ帯だけで足りる (混ぜる順が昇順である限り)
    if not (min(b) <= max(a) and min(a) <= max(b)):
        return True
    anc = anchors(text)
    flat = sorted([(x, 0) for x in a] + [(y, 1) for y in b])
    for (lo, oa), (hi, ob) in zip(flat, flat[1:]):
        if oa == ob:
            continue
        if not any(g in anc for g in range(lo + 1, hi)):
            return False
    return True


def repo(kind):
    w = tempfile.mkdtemp()
    for c in (["git", "init", "-q", "."], ["git", "config", "user.email", "a@b.c"],
              ["git", "config", "user.name", "t"],
              ["git", "config", "commit.gpgsign", "false"]):
        sh(*c, cwd=w)
    open(os.path.join(w, "f.md"), "w").write(content(kind))
    sh("git", "add", "-A", cwd=w)
    sh("git", "commit", "-qm", "base", cwd=w)
    return w, sh("git", "rev-parse", "HEAD", cwd=w).stdout.strip()


def branch(w, base, name, lines, mark):
    sh("git", "checkout", "-q", "-B", name, base, cwd=w)
    p = os.path.join(w, "f.md")
    L = open(p).read().split("\n")
    for ln in lines:
        L[ln - 1] += "  <<%s>>" % mark
    open(p, "w").write("\n".join(L))
    sh("git", "commit", "-qam", name, cwd=w)


def serial_merge(w, base, names):
    sh("git", "checkout", "-q", "-B", "INT", base, cwd=w)
    bad = 0
    for n in names:
        if sh("git", "merge", "--no-edit", "-q", n, cwd=w).returncode:
            bad += 1
            sh("git", "checkout", "--theirs", ".", cwd=w)
            sh("git", "add", "-A", cwd=w)
            sh("git", "commit", "-q", "--no-edit", cwd=w)
    return bad


bad_total = 0


def check(kind, a, b, hit):
    """モデルが通したのに衝突したら反証。逆 (断ったのに通った) は過剰報告で無害。"""
    global bad_total
    if hit and model_ok(content(kind), a, b):
        bad_total += 1
        print("   ⚠ 反証: %s A=%s B=%s" % (kind, a, b))


def run_pairs():
    gaps = (1, 2, 3, 4, 6, 8)
    print("\n[pairs] 2 本が 1 行ずつ。間隔を振る (行 100 と 100+gap)")
    print("  → 帯の**元になった主張**。反復本文でもここは崩れない")
    print("%-11s%s" % ("内容", "".join("  gap=%-3d" % g for g in gaps)))
    for k in KINDS:
        row = "%-11s" % k
        for g in gaps:
            w, b = repo(k)
            branch(w, b, "A", [100], "A")
            branch(w, b, "B", [100 + g], "B")
            n = serial_merge(w, b, ["A", "B"])
            row += "  %-7s" % ("衝突" if n else "clean")
            check(k, [100], [100 + g], n)
        print(row)


def run_packed():
    global bad_total
    print("\n[packed] N 本が 1 行ずつ間隔 S で等間隔 (衝突したマージ数)")
    print("  → **昇順で混ぜる限り 0**。順番の話であって帯の話ではない")
    print("%-11s %-4s%s" % ("内容", "N", "".join("  S=%-4d" % s for s in (2, 3, 4, 8))))
    for k in KINDS:
        for N in (4, 8, 16):
            row = "%-11s %-4d" % (k, N)
            for S in (2, 3, 4, 8):
                w, b = repo(k)
                names = []
                for i in range(N):
                    branch(w, b, "w%d" % i, [100 + i * S], "w%d" % i)
                    names.append("w%d" % i)
                # 行番号の昇順 = production の `coedit::merge_order`
                n = serial_merge(w, b, names)
                row += "  %-6d" % n
                if n and S >= BAND:
                    bad_total += n
                    print("   ⚠ 反証: %s N=%d S=%d で %d 件 (昇順なのに崩れた)"
                          % (k, N, S, n))
            print(row)


def run_order():
    global bad_total
    print("\n[order] 同じ配置を**混ぜる順だけ**変える (16 本・1 行ずつ・間隔 S)")
    print("  → 昇順なら 0。逆順・入れ替えは反復本文で崩れる")
    print("%-11s %-6s %-8s %-8s %s" % ("内容", "S", "昇順", "降順", "入れ替え"))
    for k in ("unique", "fences", "repeat"):
        for S in (2, 4, 9):
            names = ["w%d" % i for i in range(16)]
            res = []
            for label, order in (("asc", names), ("desc", names[::-1]),
                                 ("mix", [names[i] for i in
                                          (0, 8, 4, 12, 2, 10, 6, 14, 1, 9, 5, 13, 3, 11, 7, 15)])):
                w, b = repo(k)
                for i, n in enumerate(names):
                    branch(w, b, n, [100 + i * S], n)
                res.append(serial_merge(w, b, order))
            print("%-11s %-6d %-8d %-8d %d" % (k, S, res[0], res[1], res[2]))
            if res[0]:
                bad_total += res[0]
                print("   ⚠ 反証: 昇順なのに %s S=%d で %d 件" % (k, S, res[0]))


def run_bracket():
    cases = [("A={17} B={13,25}", [17], [13, 25]),
             ("A={17} B={13,21}", [17], [13, 21]),
             ("A={17} B={5,13,25}", [17], [5, 13, 25]),
             ("A={17} B={1,13,25,33}", [17], [1, 13, 25, 33]),
             ("A={100} B={92,108}", [100], [92, 108]),
             ("A={100} B={88,96,108}", [100], [88, 96, 108])]
    print("\n[bracket] 片方が複数行で相手の行を挟む (どの組も間隔は 4 行以上)")
    print("  → **穴はここ**。帯を満たしていても反復本文では衝突する。")
    print("     `モデル` 行が 断 なら、直した後の判定は既に断っている")
    print("%-26s%s" % ("形", "".join("%-11s" % k for k in KINDS)))
    for label, a, bl in cases:
        row = "%-26s" % label
        judge = "%-26s" % "  └ 直した後のモデル"
        for k in KINDS:
            w, b = repo(k)
            branch(w, b, "A", a, "A")
            branch(w, b, "B", bl, "B")
            n = serial_merge(w, b, ["A", "B"])
            row += "%-11s" % ("衝突" if n else "clean")
            judge += "%-11s" % ("通" if model_ok(content(k), a, bl) else "断")
            check(k, a, bl, n)
        print(row)
        print(judge)


if mode in ("all", "pairs"):
    run_pairs()
if mode in ("all", "packed"):
    run_packed()
if mode in ("all", "order"):
    run_order()
if mode in ("all", "bracket"):
    run_bracket()

print("\n直した後のモデルが「通してよい」と言ったのに衝突した組: %d" % bad_total)
print("0 でなければ、帯 + 交錯 (錨) + 昇順の 3 つでもまだ足りていない。")
sys.exit(1 if bad_total else 0)
PY
