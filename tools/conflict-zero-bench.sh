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
#   train     ③ 統合の順序付け。`zai train plan` が出した順序と**作成順 (素朴)** の
#             両方で同じ枝をマージし、衝突数を並べる。片方だけでは「効いた」と
#             言えない。併せて乾式検査の的中率も出す。無ければ skip
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

# この計測の適格判定: `zai help` が動き、その中身を段の判定に使う。
zai_help_text=""
cz_capable() {
    zai_help_text=$(zai_help "$1" 2>/dev/null) || return 1
    return 0
}
zai=""
zai_pick cz_capable || true

has_sub() {
    [ -n "$zai" ] || return 1
    printf '%s\n' "$zai_help_text" | grep -q "zai $1" || return 1
}

# 段②の機構を決める。**新しい名前を優先し、無ければ出荷済みの経路へ落ちる。**
# **`zai guard` はここでは使わない。**`zai guard` は commit の瞬間に git フックとして
# 走る仕組み (`guard check --staged`) で、**1 回の書き込みを止める門ではない**。
# この段が測るのは「書く直前に止まるか」なので、経路は `zai hook` (リース) 一択。
# 名前が近いだけで別物を当てると、**止めていないのに緑に見える**
# (実際に `guard check --stdin` を撃って 48 回中 0 件しか止まらず、
#  それでも「guard 段 実行」と表示された。判定が拾ったので嘘にはならなかったが、
#  拾わなければ静かな嘘になっていた)。
guard_mech=none
guard_skip_reason="zai hook / zai lease が見つかりません"
if has_sub hook && has_sub lease; then
    guard_mech=hook
    guard_skip_reason=""
fi

# ── ゲートが**本当に止められるか**の事前検査 (事故2) ──────────────
#
# **`zai help` に文字列があるかを見るだけでは足りない。** これまでの検出は
# 「重なりがあったのに衝突が残った」という**事後判定だけ**だったので、
# `--overlap 0.0` のように衝突がそもそも起きない条件で回すと、ゲートが
# 1 件も止めていなくても綺麗な数字が出て**静かな嘘**になる。
# `tools/coedit-bench.sh` の流儀 (exit 20〜23) に合わせて、**測る前に**
# 「1 件でも実際に止められるか」「持っていない相手は通すか」を確かめ、
# 落ちたら「証明」と言わずに理由を出して降りる。
#
#   20 lease enable が失敗した
#   21 lease claim が失敗した (予約が取れない)
#   22 他人が持っているファイルへの書き込みを**通した** (門が無い)
#   23 誰も持っていないファイルへの書き込みを**止めた** (門が閉じっぱなし)
gate_probe() {
    probe=$1
    mkdir -p "$probe/repo" "$probe/home"
    (
        HOME="$probe/home"
        USERPROFILE="$probe/home"
        export HOME USERPROFILE
        cd "$probe/repo" || exit 20
        git init -q -b main . >/dev/null 2>&1 || exit 20
        printf 'x\n' >a.rs
        printf 'y\n' >b.rs
        git add -A >/dev/null 2>&1
        git -c user.email=probe@example.invalid -c user.name=probe \
            commit -qm probe >/dev/null 2>&1 || exit 20
        "$zai" lease enable --dir "$probe/repo" >/dev/null 2>&1 || exit 20
        "$zai" lease claim 'a.rs' --agent probe-holder --dir "$probe/repo" >/dev/null 2>&1 || exit 21
        # 他人が持っている a.rs → **deny でないといけない**
        gate_says "$probe/repo" "$probe/repo/a.rs" | grep -q deny || exit 22
        # 誰も持っていない b.rs → **deny であってはいけない**
        gate_says "$probe/repo" "$probe/repo/b.rs" | grep -q deny && exit 23
        exit 0
    )
}

# ゲートへ 1 回だけ問い合わせて、その返事 (stdout) をそのまま返す。
# 本番と**同じ payload の形**を使う (形が違えば検査の意味が無い)。
gate_says() {
    python3 - "$zai" "$1" "$2" <<'EOS'
import json, subprocess, sys
zai, cwd, path = sys.argv[1], sys.argv[2], sys.argv[3]
payload = json.dumps({
    "session_id": "czbench-probe",
    "cwd": cwd,
    "hook_event_name": "PreToolUse",
    "tool_name": "Edit",
    "tool_input": {"file_path": path},
})
try:
    p = subprocess.run([zai, "hook", "--zaivern", "claude", "PreToolUse"],
                       input=payload, capture_output=True, text=True, timeout=60)
except Exception as e:
    sys.stderr.write(str(e))
    sys.exit(1)
sys.stdout.write(p.stdout)
EOS
}


# 段③。統合の順序付け。**ヘルプに載っている時だけ**撃つ (知らないサブコマンドを
# 投げると古い zai は GUI 起動として扱う)。
train_mech=none
if has_sub train; then
    train_mech=train
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

# ゲートの事前検査は **work が出来てから**走らせる (後始末を trap に乗せるため)。
if [ "$guard_mech" = hook ]; then
    probe_rc=0
    gate_probe "$work/gate-probe" || probe_rc=$?
    case "$probe_rc" in
    0) ;;
    20) guard_mech=none guard_skip_reason="事前検査: zai lease enable が失敗しました" ;;
    21) guard_mech=none guard_skip_reason="事前検査: zai lease claim が失敗しました (予約が取れません)" ;;
    22) guard_mech=none guard_skip_reason="事前検査: **他人が持っているファイルへの書き込みを通しました** (門が働いていないので、この段の数字は保護の証明になりません)" ;;
    23) guard_mech=none guard_skip_reason="事前検査: 誰も持っていないファイルへの書き込みを止めました (門が閉じっぱなしです)" ;;
    *) guard_mech=none guard_skip_reason="事前検査そのものが失敗しました (rc=$probe_rc)" ;;
    esac
fi

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
        # **ブランチごとの実測**。乾式検査 (`zai train`) が「この段で衝突する」と
        # 言い当てられたかを後で突き合わせるのに要る。集計値だけでは的中を測れない。
        "per_branch": [],
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
            st["per_branch"].append({"branch": b, "conflicts": []})
            continue
        files = git(["diff", "--name-only", "--diff-filter=U"], intdir).stdout.split()
        if not files:
            raise RuntimeError(
                "マージが衝突以外の理由で失敗しました: %s%s" % (p.stdout, p.stderr)
            )
        st["conflict_merges"] += 1
        st["conflict_files"] += len(files)
        st["per_branch"].append({"branch": b, "conflicts": sorted(files)})
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


def write_phase(name, repo, work, base, plan, gate_cmd=None):
    """N 本の worktree を切り、書き手を**並列で**走らせ、コミットまでやる。

    返りは `(branches, dirs, writers, t_edit_ms)`。**作成順 = `branches` の順**で、
    これがそのまま「素朴な統合順」の対照になる。
    """
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
    return branches, dirs, ws, t_edit


def run_stage(name, repo, work, base, plan, gate_cmd=None, driver=None):
    t_all = time.perf_counter()
    branches, dirs, ws, t_edit = write_phase(name, repo, work, base, plan, gate_cmd)

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


def clean_prefix(per_branch):
    """**最初の衝突に当たるまでに、自動で入った本数。**

    順序付けが減らせるとすればここで、衝突の総量ではない。
    「人手が要るまでに何本が黙って入るか」がそのまま人の割り込み回数になる。
    """
    k = 0
    for x in per_branch:
        if x["conflicts"]:
            break
        k += 1
    return k


def merge_into(repo, work, label, base, branches):
    """使い捨ての統合 worktree を作り、与えられた**順序で**直列マージする。"""
    intdir = os.path.join(work, "train", label)
    git(["worktree", "add", "-q", "-b", "train-int-%s" % label, intdir, base], repo)
    t0 = time.perf_counter()
    st = merge_all(intdir, branches)
    st["wall_merge_ms"] = (time.perf_counter() - t0) * 1000.0
    return st


def train_stage_dict(label, mech, plan, ws, st, t_edit, t_all):
    dup_files, dup_writes = duplicates([w.written for w in ws])
    d = dict(
        stage=label,
        status="ok",
        mech=mech,
        planned=sum(len(x) for x in plan),
        applied=sum(w.applied for w in ws),
        denied=0,
        dup_files=dup_files,
        dup_writes=dup_writes,
        gate_calls=0,
        gate_p50=0.0,
        gate_p95=0.0,
        gate_max=0.0,
        wall_edit_ms=t_edit,
        wall_total_ms=(time.perf_counter() - t_all) * 1000.0,
    )
    d.update(st)
    return d


def run_train_stage(zai, work, files, plan, onto):
    """③ 統合の順序付け。**専用リポジトリで測る。**

    `zai train plan` は「`--onto` より進んでいるブランチ」を**自分で探す**ので、
    baseline / guard / union の枝が同じリポジトリに居ると全部拾ってしまい、
    順序の効果を測れなくなる。この段だけ別リポジトリを立てるのはそのため。

    測るのは 2 つ:
      1. **同じ N 本を、train が出した順序と作成順 (素朴) の両方でマージした
         ときの衝突数。**片方だけ出すと「順序付けが効いた」とは言えない。
      2. **乾式検査 (`plan --json` の `dry`) が、実行前にどこまで言い当てたか。**
    """
    repo = os.path.join(work, "train", "repo")
    os.makedirs(repo, exist_ok=True)
    base = setup_repo(repo, files)
    t_all = time.perf_counter()
    branches, dirs, ws, t_edit = write_phase("train", repo, work, base, plan, None)

    def plan_json():
        pr = subprocess.run(
            [zai, "train", "plan", "--repo", repo, "--onto", onto, "--json"],
            env=ENV,
            capture_output=True,
            text=True,
        )
        if pr.returncode != 0:
            raise RuntimeError(
                "zai train plan が失敗しました (%d)\n%s%s"
                % (pr.returncode, pr.stdout, pr.stderr)
            )
        return json.loads(pr.stdout)

    # ── 観測 1: **worktree を持ったまま**撃つ = 実運用そのままの形。
    #    `train::candidates` は他の worktree が握っている枝を `held` にして
    #    候補から外すので、「N 体が各自の worktree で作業中」の状態では
    #    計画に 1 本も載らないはず。**これは仕様だが、利用者から見ると
    #    「動いている最中には使えない」ことを意味する。数字として残す。**
    attached = plan_json()
    held_while_attached = len(attached.get("held", []))
    free_while_attached = len(attached.get("plan", {}).get("steps", []))

    # ── worktree を外す (ブランチは残る) = 「エージェントが仕事を終えた後」
    for wt in dirs:
        git(["worktree", "remove", "--force", wt], repo)
    git(["worktree", "prune"], repo)

    detached = plan_json()
    tp = detached.get("plan", {})
    dry = detached.get("dry", {})
    order = [x["branch"] for x in tp.get("steps", [])]
    missing = [b for b in branches if b not in set(order)]
    if missing:
        # **落ちた枝を黙って捨てない。**同じ集合を両方へ流さないと比較にならない。
        #
        # 注意: `plan.dropped` は当てにならない。`train::touches_from_repo` が
        # `take(MAX_BRANCHES)` で先に切ってから `plan_order` へ渡すので、
        # 24 本を超えたぶんは **`dropped` に数えられないまま計画から消える**
        # (実測: 32 本を投げると計画は 24 本、`dropped` は 0)。
        # だから申告値ではなく**自分で引き算した数**を出す。
        order = order + missing

    # ── `train run --dry-run` の終了コード (0=成功見込み / 1=衝突で停止)
    pr = subprocess.run(
        [zai, "train", "run", "--repo", repo, "--onto", onto, "--dry-run"],
        env=ENV,
        capture_output=True,
        text=True,
    )
    dry_run_rc = pr.returncode

    st_order = merge_into(repo, work, "order", base, order)
    st_naive = merge_into(repo, work, "naive", base, branches)

    # ── 乾式の的中率。**予想が触れた段だけ**を分母にする (最初の衝突で
    #    打ち切る仕様なので、その先を分母に入れると不当に低く出る)。
    actual = {x["branch"]: x["conflicts"] for x in st_order["per_branch"]}
    dsteps = dry.get("steps", [])
    covered = len(dsteps)
    agree = sum(1 for d in dsteps if bool(d["conflicts"]) == bool(actual.get(d["branch"])))
    pred_first = next((d["branch"] for d in dsteps if d["conflicts"]), None)
    act_first = next((x["branch"] for x in st_order["per_branch"] if x["conflicts"]), None)
    files_exact = None
    if pred_first is not None and pred_first == act_first:
        pf = sorted(next(d["conflicts"] for d in dsteps if d["branch"] == pred_first))
        files_exact = pf == sorted(actual.get(pred_first, []))

    a = train_stage_dict("train", "zai train plan の順序", plan, ws, st_order, t_edit, t_all)
    b = train_stage_dict("train(素朴順)", "対照: 作成順", plan, ws, st_naive, t_edit, t_all)
    a["train"] = {
        "clean_prefix": clean_prefix(st_order["per_branch"]),
        "naive_clean_prefix": clean_prefix(st_naive["per_branch"]),
        "onto": detached.get("onto"),
        "order": order,
        "naive_order": branches,
        "same_order": order == branches,
        "dropped": tp.get("dropped", 0),
        "missing_from_plan": len(missing),
        "line_pairs": tp.get("line_pairs", 0),
        "held_while_attached": held_while_attached,
        "free_while_attached": free_while_attached,
        "dry_available": dry.get("available"),
        "dry_note": dry.get("note"),
        "dry_covered": covered,
        "dry_agree": agree,
        "dry_pred_first_conflict": pred_first,
        "actual_first_conflict": act_first,
        "dry_files_exact": files_exact,
        "dry_run_rc": dry_run_rc,
    }
    return a, b


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
        "   zai: %s"
        % (
            "%s (%s)" % (os.path.abspath(cfg["zai"]), cfg["zai_version"] or "版不明")
            if cfg["zai"]
            else "見つかりません (ベースラインのみ実行します)"
        ),
    ] + (["   zai の選定: %s" % cfg["zai_note"]] if cfg["zai_note"] else []) + [
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
        "guard_skip_reason": opt.get("guard-skip-reason", "")
        or "zai hook / zai lease が見つかりません",
        "zai_version": opt.get("zai-version", ""),
        "zai_note": opt.get("zai-note", ""),
        "train_mech": opt.get("train-mech", "none"),
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
                "reason": cfg["guard_skip_reason"]
                + ("" if cfg["zai"] else " (zai 自体が未検出)"),
            }
        )
    else:
        if True:
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

    # ── 段③ train — 順序付けが統合の衝突を減らすか
    if cfg["train_mech"] != "train":
        stages.append(
            {
                "stage": "train",
                "status": "skipped",
                "reason": "zai train が見つかりません"
                + ("" if cfg["zai"] else " (zai 自体が未検出)"),
            }
        )
    else:
        ta, tb = run_train_stage(cfg["zai"], work, cfg["files"], plan, "main")
        stages.append(ta)
        stages.append(tb)
        t = ta["train"]
        dh, dm = ta["hunks"] - tb["hunks"], ta["conflict_merges"] - tb["conflict_merges"]
        verdict = (
            "**総量は 1 つも変わらなかった**"
            if (dh == 0 and dm == 0)
            else "ハンク %+d 個 / 衝突したマージ %+d 回" % (dh, dm)
        )
        notes += [
            "== train 段の読み方 (**ここを誇張しない**)",
            "   同じ %d 本のブランチを、train が出した順序と作成順の**両方で**マージした。"
            % len(t["order"]),
            "   順序は %s。結果の差は %s。"
            % ("同じだった" if t["same_order"] else "**違う**", verdict),
            "   **順序付けは衝突の総量を減らす仕組みではない。**同じ行域が重なった組は",
            "   どう並べても人手が要る (train 自身が line_pairs = %d 組と申告している)。"
            % t["line_pairs"],
            "   順序付けが減らせるとすれば**最初の衝突に当たるまでに自動で入る本数**で、",
            "   これは人が割り込まれるまでの長さそのもの。実測: train 順 %d 本 / 素朴順 %d 本 (全 %d 本)。"
            % (t["clean_prefix"], t["naive_clean_prefix"], len(t["order"])),
            "",
            "== train の乾式検査 (実行前の予想)",
            "   予想が触れた段: %d / うち当たり: %d" % (t["dry_covered"], t["dry_agree"]),
            "   最初に衝突すると予想: %s / 実際に最初に衝突: %s"
            % (t["dry_pred_first_conflict"] or "(無し)", t["actual_first_conflict"] or "(無し)"),
            "   衝突ファイルまで一致したか: %s"
            % {True: "した", False: "しなかった", None: "比較できず (予想が外れたため)"}[
                t["dry_files_exact"]
            ],
            "   merge-tree が使えたか: %s / train run --dry-run の終了コード: %d"
            % (t["dry_available"], t["dry_run_rc"]),
        ]
        if t["dry_note"]:
            notes.append("   降格・打ち切りの理由: %s" % t["dry_note"])
        if t["missing_from_plan"]:
            notes.append(
                "   **計画に載らなかった枝が %d 本ある** (train::MAX_BRANCHES = 24)。"
                % t["missing_from_plan"]
            )
            notes.append(
                "   同じ集合で比べるため末尾へ足した。なお train の自己申告は dropped = %d で、"
                % t["dropped"]
            )
            notes.append(
                "   **落ちた本数を過少に報告している** (`touches_from_repo` が数える前に切るため)。"
            )
        notes += [
            "",
            "== train が「作業中」には使えないこと (仕様だが、数字として残す)",
            "   書き手が worktree を持ったまま `zai train plan` を撃つと、",
            "   計画に載った枝 %d 本 / 他の worktree が握っていて外された枝 %d 本。"
            % (t["free_while_attached"], t["held_while_attached"]),
            "   **統合順を出せるのはエージェントが worktree を手放した後**で、",
            "   走っている最中の並び替えには使えない。",
            "",
        ]

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
    --zai-version "$zai_ver" \
    --zai-note "$zai_note" \
    --guard-mech "$guard_mech" \
    --guard-skip-reason "$guard_skip_reason" \
    --train-mech "$train_mech" \
    --union-driver "$union_driver" \
    --work "$work" \
    --json "$json"
