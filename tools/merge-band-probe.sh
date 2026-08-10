#!/usr/bin/env sh
# 「行域を安全帯ぶん離して配れば git は衝突しない」という**モデルそのもの**を
# 反証しに行く。zaivern を 1 度も呼ばない — 素の git だけで測る。
#
# ## なぜ要るか
#
# `region::SAFE_BAND` (3) と `region::MERGE_ONLY_BAND` (1) は
# **2 本の組**で「何行離せば git が衝突しないか」を測って決めた値である。
# しかし実際の統合は
#
#   * 枝が **1 本あたり複数の行**を直し
#   * それを **N 本直列にマージする**
#
# ので、「2 本の組で安全だった間隔」がそのまま成り立つ保証はない。
# ここは 3 つの形を分けて測る:
#
#   pairs   2 本が 1 行ずつ。間隔を振る          (安全帯の元になった形)
#   packed  N 本が 1 行ずつ等間隔に並ぶ
#   bracket 片方が **2 行以上**で相手の行を挟む  (実リポジトリで実際に出た形)
#
# そして**内容の種類**を振る。周期的・反復的なファイル (Markdown の ```/---、
# 空行だらけ、生成コード、同じ行の連続) では diff の切れ目が行間隔だけで
# 決まらないため、離れていても畳まれて衝突しうる。
#
# ## 使い方
#
#   tools/merge-band-probe.sh              全部
#   tools/merge-band-probe.sh --mode bracket
#
# 終了コード: 0 = 反証なし / 1 = 「安全帯を満たすのに衝突した」組があった
set -eu

mode=all
case "${1:-}" in
--mode) mode=${2:-all} ;;
-h | --help)
    sed -n '2,36p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
esac

python3 - "$mode" <<'PY'
import os
import subprocess
import sys
import tempfile

mode = sys.argv[1]


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


def run_pairs():
    global bad_total
    gaps = (1, 2, 3, 4, 6, 8)
    print("\n[pairs] 2 本が 1 行ずつ。間隔を振る (行 100 と 100+gap)")
    print("%-11s%s" % ("内容", "".join("  gap=%-3d" % g for g in gaps)))
    for k in KINDS:
        row = "%-11s" % k
        for g in gaps:
            w, b = repo(k)
            branch(w, b, "A", [100], "A")
            branch(w, b, "B", [100 + g], "B")
            n = serial_merge(w, b, ["A", "B"])
            row += "  %-7s" % ("衝突" if n else "clean")
            if n and g >= 3:
                bad_total += 1
        print(row)


def run_packed():
    global bad_total
    print("\n[packed] N 本が 1 行ずつ間隔 S で等間隔 (衝突したマージ数)")
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
                n = serial_merge(w, b, names)
                row += "  %-6d" % n
                if n and S >= 3:
                    bad_total += 1
            print(row)


def run_bracket():
    global bad_total
    cases = [("A={17} B={13,25}", [17], [13, 25]),
             ("A={17} B={13,21}", [17], [13, 21]),
             ("A={17} B={5,13,25}", [17], [5, 13, 25]),
             ("A={17} B={1,13,25,33}", [17], [1, 13, 25, 33]),
             ("A={100} B={92,108}", [100], [92, 108]),
             ("A={100} B={88,96,108}", [100], [88, 96, 108])]
    print("\n[bracket] 片方が複数行で相手の行を挟む (どの組も間隔は 4 行以上)")
    print("%-26s%s" % ("形", "".join("%-11s" % k for k in KINDS)))
    for label, a, bl in cases:
        row = "%-26s" % label
        for k in KINDS:
            w, b = repo(k)
            branch(w, b, "A", a, "A")
            branch(w, b, "B", bl, "B")
            n = serial_merge(w, b, ["A", "B"])
            row += "%-11s" % ("衝突" if n else "clean")
            if n:
                bad_total += 1
        print(row)


if mode in ("all", "pairs"):
    run_pairs()
if mode in ("all", "packed"):
    run_packed()
if mode in ("all", "bracket"):
    run_bracket()

print("\n安全帯 (3 行) を満たしているのに衝突した組: %d" % bad_total)
print("0 でなければ「行域を安全帯ぶん離せば衝突しない」というモデルが"
      "その内容では成り立っていない。")
sys.exit(1 if bad_total else 0)
PY
