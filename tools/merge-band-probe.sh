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
# ## さらに崩れたもの — 交錯していなくても足りない (`--mode random`)
#
# 「交錯していなければ帯だけで足りる」は**置換だけを測っている限り**正しかった。
# 削除・挿入は行数を変えるので、histogram は*同じ側の変更をどこへ置くか*を
# 選び直せる。周期的な本文ではその自由度がファイル全体に及び、
# **上下に分かれた組でも衝突する**。
#
# いちばん短い反証 (周期 6・300 行・末尾に一意な行を置かない):
# A が 126〜127 行目を置換 / B が 32 行目の手前へ 1 行挿入 — **94 行離れていて
# 交錯もしていない**のに `git merge` は衝突する。
#
# ## 直した後のモデル (このスクリプトが今検査しているもの)
#
#   1. 帯を満たす (従来どおり)
#   2. **かつ** 持ち主が変わる境目すべてに「ファイル内で唯一の行」(錨) が
#      1 本以上ある (`region::needs_wall` + `region::interleave_safe`)。
#      **交錯しているかどうかは見ない**
#   3. **かつ** 混ぜる順が行番号の昇順 (`coedit::merge_order`)
#
# 3 つ揃って初めて「一撃でマージできる」と言ってよい。
#
# ## 使い方
#
#   tools/merge-band-probe.sh              全部 (random は 2000 通り)
#   tools/merge-band-probe.sh --mode bracket
#   tools/merge-band-probe.sh --mode order    順番だけを振る
#   tools/merge-band-probe.sh --mode random --trials 2000 --seed 20260812
#   tools/merge-band-probe.sh --mode random --real src/region.rs   実ファイルで
#
#   --seed N    乱数種 (既定 20260812)。**見逃した実行の種はログに出る**ので
#               そのまま渡せば決定的に再現できる
#   --trials N  試行数 (既定 2000)
#   --band N    安全帯 (既定 1 = region::MERGE_ONLY_BAND)
#   --real F    合成本文の代わりに実ファイルを素材にする
#
# `zai` の網羅版 (実 git で 2400 ケース) は別コマンド:
#
#   ZAIVERN_PROOF_EXHAUSTIVE=1 cargo test --bin zai coedit::tests::実gitで証明 -- --nocapture
#
# 終了コード: 0 = 反証なし / 1 = 「直した後のモデルが通したのに衝突した」組があった
set -eu

mode=all
seed=20260812
trials=2000
band=1
real=
while [ $# -gt 0 ]; do
    case "$1" in
    --mode)
        mode=${2:-all}
        shift 2
        ;;
    --seed)
        seed=${2:?--seed には数を渡す}
        shift 2
        ;;
    --trials)
        trials=${2:?--trials には数を渡す}
        shift 2
        ;;
    --band)
        band=${2:?--band には数を渡す}
        shift 2
        ;;
    --real)
        real=${2:?--real にはファイルを渡す}
        shift 2
        ;;
    -h | --help)
        sed -n '2,64p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "知らない引数: $1" >&2
        exit 2
        ;;
    esac
done

python3 - "$mode" "$seed" "$trials" "$band" "$real" <<'PY'
import collections
import os
import subprocess
import sys
import tempfile

import random
import shutil

mode = sys.argv[1]
SEED = int(sys.argv[2])
TRIALS = int(sys.argv[3])
BAND = int(sys.argv[4])  # 既定 1 = region::MERGE_ONLY_BAND (`git merge` の下限)
REAL = sys.argv[5]
PAIR_BAND = 3  # region::SAFE_BAND — 固定の表 (pairs/packed/order/bracket) 用


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


def wall_ok(text, a, b):
    """持ち主が変わる境目すべてに錨があるか (region::interleave_safe)。"""
    anc = anchors(text)
    if not anc:
        return False
    flat = sorted([(x, 0) for x in a] + [(y, 1) for y in b])
    for (lo, oa), (hi, ob) in zip(flat, flat[1:]):
        if oa == ob:
            continue
        if not any(g in anc for g in range(lo + 1, hi)):
            return False
    return True


def model_ok(text, a, b, band=PAIR_BAND):
    """**出荷中の判定** (region::needs_wall + interleave_safe)。

    帯を満たし、かつ持ち主が変わる境目すべてに錨がある。
    交錯しているかどうかは**見ない** — 削除・挿入が混ざると上下に分かれた
    組でも衝突するため (`--mode random` が毎回測り直す)。
    """
    for x in a:
        for y in b:
            if abs(x - y) - 1 < band:
                return False
    return wall_ok(text, a, b)


def model_ok_v16(text, a, b, band=PAIR_BAND):
    """0.16.0 までの判定 (交錯しているときだけ錨)。**比較用**。"""
    for x in a:
        for y in b:
            if abs(x - y) - 1 < band:
                return False
    if not (min(b) <= max(a) and min(a) <= max(b)):
        return True
    return wall_ok(text, a, b)


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
                if n and S >= PAIR_BAND:
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


# ── 無作為 — 削除・挿入を混ぜる (穴 1 を潰した実験) ────────────────────

POOL12 = ["```", "code line", "```", "", "---", "",
          "x = 1", "}", "{", "// c", "  ", "..."]


def rand_content(p, n, dens, tail_uniq, rng):
    """周期 p の本文に、密度 dens で「ファイル内で唯一の行」を混ぜる。"""
    L = [POOL12[i % p] for i in range(n)]
    for i in range(n):
        if rng.random() < dens:
            L[i] = "UNIQ-%d" % i
    if tail_uniq:
        L.append("tail-unique-line")
    return "\n".join(L) + "\n"


def apply_edits(base, spans, kinds, tag, nl_is_crlf):
    """置換 / 削除 / 挿入を当てる。**改行は base のものを保つ**。"""
    eol = "\r\n" if nl_is_crlf else "\n"
    L = base.split(eol)
    trailing = L and L[-1] == ""
    if trailing:
        L = L[:-1]
    marks = {}
    for (st, en), k in zip(spans, kinds):
        if k == "insert":
            marks.setdefault(st, []).append("insert")
        else:
            for ln in range(st, en + 1):
                marks[ln] = k
    out = []
    for i, l in enumerate(L):
        m = marks.get(i + 1)
        if isinstance(m, list):
            out.append("%s-new-%d" % (tag, i + 1))
            out.append(l)
        elif m == "delete":
            continue
        elif m == "replace":
            out.append("%s  <<%s>>" % (l, tag))
        else:
            out.append(l)
    return eol.join(out) + (eol if trailing else "")


def flat_lines(spans, kinds):
    """判定へ渡す「担当行」。挿入点は 1 点として数える (region::Span::probe)。"""
    out = []
    for (st, en), k in zip(spans, kinds):
        out.extend([st] if k == "insert" else list(range(st, en + 1)))
    return sorted(out)


def probe(span):
    """region::Span::probe — 挿入点 (end + 1 == start) は点として扱う。"""
    st, en = span
    return (st, st) if en == st - 1 else (st, en)


def disjoint_spans(spans, band):
    """同じ枝の中の域どうしが帯を満たすか (region::is_disjoint と同じ向き)。"""
    ps = [probe(s) for s in spans]
    for i, (a0, a1) in enumerate(ps):
        for b0, b1 in ps[i + 1:]:
            (lo0, lo1), (hi0, hi1) = ((a0, a1), (b0, b1)) if a0 <= b0 else ((b0, b1), (a0, a1))
            if hi0 - lo1 - 1 < band:
                return False
    return True


class Plumb:
    """1 つのリポジトリを使い回して、枝を objects だけで作る (checkout しない)。"""

    def __init__(self):
        self.w = tempfile.mkdtemp(prefix="zai-merge-band-")
        self.env = dict(os.environ, GIT_CONFIG_NOSYSTEM="1",
                        GIT_CONFIG_GLOBAL=os.path.join(self.w, "no-such-gitconfig"),
                        GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="a@b.c",
                        GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="a@b.c")
        self.calls = 0
        self.g("init", "-q", ".")
        self.g("config", "core.autocrlf", "false")

    def g(self, *a, inp=None):
        self.calls += 1
        return subprocess.run(("git",) + a, cwd=self.w, env=self.env,
                              capture_output=True, text=True, input=inp)

    def commit(self, text, parent=None):
        blob = self.g("hash-object", "-w", "--stdin", inp=text).stdout.strip()
        tree = self.g("mktree", inp="100644 blob %s\tf.md\n" % blob).stdout.strip()
        args = ["commit-tree", tree, "-m", "c"] + (["-p", parent] if parent else [])
        return self.g(*args).stdout.strip()

    def conflicts(self, ca, cb):
        r = self.g("merge-tree", "--write-tree", ca, cb)
        return None if r.returncode not in (0, 1) else r.returncode == 1

    def close(self):
        shutil.rmtree(self.w, ignore_errors=True)


def wall_ok_lines(anc, a, b):
    """`wall_ok` の、錨を数え終えている版 (行の一覧で受ける)。"""
    if not anc:
        return False
    flat = sorted([(x, 0) for x in a] + [(y, 1) for y in b])
    for (lo, oa), (hi, ob) in zip(flat, flat[1:]):
        if oa == ob:
            continue
        if not any(g in anc for g in range(lo + 1, hi)):
            return False
    return True


def anchor_fit(anc, last, taken, span, is_insert, band, max_shift):
    """region::anchor_fit の写し。壁のある空きへずらした先を返す。

    見つからなければ None (= そこには本当に置ける場所が無い)。
    距離が同じなら**下方向を先に採る** (決定的)。
    """
    if not anc:
        return None
    st, en = probe(span)
    ln = en - st

    def fits(start):
        end = start + ln
        if start < 1 or end > last:
            return None
        cand_lines = [start] if is_insert else list(range(start, end + 1))
        for y in taken:
            for x in cand_lines:
                if abs(x - y) - 1 < band:
                    return None
        if not wall_ok_lines(anc, taken, cand_lines):
            return None
        return (start, start - 1) if is_insert else (start, end)

    for d in range(max_shift + 1):
        v = fits(st + d)
        if v:
            return v
        if d and st - d >= 1:
            v = fits(st - d)
            if v:
                return v
    return None


def run_random():
    """**穴 1 を潰した実験。** 削除・挿入を混ぜた無作為配置を回す。

    3 つの判定を同じ配置で同時に採点する:
      band  — 帯だけ (錨を見ない)
      v16   — 0.16.0 まで (交錯しているときだけ錨)
      wall  — 出荷中 (`region::needs_wall` + `interleave_safe`)
    """
    global bad_total
    rng = random.Random(SEED)
    plumb = Plumb()
    judges = (("band", lambda t, a, b: all(abs(x - y) - 1 >= BAND for x in a for y in b)),
              ("v16", lambda t, a, b: model_ok_v16(t, a, b, BAND)),
              ("wall", lambda t, a, b: model_ok(t, a, b, BAND)))
    stat = {n: [0, 0, 0, 0] for n, _ in judges}  # 通した / 見逃し / 断った / 断って実衝突
    misses = {n: [] for n, _ in judges}
    skipped = 0
    # 断る代わりにずらす (region::anchor_fit) の効き目
    fit = [0, 0, 0, 0]  # 対象 / ずらし先が見つかった / 証明が立った / 実 git でも綺麗
    real_text = None
    if REAL:
        with open(REAL, newline="") as fh:
            real_text = fh.read()
    print("\n[random] 削除・挿入を混ぜた無作為配置 (種 %d・%d 通り・帯 %d)"
          % (SEED, TRIALS, BAND))
    print("  → **穴はここだった**。交錯していなくても衝突する組がある")
    for _ in range(TRIALS):
        tseed = rng.randrange(1 << 30)
        r2 = random.Random(tseed)
        if real_text is not None:
            base = real_text
        else:
            base = rand_content(r2.choice([1, 2, 3, 4, 6, 12]),
                                r2.choice([200, 300, 400, 600]),
                                r2.choice([0.0, 0.0, 0.05, 0.15, 0.4, 1.0]),
                                r2.choice([True, False]), r2)
        crlf = "\r\n" in base
        eol = "\r\n" if crlf else "\n"
        n_lines = base.count(eol)
        if n_lines < 40:
            skipped += 1
            continue

        def pick(cnt):
            sp, kd = [], []
            for _ in range(cnt):
                k = r2.choice(["replace", "delete", "insert"])
                st = r2.randrange(2, n_lines - 5)
                wd = r2.randrange(1, 4)
                sp.append((st, st - 1) if k == "insert"
                          else (st, min(st + wd - 1, n_lines - 2)))
                kd.append(k)
            return sp, kd

        sa, ka = pick(r2.randrange(1, 4))
        sb, kb = pick(r2.randrange(1, 4))
        fa, fb = flat_lines(sa, ka), flat_lines(sb, kb)
        # 同じ枝の中で近すぎる**域**は台帳が作らないので捨てる。
        # ここを行単位で見ると、幅 2 行以上の域が自分自身と近すぎることになって
        # ほとんどの試行が捨てられる (実際に 60 通り中 52 通りが消えた)。
        if not disjoint_spans(sa, BAND) or not disjoint_spans(sb, BAND):
            skipped += 1
            continue
        verds = {n: j(base, fa, fb) for n, j in judges}
        if not any(verds.values()):
            for n in verds:
                stat[n][2] += 1
            continue
        cbase = plumb.commit(base)
        ca = plumb.commit(apply_edits(base, sa, ka, "AAA", crlf), cbase)
        cb = plumb.commit(apply_edits(base, sb, kb, "BBB", crlf), cbase)
        hit = plumb.conflicts(ca, cb)
        if hit is None:
            skipped += 1
            continue
        for n, _ in judges:
            if verds[n]:
                stat[n][0] += 1
                if hit:
                    stat[n][1] += 1
                    if len(misses[n]) < 8:
                        misses[n].append((tseed, sa, ka, sb, kb))
            else:
                stat[n][2] += 1
                if hit:
                    stat[n][3] += 1
        # 出荷中の判定が断った組のうち、B が 1 本だけのものへずらしを試す。
        if not verds["wall"] and len(sb) == 1 and all(
                abs(x - y) - 1 >= BAND for x in fa for y in fb):
            fit[0] += 1
            anc = anchors(base)
            to = anchor_fit(anc, n_lines, fa, sb[0], kb[0] == "insert",
                            BAND, n_lines)
            if to:
                fit[1] += 1
                moved = [to[0]] if kb[0] == "insert" else list(range(to[0], to[1] + 1))
                if wall_ok_lines(anc, fa, moved):
                    fit[2] += 1
                cb2 = plumb.commit(apply_edits(base, [to], kb, "BBB", crlf), cbase)
                if plumb.conflicts(ca, cb2) is False:
                    fit[3] += 1
    calls = plumb.calls
    plumb.close()
    print("  試行 %d (捨てた %d) / git 起動 %d (%.1f 回/試行)"
          % (TRIALS, skipped, calls, calls / max(1, TRIALS)))
    print("  %-6s %8s %8s %8s %8s" % ("判定", "通した", "見逃し", "断った", "断って実衝突"))
    for n, _ in judges:
        v = stat[n]
        print("  %-6s %8d %8d %8d %8d" % (n, v[0], v[1], v[2], v[3]))
        for m in misses[n]:
            # **失敗した実行の種をログへ出す** (再現は --seed で決定的)
            print("      ⚠ 見逃し 種=%d A=%s/%s B=%s/%s" % m)
    print("  断る代わりにずらす (anchor_fit): 対象 %d / ずらし先あり %d / "
          "証明が立った %d / 実 git でも綺麗 %d" % tuple(fit))
    bad_total += stat["wall"][1]


if mode in ("all", "pairs"):
    run_pairs()
if mode in ("all", "packed"):
    run_packed()
if mode in ("all", "order"):
    run_order()
if mode in ("all", "bracket"):
    run_bracket()
if mode in ("all", "random"):
    run_random()

print("\n出荷中の判定が「通してよい」と言ったのに衝突した組: %d" % bad_total)
print("0 でなければ、帯 + 壁 (錨) + 昇順の 3 つでもまだ足りていない。")
sys.exit(1 if bad_total else 0)
PY
