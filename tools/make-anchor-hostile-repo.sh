#!/usr/bin/env sh
# 錨 (`region::capture_anchor` / `region::resolve`) を**わざと誤マッチさせる**
# ためのリポジトリを作る。`tools/anyrepo-prove.sh --repo <ここ>` に食わせる。
#
# ## 何を狙っているか
#
# 二重配布の真因は「持ち主が自分の行を書き換えると自分の錨が合わなくなり、
# `region::resolve` が**似た行**へ吸い寄せられて域が移動する」ことだった
# (commit eb774f4)。誤マッチは**同じ内容の行が何度も出てくるファイル**ほど
# 起きやすいので、実在リポジトリより極端な標本をその場で作って攻める:
#
#   blank.md      空行だらけ (錨の head/tail が空文字になりやすい)
#   fences.md     ``` と --- が延々と繰り返される (Markdown の実物に近い最悪形)
#   repeat.txt    まったく同じ 1 行が何百行も続く (錨が一意に決まらない)
#   generated.rs  生成コード。8 行周期でほぼ同じ形が並ぶ
#   readme.md     箇条書きが揃った README 風 (実リポジトリで実際に踏んだ形)
#   mixed.md      上の全部を 1 ファイルに混ぜたもの
#
# ## 使い方
#
#   dir=$(tools/make-anchor-hostile-repo.sh)          # 作った場所を stdout へ
#   dir=$(tools/make-anchor-hostile-repo.sh --out /path/to/dir)
#   tools/make-anchor-hostile-repo.sh --lines 600 --commits 40
#
# ホットスポット判定は `git log` の変更回数を見るので、履歴も作る。
set -eu

out=""
lines=400
commits=30

while [ $# -gt 0 ]; do
    case "$1" in
    --out)     out=${2:-}; shift 2 ;;
    --lines)   lines=${2:-}; shift 2 ;;
    --commits) commits=${2:-}; shift 2 ;;
    -h|--help) sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "知らない引数: $1" >&2; exit 2 ;;
    esac
done

[ -n "$out" ] || out=$(mktemp -d)
mkdir -p "$out"

python3 - "$out" "$lines" "$commits" <<'PY'
import os, subprocess, sys

out, lines, commits = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])


def gen(kind, n, salt=0):
    L = []
    for i in range(n):
        if kind == "blank":
            # 3 行に 2 行が空行。錨の head/tail が空文字になりやすい
            L.append("" if i % 3 else "para %d" % (i // 3))
        elif kind == "fences":
            m = i % 6
            L.append(["```", "code line", "```", "", "---", ""][m])
        elif kind == "repeat":
            # まったく同じ行。`nearest_unique` が一意に決められない
            L.append("SAME LINE")
        elif kind == "generated":
            m = i % 8
            L.append(["// AUTO-GENERATED. DO NOT EDIT.",
                      "#[derive(Debug, Clone)]",
                      "pub struct T%d {" % (i // 8),
                      "    pub a: u32,",
                      "    pub b: u32,",
                      "}",
                      "",
                      ""][m])
        elif kind == "readme":
            m = i % 5
            L.append(["- **項目**: 説明がここに入る", "", "  - 補足", "", "---"][m])
        else:  # mixed
            m = i % 24
            if m < 4:
                L.append("" if m % 2 else "para")
            elif m < 8:
                L.append(["```", "x", "```", ""][m - 4])
            elif m < 12:
                L.append("SAME LINE")
            elif m < 18:
                L.append(["// AUTO-GENERATED. DO NOT EDIT.", "#[derive(Debug)]",
                          "pub struct S {", "    pub a: u32,", "}", ""][m - 12])
            else:
                L.append(["- **項目**: 説明", "", "  - 補足", "", "---", ""][m - 18])
    L.append("tail-%d" % salt)
    return "\n".join(L) + "\n"


FILES = {
    "blank.md": "blank", "fences.md": "fences", "repeat.txt": "repeat",
    "generated.rs": "generated", "readme.md": "readme", "mixed.md": "mixed",
}
# 「同じ形のファイルが何本もある」状況も作る (担当がばらけて重なりが増える)
for k in list(FILES):
    for j in range(1, 4):
        base, ext = k.rsplit(".", 1)
        FILES["%s_%d.%s" % (base, j, ext)] = FILES[k]


def run(*a):
    subprocess.run(a, cwd=out, check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


run("git", "init", "-q", ".")
run("git", "config", "user.email", "verify@example.invalid")
run("git", "config", "user.name", "verify")
run("git", "config", "commit.gpgsign", "false")

names = sorted(FILES)
for i in range(commits):
    for j, name in enumerate(names):
        # 毎コミット全部は触らない (git log の変更回数に差を付ける = ホットスポット)
        if i and (i + j) % 3:
            continue
        open(os.path.join(out, name), "w", encoding="utf-8").write(
            gen(FILES[name], lines, salt=i))
    run("git", "add", "-A")
    run("git", "commit", "-q", "-m", "c%d" % i)

print(out)
PY
