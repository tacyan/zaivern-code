#!/usr/bin/env sh
# 「競合ゼロ」の**ベースライン**を測るハーネス。
#
# ## なぜ別に要るのか (tools/conflict-bench.sh との違い)
#
# `tools/conflict-bench.sh` は「リースを通したら衝突が減るか」の A/B で、
# **`zai` が無いと 1 行も走らない**。だが製品の主張を支える土台は
# 「**zaivern を使わなかったら何件衝突するのか**」という数字で、
# これが今まで 1 度も測られていなかった。ここを測るのがこのハーネス。
#
#   * `zai` が 1 つも無くてもベースラインだけは必ず出る
#   * 書き手の重なり具合 (`--overlap`) を 0.0 (完全独立) 〜 1.0 (全員同じ)
#     まで振れる。**重なりゼロなら衝突もゼロ**という自明な事実まで含めて
#     数字にすることで、「衝突ゼロ」がどこから来ているのかを分離できる
#   * 「4 つの鎖」を**段**として持ち、使える段だけ実行して、
#     使えない段は **skip と理由を明記して続行**する (黙って飛ばさない)
#
# ## 段 (docs/conflict-zero.md の 4 つの鎖に対応)
#
#   baseline  ① 何もしない。N 人が重なったまま書いて直列マージする
#   guard     ② 実行中の強制。書く前にゲートを通す
#             (`zai guard` があればそれ、無ければ出荷済みの `zai hook`
#              = `crate::lease::gate`。どちらも無ければ skip)
#   union     ④ 共有面の自動解決。git のマージドライバとして働かせ、
#             ベースラインと同じ書き込みを別のドライバでマージし直す
#             (`--union-driver` で任意のドライバを指せる。既定は
#              `zai merge-driver` があればそれ。無ければ skip)
#   train     ③ 統合の順序付け。`zai train` があれば乾式検査を回し、
#             「この順序なら衝突する」を実行前に出せるかを見る。無ければ skip
#
# ## 使い方
#
#   tools/conflict-zero-bench.sh                       既定 (8 人 / 48 ファイル / 重なり 0.5)
#   tools/conflict-zero-bench.sh --writers 16
#   tools/conflict-zero-bench.sh --overlap 1.0         全員が同じファイルを書く
#   tools/conflict-zero-bench.sh --overlap 0.0         完全独立 (衝突は 0 になるはず)
#   tools/conflict-zero-bench.sh --json                JSON は stdout、表は stderr
#   tools/conflict-zero-bench.sh --keep                一時リポジトリを残す
#   tools/conflict-zero-bench.sh --union-driver 'git merge-file --union %A %O %B'
#                                                      配管の検算用の参照ドライバ
#
# 環境変数 ZAIVERN_BIN で使う zai を明示できます。
#
# ## 副作用を持たない作り
#
#   * 一時リポジトリは `mktemp -d` (= `$TMPDIR` 由来)。パスを直書きしない
#   * `HOME` を一時ディレクトリへ差し替えるので、**本物の `~/.zaivern` と
#     `~/.gitconfig` には一切触らない**
#   * **cargo を呼ばない。** 既にある `zai` を使うだけで、無ければ段を skip
#     する。ホストの `target/` は 1 バイトも触らない
#   * 後始末は trap。`--keep` を付けたときだけ残す
set -eu

# shellcheck disable=SC1007  # `CDPATH= cd` は「その cd にだけ空の CDPATH を渡す」正しい書き方
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

writers=8
files=48
overlap=0.5
seed=20260810
json=0
keep=0
union_driver=""

usage() {
    cat <<'EOS'
使い方: tools/conflict-zero-bench.sh [オプション]

  --writers <N>       同時に書く人数 (既定 8)。2 以上
  --files   <N>       リポジトリのファイル数 (既定 48)。--writers 以上
  --overlap <0..1>    書き手の重なり具合 (既定 0.5)
                        0.0 = 完全独立 (各自が自分だけのファイル群を書く)
                        0.5 = 半分は共有のホットセットへ向かう
                        1.0 = 全員が同じファイル群を書く
  --seed    <N>       担当表を決める乱数種 (既定 20260810)。同じ種なら同じ表
  --union-driver <c>  union 段で使う git マージドライバ。%O %A %B %P を展開
  --json              JSON を stdout へ、人が読む表を stderr へ
  --keep              一時リポジトリを消さずに残す
  -h, --help          この使い方

環境変数 ZAIVERN_BIN で使う zai を明示できます (無くてもベースラインは出ます)。
EOS
}

is_num() {
    case "$1" in
    '' | *[!0-9]*) return 1 ;;
    *) return 0 ;;
    esac
}

# 0.0 〜 1.0 の小数。`bc` も GNU 依存も使わない (BSD/GNU 差を持ち込まない)
is_ratio() {
    case "$1" in
    '' | *[!0-9.]* | *.*.*) return 1 ;;
    esac
    case "$1" in
    0 | 0.* | 1 | 1.0 | 1.00 | 1.000) return 0 ;;
    *) return 1 ;;
    esac
}

while [ $# -gt 0 ]; do
    case "$1" in
    --writers)
        writers=${2:-}
        shift 2 || true
        ;;
    --files)
        files=${2:-}
        shift 2 || true
        ;;
    --overlap)
        overlap=${2:-}
        shift 2 || true
        ;;
    --seed)
        seed=${2:-}
        shift 2 || true
        ;;
    --union-driver)
        union_driver=${2:-}
        shift 2 || true
        ;;
    --json)
        json=1
        shift
        ;;
    --keep)
        keep=1
        shift
        ;;
    -h | --help)
        usage
        exit 0
        ;;
    *)
        echo "不明な引数: $1" >&2
        usage >&2
        exit 2
        ;;
    esac
done

for pair in "writers $writers" "files $files" "seed $seed"; do
    name=${pair%% *}
    val=${pair#* }
    if ! is_num "$val"; then
        echo "--$name には 0 以上の整数を指定してください (受け取った値: '$val')" >&2
        exit 2
    fi
done
if ! is_ratio "$overlap"; then
    echo "--overlap には 0.0 〜 1.0 の小数を指定してください (受け取った値: '$overlap')" >&2
    exit 2
fi
if [ "$writers" -lt 2 ]; then
    echo "--writers は 2 以上にしてください (1 人では衝突が定義できません)" >&2
    exit 2
fi
if [ "$files" -lt "$writers" ]; then
    echo "--files は --writers 以上にしてください (--overlap 0.0 のとき、" >&2
    echo "各自に 1 つ以上の専有ファイルを割り当てられなくなるため)" >&2
    exit 2
fi

# ── 前提の道具 ────────────────────────────────────────────────────
# python3 が要るのは (a) 種から決まる乱数を**実装非依存**に固定するため
# (POSIX sh の算術は符号付きで桁溢れの挙動が実装依存)、(b) 書き手を
# 実際に並列で走らせるため。
for need in python3 git; do
    if ! command -v "$need" >/dev/null 2>&1; then
        echo "$need が見つかりません。この計測には $need が要ります。" >&2
        exit 1
    fi
done

# ── 使う zai を探す (無くてもよい。**絶対にビルドしない**) ────────
# 古い zai は知らないサブコマンドを「GUI 起動」として扱うので、
# **help の中身に載っている段だけ**を実行対象にする (実際に固まった前例あり)。
zai_help() {
    [ -x "$1" ] || return 1
    python3 - "$1" <<'EOS'
import subprocess, sys
try:
    p = subprocess.run([sys.argv[1], "help"], capture_output=True, text=True,
                       timeout=20, stdin=subprocess.DEVNULL)
except Exception:
    sys.exit(1)
if p.returncode != 0:
    sys.exit(1)
sys.stdout.write(p.stdout)
EOS
}

target_dir=${CARGO_TARGET_DIR:-}
[ -n "$target_dir" ] || target_dir="$root/target"

zai=""
zai_help_text=""
for cand in \
    "${ZAIVERN_BIN:-}" \
    "$target_dir/release/zai" "$target_dir/release/zai.exe" \
    "$target_dir/debug/zai" "$target_dir/debug/zai.exe" \
    "$(command -v zai 2>/dev/null || true)"; do
    [ -n "$cand" ] || continue
    if zai_help_text=$(zai_help "$cand" 2>/dev/null); then
        zai=$cand
        break
    fi
done

has_sub() {
    [ -n "$zai" ] || return 1
    printf '%s\n' "$zai_help_text" | grep -q "zai $1" || return 1
}

# 段②の機構を決める。**新しい名前を優先し、無ければ出荷済みの経路へ落ちる。**
guard_mech=none
if has_sub guard; then
    guard_mech=guard
elif has_sub hook && has_sub lease; then
    guard_mech=hook
fi

# 段④。`--union-driver` が明示されていればそれが最優先 (配管の検算に使う)。
# 実装は `zai merge-driver`。`%L` (marker size) まで渡すのが git の規約。
if [ -z "$union_driver" ] && has_sub merge-driver; then
    union_driver="$zai merge-driver %O %A %B %L %P"
fi

# ── 使い捨ての作業場 ──────────────────────────────────────────────
work=$(mktemp -d 2>/dev/null || mktemp -d -t czbench)
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

# **本物の ~/.zaivern と ~/.gitconfig に触らせない。**
mkdir -p "$work/home"
HOME="$work/home"
USERPROFILE="$work/home" # Windows 側の同等物。片側だけ書かない
export HOME USERPROFILE
export GIT_CONFIG_NOSYSTEM=1
export GIT_TERMINAL_PROMPT=0
export GIT_AUTHOR_NAME=conflict-zero-bench
export GIT_AUTHOR_EMAIL=conflict-zero-bench@example.invalid
export GIT_COMMITTER_NAME=conflict-zero-bench
export GIT_COMMITTER_EMAIL=conflict-zero-bench@example.invalid

cat >"$work/czbench.py" <<'CONFLICT_ZERO_PY'
#!/usr/bin/env python3
"""conflict-zero-bench の計測エンジン。tools/conflict-zero-bench.sh から呼ばれる。"""

import json
import os
import subprocess
import sys
import threading
import time
import unicodedata

# ═════════════════════════════════════════════════════════════════════
#  合成リポジトリの形 (計測の前提。docs/conflict-zero.md と対にすること)
# ═════════════════════════════════════════════════════════════════════

BLOCKS = 6      # 1 ファイルあたりのブロック数 (= 独立に直せる「関数」)
STRIDE = 8      # 1 ブロックの行数。編集行どうしがこの距離だけ離れる
VALUE_OFF = 2   # ブロック先頭から数えた「書き換えてよい行」
HEADER = 2      # ファイル先頭のコメント行数

CONF_OURS, CONF_BASE, CONF_THEIRS = 1, 2, 3
SCALE = 1000000  # --overlap を整数比較に落とすための分母


class Rng:
    """再現できる乱数 (64bit LCG)。

    `random` を使わないのは、実装が変わると同じ種でも別の担当表が出て
    「同じ作業を全段へ流した」という前提が壊れるため。
    """

    M = (1 << 64) - 1
    A = 6364136223846793005
    C = 1442695040888963407

    def __init__(self, seed):
        self.s = (seed * self.A + self.C) & self.M

    def next(self):
        self.s = (self.s * self.A + self.C) & self.M
        return (self.s >> 33) & 0xFFFFFFFF

    def below(self, n):
        return self.next() % n if n > 0 else 0


def rel_path(i):
    return "src/mod_%04d.rs" % i


def file_text(i):
    lines = [
        "// %s — conflict-zero-bench が生成した合成ファイル" % rel_path(i),
        "// 各書き手は 1 ブロックの `let value` 行だけを書き換える。",
    ]
    for b in range(BLOCKS):
        lines += [
            "fn block_%02d() {" % b,
            "    let head = 0;",
            "    let value = 0;",
            "    let tail = 0;",
            "}",
            "",
            "",
            "",
        ]
    return "\n".join(lines) + "\n"


def make_plan(rng, writers, files, overlap):
    """(書き手 → [(ファイル番号, ブロック番号)]) の担当表を作る。

    * 各書き手は `per = files // writers` 件を書く
    * 専有スライス   writer i は [i*per, (i+1)*per) — **互いに素**
    * 共有ホットセット [0, per) — 全員が向かう先
    * 1 件ごとに確率 `overlap` でホットセットから、残りは専有から選ぶ

    したがって `--overlap 0.0` は**構造的に衝突し得ない**担当表になり、
    `--overlap 1.0` は全員が同じ per 件を奪い合う。この 2 つの端が
    「衝突ゼロ」の下限と上限を挟む。
    """
    per = max(1, files // writers)
    thr = int(round(overlap * SCALE))
    plan = []
    for i in range(writers):
        picks = []
        for k in range(per):
            if rng.below(SCALE) < thr:
                f = rng.below(per)               # 共有ホットセット
            else:
                f = (i * per + k) % files        # 専有スライス
            picks.append((f, rng.below(BLOCKS)))
        plan.append(picks)
    return plan


def duplicates(sets):
    """「2 人以上が実際に書いたファイル」の数と、重複した書き込みの数。

    **モデルに依存しない一次データ。**ここが 0 なら衝突は起こり得ない。
    """
    count = {}
    for s in sets:
        for f in s:
            count[f] = count.get(f, 0) + 1
    dup_files = sum(1 for v in count.values() if v > 1)
    dup_writes = sum(v - 1 for v in count.values() if v > 1)
    return dup_files, dup_writes


# ═════════════════════════════════════════════════════════════════════
#  git (使い捨ての一時リポジトリの中でだけ動く)
# ═════════════════════════════════════════════════════════════════════

ENV = None


def git(args, cwd, check=True):
    p = subprocess.run(["git"] + args, cwd=cwd, env=ENV, capture_output=True, text=True)
    if check and p.returncode != 0:
        raise RuntimeError(
            "git %s が失敗しました (%d)\n%s%s"
            % (" ".join(args), p.returncode, p.stdout, p.stderr)
        )
    return p


def setup_repo(repo, files):
    os.makedirs(os.path.join(repo, "src"), exist_ok=True)
    git(["init", "-q", "."], repo)
    # `git init -b` は git 2.28+。古い git でも動くようこちらで既定枝を決める。
    git(["symbolic-ref", "HEAD", "refs/heads/main"], repo)
    for i in range(files):
        with open(os.path.join(repo, rel_path(i)), "w", encoding="utf-8") as fh:
            fh.write(file_text(i))
    git(["add", "-A"], repo)
    git(["commit", "-q", "-m", "初期状態"], repo)
    return git(["rev-parse", "HEAD"], repo).stdout.strip()


def apply_edit(path, block, tag):
    with open(path, "r", encoding="utf-8") as fh:
        lines = fh.read().split("\n")
    lines[HEADER + block * STRIDE + VALUE_OFF] = "    let value = %s;" % tag
    with open(path, "w", encoding="utf-8") as fh:
        fh.write("\n".join(lines))


# ═════════════════════════════════════════════════════════════════════
#  疑似的な書き手 (LLM は 1 度も呼ばない。**再現性が命**)
# ═════════════════════════════════════════════════════════════════════


def hook_payload(session, cwd, path):
    return json.dumps(
        {
            "session_id": session,
            "cwd": cwd,
            "hook_event_name": "PreToolUse",
            "tool_name": "Edit",
            "tool_input": {"file_path": path},
        }
    )


class Writer(threading.Thread):
    """1 人ぶんの書き手。段が違っても**編集の適用コードは同一**。

    違うのは「書く前にゲートを通すかどうか」の 1 点だけ。
    """

    def __init__(self, stage, idx, wt, picks, gate_cmd):
        super().__init__(daemon=True)
        self.stage, self.idx, self.wt, self.picks = stage, idx, wt, picks
        self.gate_cmd = gate_cmd  # None ならゲート無し
        self.session = "czbench-%s-%d" % (stage, idx)
        self.tag = "%d /* writer-%d */" % (idx + 1, idx + 1)
        self.written = set()
        self.applied = 0
        self.denied = 0
        self.gate_ms = []
        self.error = None

    def ask_gate(self, abspath):
        payload = hook_payload(self.session, self.wt, abspath)
        t0 = time.perf_counter()
        p = subprocess.run(
            self.gate_cmd, input=payload, env=ENV, capture_output=True, text=True
        )
        dt = (time.perf_counter() - t0) * 1000.0
        blocked = "permissionDecision" in p.stdout and "deny" in p.stdout
        return blocked, dt

    def run(self):
        try:
            for f, b in self.picks:
                ap = os.path.join(self.wt, rel_path(f))
                if self.gate_cmd:
                    blocked, dt = self.ask_gate(ap)
                    self.gate_ms.append(dt)
                    if blocked:
                        self.denied += 1
                        continue  # **諦めて次の担当へ移る** = 実運用と同じ挙動
                apply_edit(ap, b, self.tag)
                self.written.add(rel_path(f))
                self.applied += 1
        except Exception as e:  # スレッドの例外を握り潰さない
            self.error = e


# ═════════════════════════════════════════════════════════════════════
#  マージと衝突の計数
# ═════════════════════════════════════════════════════════════════════


def resolve_union(path):
    """衝突マーカを外して**両側を残す**。返りは (ハンク数, 衝突行数)。

    片側を捨てると先行した書き手の成果が消えて、後続のマージが不自然に
    綺麗になる。両側を残すのが人手の解決にいちばん近い。
    """
    with open(path, "r", encoding="utf-8", errors="replace") as fh:
        src = fh.read().split("\n")
    out, hunks, clines, mode = [], 0, 0, 0
    for line in src:
        if line.startswith("<<<<<<< "):
            hunks += 1
            mode = CONF_OURS
            continue
        if mode and line.startswith("||||||| "):
            mode = CONF_BASE
            continue
        if mode and line == "=======":
            mode = CONF_THEIRS
            continue
        if mode and line.startswith(">>>>>>> "):
            mode = 0
            continue
        if mode == CONF_BASE:
            continue
        if mode:
            clines += 1
        out.append(line)
    with open(path, "w", encoding="utf-8") as fh:
        fh.write("\n".join(out))
    return hunks, clines


UNION_ATTR = "* merge=czunion\n"


def union_config(driver):
    """`--union-driver` を git のマージドライバ設定へ落とす。

    git の契約は固定 (`%O` 共通祖先 / `%A` こちら側 = 結果の書き込み先 /
    `%B` あちら側 / `%P` 元のパス、終了コード 0 = 綺麗に解決)。**この形は
    git が決めているので、`zai union` がまだ無くても正しく書ける。**
    参照ドライバ (`git merge-file --union %A %O %B`) で配管だけ検算できる。
    """
    return [
        "-c",
        "merge.czunion.name=conflict-zero-bench union driver",
        "-c",
        "merge.czunion.driver=" + driver,
    ]


def merge_all(intdir, branches, driver=None):
    st = {
        "merges": len(branches),
        "conflict_merges": 0,
        "conflict_files": 0,
        "conflict_files_unique": [],
        "hunks": 0,
        "lines": 0,
    }
    uniq = set()
    pre = union_config(driver) if driver else []
    for b in branches:
        p = git(
            pre + ["-c", "merge.conflictstyle=merge", "merge", "--no-edit", "-q", b],
            intdir,
            check=False,
        )
        if p.returncode == 0:
            continue
        files = git(["diff", "--name-only", "--diff-filter=U"], intdir).stdout.split()
        if not files:
            raise RuntimeError(
                "マージが衝突以外の理由で失敗しました: %s%s" % (p.stdout, p.stderr)
            )
        st["conflict_merges"] += 1
        st["conflict_files"] += len(files)
        for f in files:
            uniq.add(f)
            h, c = resolve_union(os.path.join(intdir, f))
            st["hunks"] += h
            st["lines"] += c
        git(["add", "-A"], intdir)
        git(["commit", "-q", "--no-edit"], intdir)
    st["conflict_files_unique"] = sorted(uniq)
    return st


def attrs_path(intdir):
    return git(["rev-parse", "--git-path", "info/attributes"], intdir).stdout.strip()


# ═════════════════════════════════════════════════════════════════════
#  段
# ═════════════════════════════════════════════════════════════════════


def run_stage(name, repo, work, base, plan, gate_cmd=None, driver=None):
    t_all = time.perf_counter()
    branches, dirs = [], []
    for i in range(len(plan)):
        br = "%s-w-%d" % (name, i + 1)
        wt = os.path.join(work, name, "w-%d" % (i + 1))
        git(["worktree", "add", "-q", "-b", br, wt, base], repo)
        branches.append(br)
        dirs.append(wt)

    ws = [Writer(name, i, dirs[i], plan[i], gate_cmd) for i in range(len(plan))]
    t0 = time.perf_counter()
    for w in ws:
        w.start()
    for w in ws:
        w.join()
    for w in ws:
        if w.error:
            raise w.error
    t_edit = (time.perf_counter() - t0) * 1000.0

    for i, wt in enumerate(dirs):
        if git(["status", "--porcelain"], wt).stdout.strip():
            git(["add", "-A"], wt)
            git(["commit", "-q", "-m", "writer-%d の作業" % (i + 1)], wt)

    intdir = os.path.join(work, name, "integration")
    git(["worktree", "add", "-q", "-b", "%s-integration" % name, intdir, base], repo)

    ap = None
    if driver:
        ap = attrs_path(intdir)
        if not os.path.isabs(ap):
            ap = os.path.join(intdir, ap)
        os.makedirs(os.path.dirname(ap), exist_ok=True)
        with open(ap, "w", encoding="utf-8") as fh:
            fh.write(UNION_ATTR)
    try:
        t0 = time.perf_counter()
        st = merge_all(intdir, branches, driver)
        t_merge = (time.perf_counter() - t0) * 1000.0
    finally:
        # **段をまたいで漏らさない。**属性は共有ディレクトリに置かれる。
        if ap and os.path.exists(ap):
            os.remove(ap)

    dup_files, dup_writes = duplicates([w.written for w in ws])
    gate_ms = [x for w in ws for x in w.gate_ms]
    return dict(
        stage=name,
        status="ok",
        planned=sum(len(p) for p in plan),
        applied=sum(w.applied for w in ws),
        denied=sum(w.denied for w in ws),
        dup_files=dup_files,
        dup_writes=dup_writes,
        gate_calls=len(gate_ms),
        gate_p50=pct(gate_ms, 0.50),
        gate_p95=pct(gate_ms, 0.95),
        gate_max=max(gate_ms) if gate_ms else 0.0,
        wall_edit_ms=t_edit,
        wall_merge_ms=t_merge,
        wall_total_ms=(time.perf_counter() - t_all) * 1000.0,
        **st
    )


def pct(xs, q):
    if not xs:
        return 0.0
    ys = sorted(xs)
    i = min(len(ys) - 1, max(0, int(round(q * (len(ys) - 1)))))
    return ys[i]


def count_gate_log(needle):
    """`zai` の診断ログ (`gate.log`) を差し替えた HOME の下から探して数える。

    置き場所を直書きしない。この計測では HOME 自体が使い捨てなので安い。
    """
    n = 0
    home = os.path.expanduser("~")
    for dirpath, _dirs, names in os.walk(home):
        if "gate.log" not in names:
            continue
        try:
            with open(
                os.path.join(dirpath, "gate.log"), encoding="utf-8", errors="replace"
            ) as fh:
                for line in fh:
                    if needle in line:
                        n += 1
        except OSError:
            pass
    return n


# ═════════════════════════════════════════════════════════════════════
#  表示
# ═════════════════════════════════════════════════════════════════════


def dw(s):
    return sum(2 if unicodedata.east_asian_width(c) in "WF" else 1 for c in s)


def pad(s, n):
    return s + " " * max(0, n - dw(s))


def table(headers, rows):
    cols = len(headers)
    w = [dw(h) for h in headers]
    for r in rows:
        for i in range(cols):
            w[i] = max(w[i], dw(r[i]))
    def line(l, m, r):
        return l + m.join("─" * (x + 2) for x in w) + r
    out = [line("┌", "┬", "┐"), "│ " + " │ ".join(pad(headers[i], w[i]) for i in range(cols)) + " │",
           line("├", "┼", "┤")]
    for r in rows:
        out.append("│ " + " │ ".join(pad(r[i], w[i]) for i in range(cols)) + " │")
    out.append(line("└", "┴", "┘"))
    return "\n".join(out)


def ms(x):
    return "%.0f ms" % x if x < 1000 else "%.1f 秒" % (x / 1000.0)


def render(cfg, stages, notes):
    ok = [s for s in stages if s["status"] == "ok"]
    head = ["指標"] + [s["stage"] for s in ok]
    rows = [
        ["計画した書き込み"] + ["%d 件" % s["planned"] for s in ok],
        ["実際に書けた書き込み"] + ["%d 件" % s["applied"] for s in ok],
        ["ゲートが止めた回数"] + [("—" if not s["gate_calls"] else "%d 件" % s["denied"]) for s in ok],
        ["2 人以上が書いたファイル"] + ["%d 件" % s["dup_files"] for s in ok],
        ["衝突したマージ"] + ["%d / %d 回" % (s["conflict_merges"], s["merges"]) for s in ok],
        ["衝突ファイル (実数)"] + ["%d 件" % len(s["conflict_files_unique"]) for s in ok],
        ["衝突ハンク"] + ["%d 個" % s["hunks"] for s in ok],
        ["衝突ハンクの行数合計"] + ["%d 行" % s["lines"] for s in ok],
        ["壁時計: 編集"] + [ms(s["wall_edit_ms"]) for s in ok],
        ["壁時計: マージ〜解消"] + [ms(s["wall_merge_ms"]) for s in ok],
        ["壁時計: 段の全体"] + [ms(s["wall_total_ms"]) for s in ok],
    ]
    body = [
        "",
        "== 計測条件",
        "   書き手 %d 人 / ファイル %d 個 / 重なり %s / 種 %d"
        % (cfg["writers"], cfg["files"], cfg["overlap"], cfg["seed"]),
        "   1 ファイル %d ブロック (同じファイルでも別ブロックなら git は自動マージする)"
        % BLOCKS,
        "   zai: %s" % (cfg["zai"] or "見つかりません (ベースラインのみ実行します)"),
        "   一時リポジトリ: %s" % cfg["work"],
        "",
        "== 段ごとの実測",
        table(head, rows),
        "",
        "== 段の状態 (**skip は必ず理由とともに出す**)",
    ]
    for s in stages:
        if s["status"] == "ok":
            body.append("   %-9s 実行  %s" % (s["stage"], s.get("mech", "")))
        else:
            body.append("   %-9s skip  %s" % (s["stage"], s["reason"]))
    body += [""] + notes
    return "\n".join(body)


# ═════════════════════════════════════════════════════════════════════


def main(argv):
    global ENV
    ENV = os.environ.copy()
    ENV["GIT_TERMINAL_PROMPT"] = "0"

    opt = {}
    it = iter(argv)
    for a in it:
        if a.startswith("--"):
            opt[a[2:]] = next(it)
    cfg = {
        "writers": int(opt["writers"]),
        "files": int(opt["files"]),
        "overlap": float(opt["overlap"]),
        "seed": int(opt["seed"]),
        "zai": opt.get("zai") or "",
        "guard_mech": opt.get("guard-mech", "none"),
        "union_driver": opt.get("union-driver", ""),
        "work": opt["work"],
        "json": opt.get("json", "0") == "1",
    }
    work = cfg["work"]
    repo = os.path.join(work, "repo")
    os.makedirs(repo, exist_ok=True)

    # **全ての段へ 1 バイト同じ担当表を流す。**
    plan = make_plan(Rng(cfg["seed"]), cfg["writers"], cfg["files"], cfg["overlap"])
    base = setup_repo(repo, cfg["files"])

    stages = []
    notes = []

    # ── 段① baseline — 何もしない。**これがベースライン**
    b = run_stage("baseline", repo, work, base, plan)
    b["mech"] = "ガード無し (git worktree で分けただけ)"
    stages.append(b)

    # ── 段② guard — 書く前にゲートを通す
    if cfg["guard_mech"] == "none":
        stages.append(
            {
                "stage": "guard",
                "status": "skipped",
                "reason": "zai guard も zai hook も見つかりません"
                + ("" if cfg["zai"] else " (zai 自体が未検出)"),
            }
        )
    else:
        if cfg["guard_mech"] == "guard":
            gate_cmd = [cfg["zai"], "guard", "check", "--stdin"]
            mech = "zai guard"
        else:
            r = subprocess.run(
                [cfg["zai"], "lease", "enable", "--dir", repo],
                env=ENV,
                capture_output=True,
                text=True,
            )
            if r.returncode != 0:
                raise RuntimeError(
                    "zai lease enable が失敗しました: %s%s" % (r.stdout, r.stderr)
                )
            gate_cmd = [cfg["zai"], "hook", "--zaivern", "claude", "PreToolUse"]
            mech = "zai hook (crate::lease::gate)"
        g = run_stage("guard", repo, work, base, plan, gate_cmd=gate_cmd)
        g["mech"] = mech
        g["fail_open"] = count_gate_log(" fail-open ")
        g["busy_deny"] = count_gate_log("busy-deny")
        stages.append(g)
        notes += [
            "== ゲートの内訳 (%s)" % mech,
            "   呼び出し %d 回 / p50 %.1f ms / p95 %.1f ms / max %.1f ms"
            % (g["gate_calls"], g["gate_p50"], g["gate_p95"], g["gate_max"]),
            "   判定せずに通した回数 (fail-open): %d" % g["fail_open"],
            "   混雑で止めた回数 (busy-deny, 再試行すれば通る): %d" % g["busy_deny"],
            "",
        ]

    # ── 段③ train — **未実装。推測で呼ばない**
    stages.append(
        {
            "stage": "train",
            "status": "skipped",
            "reason": "統合の順序付け (train) は未実装。契約が決まっていないものを推測で呼ぶコードは書きません",
        }
    )

    # ── 段④ union — 同じ書き込みを別のマージドライバで解決し直す
    if not cfg["union_driver"]:
        stages.append(
            {
                "stage": "union",
                "status": "skipped",
                "reason": "zai merge-driver が見つかりません (--union-driver で任意のドライバを指せます)",
            }
        )
    else:
        u = run_stage("union", repo, work, base, plan, driver=cfg["union_driver"])
        u["mech"] = "マージドライバ: %s" % cfg["union_driver"]
        stages.append(u)
        notes += [
            "== union 段の読み方 (**ここを誇張しない**)",
            "   ドライバは baseline と同じ書き込みを**マージ時に**吸収するだけで、",
            "   2 人以上が書いたファイルは %d 件のまま残っています。" % u["dup_files"],
            "   テキストとして綺麗にマージできたことと、結果がコンパイルできることは別です。",
            "",
        ]

    text = render(cfg, stages, notes)
    sink = sys.stderr if cfg["json"] else sys.stdout
    print(text, file=sink)

    if cfg["json"]:
        json.dump(
            {"config": {k: v for k, v in cfg.items() if k != "json"}, "stages": stages},
            sys.stdout,
            ensure_ascii=False,
            indent=2,
        )
        print()

    # ── 判定。**緑の嘘を作らない**
    # 合否を問えるのは **guard 段だけ**。guard は「起こさせない」と主張して
    # いるので、2 人が同じファイルを書いた時点で主張が破れている。
    # union 段は起こった衝突をマージ時に吸収する仕組みなので、
    # `dup_files > 0` は**設計どおり**であって失敗ではない。
    bad = []
    for s in stages:
        if s["status"] != "ok" or s["stage"] != "guard":
            continue
        if s["dup_files"] or s["hunks"]:
            bad.append(
                "guard 段で衝突が残りました (2 人以上が書いたファイル %d / ハンク %d)"
                % (s["dup_files"], s["hunks"])
            )
    if bad:
        print("\n".join("❌ " + x for x in bad), file=sys.stderr)
        return 1
    if b["hunks"] == 0 and cfg["overlap"] >= 0.5:
        print(
            "⚠ ベースラインの衝突が 0 件でした。この規模では衝突が起きないので、"
            "\n   比較としては弱い数字です (--writers か --files を増やすか、--overlap を上げてください)",
            file=sys.stderr,
        )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
CONFLICT_ZERO_PY

python3 "$work/czbench.py" \
    --writers "$writers" \
    --files "$files" \
    --overlap "$overlap" \
    --seed "$seed" \
    --zai "$zai" \
    --guard-mech "$guard_mech" \
    --union-driver "$union_driver" \
    --work "$work" \
    --json "$json"
