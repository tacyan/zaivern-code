#!/usr/bin/env sh
# 🧵 鎖④ (**共有面の自動解決**) を、**このリポジトリ自身の実際の共有面**で測る。
#
# ## なぜ別に要るのか
#
# * `tools/conflict-zero-bench.sh` の合成リポジトリは `let value = N;` の
#   **書き換え**しかしないので union の守備範囲の外で、効果ゼロにしかならない
#   (`docs/conflict-zero.md` §3.7)
# * `tools/union-bench.sh` は共有面を**模した**フィクスチャで測る。形は似せて
#   あるが、**本物の `src/config.rs` は 6900 行あって周りに何百行も別の中身が
#   あり、`src/keybinds.rs` の配列は 89 要素**という、フィクスチャには無い条件が
#   落ちている
#
# ここは CLAUDE.md が「ゼロにできていない共有面が 2 つ残る」と名指ししている
# **その 2 つのファイルそのもの**と、「新しい機能は `src/features/<名前>.rs` を
# 新規作成するだけ」という**構造的な解**を、同じ土俵で並べる。
#
# ## 測る共有面 (すべて実物)
#
#   config    `src/config.rs`   `struct Config` へ 1 フィールド追加 +
#                               `impl Default for Config` へ 1 行追加 (2 箇所)
#   keybinds  `src/keybinds.rs` `enum BindAction` へ 1 variant 追加 +
#                               `ALL_ACTIONS` へ 1 行追加 +
#                               **固定長 `[BindAction; N]` の N を書き換え** (3 箇所)
#   features  `src/features/`   `bench_feat_<i>.rs` を**新規作成するだけ**
#
# `features` を必ず入れてあるのは、**「共有ファイルを 1 バイトも触らない」設計が
# ドライバより強いこと**を同じ数字で見せるため。マージドライバは共有面の傷を
# 縫うが、傷を作らない構造には勝てない。
#
# ## 条件
#
#   baseline      ドライバ無し (素の git)。**時間のハーネス床でもある**
#   union-auto    `zai merge-driver --auto`。**実物にはマーカが無い**ので、
#                 マーカ無しで一覧を見つけられるかどうかが問われる
#   union-marked  `zaivern:union-begin/end` を base に入れてから
#                 `zai merge-driver`。プロジェクトが採用した場合の上限
#
# ## 衝突数だけを見ない (§3.11.5 の教訓)
#
# `verdict` は衝突数と**同時に中身**を見る。N 本の枝が 1 つずつ足したのだから、
# 統合後には N 個が**ちょうど 1 回ずつ**無ければならない。
# `keybinds` の宣言長 (`[BindAction; N]`) は**両側が同じ 90 を書く**ので
# git は綺麗に通すが、実際の要素数とはずれる。
# **「衝突 0」と「正しい」は別物**で、ここを混ぜたら嘘になる。
#
# ## 使い方 (再現は 1 行)
#
#   tools/shared-surface-bench.sh                    既定 (4 8 人)
#   tools/shared-surface-bench.sh --writers "2 4 8 16"
#   tools/shared-surface-bench.sh --surfaces config
#   tools/shared-surface-bench.sh --repo /path/to/other-repo
#   tools/shared-surface-bench.sh --json
#   tools/shared-surface-bench.sh --keep
#
# ## 副作用を持たない作り
#
#   * **対象リポジトリを 1 バイトも汚さない。** `git clone --no-hardlinks` で
#     一時領域へ複製し、複製の側でしか作業しない
#   * 改行は保存する。差し込む行は**その場所の改行 (CRLF/LF) に合わせる**
#     (過去に CRLF を全部 LF へ書き換えて「証明できず」と誤判定した事故がある)
#   * HOME / ZAIVERN_HOME を一時ディレクトリへ差し替える
#     (**本物の ~/.zaivern と ~/.gitconfig に 1 バイトも触らない**)
#   * cargo を呼ばない。既にある zai を使うだけ
#
# ## 終了コード
#
#   0  測れた   1  失敗   2  引数が変
#   20 使える zai が無い   21 能力検査に落ちた   23 共有面の錨が見つからない
# shellcheck disable=SC1007  # `CDPATH= cd` は「その cd にだけ空の CDPATH を渡す」正しい書き方
# shellcheck disable=SC2086  # `for w in $writers` は意図的な語分割
set -eu

writers_list="4 8"
surfaces="config keybinds features"
conds="baseline union-auto union-marked"
repo=""
json=0
keep=0

usage() {
    sed -n '2,70p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
}

while [ $# -gt 0 ]; do
    case "$1" in
        --writers)  writers_list=${2:-}; shift 2 ;;
        --surfaces) surfaces=${2:-}; shift 2 ;;
        --conds)    conds=${2:-}; shift 2 ;;
        --repo)     repo=${2:-}; shift 2 ;;
        --json)     json=1; shift ;;
        --keep)     keep=1; shift ;;
        -h | --help) usage ;;
        *) echo "不明な引数: $1 (--help で使い方)" >&2; exit 2 ;;
    esac
done

for w in $writers_list; do
    case "$w" in
        '' | *[!0-9]*) echo "--writers は正の整数の並びです: $w" >&2; exit 2 ;;
    esac
    [ "$w" -ge 2 ] || { echo "--writers は 2 以上 (1 人では衝突が定義できません): $w" >&2; exit 2; }
done
for s in $surfaces; do
    case "$s" in config | keybinds | features) ;; *) echo "--surfaces は config / keybinds / features: $s" >&2; exit 2 ;; esac
done

command -v git >/dev/null 2>&1 || { echo "git がありません。" >&2; exit 1; }
command -v awk >/dev/null 2>&1 || { echo "awk がありません。" >&2; exit 1; }

# shellcheck disable=SC1007
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-}
[ -n "$target_dir" ] || target_dir="$root/target"
[ -n "$repo" ] || repo=$root

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

# この計測の適格判定: `merge-driver` を**実際に走らせて**使い方が出ること。
# 「サブコマンドを知っている」だけでは、知らない語を GUI 起動と解釈する
# 古い実行ファイルを掴んでしまう。
union_capable() {
    _o=$("$1" merge-driver 2>&1 || true)
    case "$_o" in
        *merge-driver*) return 0 ;;
    esac
    return 1
}

zai=""
if ! zai_pick union_capable; then
    zai_identity "" >&2
    echo "使える zai がありません。ZAIVERN_BIN で指定してください。" >&2
    echo "  直し方: cargo build --bin zai" >&2
    exit 20
fi

# ── ミリ秒時計 ────────────────────────────────────────────────────
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
        date) _n=$(date +%s%N); printf '%s\n' "${_n%??????}" ;;
        perl) perl -MTime::HiRes=time -e 'printf "%.0f\n", time()*1000' ;;
        *) printf '%s000\n' "$(date +%s)" ;;
    esac
}

# ── 一時領域 ──────────────────────────────────────────────────────
tmp=$(mktemp -d "${TMPDIR:-/tmp}/zv-shared-bench.XXXXXX")
cleanup() { [ "$keep" = 1 ] || rm -rf "$tmp"; }
trap cleanup EXIT INT TERM
mkdir -p "$tmp/home" "$tmp/zhome"
HOME=$tmp/home
ZAIVERN_HOME=$tmp/zhome
export HOME ZAIVERN_HOME
GIT_CONFIG_NOSYSTEM=1; export GIT_CONFIG_NOSYSTEM
GIT_TERMINAL_PROMPT=0; export GIT_TERMINAL_PROMPT

# **対象を 1 バイトも汚さない。** 複製の側でしか作業しない。
R=$tmp/clone
git clone -q --no-hardlinks --local "$repo" "$R" 2>/dev/null || {
    echo "複製できませんでした: $repo" >&2
    exit 1
}
git -C "$R" config user.email bench@example.invalid
git -C "$R" config user.name bench
git -C "$R" config commit.gpgsign false
git -C "$R" checkout -q -B zvbase

CONFIG_RS=src/config.rs
KEYBINDS_RS=src/keybinds.rs
FEATURES_DIR=src/features

# ── 錨の確認。**無ければ「測った」と言わずに降りる** ───────────────
have_surface() {
    case "$1" in
        config)
            [ -f "$R/$CONFIG_RS" ] || return 1
            grep -q '^pub struct Config {' "$R/$CONFIG_RS" || return 1
            grep -q '^impl Default for Config {' "$R/$CONFIG_RS" || return 1
            ;;
        keybinds)
            [ -f "$R/$KEYBINDS_RS" ] || return 1
            grep -q '^pub enum BindAction {' "$R/$KEYBINDS_RS" || return 1
            grep -q '^pub const ALL_ACTIONS: \[BindAction; [0-9]*\] = \[' "$R/$KEYBINDS_RS" || return 1
            ;;
        features)
            [ -d "$R/$FEATURES_DIR" ] || return 1
            ;;
    esac
    return 0
}

kb_len() {
    awk -F'[;]' '/^pub const ALL_ACTIONS: \[BindAction; [0-9]*\] = \[/ {
        n = $2; gsub(/[^0-9]/, "", n); print n; exit
    }' "$R/$KEYBINDS_RS"
}

# ── 行の差し込み。**その場所の改行に合わせる** ────────────────────
#
# $1 ファイル / $2 開始行の**前方一致** / $3 終了行 (完全一致) / $4 差し込む行
#
# 照合を正規表現にしない。awk の動的正規表現は文字列としても 1 度解釈される
# ので `\[` が `[` に化けて「nonterminated character class」で落ちる
# (実際に踏んだ)。`[` `{` を含む Rust の宣言行を錨にする以上、**前方一致で
# 十分**であり、そのほうが OS ごとの awk の差も踏まない。
insert_before_end() {
    _f=$1
    awk -v st="$2" -v en="$3" -v ins="$4" '
        BEGIN { seen = 0; done_ = 0 }
        {
            l = $0
            cr = (substr(l, length(l), 1) == "\r") ? "\r" : ""
            bare = (cr == "") ? l : substr(l, 1, length(l) - 1)
            if (!seen && index(bare, st) == 1) { seen = 1; print l; next }
            if (seen && !done_ && bare == en) { printf "%s%s\n", ins, cr; done_ = 1 }
            print l
        }
        END { if (!done_) exit 3 }
    ' "$_f" > "$_f.n" || { rm -f "$_f.n"; return 1; }
    mv "$_f.n" "$_f"
}

# 共有面の錨 (前方一致で使う)。
ANCHOR_STRUCT='pub struct Config {'
ANCHOR_DEFAULT='impl Default for Config {'
ANCHOR_ENUM='pub enum BindAction {'
ANCHOR_ARRAY='pub const ALL_ACTIONS: [BindAction; '

# 既存行の**書き換え** ($1 ファイル / $2 元 / $3 先。どちらも固定文字列)。
replace_literal() {
    awk -v a="$2" -v b="$3" '
        {
            i = index($0, a)
            if (i > 0) $0 = substr($0, 1, i - 1) b substr($0, i + length(a))
            print
        }
    ' "$1" > "$1.n" && mv "$1.n" "$1"
}

# ── マーカを入れる (union-marked 条件だけ) ────────────────────────
add_markers() {
    case "$1" in
        config)
            insert_before_end "$R/$CONFIG_RS" "$ANCHOR_STRUCT" '}' '    // zaivern:union-end' || return 1
            insert_before_end "$R/$CONFIG_RS" "$ANCHOR_STRUCT" '    pub theme: String,' '    // zaivern:union-begin' || return 1
            insert_before_end "$R/$CONFIG_RS" "$ANCHOR_DEFAULT" '        }' '            // zaivern:union-end' || return 1
            insert_before_end "$R/$CONFIG_RS" "$ANCHOR_DEFAULT" '            theme: "zaivern-dark".into(),' '            // zaivern:union-begin' || return 1
            ;;
        keybinds)
            insert_before_end "$R/$KEYBINDS_RS" "$ANCHOR_ENUM" '}' '    // zaivern:union-end' || return 1
            insert_before_end "$R/$KEYBINDS_RS" "$ANCHOR_ENUM" '    Save,' '    // zaivern:union-begin' || return 1
            insert_before_end "$R/$KEYBINDS_RS" "$ANCHOR_ARRAY" '];' '    // zaivern:union-end' || return 1
            insert_before_end "$R/$KEYBINDS_RS" "$ANCHOR_ARRAY" '    BindAction::Save,' '    // zaivern:union-begin' || return 1
            ;;
        features) : ;;
    esac
    return 0
}

# ── 書き手 $2 の変更を、共有面 $1 へ適用する ──────────────────────
#
# **マーカがある条件では、マーカの内側へ入れる。** 外側へ足すと
# 「マーカを入れたのに効かない」という、原因が配管でも方針でもない
# ただのハーネスのバグを測ることになる (実際に 1 度そうなった)。
apply_writer() {
    case "$1" in
        config)
            _end='}'; [ "$MARK" = 1 ] && _end='    // zaivern:union-end'
            insert_before_end "$R/$CONFIG_RS" "$ANCHOR_STRUCT" "$_end" \
                "    pub bench_opt_$2: bool," || return 1
            _end='        }'; [ "$MARK" = 1 ] && _end='            // zaivern:union-end'
            insert_before_end "$R/$CONFIG_RS" "$ANCHOR_DEFAULT" "$_end" \
                "            bench_opt_$2: false," || return 1
            ;;
        keybinds)
            _end='}'; [ "$MARK" = 1 ] && _end='    // zaivern:union-end'
            insert_before_end "$R/$KEYBINDS_RS" "$ANCHOR_ENUM" "$_end" \
                "    BenchAct$2," || return 1
            _end='];'; [ "$MARK" = 1 ] && _end='    // zaivern:union-end'
            insert_before_end "$R/$KEYBINDS_RS" "$ANCHOR_ARRAY" "$_end" \
                "    BindAction::BenchAct$2," || return 1
            # **固定長の N を書き換える。** どの枝も「今の N + 1」を書くので、
            # git は綺麗に通すが実際の要素数とはずれる (kb_same 型)。
            replace_literal "$R/$KEYBINDS_RS" "[BindAction; $KB0]" "[BindAction; $(( KB0 + 1 ))]"
            ;;
        features)
            printf 'pub const ID: &str = "bench_feat_%s";\n' "$2" > "$R/$FEATURES_DIR/bench_feat_$2.rs"
            ;;
    esac
    return 0
}

# ── 衝突の解決 (全条件で同じ規則) ─────────────────────────────────
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

# ドライバは**小さな包み**越しに起こす。呼ばれた回数を数えないと
# 「効かなかった」の原因が「配管が繋がっていない」なのか
# 「ドライバが自分で降りた」なのか区別できない (ここを混ぜると
# §3.7 の「効果ゼロ」がどちらの意味なのか永久に分からない)。
DRVLOG=$tmp/driver.log
make_wrapper() {
    _flags=$1
    {
        printf '#!/bin/sh\n'
        # shellcheck disable=SC2016  # `$5` は**包みの中で**展開する (ここでは展開しない)
        printf 'printf "%%s\\n" "$5" >> %s\n' "$DRVLOG"
        printf 'exec "%s" merge-driver %s "$@"\n' "$zai" "$_flags"
    } > "$tmp/drv.sh"
    chmod +x "$tmp/drv.sh"
}

install_driver() {
    make_wrapper "$1"
    git -C "$R" config merge.zaivern-union.name "bench union driver"
    git -C "$R" config merge.zaivern-union.driver "'$tmp/drv.sh' %O %A %B %L %P"
    {
        printf '%s merge=zaivern-union\n' "$CONFIG_RS"
        printf '%s merge=zaivern-union\n' "$KEYBINDS_RS"
    } > "$R/.gitattributes"
}

# ── 1 条件 1 共有面 1 人数ぶんの実験 ──────────────────────────────
run_case() {
    _surf=$1; _cond=$2; _n=$3
    _tag="$_surf-$_cond-$_n"

    git -C "$R" checkout -q -f zvbase
    git -C "$R" clean -qfd >/dev/null 2>&1 || true
    git -C "$R" checkout -q -B "b-$_tag" zvbase

    KB0=$(kb_len)
    rm -f "$R/.gitattributes"
    : > "$DRVLOG"
    MARK=0
    case "$_cond" in
        baseline) ;;
        union-auto) install_driver --auto ;;
        union-marked)
            MARK=1
            install_driver ""
            add_markers "$_surf" || { echo "マーカの錨が見つかりません ($_surf)" >&2; exit 23; }
            ;;
    esac
    if [ -n "$(git -C "$R" status --porcelain)" ]; then
        git -C "$R" add -A >/dev/null 2>&1
        git -C "$R" commit -q -m "base $_tag"
    fi

    _branches=""
    _i=1
    while [ "$_i" -le "$_n" ]; do
        git -C "$R" checkout -q -B "w-$_tag-$_i" "b-$_tag"
        KB0=$(kb_len)
        apply_writer "$_surf" "$_i" || { echo "共有面の錨が見つかりません ($_surf)" >&2; exit 23; }
        git -C "$R" add -A >/dev/null 2>&1
        git -C "$R" commit -q -m "w$_i"
        _branches="$_branches w-$_tag-$_i"
        _i=$(( _i + 1 ))
    done

    res_cmerges=0; res_cfiles=0; res_hunks=0
    git -C "$R" checkout -q -B "i-$_tag" "b-$_tag"
    _t0=$(now_ms)
    for _b in $_branches; do
        if git -C "$R" merge -q --no-edit "$_b" >/dev/null 2>&1; then
            continue
        fi
        res_cmerges=$(( res_cmerges + 1 ))
        for _f in $(git -C "$R" diff --name-only --diff-filter=U); do
            res_cfiles=$(( res_cfiles + 1 ))
            _h=$(count_hunks "$R/$_f")
            res_hunks=$(( res_hunks + _h ))
            resolve_keeping_both "$R/$_f"
            git -C "$R" add -- "$_f" >/dev/null 2>&1
        done
        git -C "$R" commit -q --no-edit >/dev/null 2>&1 || true
    done
    _t1=$(now_ms)
    res_ms=$(( _t1 - _t0 ))
    # `grep -c` は 0 件でも "0" を出したうえで rc=1 を返す。`|| echo 0` を
    # 足すと **"0\n0" になって表が崩れる** (実際に崩れた)。`|| true` で受ける。
    res_drv=$(grep -c . "$DRVLOG" 2>/dev/null || true)
    [ -n "$res_drv" ] || res_drv=0

    # ── 中身の検算。**衝突 0 と「正しい」は別物** ──────────────────
    res_kept=0; res_dupent=0; res_declared="-"; res_want="-"; res_ok=OK
    case "$_surf" in
        config)
            res_kept=$(grep -c '^    pub bench_opt_[0-9]*: bool,$' "$R/$CONFIG_RS" || true)
            _uniq=$(grep '^    pub bench_opt_[0-9]*: bool,$' "$R/$CONFIG_RS" | sort -u | grep -c . || true)
            res_dupent=$(( res_kept - _uniq ))
            _d=$(grep -c '^            bench_opt_[0-9]*: false,$' "$R/$CONFIG_RS" || true)
            [ "$res_kept" -eq "$_n" ] && [ "$_d" -eq "$_n" ] && [ "$res_dupent" -eq 0 ] || res_ok=NG
            ;;
        keybinds)
            res_kept=$(grep -c '^    BindAction::BenchAct[0-9]*,$' "$R/$KEYBINDS_RS" || true)
            _uniq=$(grep '^    BindAction::BenchAct[0-9]*,$' "$R/$KEYBINDS_RS" | sort -u | grep -c . || true)
            res_dupent=$(( res_kept - _uniq ))
            _e=$(grep -c '^    BenchAct[0-9]*,$' "$R/$KEYBINDS_RS" || true)
            res_declared=$(kb_len)
            # 実際の要素数 = 元の宣言長 + 残った追記
            res_want=$(( KB_ORIG + res_kept ))
            [ "$res_kept" -eq "$_n" ] && [ "$_e" -eq "$_n" ] && [ "$res_dupent" -eq 0 ] || res_ok=NG
            [ "$res_declared" = "$res_want" ] || res_ok="NG(長さ)"
            ;;
        features)
            res_kept=$(find "$R/$FEATURES_DIR" -name 'bench_feat_*.rs' | grep -c . || true)
            [ "$res_kept" -eq "$_n" ] || res_ok=NG
            ;;
    esac
    _left=$(grep -rl '^<<<<<<< ' "$R/src" 2>/dev/null | grep -c . || true)
    [ "$_left" -eq 0 ] || res_ok="NG(マーカ残)"
}

# ── 能力検査。**測る前にドライバが本当に働くか確かめる** ───────────
cap=$tmp/cap
mkdir -p "$cap"
printf 'a\nb\n' > "$cap/base.txt"
printf 'a\nx\nb\n' > "$cap/ours.txt"
printf 'a\ny\nb\n' > "$cap/theirs.txt"
cp "$cap/ours.txt" "$cap/A.txt"
"$zai" merge-driver --auto "$cap/base.txt" "$cap/A.txt" "$cap/theirs.txt" 7 list.txt >/dev/null 2>&1 || true
if ! grep -q '^x$' "$cap/A.txt" || ! grep -q '^y$' "$cap/A.txt"; then
    echo "能力検査: merge-driver --auto が両側の追記を残しませんでした" >&2
    cat "$cap/A.txt" >&2
    exit 21
fi

KB_ORIG=0
if have_surface keybinds; then KB_ORIG=$(kb_len); fi

{
    zai_identity "$zai"
    printf '対象リポジトリ: %s (複製 %s)\n' "$repo" "$R"
    printf 'HEAD: %s\n' "$(git -C "$R" rev-parse --short HEAD)"
    [ "$KB_ORIG" != 0 ] && printf 'ALL_ACTIONS の元の宣言長: %s\n' "$KB_ORIG"
    printf '時計: %s\n\n' "$clock"
} >&2

jrows=""
for surf in $surfaces; do
    if ! have_surface "$surf"; then
        printf '共有面 %s の錨がこのリポジトリにありません — 測っていません\n\n' "$surf" >&2
        continue
    fi
    {
        printf '== 共有面 %s ==\n' "$surf"
        printf '%-14s %3s | %5s %6s %6s %6s | %5s %5s %7s %7s | %8s %7s\n' \
            条件 人数 駆動 衝突M 衝突F ハンク 残った 重複 宣言長 あるべき 検算 統合ms
    } >&2
    base_ms=0
    for n in $writers_list; do
        for c in $conds; do
            run_case "$surf" "$c" "$n"
            [ "$c" = baseline ] && base_ms=$res_ms
            printf '%-14s %3s | %5s %6s %6s %6s | %5s %5s %7s %7s | %8s %7s\n' \
                "$c" "$n" "$res_drv" "$res_cmerges" "$res_cfiles" "$res_hunks" \
                "$res_kept" "$res_dupent" "$res_declared" "$res_want" "$res_ok" "$res_ms" >&2
            jrows="$jrows{\"surface\":\"$surf\",\"cond\":\"$c\",\"writers\":$n,\"driver_calls\":$res_drv,\"conflict_merges\":$res_cmerges,\"conflict_files\":$res_cfiles,\"hunks\":$res_hunks,\"kept\":$res_kept,\"dup_entries\":$res_dupent,\"declared\":\"$res_declared\",\"want\":\"$res_want\",\"verdict\":\"$res_ok\",\"merge_ms\":$res_ms,\"merge_net_ms\":$(( res_ms - base_ms ))},"
        done
    done
    printf '\n' >&2
done

{
    printf 'ハーネスの費用: baseline はドライバのプロセスを 1 つも起こさない空回しです。\n'
    printf '  統合ms の baseline が床で、union-* との差がドライバの正味です。\n'
    printf '駆動 = ドライバが実際に起こされた回数。**0 なら配管が繋がっていない**、\n'
    printf '  0 でないのにハンクが減らないなら**ドライバが自分で降りた**という意味です。\n'
    printf '検算は衝突数と**別**に見ています。N 本の枝が 1 つずつ足したのだから、\n'
    printf '  統合後には N 個がちょうど 1 回ずつ無ければ NG です。\n'
    printf '  NG(長さ) は「衝突は 0 だが [BindAction; N] の N が実際の要素数と違う」。\n'
} >&2

if [ "$json" = 1 ]; then
    printf '{"repo":"%s","zai":"%s","kb_orig":%s,"rows":[%s]}\n' \
        "$repo" "$zai" "$KB_ORIG" "${jrows%,}"
fi
