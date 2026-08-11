#!/usr/bin/env sh
# 並列エージェントの「衝突ゼロ」を **A/B の実測**で証明するハーネス。
#
# ## なぜ要るか
#
# 「衝突が減ります」は印象であって主張ではない。競合 (orca / swarm 系) が
# 揃って出している答えは「git worktree でファイルシステムを分ける」だけで、
# これは**同じファイルを 2 つの worktree で編集する 2 人**には 1 ミリも効かない。
# 効かないことを示すには、同じ作業量を 2 通りの条件で流して差を数字にするしかない。
#
#   A 群 (ガード無し)  N 個の worktree で、わざと重なるファイル集合を編集 →
#                      各ブランチでコミット → 直列マージ。競合他社と同じ条件。
#   B 群 (ガード有り)  **同じ担当表**を流すが、各書き込みの前に `zai hook`
#                      (= `crate::lease::gate`) を通す。拒否されたエージェントは
#                      そのファイルを諦めて次の担当へ移る (実運用と同じ挙動)。
#
# 担当表は `--seed` から決まるので、**両群へ流れる作業内容は 1 バイト同じ**。
# 差が作業内容の違いから出たら計測になっていない。
#
# ## 使い方
#
#   tools/conflict-bench.sh                        既定 (エージェント 4 体 / ファイル 12 個)
#   tools/conflict-bench.sh --agents 8 --files 40  規模を変える
#   tools/conflict-bench.sh --seed 12345           別の担当表で再現する
#   tools/conflict-bench.sh --hunk-cost 90         モデルの仮定を変える (下記)
#   tools/conflict-bench.sh --json                 機械可読 (JSON は stdout、表は stderr)
#   tools/conflict-bench.sh --keep                 一時リポジトリを消さずに残す
#
# 環境変数:
#   ZAIVERN_BIN   使う zai を明示する (既定は CARGO_TARGET_DIR / cargo metadata から探す)
#
# ## 何が実測で、何が仮定か (ここを混ぜない)
#
#   実測: 衝突ファイル数 / 衝突ハンク数 / 衝突行数 / 各フェーズの壁時計 /
#         ゲートが止めた回数 / ゲート 1 回の所要時間 (p50 / p95 / max)
#   仮定: `--hunk-cost` (1 ハンクの解消に人 or AI が何秒かかるか)。**測っていない**。
#         「45 分の作業で待ち 18 分 → 4 分」のような文にするには実測ハンク数へ
#         この係数を掛けるしかないので、係数は引数として外に出してある。
#
# ## 副作用を持たない作り
#
#   * 一時リポジトリは `mktemp -d` (= `$TMPDIR` 由来)。パスを直書きしない。
#   * `HOME` を一時ディレクトリへ差し替えるので、**本物の `~/.zaivern` と
#     `~/.gitconfig` には一切触らない**。`zai` の台帳もその中にできる。
#   * 後始末は trap。`--keep` を付けたときだけ残す。
#   * ホストの `target/` は汚さない (既にある `zai` を再利用するだけ)。
set -eu

# プロジェクトのルート (このスクリプトの 1 つ上)。パスを直書きしない。
# shellcheck disable=SC1007  # `CDPATH= cd` は「その cd にだけ空の CDPATH を渡す」正しい書き方
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

agents=4
files=12
seed=20260810
hunk_cost=45
json=0
keep=0

usage() {
    cat <<'EOS'
使い方: tools/conflict-bench.sh [オプション]

  --agents <N>       疑似エージェントの数 (既定 4)
  --files  <N>       リポジトリのファイル数 (既定 12)
  --seed   <N>       担当表を決める乱数種 (既定 20260810)。同じ種なら同じ表
  --hunk-cost <秒>   衝突 1 ハンクの解消コスト (既定 45)。**測っていない仮定**
  --json             JSON を stdout へ、人が読む表を stderr へ
  --keep             一時リポジトリを消さずに残す
  -h, --help         この使い方

環境変数 ZAIVERN_BIN で使う zai を明示できます。
EOS
}

is_num() {
    case "$1" in
    '' | *[!0-9]*) return 1 ;;
    *) return 0 ;;
    esac
}

while [ $# -gt 0 ]; do
    case "$1" in
    --agents)
        agents=${2:-}
        shift 2 || true
        ;;
    --files)
        files=${2:-}
        shift 2 || true
        ;;
    --seed)
        seed=${2:-}
        shift 2 || true
        ;;
    --hunk-cost)
        hunk_cost=${2:-}
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

for pair in "agents $agents" "files $files" "seed $seed" "hunk-cost $hunk_cost"; do
    name=${pair%% *}
    val=${pair#* }
    if ! is_num "$val"; then
        echo "--$name には 0 以上の整数を指定してください (受け取った値: '$val')" >&2
        exit 2
    fi
done
if [ "$agents" -lt 2 ] || [ "$files" -lt 2 ]; then
    echo "--agents と --files は 2 以上にしてください (1 体では衝突が定義できません)" >&2
    exit 2
fi

# ── 前提の道具 ────────────────────────────────────────────────────
# python3 が要るのは、測りたいのが「ゲート 1 回あたり数十 ms」だから。
# macOS の `date` に `%N` が無く、`date` の起動そのものが数 ms 乗るので
# sh だけでは分解能が足りない。ここは `time.perf_counter()` で測る。
for need in python3 git; do
    if ! command -v "$need" >/dev/null 2>&1; then
        echo "$need が見つかりません。この計測には $need が要ります。" >&2
        exit 1
    fi
done

# ── 使う zai を決める ────────────────────────────────────────────
# **バージョンではなく能力で選ぶ。** 古い zai は `lease` サブコマンドを
# 知らず、`zai lease …` を「不明なサブコマンド → GUI 起動」として扱うので、
# 素朴にパスだけ見て選ぶと GUI が立ち上がって計測が固まる (実際に踏んだ)。
# 判定は `zai help` の中身で行い、タイムアウトも付ける。
zai_capable() {
    [ -x "$1" ] || return 1
    python3 - "$1" <<'EOS'
import subprocess, sys
try:
    p = subprocess.run([sys.argv[1], "help"], capture_output=True, text=True,
                       timeout=20, stdin=subprocess.DEVNULL)
except Exception:
    sys.exit(1)
sys.exit(0 if "zai lease" in p.stdout else 1)
EOS
}

target_dir=${CARGO_TARGET_DIR:-}
if [ -z "$target_dir" ]; then
    target_dir=$( (cd "$root" && cargo metadata --format-version 1 --no-deps 2>/dev/null) |
        python3 -c 'import json,sys
try: print(json.load(sys.stdin)["target_directory"])
except Exception: pass' 2>/dev/null) || target_dir=""
fi
[ -n "$target_dir" ] || target_dir="$root/target"

# ── 使う zai を決める (**全ベンチで 1 バイトも変えない共通ブロック**) ──
#
# ## なぜ「共通」でないと嘘になるか (事故3)
#
# 探索順が `conflict-zero-bench.sh` は release→debug、`coedit-bench.sh` は
# debug→release で**逆だった**。この環境は release が 0.12.0・debug が
# 0.14.0 だったので、**同一セッションで別のバイナリを測って**その数字を
# 並べていた。順序は **release → debug → PATH** で統一する。
#
# ## なぜ版の照合だけでは足りないか (事故1)
#
# `cargo test` も `cargo test --bin zai --no-run` も **bin を作らない**ので、
# `target/<profile>/zai` は前の実行の残骸のまま残る。実際に
# 「ソースは 06:00 なのにバイナリは 02:40 のビルド」で `guard` の実フック
# 試験が赤くなり、**`--version` は両方 0.14.0 だった**。版が同じでも中身は
# 別物なので、**ソースより古いバイナリは使わない**。
#
# 内容ハッシュ (ビルド ID) の方が確実だが、実測で mtime 0.51ms に対し
# ハッシュ 62.9ms (124 倍) で、しかも `build.rs` と `cli.rs` の両方を
# 触らないと実現できない。詳細は `docs/bench-honesty.md`。
#
# 前提: `$root` (リポジトリルート) と `$target_dir` が決まっていること。
# 使い方: `zai_pick <追加判定の関数名>` → 見つかれば `$zai` に入る。
# @zai-honesty-begin
expect_ver=$(awk -F'"' '/^version[ ]*=/ { print $2; exit }' "$root/Cargo.toml" 2>/dev/null || true)
zai_ver=""
zai_note=""
# shellcheck disable=SC2329  # 共通ブロック外／間接 (`"$1"`) から呼ぶ
zai_reject() { zai_note="${zai_note}${zai_note:+ / }$1"; }

# `$1` より新しいソースの一覧 (空なら `$1` の方が新しい = 使ってよい)。
#
# **mtime を数値で取らない。** GNU は `stat -c %Y`、BSD は `stat -f %m` と
# 引数が違うため、どちらかを直書きすると必ず片方の OS で壊れる。
# `find -newer` は POSIX にあるのでどちらでも動く。
# shellcheck disable=SC2329  # 共通ブロック外／間接 (`"$1"`) から呼ぶ
newer_sources() {
    [ -e "$1" ] || return 0
    find "$root/src" -name '*.rs' -newer "$1" -print 2>/dev/null || true
    find "$root/Cargo.toml" "$root/build.rs" -newer "$1" -print 2>/dev/null || true
}

# 候補 `$1` が「版が合っていて、ソースより新しい」なら 0 で `$zai_ver` を更新。
# 駄目なら**理由を積んでから** 1 を返す (黙って次の候補へ行かない)。
# shellcheck disable=SC2329  # 共通ブロック外／間接 (`"$1"`) から呼ぶ
zai_fresh() {
    # **無い候補は黙って飛ばす。** unix で `.exe` を、release を建てていない
    # 環境で release を、毎回「実行できません」と報告すると、本当に見てほしい
    # 「有るのに使えない」理由が雑音に埋もれる。
    [ -e "$1" ] || return 1
    [ -x "$1" ] || {
        zai_reject "$1: 実行権がありません"
        return 1
    }
    _v=$("$1" --version 2>/dev/null) || {
        zai_reject "$1: --version が動きません"
        return 1
    }
    # ZAIVERN_BIN は**利用者の明示**。照合は飛ばすが、飛ばしたことは書き残す。
    if [ -n "${ZAIVERN_BIN:-}" ] && [ "$1" = "$ZAIVERN_BIN" ]; then
        zai_reject "ZAIVERN_BIN で明示 (版と古さの照合は飛ばしました)"
        zai_ver=$_v
        return 0
    fi
    if [ -n "$expect_ver" ]; then
        case "$_v" in
        *"$expect_ver"*) ;;
        *)
            zai_reject "$1 は版が違うので使いません ($_v != $expect_ver)"
            return 1
            ;;
        esac
    fi
    _n=$(newer_sources "$1")
    if [ -n "$_n" ]; then
        # 何件が新しいのか・どれが新しいのかまで出す (出さないと直せない)。
        # `$?` を見ないので、ここのパイプは終了コードを壊さない。
        _cnt=$(printf '%s\n' "$_n" | grep -c . || true)
        _one=$(printf '%s\n' "$_n" | sed -n '1p')
        # **`${...}` で必ず囲む。** 変数の直後に日本語を置くと、macOS の
        # /bin/sh は多バイト文字の先頭バイトを変数名に取り込んでしまい
        # (`_one\xe3: unbound variable`)、`set -u` でその場で落ちる。
        # しかも落ちるのは**古いバイナリを弾く経路だけ**なので、
        # 「普段は動くのに、いちばん大事なときに死ぬ」形になる (実際に踏んだ)。
        zai_reject "$1 はソースより古いので使いません (${_cnt} 件が新しい。例: ${_one}。\`cargo build --bin zai\` を先に走らせること)"
        return 1
    fi
    zai_ver=$_v
    return 0
}

# 候補 `$2` を、鮮度 → 計測ごとの適格判定 (`$1` の関数) の順で見る。
# shellcheck disable=SC2329  # 共通ブロック外／間接 (`"$1"`) から呼ぶ
zai_try() {
    zai_fresh "$2" || return 1
    "$1" "$2" || {
        zai_reject "$2: この計測に要る機能がありません"
        return 1
    }
    zai=$2
    return 0
}

# 探索順 (**全ベンチ共通**): ZAIVERN_BIN → release → debug → PATH。
# `for` の各語を引用しているので、**空白を含むパスでも壊れない**。
# shellcheck disable=SC2329  # 共通ブロック外／間接 (`"$1"`) から呼ぶ
zai_pick() {
    zai=""
    if [ -n "${ZAIVERN_BIN:-}" ]; then
        zai_try "$1" "$ZAIVERN_BIN"
        return $?
    fi
    for _c in \
        "$target_dir/release/zai" "$target_dir/release/zai.exe" \
        "$target_dir/debug/zai" "$target_dir/debug/zai.exe" \
        "$(command -v zai 2>/dev/null || true)"; do
        [ -n "$_c" ] || continue
        zai_try "$1" "$_c" && return 0
    done
    return 1
}

# **使ったバイナリを絶対パスと版で必ず出す。** 出さない計測は再現できない。
# shellcheck disable=SC2329  # 各ベンチの出力部から呼ぶ (静的には見えない)
zai_identity() {
    if [ -n "${1:-}" ]; then
        case "$1" in
        /*) _p=$1 ;;
        *) _p="$PWD/$1" ;;
        esac
        printf 'zai: %s (%s)\n' "$_p" "${zai_ver:-版不明}"
    else
        printf 'zai: 見つかりません\n'
    fi
    [ -n "$zai_note" ] && printf 'zai の選定: %s\n' "$zai_note"
    return 0
}
# @zai-honesty-end

zai=""
zai_pick zai_capable || true

# 使える zai が 1 つも無ければビルドして**もう一度同じ関所を通す**。
# (ビルド直後なら必ずソースより新しいので、ここは普通に通る)
if [ -z "$zai" ]; then
    echo "== 使える zai がありません。理由:" >&2
    echo "   ${zai_note:-(候補なし)}" >&2
    echo "== ビルドします: cargo build --release --bin zai" >&2
    (cd "$root" && cargo build --release --bin zai)
    zai_note=""
    zai_pick zai_capable || true
fi
if [ -z "$zai" ]; then
    echo "zai をビルドできませんでした。ZAIVERN_BIN で明示してください。" >&2
    echo "  理由: ${zai_note:-(候補なし)}" >&2
    exit 1
fi
zai_identity "$zai" >&2

# ── 使い捨ての作業場 ──────────────────────────────────────────────
work=$(mktemp -d 2>/dev/null || mktemp -d -t conflict-bench)
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
# `zai` の台帳は `dirs::home_dir()/.zaivern` にできるので、HOME ごと差し替える。
# (cargo は HOME を見るので、ビルドはこれより前に済ませてある)
mkdir -p "$work/home"
HOME="$work/home"
USERPROFILE="$work/home" # Windows 側の同等物。片側だけ書かない
export HOME USERPROFILE
export GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=conflict-bench
export GIT_AUTHOR_EMAIL=conflict-bench@example.invalid
export GIT_COMMITTER_NAME=conflict-bench
export GIT_COMMITTER_EMAIL=conflict-bench@example.invalid

cat >"$work/bench.py" <<'CONFLICT_BENCH_PY'
#!/usr/bin/env python3
"""conflict-bench の計測エンジン。tools/conflict-bench.sh から呼ばれる。

**なぜ sh ではなく python なのか** — 測りたいのが「ゲート 1 回あたり数十 ms」
なので、`date +%s%N` を持たない macOS では sh だけでは分解能が足りない
(`date` の起動そのものが数 ms 乗る)。ここは `time.perf_counter()` で測る。
"""

import json
import math
import os
import subprocess
import sys
import threading
import time
import unicodedata

# ═════════════════════════════════════════════════════════════════════
#  合成リポジトリの形 (計測の前提。docs/conflict-bench.md と対にすること)
# ═════════════════════════════════════════════════════════════════════

BLOCKS = 6      # 1 ファイルあたりのブロック数 (= 独立に直せる「関数」)
STRIDE = 8      # 1 ブロックの行数。編集行どうしがこの距離だけ離れる
VALUE_OFF = 2   # ブロック先頭から数えた「書き換えてよい行」
HEADER = 2      # ファイル先頭のコメント行数
OVERSUB = 1.5   # 1 ファイルを平均何人が担当するか (> 1 なら必ず重なる)

CONF_OURS, CONF_BASE, CONF_THEIRS = 1, 2, 3


class Rng:
    """再現できる乱数 (64bit LCG)。

    `random` モジュールを使わないのは、実装が変わると同じ種でも別の計画が
    出てしまい「同じ作業量を A/B へ流した」という前提が壊れるため。
    """

    M = (1 << 64) - 1

    def __init__(self, seed):
        self.s = (seed * 6364136223846793005 + 1442695040888963407) & self.M

    def next(self):
        self.s = (self.s * 6364136223846793005 + 1442695040888963407) & self.M
        return (self.s >> 33) & 0xFFFFFFFF

    def below(self, n):
        return self.next() % n if n > 0 else 0


def rel_path(i):
    return "src/mod_%02d.rs" % i


def file_text(i):
    lines = [
        "// %s — conflict-bench が生成した合成ファイル" % rel_path(i),
        "// 各エージェントは 1 ブロックの `let value` 行だけを書き換える。",
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


def make_plan(rng, agents, files):
    """(エージェント → [(ファイル番号, ブロック番号)]) の担当表を作る。

    同じ種なら必ず同じ表になる。**この表を A 群と B 群へそのまま流す** —
    差が作業内容の違いから出たら計測になっていない。
    """
    per = max(1, math.ceil(files * OVERSUB / agents))
    plan = []
    for _ in range(agents):
        picks, seen, guard = [], set(), 0
        while len(picks) < per and guard < per * 32:
            guard += 1
            f = rng.below(files)
            if f in seen:
                continue
            seen.add(f)
            picks.append((f, rng.below(BLOCKS)))
        plan.append(picks)
    return plan


def duplicates(sets):
    """「2 人以上が実際に書いたファイル」の数と、重複した書き込みの数。

    **モデルに依存しない一次データ**。ここが 0 なら衝突は起こり得ない。
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


# ═════════════════════════════════════════════════════════════════════
#  疑似エージェント
# ═════════════════════════════════════════════════════════════════════


def apply_edit(path, block, tag):
    with open(path, "r", encoding="utf-8") as fh:
        lines = fh.read().split("\n")
    lines[HEADER + block * STRIDE + VALUE_OFF] = "    let value = %s;" % tag
    with open(path, "w", encoding="utf-8") as fh:
        fh.write("\n".join(lines))


PROBE_N = 30    # 直列プローブの回数 (内訳を出すためだけ。長くしない)


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


def probe(cmd, n=PROBE_N, inp=None):
    """同じコマンドを **直列**で n 回撃って所要時間 (ms) を返す。

    並列実測の p50 は待ち合わせを含むので、「1 回いくらか」の内訳は
    直列で取る。差を取れば計測系の下駄 (fork+exec) は相殺される。
    """
    xs = []
    for _ in range(n):
        t0 = time.perf_counter()
        subprocess.run(cmd, input=inp, env=ENV, capture_output=True, text=True)
        xs.append((time.perf_counter() - t0) * 1000.0)
    return xs


class Agent(threading.Thread):
    """1 体ぶんの疑似エージェント。

    A 群 (`zai_bin=None`) と B 群で**編集の適用コードは同一**。違うのは
    「書く前に `zai hook` を通すかどうか」の 1 点だけ。
    """

    def __init__(self, group, idx, wt, picks, zai_bin, gate_only=False):
        super().__init__(daemon=True)
        self.group, self.idx, self.wt, self.picks = group, idx, wt, picks
        self.zai = zai_bin
        self.gate_only = gate_only
        self.session = "conflict-bench-%s-%d" % (group, idx)
        self.tag = "%d /* agent-%d */" % (idx + 1, idx + 1)
        self.written = set()
        self.applied = 0
        self.denied = 0
        self.denied_files = []
        self.gate_ms = []
        self.gate_deny_ms = []
        self.error = None

    def ask_gate(self, abspath):
        """`zai hook` を PreToolUse として撃つ。返りは (止められたか, 秒)。

        止めるときだけ stdout に deny の JSON が出る (許可のときは何も
        出さない = ユーザー自身の許可設定を飛び越えないため)。
        """
        payload = hook_payload(self.session, self.wt, abspath)
        t0 = time.perf_counter()
        p = subprocess.run(
            [self.zai, "hook", "--zaivern", "claude", "PreToolUse"],
            input=payload,
            env=ENV,
            capture_output=True,
            text=True,
        )
        dt = time.perf_counter() - t0
        return ("permissionDecision" in p.stdout and "deny" in p.stdout), dt

    def run(self):
        try:
            for f, b in self.picks:
                ap = os.path.join(self.wt, rel_path(f))
                if self.zai:
                    denied, dt = self.ask_gate(ap)
                    self.gate_ms.append(dt * 1000.0)
                    if denied:
                        self.gate_deny_ms.append(dt * 1000.0)
                        self.denied += 1
                        self.denied_files.append(rel_path(f))
                        # **諦めて次の担当へ移る** = 実運用と同じ挙動
                        continue
                if self.gate_only:
                    continue  # 基準値の計測 — 判定の費用だけを見たい
                apply_edit(ap, b, self.tag)
                self.written.add(rel_path(f))
                self.applied += 1
        except Exception as e:  # スレッドの例外を握り潰さない
            self.error = e


def run_parallel(agents):
    for a in agents:
        a.start()
    for a in agents:
        a.join()
    for a in agents:
        if a.error:
            raise a.error


# ═════════════════════════════════════════════════════════════════════
#  マージと衝突の計数
# ═════════════════════════════════════════════════════════════════════


def resolve_union(path):
    """衝突マーカを外して**両側を残す**。返りは (ハンク数, 衝突行数)。

    片側を捨てる (`--theirs` 等) と先行エージェントの成果が消えて、後続の
    マージが不自然に綺麗になる。両側を残すのが人手の解決にいちばん近く、
    「衝突が次の衝突を生む」という実運用の性質も再現できる。
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
            continue  # 共通祖先の再掲は残さない
        if mode:
            clines += 1
        out.append(line)
    with open(path, "w", encoding="utf-8") as fh:
        fh.write("\n".join(out))
    return hunks, clines


def merge_all(intdir, branches):
    st = {
        "merges": len(branches),
        "conflict_merges": 0,
        "conflict_files": 0,
        "conflict_files_unique": [],
        "hunks": 0,
        "lines": 0,
    }
    uniq = set()
    for b in branches:
        p = git(
            ["-c", "merge.conflictstyle=merge", "merge", "--no-edit", "-q", b],
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


def run_group(group, repo, work, base, plan, zai_bin):
    """1 群ぶんを丸ごと走らせる。返りは計測値の dict。"""
    t_all = time.perf_counter()
    branches, dirs = [], []
    t0 = time.perf_counter()
    for i in range(len(plan)):
        br = "%s-agent-%d" % (group, i + 1)
        wt = os.path.join(work, group, "agent-%d" % (i + 1))
        git(["worktree", "add", "-q", "-b", br, wt, base], repo)
        branches.append(br)
        dirs.append(wt)
    t_setup = time.perf_counter() - t0

    # ── 編集フェーズ: **並列**。ゲートのロック競合もここで実際に起きる
    agents = [Agent(group, i, dirs[i], plan[i], zai_bin) for i in range(len(plan))]
    t0 = time.perf_counter()
    run_parallel(agents)
    t_edit = time.perf_counter() - t0

    # ── コミット (直列。計測の主目的ではない)
    t0 = time.perf_counter()
    for i, wt in enumerate(dirs):
        if git(["status", "--porcelain"], wt).stdout.strip():
            git(["add", "-A"], wt)
            git(["commit", "-q", "-m", "agent-%d の作業" % (i + 1)], wt)
    t_commit = time.perf_counter() - t0

    # ── マージ 〜 衝突解消
    intdir = os.path.join(work, group, "integration")
    git(["worktree", "add", "-q", "-b", "%s-integration" % group, intdir, base], repo)
    t0 = time.perf_counter()
    st = merge_all(intdir, branches)
    t_merge = time.perf_counter() - t0

    dup_files, dup_writes = duplicates([a.written for a in agents])
    return dict(
        group=group,
        planned=sum(len(p) for p in plan),
        applied=sum(a.applied for a in agents),
        denied=sum(a.denied for a in agents),
        denied_files=sorted({f for a in agents for f in a.denied_files}),
        dup_files=dup_files,
        dup_writes=dup_writes,
        gate_ms=[x for a in agents for x in a.gate_ms],
        gate_deny_ms=[x for a in agents for x in a.gate_deny_ms],
        wall_worktree=t_setup,
        wall_edit=t_edit,
        wall_commit=t_commit,
        wall_merge=t_merge,
        wall_total=time.perf_counter() - t_all,
        **st
    )


def breakdown(repo, zai_bin, enabled):
    """`zai hook` 1 回の内訳を**直列**で取る。

    (a) `git --version`  = 計測系の下駄 (fork + exec)
    (b) `zai --version`  = + zai 自身のプロセス起動
    (c) `zai hook`       = + フック投函と判定

    台帳が無いときの (c) は `enabled()` の `stat` 1 回で戻るので
    (b) とほぼ同じになるはず — 設計原則 3「使っていない人が払うコストは
    ゼロ」の検算になる。台帳があるときの (c) との差が**判定そのものの費用**。
    """
    pay = hook_payload(
        "conflict-bench-probe",
        repo,
        os.path.join(repo, "src", "probe_only.rs"),  # 誰の担当でもないパス
    )
    hook = probe([zai_bin, "hook", "--zaivern", "claude", "PreToolUse"], inp=pay)
    if enabled:
        return {"hook_on": hook}
    return {
        "spawn": probe(["git", "--version"]),
        "startup": probe([zai_bin, "--version"]),
        "hook_off": hook,
    }


def gate_log_lines():
    """`zai` の診断ログ (`gate.log`) を探して 1 行ずつ返す。

    置き場所を直書きしない — 差し替えた HOME の下を歩いて探す
    (この計測では HOME 自体が使い捨ての一時ディレクトリなので安い)。
    """
    home = os.path.expanduser("~")
    for dirpath, _dirs, names in os.walk(home):
        if "gate.log" in names:
            with open(
                os.path.join(dirpath, "gate.log"), encoding="utf-8", errors="replace"
            ) as fh:
                return fh.read().splitlines()
    return []


def count_fail_open():
    """**ゲートが素通りした回数**。

    `crate::lease::gate` は内部エラーを fail-open (許可) にする設計なので、
    台帳のロックを `LOCK_WAIT_MS` 以内に取れないと**判定せずに通す**。
    これが起きた回数を数えないと「0 件でした」が嘘になり得る。
    """
    return sum(1 for l in gate_log_lines() if " fail-open " in l)


# ═════════════════════════════════════════════════════════════════════
#  出力 (CJK 幅を数えて桁を揃える)
# ═════════════════════════════════════════════════════════════════════


def w(s):
    return sum(2 if unicodedata.east_asian_width(c) in "WF" else 1 for c in s)


def pad(s, n):
    return s + " " * max(0, n - w(s))


def table(header, rows):
    cols = len(header)
    width = [max([w(header[c])] + [w(r[c]) for r in rows]) for c in range(cols)]

    def bar(l, m, r):
        return l + m.join("─" * (width[c] + 2) for c in range(cols)) + r

    out = [bar("┌", "┬", "┐")]
    out.append("│ " + " │ ".join(pad(header[c], width[c]) for c in range(cols)) + " │")
    out.append(bar("├", "┼", "┤"))
    for r in rows:
        out.append("│ " + " │ ".join(pad(r[c], width[c]) for c in range(cols)) + " │")
    out.append(bar("└", "┴", "┘"))
    return "\n".join(out)


def pct(vals, p):
    if not vals:
        return 0.0
    v = sorted(vals)
    return v[min(len(v) - 1, max(0, math.ceil(p * len(v)) - 1))]


def hms(sec):
    """秒 → 人が読める形。「18 分 → 4 分」の形で読めることが要件。"""
    sec = float(sec)
    if abs(sec) < 1:
        return "%.0f ms" % (sec * 1000)
    if abs(sec) < 60:
        return "%.1f 秒" % sec
    sign = "-" if sec < 0 else ""
    m, s = divmod(int(round(abs(sec))), 60)
    if m < 60:
        return "%s%d 分 %02d 秒" % (sign, m, s)
    h, m = divmod(m, 60)
    return "%s%d 時間 %02d 分" % (sign, h, m)


def diff_n(a, b, unit=""):
    """B − A を「減った量」として読める形に (減ったら負の数)。"""
    d = b - a
    return "±0" if d == 0 else "%+d%s" % (d, unit)


def render(cfg, A, B, gate):
    L = []
    L.append("")
    L.append("== 計測条件")
    L.append(
        "   エージェント %d 体 / ファイル %d 個 / 種 %d"
        % (cfg["agents"], cfg["files"], cfg["seed"])
    )
    L.append(
        "   1 ファイルあたり平均 %.1f 人が担当 / 1 ファイル %d ブロック "
        "(同じファイルでも別ブロックなら git は自動マージする)" % (OVERSUB, BLOCKS)
    )
    L.append("   zai:  %s" % cfg["zai"])
    L.append("   一時リポジトリ: %s" % cfg["work"])
    L.append("")
    L.append("== A 群 (worktree で分けただけ) vs B 群 (zai hook のリース強制)")
    rows = [
        ["計画した編集", "%d 件" % A["planned"], "%d 件" % B["planned"], "同一 (同じ種)"],
        [
            "実際に書けた編集",
            "%d 件" % A["applied"],
            "%d 件" % B["applied"],
            diff_n(A["applied"], B["applied"], " 件"),
        ],
        [
            "2 人以上が書いたファイル",
            "%d 件" % A["dup_files"],
            "%d 件" % B["dup_files"],
            diff_n(A["dup_files"], B["dup_files"], " 件"),
        ],
        [
            "ゲートが事前に止めた回数",
            "—",
            "%d 件" % B["denied"],
            "起こさせなかった衝突",
        ],
        [
            "衝突したマージ",
            "%d / %d 回" % (A["conflict_merges"], A["merges"]),
            "%d / %d 回" % (B["conflict_merges"], B["merges"]),
            diff_n(A["conflict_merges"], B["conflict_merges"], " 回"),
        ],
        [
            "衝突ファイル (延べ)",
            "%d 件" % A["conflict_files"],
            "%d 件" % B["conflict_files"],
            diff_n(A["conflict_files"], B["conflict_files"], " 件"),
        ],
        [
            "衝突ファイル (実数)",
            "%d 件" % len(A["conflict_files_unique"]),
            "%d 件" % len(B["conflict_files_unique"]),
            diff_n(
                len(A["conflict_files_unique"]), len(B["conflict_files_unique"]), " 件"
            ),
        ],
        [
            "衝突ハンク",
            "%d 個" % A["hunks"],
            "%d 個" % B["hunks"],
            diff_n(A["hunks"], B["hunks"], " 個"),
        ],
        [
            "衝突ハンクの行数合計",
            "%d 行" % A["lines"],
            "%d 行" % B["lines"],
            diff_n(A["lines"], B["lines"], " 行"),
        ],
        [
            "壁時計: 編集フェーズ",
            hms(A["wall_edit"]),
            hms(B["wall_edit"]),
            "%+.0f ms (ゲートの実費)" % ((B["wall_edit"] - A["wall_edit"]) * 1000),
        ],
        [
            "壁時計: マージ〜衝突解消",
            hms(A["wall_merge"]),
            hms(B["wall_merge"]),
            "%+.0f ms" % ((B["wall_merge"] - A["wall_merge"]) * 1000),
        ],
        [
            "壁時計: 群の全体",
            hms(A["wall_total"]),
            hms(B["wall_total"]),
            "%+.0f ms" % ((B["wall_total"] - A["wall_total"]) * 1000),
        ],
    ]
    L.append(table(["指標", "A: ガード無し", "B: ガード有り", "差 (B − A)"], rows))
    L.append("")
    L.append("== ゲートの費用 — `zai hook` は書き込みのたびに通る (= 臨界路)")
    L.append(
        "   [並列 %d 体の実測 / %d 回] p50 %.1f ms / p95 %.1f ms / max %.1f ms "
        "/ 平均 %.1f ms / 合計 %s"
        % (
            cfg["agents"],
            gate["calls"],
            gate["p50"],
            gate["p95"],
            gate["max"],
            gate["mean"],
            hms(gate["total_s"]),
        )
    )
    L.append(
        "        うち deny %d 回 (p50 %.1f ms / max %.1f ms)"
        % (gate["deny_calls"], gate["deny_p50"], gate["deny_max"])
    )
    L.append("   [直列プローブ %d 回ずつ — 1 回いくらかの内訳]" % gate["probe_n"])
    for label, key, note in (
        ("git --version", "probe_spawn_p50", "計測系の下駄 (fork + exec)"),
        ("zai --version", "probe_startup_p50", "+ zai のプロセス起動"),
        ("zai hook (台帳なし)", "probe_hook_off_p50", "+ 投函と stat 1 回 = 使っていない人が払う全額"),
        ("zai hook (台帳あり)", "probe_hook_on_p50", "+ リース判定 = ゲートの全額"),
    ):
        L.append("     %s p50 %6.1f ms   %s" % (pad(label, 20), gate[key], note))
    L.append(
        "   → リース判定そのもの %+.1f ms ／ 台帳を持たない人の追加負担 %+.1f ms "
        "(p50 の差。数 ms は計測の揺れ)"
        % (
            gate["probe_hook_on_p50"] - gate["probe_hook_off_p50"],
            gate["probe_hook_off_p50"] - gate["probe_startup_p50"],
        )
    )
    L.append(
        "   → ゲートが fail-open した回数 (ロックを取れず**判定せずに通した**): %d"
        % gate["fail_open"]
    )
    L.append("")
    a_cost = A["hunks"] * cfg["hunk_cost"]
    b_cost = B["hunks"] * cfg["hunk_cost"] + gate["total_s"]
    L.append(
        "== モデル: 衝突の解消に費やす時間 (実測ハンク数 × --hunk-cost %g 秒/ハンク)"
        % cfg["hunk_cost"]
    )
    L.append("   A: %d ハンク × %g 秒 = %s" % (A["hunks"], cfg["hunk_cost"], hms(a_cost)))
    L.append(
        "   B: %d ハンク × %g 秒 + ゲート %s = %s"
        % (B["hunks"], cfg["hunk_cost"], hms(gate["total_s"]), hms(b_cost))
    )
    L.append(
        "   → %s → %s  (%s の短縮)" % (hms(a_cost), hms(b_cost), hms(a_cost - b_cost))
    )
    L.append(
        "   ※ --hunk-cost は**測っていない仮定**です。実測値はハンク数と "
        "ゲート時間だけ。仮定を変えたいときは --hunk-cost で。"
    )
    L.append("")
    return "\n".join(L)


# ═════════════════════════════════════════════════════════════════════
#  入口
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
        "agents": int(opt["agents"]),
        "files": int(opt["files"]),
        "seed": int(opt["seed"]),
        "hunk_cost": float(opt["hunk-cost"]),
        "zai": opt["zai"],
        "work": opt["work"],
        "json": opt.get("json", "0") == "1",
    }
    work = cfg["work"]
    repo = os.path.join(work, "repo")
    os.makedirs(repo, exist_ok=True)

    plan = make_plan(Rng(cfg["seed"]), cfg["agents"], cfg["files"])
    base = setup_repo(repo, cfg["files"])

    # 1. 台帳が無い状態での内訳 (= 使っていない人が払う額)
    off = breakdown(repo, cfg["zai"], enabled=False)
    # 2. A 群 — ゲートを 1 度も通らない
    A = run_group("a", repo, work, base, plan, None)
    # 3. B 群だけ台帳を有効にする
    r = subprocess.run(
        [cfg["zai"], "lease", "enable", "--dir", repo],
        env=ENV,
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        raise RuntimeError("zai lease enable が失敗しました: %s%s" % (r.stdout, r.stderr))
    on = breakdown(repo, cfg["zai"], enabled=True)
    B = run_group("b", repo, work, base, plan, cfg["zai"])

    g = B["gate_ms"]
    gate = {
        "calls": len(g),
        "p50": pct(g, 0.50),
        "p95": pct(g, 0.95),
        "max": max(g) if g else 0.0,
        "mean": sum(g) / len(g) if g else 0.0,
        "total_s": sum(g) / 1000.0,
        "deny_calls": len(B["gate_deny_ms"]),
        "deny_p50": pct(B["gate_deny_ms"], 0.50),
        "deny_max": max(B["gate_deny_ms"]) if B["gate_deny_ms"] else 0.0,
        "probe_n": PROBE_N,
        "probe_spawn_p50": pct(off["spawn"], 0.50),
        "probe_startup_p50": pct(off["startup"], 0.50),
        "probe_hook_off_p50": pct(off["hook_off"], 0.50),
        "probe_hook_on_p50": pct(on["hook_on"], 0.50),
        "probe_hook_on_p95": pct(on["hook_on"], 0.95),
        "probe_hook_on_max": max(on["hook_on"]),
        "fail_open": count_fail_open(),
    }

    text = render(cfg, A, B, gate)
    sink = sys.stderr if cfg["json"] else sys.stdout
    print(text, file=sink)

    # ── 判定。**緑の嘘を作らない**
    bad = []
    if B["conflict_files"] or B["hunks"]:
        bad.append(
            "B 群にマージ衝突が残りました (ファイル %d / ハンク %d)"
            % (B["conflict_files"], B["hunks"])
        )
    if B["dup_files"]:
        bad.append(
            "B 群で %d 個のファイルを 2 人以上が書きました "
            "(ゲートの fail-open %d 回 / 並列 %d 体でのゲート p95 %.0f ms・max %.0f ms)。"
            "\n   `crate::lease::gate` は内部エラーを fail-open にする設計なので、"
            "台帳のロックが `LOCK_WAIT_MS` 以内に取れないと判定せずに通します。"
            "\n   同時実行数を下げるか、src/lease.rs のロック保持時間を短くしてください"
            % (B["dup_files"], gate["fail_open"], cfg["agents"], gate["p95"], gate["max"])
        )
    if A["conflict_files"] == 0:
        bad.append(
            "A 群で 1 件も衝突しませんでした。担当が重なっていないので計測に "
            "なっていません (--agents / --files を見直してください)"
        )
    if B["applied"] + B["denied"] != B["planned"]:
        bad.append(
            "B 群の 書けた+止めた (%d) が計画 (%d) と合いません"
            % (B["applied"] + B["denied"], B["planned"])
        )

    if cfg["json"]:
        for d in (A, B):
            d.pop("gate_ms", None)
            d.pop("gate_deny_ms", None)
        print(
            json.dumps(
                {"config": cfg, "a": A, "b": B, "gate": gate, "failures": bad},
                ensure_ascii=False,
                indent=2,
                default=str,
            )
        )

    sink.flush()
    sys.stdout.flush()
    if bad:
        for m in bad:
            print("❌ %s" % m, file=sys.stderr)
        return 1
    print(
        "✅ B 群のマージ衝突は 0 件。ゲート %d 回のうち %d 件を書き込み前に止めました "
        "(A 群は %d ハンクの衝突を後から直すことになった)"
        % (gate["calls"], B["denied"], A["hunks"]),
        file=sink,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
CONFLICT_BENCH_PY

# `exec` は使わない (trap が動かず一時リポジトリが残る)。
set +e
python3 "$work/bench.py" \
    --agents "$agents" --files "$files" --seed "$seed" \
    --hunk-cost "$hunk_cost" --zai "$zai" --work "$work" --json "$json"
rc=$?
set -e
exit "$rc"
