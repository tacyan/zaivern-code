#!/usr/bin/env sh
# 🔗 鎖① (**配る前の分割**) を端から端まで測る。
#
# ## なぜ要るのか
#
# `docs/conflict-zero.md` §2 は鎖① を「**未測定**」と書いていた。
# `coordinator::admit` / `overlap_reason` / `overlap_split` の単体テストは
# 「重なったら配らない」を固定しているが、**配り方を変えた結果、実際の
# git マージで衝突ハンクが何件減ったのか**は 1 度も測っていなかった。
#
# ## 何と何を比べるか
#
#   naive   分割を通さない。担当表をそのまま N 人へ配る (round-robin)
#   coord   `coordinator` を通す。重なったら `overlap_split` で
#           「いま渡してよい部分」だけを配り、残りは直列送り / 拒否
#
# どちらの段も**同じ担当表・同じ種・同じ編集規則**で、実際にブランチを作って
# `git merge` する。落とす数字は
#
#   衝突ファイル / 衝突ハンク / 衝突行 … git が実際に出したもの
#   二重書き (dup)                    … **2 つ以上の枝が同じファイルを書いた件数**
#   配れた / 分割 / 拒否              … ゼロを買った代償 (作業量)
#
# **二重書きを必ず出す**のは §3.11.5 の教訓 (判定関数が `conflict_files` しか
# 見ておらず、2 人が同じ行を書いても git がたまたま綺麗に通れば「証明できた」と
# 言っていた) による。中核の主張は「同じものを 2 人に配らない」なので、
# 衝突が 0 でも二重書きが 1 件でもあれば**その段は失格**である。
#
# ## `--overlap` の意味 (誤解しやすいので明記する)
#
# `--overlap` は「**ホットプール (先頭 1/4 のファイル) から引く確率**」であって、
# 「重なりの量」そのものではない。`--overlap 0.0` にしても、
# タスク 12 × 1 タスク 3 個 = 36 回の抽選をファイル 24 個へ入れる以上、
# **鳩の巣原理で必ず重なる**。本当に重なりを消したいなら
# `--files` を `--tasks × --per` 以上へ広げること
# (例: `--tasks 12 --per 3 --files 120 --overlap 0.0`)。
#
# ## 編集規則を 2 つ用意してある (片方だけでは必ず嘘になる)
#
#   --edit same    2 つの枝が**同じ行**を触る (要約行のような 1 点)
#   --edit spread  各枝が**自分の id から決まる別の行**を触る
#
# §3.8.1 が示したとおり、離れた行なら素の git がもとから 0 件でマージする。
# つまり鎖① が衝突を減らすかどうかは**「同じファイルの中で編集が当たるか」に
# 丸ごと依存する**。片方だけを載せるとどちらかの嘘になるので、両方出す。
#
# ## ハーネスの費用を差し引く
#
# `naive` 段は「**同じプロセス起動・同じ解析・同じ出力で、判定だけしない**」
# 空回しでもある。判定の正味 = `coord` の判定時間 - `naive` の判定時間で、
# ハーネス率も表に出す (CLAUDE.md の実測ではここが最大 42.6% を占めた)。
#
# ## 使い方 (再現は 1 行)
#
#   tools/coordinator-bench.sh                        既定 (種 20260812)
#   tools/coordinator-bench.sh --seeds "1 2 3"        種を振って複数回
#   tools/coordinator-bench.sh --tasks 24 --files 40 --per 3 --overlap 0.5
#   tools/coordinator-bench.sh --edit same            編集規則を片方だけ
#   tools/coordinator-bench.sh --reps 15              判定時間の最小値を取る回数
#   tools/coordinator-bench.sh --json                 JSON は stdout / 表は stderr
#   tools/coordinator-bench.sh --keep                 一時リポジトリを残す
#
# ## `zai` ではなく**テストバイナリ**を使う理由
#
# `coordinator` は GUI からしか到達せず、`zai` に CLI 入口が無い。入口を足すと
# `src/cli.rs` (共有ファイル) を触ることになるので、`union` の
# `merge_driver_helper` と同じ形で**テストバイナリ自身**を呼ぶ
# (`coordinator::tests::assign_helper`)。名前がずれるとハーネスは
# 「0 件のテストが走った」で静かに緑になるので、
# `coordinator::tests::assign_helper_name_matches_harness` が番人。
#
#   cargo test --bin zai --no-run     # これで deps/zai-<hash> が出来る
#
# ## 副作用を持たない作り
#
#   * 一時リポジトリは `mktemp -d` (= $TMPDIR 由来)。パスを直書きしない
#   * HOME / ZAIVERN_HOME を一時ディレクトリへ差し替える
#     (**本物の ~/.zaivern と ~/.gitconfig に 1 バイトも触らない**)
#   * cargo を呼ばない。既にあるテストバイナリを使うだけ
#
# ## 終了コード
#
#   0  測れた   1  引数以外の失敗   2  引数が変
#   20 テストバイナリが無い / 古い   21 能力検査に落ちた
#   22 配ったものが互いに素でなかった (ハーネス自身の失格)
# shellcheck disable=SC1007  # `CDPATH= cd` は「その cd にだけ空の CDPATH を渡す」正しい書き方
# shellcheck disable=SC2086  # `for s in $seeds` は意図的な語分割
# shellcheck disable=SC2034  # 共通ブロックの `$zai` はこのベンチでは使わない
#                            (`zai` に coordinator の入口が無いため。**ブロックは
#                             1 バイトも変えない**決まりなので、こちらで黙らせる)
set -eu

seeds="20260812"
tasks=12
files=24
per=3
overlap=0.5
agents=""
edits="same spread"
json=0
keep=0
testbin=""
reps=7

usage() {
    sed -n '2,72p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
}

while [ $# -gt 0 ]; do
    case "$1" in
        --seed | --seeds) seeds=${2:-}; shift 2 ;;
        --tasks)    tasks=${2:-}; shift 2 ;;
        --files)    files=${2:-}; shift 2 ;;
        --per)      per=${2:-}; shift 2 ;;
        --overlap)  overlap=${2:-}; shift 2 ;;
        --agents)   agents=${2:-}; shift 2 ;;
        --edit)     edits=${2:-}; shift 2 ;;
        --test-bin) testbin=${2:-}; shift 2 ;;
        --reps)     reps=${2:-}; shift 2 ;;
        --json)     json=1; shift ;;
        --keep)     keep=1; shift ;;
        -h | --help) usage ;;
        *) echo "不明な引数: $1 (--help で使い方)" >&2; exit 2 ;;
    esac
done

for n in "$tasks" "$files" "$per"; do
    case "$n" in
        '' | *[!0-9]*) echo "--tasks / --files / --per は正の整数です: $n" >&2; exit 2 ;;
    esac
done
[ "$tasks" -ge 2 ] || { echo "--tasks は 2 以上 (1 人では衝突が定義できません)" >&2; exit 2; }
[ "$per" -ge 1 ] || { echo "--per は 1 以上" >&2; exit 2; }
[ "$files" -ge "$per" ] || { echo "--files は --per 以上" >&2; exit 2; }
case "$overlap" in
    '' | *[!0-9.]*) echo "--overlap は 0.0〜1.0 です: $overlap" >&2; exit 2 ;;
esac
for e in $edits; do
    case "$e" in same | spread) ;; *) echo "--edit は same / spread: $e" >&2; exit 2 ;; esac
done
for s in $seeds; do
    case "$s" in
        '' | *[!0-9]*) echo "--seeds は正の整数の並びです: $s" >&2; exit 2 ;;
    esac
done
[ -n "$agents" ] || agents=$tasks
case "$agents" in
    '' | *[!0-9]*) echo "--agents は正の整数です: $agents" >&2; exit 2 ;;
esac
[ "$agents" -ge 1 ] || { echo "--agents は 1 以上" >&2; exit 2; }
case "$reps" in
    '' | *[!0-9]*) echo "--reps は正の整数です: $reps" >&2; exit 2 ;;
esac
[ "$reps" -ge 1 ] || { echo "--reps は 1 以上" >&2; exit 2; }

command -v git >/dev/null 2>&1 || { echo "git がありません。" >&2; exit 1; }
command -v awk >/dev/null 2>&1 || { echo "awk がありません。" >&2; exit 1; }

# shellcheck disable=SC1007
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
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

# ── テストバイナリを探す ──────────────────────────────────────────
#
# libtest のバイナリは `--version` に crate の版を答えないので、上の
# `zai_fresh` は使えない。**古さの判定 (`newer_sources`) だけを借りて**
# 「ヘルパを一覧に持っているか」を適格判定にする。
HELPER=coordinator::tests::assign_helper
testbin_note=""

# **`--list` を撃つ前に「これは本当にテストバイナリか」を確かめる。**
#
# `target/debug/deps/zai-<hash>` には**素の `zai` 実行ファイルも入っている**
# (cargo は bin ターゲットもここへ置き、`target/debug/zai` はそこへのハードリンク)。
# `zai` は知らない語をワークスペース指定として扱うので、`zai --list` は
# **GUI の窓を開いて永久に返ってこない**。実際にこのハーネスがそれで固まった。
#
# 中身が `target/{debug,release}/zai` と同じものは除く。`cmp` は POSIX にあり、
# ハードリンクでもコピーでも同じ判定になる。
same_as_plain_zai() {
    for _p in "$target_dir/debug/zai" "$target_dir/release/zai" \
        "$target_dir/debug/zai.exe" "$target_dir/release/zai.exe"; do
        [ -f "$_p" ] || continue
        cmp -s "$1" "$_p" && return 0
    done
    return 1
}

testbin_ok() {
    same_as_plain_zai "$1" && return 1
    _l=$("$1" --list 2>/dev/null </dev/null) || return 1
    case "$_l" in *"$HELPER"*) return 0 ;; esac
    return 1
}

testbin_pick() {
    _found=""
    if [ -n "$testbin" ]; then
        _cands=$testbin
    elif [ -n "${ZAIVERN_TEST_BIN:-}" ]; then
        _cands=$ZAIVERN_TEST_BIN
    else
        _cands=""
        for _c in "$target_dir"/debug/deps/zai-* "$target_dir"/release/deps/zai-*; do
            case "$_c" in
                *.d | *.dSYM | *.o | *'*') continue ;;
            esac
            [ -f "$_c" ] && [ -x "$_c" ] || continue
            _cands="$_cands
$_c"
        done
    fi
    # 新しい順に見る (同じ hash で何度も建て直されるため)。
    _cands=$(printf '%s\n' "$_cands" | grep -v '^$' || true)
    [ -n "$_cands" ] || return 1
    # 新しい順。ファイル名に改行を含まない前提 (cargo が作る `zai-<hash>`)。
    _sorted=$(printf '%s\n' "$_cands" | xargs ls -t -d 2>/dev/null || printf '%s\n' "$_cands")
    for _c in $_sorted; do
        [ -n "$_c" ] || continue
        if same_as_plain_zai "$_c"; then
            testbin_note="${testbin_note}${testbin_note:+ / }$_c は素の zai 実行ファイルなので飛ばしました (--list は GUI を開いて返りません)"
            continue
        fi
        testbin_ok "$_c" || {
            testbin_note="${testbin_note}${testbin_note:+ / }$_c: ${HELPER} を持っていません"
            continue
        }
        _n=$(newer_sources "$_c")
        if [ -n "$_n" ]; then
            _cnt=$(printf '%s\n' "$_n" | grep -c . || true)
            _one=$(printf '%s\n' "$_n" | sed -n '1p')
            testbin_note="${testbin_note}${testbin_note:+ / }$_c はソースより古いので使いません (${_cnt} 件が新しい。例: ${_one})"
            continue
        fi
        _found=$_c
        break
    done
    [ -n "$_found" ] || return 1
    tbin=$_found
    return 0
}

tbin=""
if ! testbin_pick; then
    echo "使えるテストバイナリがありません。" >&2
    echo "  理由: ${testbin_note:-(候補なし)}" >&2
    echo "  直し方: cargo test --bin zai --no-run" >&2
    echo "  (--test-bin / ZAIVERN_TEST_BIN で明示もできます)" >&2
    exit 20
fi

# ── ミリ秒時計。無ければ綺麗に粗くなる ────────────────────────────
# `date +%s%N` は GNU coreutils だけ。BSD (macOS) は末尾に literal な "N" を返す。
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
            printf '%s000\n' "$(date +%s)" ;;
    esac
}

# ── 一時領域。**本物の ~/.zaivern と ~/.gitconfig に触らない** ────
tmp=$(mktemp -d "${TMPDIR:-/tmp}/zv-coord-bench.XXXXXX")
# shellcheck disable=SC2329  # trap から呼ぶ (静的には見えない)
cleanup() { [ "$keep" = 1 ] || rm -rf "$tmp"; }
trap cleanup EXIT INT TERM
mkdir -p "$tmp/home" "$tmp/zhome"
HOME=$tmp/home
ZAIVERN_HOME=$tmp/zhome
export HOME ZAIVERN_HOME
GIT_CONFIG_NOSYSTEM=1; export GIT_CONFIG_NOSYSTEM
GIT_TERMINAL_PROMPT=0; export GIT_TERMINAL_PROMPT

# ── 担当表の生成 (種で完全に決まる) ───────────────────────────────
#
# 乱数は MINSTD (`s = s * 16807 mod 2^31-1`)。awk は倍精度なので、
# 積が 2^53 を超える乗数 (1103515245 など) を使うと**精度が落ちて
# OS / awk 実装ごとに違う列が出る**。16807 なら最大 3.6e13 で安全。
gen_tasks() {
    _seed=$1; _out=$2
    awk -v seed="$_seed" -v tasks="$tasks" -v files="$files" -v per="$per" -v ov="$overlap" '
        function nxt() { s = (s * 16807) % 2147483647; return s }
        function u() { return nxt() / 2147483647.0 }
        function pick(n) { return int(u() * n) % n }
        BEGIN {
            s = seed % 2147483647
            if (s <= 0) s = 1
            hot = int(files / 4); if (hot < 1) hot = 1
            for (t = 1; t <= tasks; t++) {
                delete got
                line = ""; n = 0; tries = 0
                while (n < per && tries < 400) {
                    tries++
                    if (u() < ov) f = pick(hot) + 1; else f = pick(files) + 1
                    if (f in got) continue
                    got[f] = 1; n++
                    line = line (n > 1 ? "," : "") sprintf("src/f%02d.rs", f)
                }
                # hot が per より小さいと足りなくなる。残りは全体から埋める。
                for (f = 1; n < per && f <= files; f++) {
                    if (f in got) continue
                    got[f] = 1; n++
                    line = line "," sprintf("src/f%02d.rs", f)
                }
                printf "t%02d\t%s\n", t, line
            }
        }
    ' </dev/null > "$_out"
}

# ── 素のリポジトリ。**全ての行の中身を一意にする** ────────────────
#
# §3.16 の教訓: 内容が周期的 (同じ行の反復) だと diff がハンクを別の位置へ
# 合わせるので、**行距離ではハンク境界が決まらなくなる**。ここで測りたいのは
# 「配り方」の効果だけなので、その交絡を最初から入れない。
LINES=40
make_repo() {
    _r=$1
    mkdir -p "$_r/src"
    git -C "$_r" init -q -b main . >/dev/null 2>&1
    git -C "$_r" config user.email bench@example.invalid
    git -C "$_r" config user.name bench
    git -C "$_r" config commit.gpgsign false
    awk -v files="$files" -v lines="$LINES" -v dir="$_r/src" '
        BEGIN {
            for (f = 1; f <= files; f++) {
                p = sprintf("%s/f%02d.rs", dir, f)
                printf "// f%02d — 合成モジュール\n", f > p
                for (i = 1; i <= lines; i++)
                    printf "pub const F%02d_L%02d: u32 = %d; // 一意トークン %04d-%04d\n", f, i, f * 1000 + i, f, i > p
                close(p)
            }
        }
    ' </dev/null
    git -C "$_r" add -A >/dev/null 2>&1
    git -C "$_r" commit -q -m base
    git -C "$_r" branch -q base
}

# 枝 `$2` (= セッション番号) が、ファイル `$3` の 1 行を書き換える。
# same   : どの枝も同じ行 (5 行目) を触る → 同じファイルなら必ず当たる
# spread : 枝ごとに違う行を触る → 同じファイルでも当たらない
edit_file() {
    _r=$1; _s=$2; _f=$3; _rule=$4
    case "$_rule" in
        same)   _ln=6 ;;
        *)      _ln=$(( 2 + ( _s % 12 ) * 3 )) ;;
    esac
    awk -v ln="$_ln" -v s="$_s" '
        NR == ln { printf "pub const CHANGED_BY_%d: u32 = %d; // 担当 %d が書いた\n", s, s, s; next }
        { print }
    ' "$_r/$_f" > "$_r/$_f.n"
    mv "$_r/$_f.n" "$_r/$_f"
}

# ── 判定 (`coordinator` を通す / 通さない) ─────────────────────────
#
# `$decide_ms` は **`--reps` 回の最小値**。平均や 1 点だと、同時に走っている
# ビルドやテストの谷と山で 1.3〜2.8 倍に散る (§3.14 で実測済み)。
# 費用の床を知りたいので最小値を採る。
decide_ms=0
decide() {
    _mode=$1; _table=$2; _out=$3
    decide_ms=-1
    _k=0
    while [ "$_k" -lt "$reps" ]; do
        _k=$(( _k + 1 ))
        _t0=$(now_ms)
        ZV_COORD_TASKS=$_table ZV_COORD_MODE=$_mode ZV_COORD_AGENTS=$agents \
            "$tbin" --exact "$HELPER" --nocapture --test-threads 1 > "$_out.raw" 2>"$_out.err" || true
        _t1=$(now_ms)
        _d=$(( _t1 - _t0 ))
        # **`A || B && C` で書かない。** sh は `(A||B) && C` と読むので、
        # 偽になった回のループ末尾が非ゼロ終了になり `set -e` がその場で落ちる。
        if [ "$decide_ms" -lt 0 ] || [ "$_d" -lt "$decide_ms" ]; then
            decide_ms=$_d
        fi
    done
    # libtest の枠 (running 1 test / test result: …) を落として本体だけ残す。
    grep -E '^(task|serial|summary|#)	?' "$_out.raw" > "$_out" 2>/dev/null || : > "$_out"
    grep -E '^summary' "$_out" >/dev/null 2>&1
}

# ── 統合。衝突は「両側を残す」で機械的に解決して先へ進む ──────────
# ベースラインと分割段で同じ解決規則を使わないと数字が比べられない。
resolve_keeping_both() {
    awk '
        BEGIN { inb = 0 }
        /^<<<<<<< / { inb = 1; no = 0; nt = 0; side = 1; delete seen; next }
        inb && /^=======$/ { side = 2; next }
        inb && /^>>>>>>> / {
            for (i = 1; i <= no; i++) print o[i]
            for (i = 1; i <= nt; i++) if (!(t[i] in seen)) print t[i]
            inb = 0; next
        }
        inb { if (side == 1) { o[++no] = $0; seen[$0] = 1 } else { t[++nt] = $0 }; next }
        { print }
    ' "$1" > "$1.r" && mv "$1.r" "$1"
}

count_hunks() { grep -c '^<<<<<<< ' "$1" 2>/dev/null || echo 0; }
count_clines() {
    awk '
        /^<<<<<<< / { inb = 1; next }
        /^=======$/ { next }
        /^>>>>>>> / { inb = 0; next }
        inb { n++ }
        END { print n + 0 }
    ' "$1"
}

# 1 段ぶんの実験。`$1`=mode `$2`=edit規則 `$3`=種
# 結果は `res_*` 変数へ入れる (POSIX sh には配列も戻り値も無い)。
run_arm() {
    _mode=$1; _rule=$2; _seed=$3
    _r=$tmp/$_seed-$_rule-$_mode
    _tbl=$tmp/$_seed.tasks
    _plan=$tmp/$_seed-$_rule-$_mode.plan

    decide "$_mode" "$_tbl" "$_plan" || {
        echo "判定が summary を返しませんでした ($_mode)。" >&2
        sed -n '1,12p' "$_plan.raw" >&2
        exit 21
    }
    res_decide_ms=$decide_ms

    res_full=$(awk -F'\t' '$1=="summary"{ for(i=2;i<=NF;i++){split($i,kv,"="); if(kv[1]=="full") print kv[2]} }' "$_plan")
    res_split=$(awk -F'\t' '$1=="summary"{ for(i=2;i<=NF;i++){split($i,kv,"="); if(kv[1]=="split") print kv[2]} }' "$_plan")
    res_refused=$(awk -F'\t' '$1=="summary"{ for(i=2;i<=NF;i++){split($i,kv,"="); if(kv[1]=="refused") print kv[2]} }' "$_plan")
    res_granted=$(awk -F'\t' '$1=="summary"{ for(i=2;i<=NF;i++){split($i,kv,"="); if(kv[1]=="granted") print kv[2]} }' "$_plan")
    res_dropped=$(awk -F'\t' '$1=="summary"{ for(i=2;i<=NF;i++){split($i,kv,"="); if(kv[1]=="dropped") print kv[2]} }' "$_plan")

    # ── 配ったものを枝ごとにまとめる (同じセッションの 2 タスクは 1 枝) ──
    awk -F'\t' '$1=="task" && $5!="" { n=split($5,a," "); for(i=1;i<=n;i++) print $3"\t"a[i] }' "$_plan" \
        | sort -u > "$_plan.grants"

    # **二重書き**: 2 つ以上の枝が同じファイルを書いた件数。
    # 中核の主張はここで、衝突が 0 でもこれが 0 でなければ失格。
    res_dup=$(awk -F'\t' '{ c[$2]++ } END { n=0; for (f in c) if (c[f] > 1) n++; print n+0 }' "$_plan.grants")
    res_wfiles=$(awk -F'\t' '{ c[$2]=1 } END { n=0; for (f in c) n++; print n+0 }' "$_plan.grants")

    make_repo "$_r"
    _branches=""
    # shellcheck disable=SC2013  # 値はセッション番号 (整数) なので語分割で安全
    for _s in $(awk -F'\t' '{print $1}' "$_plan.grants" | sort -un); do
        git -C "$_r" checkout -q -B "w$_s" base
        awk -F'\t' -v s="$_s" '$1==s {print $2}' "$_plan.grants" | while IFS= read -r _f; do
            [ -n "$_f" ] || continue
            edit_file "$_r" "$_s" "$_f" "$_rule"
        done
        if [ -n "$(git -C "$_r" status --porcelain)" ]; then
            git -C "$_r" add -A >/dev/null 2>&1
            git -C "$_r" commit -q -m "w$_s"
            _branches="$_branches w$_s"
        fi
    done

    res_cfiles=0; res_hunks=0; res_clines=0; res_cmerges=0
    git -C "$_r" checkout -q -B integ base
    for _b in $_branches; do
        if git -C "$_r" merge -q --no-edit "$_b" >/dev/null 2>&1; then
            continue
        fi
        res_cmerges=$(( res_cmerges + 1 ))
        for _f in $(git -C "$_r" diff --name-only --diff-filter=U); do
            res_cfiles=$(( res_cfiles + 1 ))
            _h=$(count_hunks "$_r/$_f")
            _l=$(count_clines "$_r/$_f")
            res_hunks=$(( res_hunks + _h ))
            res_clines=$(( res_clines + _l ))
            resolve_keeping_both "$_r/$_f"
            git -C "$_r" add -- "$_f" >/dev/null 2>&1
        done
        git -C "$_r" commit -q --no-edit >/dev/null 2>&1 || true
    done
    [ "$keep" = 1 ] || rm -rf "$_r"
}

# ── 能力検査。**測る前に「守りたい性質が働くか」を確かめる** ───────
# 同じファイルを 2 タスクが要求したら、naive は両方配り、coord は
# 片方を割らなければならない。ここが崩れていたら測定と呼ばない。
cap=$tmp/cap.tasks
printf 'a\tsrc/x.rs,src/y.rs\nb\tsrc/x.rs,src/z.rs\n' > "$cap"
decide naive "$cap" "$tmp/cap.naive" || { echo "能力検査: naive が動きません" >&2; exit 21; }
decide coord "$cap" "$tmp/cap.coord" || { echo "能力検査: coord が動きません" >&2; exit 21; }
cap_n=$(awk -F'\t' '$1=="summary"{print}' "$tmp/cap.naive")
cap_c=$(awk -F'\t' '$1=="summary"{print}' "$tmp/cap.coord")
case "$cap_n" in *'full=2'*) ;; *) echo "能力検査: naive が 2 件配っていません ($cap_n)" >&2; exit 21 ;; esac
case "$cap_c" in *'split=1'*) ;; *) echo "能力検査: coord が分割していません ($cap_c)" >&2; exit 21 ;; esac
grep -q '^serial	b	src/x.rs' "$tmp/cap.coord" || {
    echo "能力検査: coord が直列送りを出していません" >&2
    sed -n '1,12p' "$tmp/cap.coord" >&2
    exit 21
}

# ── 本測定 ────────────────────────────────────────────────────────
{
    printf 'テストバイナリ: %s\n' "$tbin"
    printf '担当表: タスク %s / ファイル %s / 1 タスク %s 個 / 重なり %s / セッション %s\n' \
        "$tasks" "$files" "$per" "$overlap" "$agents"
    printf '種: %s   時計: %s%s\n' "$seeds" "$clock" \
        "$([ "$clock" = none ] && printf ' (秒精度。時間の列は粗い)' || true)"
    [ -n "$testbin_note" ] && printf 'テストバイナリの選定: %s\n' "$testbin_note"
    printf '\n'
} >&2

jrows=""
fail=0
for e in $edits; do
    {
        printf '== 編集規則 %s ==\n' "$e"
        printf '%-8s %-6s | %6s %6s %6s %6s | %5s | %5s %5s %5s %7s | %8s\n' \
            種 段 衝突枝 衝突F ハンク 衝突行 二重 配布 分割 拒否 書込P 判定ms
    } >&2
    for s in $seeds; do
        gen_tasks "$s" "$tmp/$s.tasks"
        base_ms=0
        for m in naive coord; do
            run_arm "$m" "$e" "$s"
            [ "$m" = naive ] && base_ms=$res_decide_ms
            net_ms=$(( res_decide_ms - base_ms ))
            printf '%-8s %-6s | %6s %6s %6s %6s | %5s | %5s %5s %5s %7s | %8s\n' \
                "$s" "$m" "$res_cmerges" "$res_cfiles" "$res_hunks" "$res_clines" \
                "$res_dup" "$res_full" "$res_split" "$res_refused" "$res_granted" \
                "$res_decide_ms" >&2
            # **配ったものが互いに素かをハーネスが独立に検査する** (§3.11.5)。
            if [ "$m" = coord ] && [ "$res_dup" -ne 0 ]; then
                echo "失格: coord が同じファイルを $res_dup 件、2 つ以上の枝へ配りました" >&2
                fail=22
            fi
            jrows="$jrows{\"edit\":\"$e\",\"seed\":$s,\"arm\":\"$m\",\"conflict_merges\":$res_cmerges,\"conflict_files\":$res_cfiles,\"hunks\":$res_hunks,\"conflict_lines\":$res_clines,\"dup_files\":$res_dup,\"written_files\":$res_wfiles,\"full\":$res_full,\"split\":$res_split,\"refused\":$res_refused,\"granted_patterns\":$res_granted,\"dropped_patterns\":$res_dropped,\"decide_ms\":$res_decide_ms,\"decide_net_ms\":$net_ms},"
        done
        printf '%-8s %-6s | %6s %6s %6s %6s | %5s | %5s %5s %5s %7s | %8s\n' \
            "$s" "判定差" "-" "-" "-" "-" "-" "-" "-" "-" "-" "$net_ms" >&2
    done
    printf '\n' >&2
done

{
    printf 'ハーネスの費用: naive 段は「同じ起動・同じ解析・判定なし」の空回しです。\n'
    printf '  判定ms の naive がハーネスの床で、coord - naive が判定の正味です。\n'
    printf '二重 (dup) は「2 つ以上の枝へ同じファイルを配った件数」。\n'
    printf '  **coord でこれが 0 でなければ、衝突が 0 でもその段は失格**です。\n'
} >&2

if [ "$json" = 1 ]; then
    printf '{"tasks":%s,"files":%s,"per":%s,"overlap":%s,"agents":%s,"test_bin":"%s","rows":[%s]}\n' \
        "$tasks" "$files" "$per" "$overlap" "$agents" "$tbin" "${jrows%,}"
fi

exit "$fail"
