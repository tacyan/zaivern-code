#!/usr/bin/env sh
# 「競合ゼロ」のベンチを **ホストと Linux コンテナの両方**で走らせて並べる。
#
# ## なぜ要るか
#
# 競合ゼロの数字 (`docs/conflict-zero.md` / `docs/conflict-bench.md`) は
# **全部 macOS でしか測っていない**。だがこの機能が寄りかかっているのは
# 言語機能ではなく**ファイルシステムの挙動**である:
#
#   * `create_new` / `rename` のアトミック性
#   * 削除の見え方 (Windows は *delete pending* を経て `ACCESS_DENIED` を返す)
#   * ロックの取り合いで何が返るか (`lease::lock_contended`)
#
# OS が違えば数字も、場合によっては**結論も**変わりうる。片方の OS の数字を
# 「実測」と書くのは、測っていない側について黙って嘘をつくことになる。
#
# ## 何をするか
#
#   1. ホスト側で `zai` を用意する (無ければビルドする)
#   2. **同じソースから** Linux コンテナ内でも `zai` をビルドする
#   3. 両側で `tools/conflict-zero-bench.sh --json` と
#      `tools/coedit-bench.sh --json` を同じ引数で走らせる
#   4. 並べた表と JSON を出す
#
# ## 判定の線引き (**ここを間違えると嘘の赤が出る**)
#
# OS 間で必ずずれるもの (時間・スケジューリングに依存するもの) で合否を
# 決めない。CLAUDE.md の「絶対時間で性能テストの線を引かない」と同じ理由で、
# **壁時計・ゲートの待ち時間・混雑による拒否数は情報として出すだけ**にする。
#
#   ✗ 落とす (主張が壊れている): 衝突ハンク / 2 人以上が書いたファイル /
#     衝突したマージ / 範囲外配分 / 契約違反 / 近すぎる配分 / ベンチ自体の終了コード
#   △ 出すだけ (OS 差として当然ありうる): 書けた件数 / 断った件数 / 壁時計 /
#     ゲートの p50・p95・max / ずらした距離
#
# ## 使い方
#
#   tools/xplat-bench.sh                          既定 (8,16 体)
#   tools/xplat-bench.sh --writers 8,16,32,64     掃引する
#   tools/xplat-bench.sh --json                   JSON は stdout、表は stderr
#   tools/xplat-bench.sh --bench cz               conflict-zero だけ
#   tools/xplat-bench.sh --host-only              Docker を使わない
#   tools/xplat-bench.sh --linux-fs overlay       コンテナ側を overlayfs で測る
#   tools/xplat-bench.sh --help
#
# ## 副作用を持たない作り
#
#   * 作業場は `mktemp -d` (= `$TMPDIR` 由来)。**パスを 1 つも直書きしない**。
#     リポジトリのルートは `git rev-parse --show-toplevel` から導く
#   * コンテナは **ホストの `target/` を 1 バイトも触らない** (`CARGO_TARGET_DIR`
#     をワークツリーごとの名前付きボリュームへ分ける。`tools/linux-test.sh` と同じ流儀)
#   * 下位のベンチが `HOME` を一時ディレクトリへ差し替えるので、本物の
#     `~/.zaivern` と `~/.gitconfig` には触らない
#   * Docker が無い / 起動していないときは **skip の理由を名指しして続行**する
#
# ## 終了コード
#
#   0  両側 (または片側 + 明示された skip) が走り、主張が壊れていない
#   1  主張が壊れた。落とす側の指標が食い違ったか、下位ベンチが 0 以外を返した
#   2  引数の指定ミス
#   3  前提が無い (git / python3 が無い、--linux-only なのに Docker が無い 等)
set -eu

writers=8,16
files=""
lines=""
overlap=0.5
seed=20260810
coedit_mode=all
coedit_layout=both
bench=both
sides=both
build=auto
linux_fs=volume
json=0
keep=0
image=${ZAIVERN_LINUX_IMAGE:-rust:1.90-slim}

usage() {
    cat <<'EOS'
使い方: tools/xplat-bench.sh [オプション]

  --writers N[,N...]  同時に書く体数。カンマ区切りで掃引 (既定 8,16)
  --files N           conflict-zero のファイル数 (既定: 48 と体数の大きい方)
  --lines N           coedit の合成ファイル行数 (既定: 800 と 体数×32 の大きい方)
  --overlap 0..1      conflict-zero の重なり具合 (既定 0.5)
  --seed N            乱数の種 (既定 20260810)。両側へ同じ値を渡す
  --mode <list>       coedit の段 (既定 all)。そのまま coedit-bench へ渡す
  --layout <l>        coedit の並べ方 (既定 both)。そのまま渡す
  --bench cz|coedit|both
                      走らせるベンチ (既定 both)
  --host-only         コンテナ側を走らせない
  --linux-only        ホスト側を走らせない (Docker が無ければ終了コード 3)
  --linux-fs volume|overlay
                      コンテナ側の作業場 (既定 volume = VM の実ファイルシステム。
                      overlay はコンテナの書き込み層)
  --image <img>       コンテナの土台イメージ (環境変数 ZAIVERN_LINUX_IMAGE でも可)
  --no-build          zai をビルドしない。既にあるものだけを使う
  --build             既にあっても必ずビルドし直す
  --json              JSON を stdout へ、人が読む表を stderr へ
  --keep              JSON と生ログを残す (場所を最後に表示)
  -h, --help          この使い方

環境変数 ZAIVERN_BIN でホスト側の zai を明示できます (版の照合を飛ばします)。
EOS
}

die2() {
    echo "$1" >&2
    usage >&2
    exit 2
}

need_val() {
    [ "$2" -ge 2 ] || die2 "$1 に値がありません"
}

while [ $# -gt 0 ]; do
    case "$1" in
    --writers) need_val "$1" $#; writers=$2; shift 2 ;;
    --files) need_val "$1" $#; files=$2; shift 2 ;;
    --lines) need_val "$1" $#; lines=$2; shift 2 ;;
    --overlap) need_val "$1" $#; overlap=$2; shift 2 ;;
    --seed) need_val "$1" $#; seed=$2; shift 2 ;;
    --mode) need_val "$1" $#; coedit_mode=$2; shift 2 ;;
    --layout) need_val "$1" $#; coedit_layout=$2; shift 2 ;;
    --bench) need_val "$1" $#; bench=$2; shift 2 ;;
    --image) need_val "$1" $#; image=$2; shift 2 ;;
    --linux-fs) need_val "$1" $#; linux_fs=$2; shift 2 ;;
    --host-only) sides=host; shift ;;
    --linux-only) sides=linux; shift ;;
    --no-build) build=never; shift ;;
    --build) build=force; shift ;;
    --json) json=1; shift ;;
    --keep) keep=1; shift ;;
    -h | --help) usage; exit 0 ;;
    *) die2 "知らない引数です: $1" ;;
    esac
done

case "$bench" in cz | coedit | both) ;; *) die2 "--bench は cz / coedit / both です: $bench" ;; esac
case "$linux_fs" in volume | overlay) ;; *) die2 "--linux-fs は volume / overlay です: $linux_fs" ;; esac

is_num() {
    case "$1" in
    '' | *[!0-9]*) return 1 ;;
    *) return 0 ;;
    esac
}

writer_list=$(printf '%s' "$writers" | tr ',' ' ')
for w in $writer_list; do
    is_num "$w" && [ "$w" -ge 2 ] || die2 "--writers は 2 以上の整数です: $w"
done
[ -n "$files" ] && { is_num "$files" || die2 "--files は整数です: $files"; }
[ -n "$lines" ] && { is_num "$lines" || die2 "--lines は整数です: $lines"; }
is_num "$seed" || die2 "--seed は整数です: $seed"

# ── 前提 ──────────────────────────────────────────────────────────
# python3 は (a) 版の照合を時間切れ付きで撃つため (b) 2 つの JSON を突き合わせる
# ため。git は下位ベンチが要求する。**どちらも「無ければ静かに飛ばす」にしない。**
for need in python3 git; do
    if ! command -v "$need" >/dev/null 2>&1; then
        echo "$need が見つかりません。この計測には $need が要ります。" >&2
        exit 3
    fi
done

# リポジトリのルート。**パスを直書きしない。** git が使えない場所に置かれた
# 場合だけスクリプトの位置から導く。
# shellcheck disable=SC1007  # `CDPATH= cd` は「その cd にだけ空の CDPATH を渡す」正しい書き方
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
root=$(git -C "$script_dir" rev-parse --show-toplevel 2>/dev/null || true)
# shellcheck disable=SC1007
[ -n "$root" ] || root=$(CDPATH= cd -- "$script_dir/.." && pwd)

cz_bench="$root/tools/conflict-zero-bench.sh"
coedit_bench="$root/tools/coedit-bench.sh"
for f in "$cz_bench" "$coedit_bench"; do
    [ -f "$f" ] || {
        echo "下位のベンチが見つかりません: $f" >&2
        exit 3
    }
done

work=$(mktemp -d 2>/dev/null || mktemp -d -t xplatbench)
out="$work/out"
mkdir -p "$out"
# shellcheck disable=SC2329  # trap から呼ぶ
cleanup() {
    if [ "$keep" = 1 ]; then
        echo "== 結果と生ログを残しました: $work" >&2
    else
        rm -rf "$work"
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# 時間切れ付きでコマンドを撃つ。**古い zai は知らない語をワークスペース指定と
# して扱い GUI を起こす**ので、版の照合そのものが固まりうる。
# `timeout(1)` は macOS の既定に無いので python3 で行う (下位ベンチと同じ作法)。
cap_run() {
    python3 - "$@" <<'EOS'
import subprocess, sys
try:
    p = subprocess.run(sys.argv[2:], capture_output=True, text=True,
                       timeout=float(sys.argv[1]), stdin=subprocess.DEVNULL)
except Exception:
    sys.exit(1)
if p.returncode != 0:
    sys.exit(1)
sys.stdout.write(p.stdout)
EOS
}

# ファイルシステムの種類。**分からないときは「不明」と書く。でっち上げない。**
fs_type() {
    d=$1
    t=""
    if [ "$(uname -s)" = Linux ]; then
        # /proc/mounts の実名を優先する (GNU stat は ext4 を "ext2/ext3" と呼ぶ)
        mp=$(df -P "$d" 2>/dev/null | awk 'NR==2 {print $6}')
        [ -n "$mp" ] && t=$(awk -v m="$mp" '$2 == m { print $3 }' /proc/mounts 2>/dev/null | tail -1)
        [ -n "$t" ] || t=$(stat -f -c %T "$d" 2>/dev/null || true)
    else
        # BSD / macOS: df でデバイスを引いて mount の括弧から種類を取る
        dev=$(df -P "$d" 2>/dev/null | awk 'NR==2 {print $1}')
        if [ -n "$dev" ]; then
            t=$(mount 2>/dev/null | awk -v d="$dev" '$1 == d {
                if (match($0, /\(([^,)]+)/)) print substr($0, RSTART + 1, RLENGTH - 1)
                exit
            }')
        fi
    fi
    [ -n "$t" ] || t="不明"
    printf '%s' "$t"
}

# 体数から下位ベンチの規模を決める。
#   conflict-zero は `--files >= --writers` が要る (既定 48)
#   coedit は行が足りないと行域を配れない (docs の例は 64 体で 2000 行)
files_for() {
    if [ -n "$files" ]; then printf '%s' "$files"; return; fi
    if [ "$1" -gt 48 ]; then printf '%s' "$1"; else printf '48'; fi
}
lines_for() {
    if [ -n "$lines" ]; then printf '%s' "$lines"; return; fi
    n=$(($1 * 32))
    if [ "$n" -gt 800 ]; then printf '%s' "$n"; else printf '800'; fi
}

# ── ホスト側の zai を決める ───────────────────────────────────────
#
# **版を照合する。** `target/release/zai` は前の実行の残骸かもしれず、
# 古い版は新しいサブコマンドを知らないので段がまるごと skip になる
# (このリポジトリで実際に起きている: release が 0.12.0 で debug が 0.14.0)。
# さらに**別のチェックアウトで作られた zai を拾うと、比較そのものが無意味**に
# なる (ホストと Linux で違うコードを測ってしまう)。なので既定では
# **このワークツリーの target 配下しか信用しない**。
target_dir=${CARGO_TARGET_DIR:-$root/target}

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

host_zai=""
host_zai_ver=""
host_zai_note=""

# この計測の適格判定: 実行できれば足りる (下位ベンチが能力検査を持っている)。
xplat_capable() { [ -x "$1" ]; }

# 共通ブロックの結果を、このスクリプトが使う `host_zai*` へ写す。
# **別のチェックアウトで作られた zai を拾うと比較そのものが無意味**になるので
# (ホストと Linux で違うコードを測る)、既定ではこのワークツリーの target
# 配下しか信用しない — その判断は共通ブロックの版・古さの照合が担う。
resolve_host_zai() {
    zai_note=""
    zai_pick xplat_capable || {
        host_zai=""
        host_zai_note=$zai_note
        return 1
    }
    host_zai=$zai
    host_zai_ver=$(printf '%s' "$zai_ver" | awk '{print $NF}')
    host_zai_note=$zai_note
    return 0
}

build_host_zai() {
    command -v cargo >/dev/null 2>&1 || {
        host_zai_note="${host_zai_note}${host_zai_note:+ / }cargo が無いのでビルドできません"
        return 1
    }
    echo "== ホスト側の zai をビルドします (このワークツリーの target)" >&2
    rc=0
    (cd "$root" && CARGO_PROFILE_DEV_DEBUG=0 cargo build --bin zai) >"$out/host-build.log" 2>&1 || rc=$?
    if [ "$rc" != 0 ]; then
        echo "!! ホスト側のビルドに失敗しました (rc=$rc)。末尾:" >&2
        tail -20 "$out/host-build.log" >&2 || true
        return 1
    fi
    resolve_host_zai
}

if [ "$sides" != linux ]; then
    if [ "$build" = force ]; then
        build_host_zai || true
    elif ! resolve_host_zai; then
        if [ "$build" = never ]; then
            echo "!! ホスト側の zai が見つかりません (--no-build)。" >&2
            echo "   下位ベンチは PATH の zai を拾う可能性があります。**別の版かもしれません。**" >&2
        else
            build_host_zai || true
        fi
    fi
fi

host_os="$(uname -s) $(uname -m)"
host_fs=$(fs_type "$work")
host_subs=""
if [ -n "$host_zai" ]; then
    host_subs=$(cap_run 20 "$host_zai" help 2>/dev/null |
        awk '{ for (i = 1; i < NF; i++) if ($i == "zai" && $(i + 1) ~ /^[a-z][a-z-]+$/) print $(i + 1) }' |
        sort -u | tr '\n' ' ' || true)
fi

# ── ホスト側を走らせる ────────────────────────────────────────────
run_one() {
    # $1 = ラベル (host/linux は呼び分ける), $2 = 出力の接頭辞, 残り = コマンド
    label=$1
    prefix=$2
    shift 2
    rc=0
    if [ -n "$host_zai" ]; then
        ZAIVERN_BIN="$host_zai" "$@" >"$out/$prefix.json" 2>"$out/$prefix.log" || rc=$?
    else
        "$@" >"$out/$prefix.json" 2>"$out/$prefix.log" || rc=$?
    fi
    echo "$rc" >"$out/$prefix.rc"
    echo "   $label rc=$rc" >&2
}

host_ran=0
if [ "$sides" != linux ]; then
    echo "== ホスト ($host_os / $host_fs) で計測します" >&2
    echo "   zai: ${host_zai:-見つかりません}${host_zai_ver:+ ($host_zai_ver)}" >&2
    [ -n "$host_zai_note" ] && echo "   注意: $host_zai_note" >&2
    for w in $writer_list; do
        if [ "$bench" != coedit ]; then
            echo "== host conflict-zero --writers $w --files $(files_for "$w")" >&2
            run_one "conflict-zero($w)" "host-cz-$w" \
                sh "$cz_bench" --writers "$w" --files "$(files_for "$w")" \
                --overlap "$overlap" --seed "$seed" --json
        fi
        if [ "$bench" != cz ]; then
            echo "== host coedit --agents $w --lines $(lines_for "$w")" >&2
            run_one "coedit($w)" "host-coedit-$w" \
                sh "$coedit_bench" --agents "$w" --lines "$(lines_for "$w")" \
                --mode "$coedit_mode" --layout "$coedit_layout" --seed "$seed" --json
        fi
    done
    host_ran=1
fi

# ── Linux コンテナ側 ──────────────────────────────────────────────
linux_ran=0
linux_skip=""

docker_ok() {
    if ! command -v docker >/dev/null 2>&1; then
        linux_skip="docker コマンドがありません"
        return 1
    fi
    if ! docker info >/dev/null 2>&1; then
        linux_skip="docker は入っていますがデーモンが動いていません (Docker Desktop を起動してください)"
        return 1
    fi
    return 0
}

# 土台イメージに git と python3 があるか。**無ければ足した派生イメージを 1 度だけ作る。**
# conflict-zero-bench は python3 が無いと 1 行も走らない (rust:*-slim には入っていない)。
image_has_tools() {
    docker run --rm --entrypoint sh "$1" -c \
        'command -v git >/dev/null 2>&1 && command -v python3 >/dev/null 2>&1' >/dev/null 2>&1
}

prepare_image() {
    if image_has_tools "$image"; then return 0; fi
    derived="zaivern-xb-$(printf '%s' "$image" | cksum | cut -d' ' -f1)"
    if docker image inspect "$derived" >/dev/null 2>&1 && image_has_tools "$derived"; then
        image=$derived
        return 0
    fi
    echo "== $image に git / python3 がありません。足した派生イメージを 1 度だけ作ります: $derived" >&2
    rc=0
    printf 'FROM %s
RUN (command -v apt-get >/dev/null 2>&1 && apt-get update && apt-get install -y --no-install-recommends git python3 && rm -rf /var/lib/apt/lists/*) || (command -v apk >/dev/null 2>&1 && apk add --no-cache git python3)
' "$image" | docker build -q -t "$derived" - >/dev/null 2>&1 || rc=$?
    if [ "$rc" = 0 ] && image_has_tools "$derived"; then
        image=$derived
        return 0
    fi
    linux_skip="イメージに git / python3 を入れられませんでした ($image)。ZAIVERN_LINUX_IMAGE で両方入りのイメージを指定してください"
    return 1
}

if [ "$sides" != host ]; then
    if docker_ok && prepare_image; then
        # target はワークツリーごとの名前付きボリューム。**ホストの target/ を
        # 汚さない**のが要点 (linux-test.sh と同じ流儀・同じ命名)。
        if [ -n "${ZAIVERN_LINUX_TARGET:-}" ]; then
            target_mount=$ZAIVERN_LINUX_TARGET
            mkdir -p "$target_mount"
        else
            slug=$(printf '%s' "$root" | cksum | cut -d' ' -f1)
            target_mount="zaivern-lx-$(basename "$root")-$slug"
        fi
        registry_vol=${ZAIVERN_LINUX_REGISTRY:-zaivern-lx-cargo-registry}

        # コンテナの中で回す手順。**ホスト側で値を埋めて生成する**ので、
        # コンテナ側に変数が無くて見出しが空になる事故が起きない。
        # ここから先の '$...' は**コンテナ側で展開させたい**ので単引用のまま。
        # shellcheck disable=SC2016
        {
            echo '#!/bin/sh'
            echo 'set -eu'
            echo 'git config --global --add safe.directory /w >/dev/null 2>&1 || true'
            if [ "$linux_fs" = volume ]; then
                echo 'TMPDIR=/bench; export TMPDIR; mkdir -p "$TMPDIR"'
            fi
            echo 'uname -s > /out/linux-uname; uname -m >> /out/linux-uname'
            # GNU stat は ext4 を "ext2/ext3" と呼ぶ (magic が同じ) ので
            # /proc/mounts の実名を先に見る。無ければ stat へ落ちる。
            # awk のプログラムに単引用符が要るので、この塊だけ heredoc で出す。
            cat <<'XB_FS'
xb_fs=$(awk -v d="${TMPDIR:-/tmp}" '$2 == d { print $3; exit }' /proc/mounts 2>/dev/null || true)
[ -n "$xb_fs" ] || xb_fs=$(stat -f -c %T "${TMPDIR:-/tmp}" 2>/dev/null || echo 不明)
echo "$xb_fs" > /out/linux-fs
XB_FS
            if [ "$build" != never ]; then
                echo 'echo "== コンテナ内で zai をビルドします" >&2'
                echo 'cargo build --bin zai'
            fi
            echo 'zai=""'
            echo 'for c in /target/debug/zai /target/release/zai; do'
            echo '    [ -x "$c" ] && { zai=$c; break; }'
            echo 'done'
            echo 'if [ -n "$zai" ]; then'
            echo '    "$zai" --version > /out/linux-zai-version 2>/dev/null || true'
            echo '    "$zai" help > /out/linux-zai-help 2>/dev/null || true'
            echo '    ZAIVERN_BIN=$zai; export ZAIVERN_BIN'
            echo 'fi'
            echo 'echo "${zai:-}" > /out/linux-zai-path'
            for w in $writer_list; do
                if [ "$bench" != coedit ]; then
                    printf 'echo "== linux conflict-zero --writers %s" >&2\n' "$w"
                    printf 'rc=0; sh /w/tools/conflict-zero-bench.sh --writers %s --files %s --overlap %s --seed %s --json > /out/linux-cz-%s.json 2> /out/linux-cz-%s.log || rc=$?\n' \
                        "$w" "$(files_for "$w")" "$overlap" "$seed" "$w" "$w"
                    printf 'echo "$rc" > /out/linux-cz-%s.rc; echo "   conflict-zero(%s) rc=$rc" >&2\n' "$w" "$w"
                fi
                if [ "$bench" != cz ]; then
                    printf 'echo "== linux coedit --agents %s" >&2\n' "$w"
                    printf 'rc=0; sh /w/tools/coedit-bench.sh --agents %s --lines %s --mode %s --layout %s --seed %s --json > /out/linux-coedit-%s.json 2> /out/linux-coedit-%s.log || rc=$?\n' \
                        "$w" "$(lines_for "$w")" "$coedit_mode" "$coedit_layout" "$seed" "$w" "$w"
                    printf 'echo "$rc" > /out/linux-coedit-%s.rc; echo "   coedit(%s) rc=$rc" >&2\n' "$w" "$w"
                fi
            done
            echo 'exit 0'
        } >"$out/in-container.sh"
        chmod +x "$out/in-container.sh"

        echo "== Linux ($image) で計測します" >&2
        echo "   target:   $target_mount (ワークツリーごとに分離。2 回目以降は warm)" >&2
        echo "   registry: $registry_vol (全ワークツリーで共有)" >&2
        echo "   作業場:   $linux_fs" >&2
        # 作業場を VM の実ファイルシステム (ext4 等) に置くための無名ボリューム。
        # 付けないとコンテナの書き込み層 = overlayfs で測ることになる。
        # 値は固定の literal なので、語分割させるために**わざと引用しない**。
        bench_mount=""
        [ "$linux_fs" = volume ] && bench_mount="-v /bench"
        rc=0
        # `| head` を挟まない。挟むと $? がそちらのものになる。
        # shellcheck disable=SC2086  # $bench_mount は "-v /bench" か空。語分割させたい
        docker run --rm \
            -v "$root":/w -w /w \
            -v "$target_mount":/target \
            -v "$registry_vol":/usr/local/cargo/registry \
            -v "$out":/out \
            $bench_mount \
            -e CARGO_TARGET_DIR=/target \
            -e CARGO_PROFILE_DEV_DEBUG=0 \
            -e GIT_TERMINAL_PROMPT=0 \
            "$image" \
            sh /out/in-container.sh >"$out/linux-run.log" 2>&1 || rc=$?
        cat "$out/linux-run.log" >&2 || true
        if [ "$rc" != 0 ]; then
            linux_skip="コンテナの実行が失敗しました (rc=$rc)。上のログを見てください"
        else
            linux_ran=1
        fi
    fi
    if [ "$linux_ran" = 0 ] && [ -n "$linux_skip" ]; then
        echo "!! Linux 側は skip します: $linux_skip" >&2
        if [ "$sides" = linux ]; then
            echo "   --linux-only なので、ここで終わります。" >&2
            exit 3
        fi
    fi
fi

if [ "$host_ran" = 0 ] && [ "$linux_ran" = 0 ]; then
    echo "!! どちら側も走りませんでした。" >&2
    exit 3
fi

# ── 突き合わせ ────────────────────────────────────────────────────
verdict=0
python3 - \
    --out "$out" \
    --writers "$writers" \
    --overlap "$overlap" \
    --seed "$seed" \
    --bench "$bench" \
    --host-os "$host_os" \
    --host-fs "$host_fs" \
    --host-zai "${host_zai:-}" \
    --host-zai-ver "${host_zai_ver:-}" \
    --host-subs "${host_subs:-}" \
    --host-ran "$host_ran" \
    --linux-ran "$linux_ran" \
    --linux-skip "${linux_skip:-}" \
    --linux-image "$image" \
    --linux-fs "$linux_fs" \
    --json "$json" <<'XPLAT_PY' || verdict=$?
# -*- coding: utf-8 -*-
"""ホスト側と Linux 側の JSON を突き合わせて表と JSON を出す。

**落とす指標と出すだけの指標を分ける。** OS が違えばスケジューリングも
ファイルシステムも違うので、時間と「混雑で断った回数」で合否を決めると
必ず嘘の赤が出る (CLAUDE.md「絶対時間で性能テストの線を引かない」)。
"""
import json
import os
import re
import sys
import unicodedata

# **主張が壊れたと言える指標** (どちらの OS でも同じでなければおかしい)
CZ_HARD = ["dup_files", "conflict_merges", "hunks"]
# **OS 差として当然ありうる指標** (出すだけ)
CZ_SOFT = ["applied", "denied", "wall_edit_ms", "wall_merge_ms", "wall_total_ms"]
CO_HARD = ["hunks", "out_of_bounds", "protocol_violations", "granted_too_close_pairs"]
CO_SOFT = ["completed", "denied", "shifted", "edit_ms", "merge_ms"]
CZ_KEYS = CZ_HARD + CZ_SOFT + ["merges", "gate_calls", "gate_p50", "gate_p95", "gate_max"]
CO_KEYS = CO_HARD + CO_SOFT


def dw(s):
    return sum(2 if unicodedata.east_asian_width(c) in "WF" else 1 for c in s)


def pad(s, n):
    return s + " " * max(0, n - dw(s))


def table(headers, rows):
    if not rows:
        return "   (行がありません)"
    cols = len(headers)
    w = [dw(h) for h in headers]
    for r in rows:
        for i in range(cols):
            w[i] = max(w[i], dw(r[i]))

    def line(l, m, r):
        return l + m.join("─" * (x + 2) for x in w) + r

    out = [line("┌", "┬", "┐"),
           "│ " + " │ ".join(pad(headers[i], w[i]) for i in range(cols)) + " │",
           line("├", "┼", "┤")]
    for r in rows:
        out.append("│ " + " │ ".join(pad(r[i], w[i]) for i in range(cols)) + " │")
    out.append(line("└", "┴", "┘"))
    return "\n".join(out)


def opts(argv):
    o, it = {}, iter(argv)
    for a in it:
        if a.startswith("--"):
            o[a[2:]] = next(it)
    return o


def load(path):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except Exception:
        return None


def rc_of(path):
    try:
        with open(path, encoding="utf-8") as f:
            return int(f.read().strip())
    except Exception:
        return None


def both(a, b):
    """両側の値を "a / b" で出す。片側しか無ければ「—」。"""
    fa = "—" if a is None else str(a)
    fb = "—" if b is None else str(b)
    return "%s / %s" % (fa, fb)


def ms(x):
    if x is None:
        return "—"
    return "%.0fms" % x if x < 1000 else "%.1fs" % (x / 1000.0)


def main(argv):
    o = opts(argv)
    out = o["out"]
    ws = [w for w in o["writers"].replace(",", " ").split() if w]
    host_ran = o["host-ran"] == "1"
    linux_ran = o["linux-ran"] == "1"
    as_json = o["json"] == "1"

    lx_uname = ""
    lx_fs = ""
    lx_ver = ""
    lx_path = ""
    lx_subs = ""
    p = os.path.join(out, "linux-uname")
    if os.path.exists(p):
        lx_uname = " ".join(open(p, encoding="utf-8").read().split())
    p = os.path.join(out, "linux-fs")
    if os.path.exists(p):
        lx_fs = open(p, encoding="utf-8").read().strip()
    p = os.path.join(out, "linux-zai-version")
    if os.path.exists(p):
        parts = open(p, encoding="utf-8", errors="replace").read().split()
        lx_ver = parts[-1] if parts else ""
    p = os.path.join(out, "linux-zai-path")
    if os.path.exists(p):
        lx_path = open(p, encoding="utf-8").read().strip()
    p = os.path.join(out, "linux-zai-help")
    if os.path.exists(p):
        txt = open(p, encoding="utf-8", errors="replace").read().split()
        # **ホスト側の awk (^[a-z][a-z-]+$) と同じ規則で拾う。** 規則がずれると
        # `--help` / `--version` が片側にだけ現れて「能力が違う」という嘘が出る
        # (実際に出た)。
        subs = sorted({txt[i + 1] for i, t in enumerate(txt[:-1])
                       if t == "zai" and re.match(r"^[a-z][a-z-]+$", txt[i + 1])})
        lx_subs = " ".join(subs)

    hostlab = "%s/%s" % (o["host-os"].split()[0], o["host-fs"])
    lxlab = "%s/%s" % (lx_uname.split()[0] if lx_uname else "Linux", lx_fs or "?")

    body = []
    body.append("== xplat-bench: 同じベンチを 2 つの OS で走らせて並べる")
    body.append("")
    body.append("== 測った相手")
    body.append("   ホスト: %s / 作業場 %s" % (o["host-os"], o["host-fs"]))
    body.append("     zai: %s%s" % (o["host-zai"] or "(未解決)",
                                    " (%s)" % o["host-zai-ver"] if o["host-zai-ver"] else ""))
    body.append("     使えるサブコマンド: %s" % (o["host-subs"].strip() or "(不明)"))
    if linux_ran:
        body.append("   Linux : %s / 作業場 %s (%s, --linux-fs %s)"
                    % (lx_uname or "?", lx_fs or "?", o["linux-image"], o["linux-fs"]))
        body.append("     zai: %s%s" % (lx_path or "(未解決)", " (%s)" % lx_ver if lx_ver else ""))
        body.append("     使えるサブコマンド: %s" % (lx_subs or "(不明)"))
    else:
        body.append("   Linux : **skip** — %s" % (o["linux-skip"] or "理由が記録されていません"))
    body.append("")

    # **能力が食い違っていたら、数字を比べる前にそれを言う。**
    # 片側だけ段が動いていれば、揃った表は「同じものを測った」という嘘になる。
    caps = []
    if host_ran and linux_ran and o["host-subs"].strip() and lx_subs:
        hs = set(o["host-subs"].split())
        ls = set(lx_subs.split())
        if hs - ls:
            caps.append("   ホストにしか無い: %s" % " ".join(sorted(hs - ls)))
        if ls - hs:
            caps.append("   Linux にしか無い: %s" % " ".join(sorted(ls - hs)))
    if caps:
        body.append("== **両側の zai の能力が違います。段の数が揃わない原因になります**")
        body += caps
        body.append("")

    hard_bad = []
    soft_note = []
    result = {"config": {k: o[k] for k in
                         ("writers", "overlap", "seed", "bench", "host-os", "host-fs",
                          "host-zai", "host-zai-ver", "linux-image", "linux-fs")},
              "host": {"ran": host_ran, "zai": o["host-zai"], "version": o["host-zai-ver"],
                       "subcommands": o["host-subs"].split(), "os": o["host-os"],
                       "fs": o["host-fs"]},
              "linux": {"ran": linux_ran, "skip_reason": o["linux-skip"],
                        "zai": lx_path, "version": lx_ver, "subcommands": lx_subs.split(),
                        "os": lx_uname, "fs": lx_fs, "image": o["linux-image"]},
              "conflict_zero": [], "coedit": [], "verdict": {}}

    # ── conflict-zero ────────────────────────────────────────────
    if o["bench"] != "coedit":
        rows = []
        trows = []
        for w in ws:
            h = load(os.path.join(out, "host-cz-%s.json" % w))
            l = load(os.path.join(out, "linux-cz-%s.json" % w))
            hrc = rc_of(os.path.join(out, "host-cz-%s.rc" % w))
            lrc = rc_of(os.path.join(out, "linux-cz-%s.rc" % w))
            if host_ran and hrc not in (0, None):
                hard_bad.append("conflict-zero(%s): ホスト側が rc=%s で終わりました" % (w, hrc))
            if linux_ran and lrc not in (0, None):
                hard_bad.append("conflict-zero(%s): Linux 側が rc=%s で終わりました" % (w, lrc))
            hs = {s["stage"]: s for s in (h or {}).get("stages", [])}
            ls = {s["stage"]: s for s in (l or {}).get("stages", [])}
            for st in sorted(set(hs) | set(ls)):
                a, b = hs.get(st), ls.get(st)
                aok = a is not None and a.get("status") == "ok"
                bok = b is not None and b.get("status") == "ok"
                if not aok and not bok:
                    reason = (a or b or {}).get("reason", "")
                    rows.append([w, st, "skip", "skip", "—", "—", "—", "△ 両側 skip: %s" % reason])
                    continue
                if not (aok and bok):
                    who = "ホストのみ" if aok else "Linux のみ"
                    src = a if aok else b
                    other = b if aok else a
                    rows.append([w, st,
                                 both(a.get("applied") if aok else None,
                                      b.get("applied") if bok else None),
                                 both(a.get("denied") if aok else None,
                                      b.get("denied") if bok else None),
                                 str(src.get("dup_files")), str(src.get("hunks")),
                                 "%d/%d" % (src.get("conflict_merges", 0), src.get("merges", 0)),
                                 "△ %s で実行 (%s)" % (who, (other or {}).get("reason", "片側は未実行"))])
                    # **片側しか走っていなくても JSON には残す。** 落とすと
                    # `--host-only` / `--linux-only` の JSON が空になり、
                    # 後から別の実行と突き合わせられなくなる (実際にそうなった)。
                    result["conflict_zero"].append(
                        {"writers": int(w), "stage": st,
                         "host": {k: a.get(k) for k in CZ_KEYS} if aok else None,
                         "linux": {k: b.get(k) for k in CZ_KEYS} if bok else None,
                         "diverged": []})
                    continue
                bad = [k for k in CZ_HARD if a.get(k) != b.get(k)]
                mark = "✓" if not bad else "✗ %s が違う" % " ".join(bad)
                if bad:
                    hard_bad.append("conflict-zero(%s) 段 %s: %s"
                                    % (w, st, " / ".join("%s %s→%s" % (k, a.get(k), b.get(k))
                                                         for k in bad)))
                for k in CZ_SOFT:
                    if k.endswith("_ms"):
                        continue
                    if a.get(k) != b.get(k):
                        soft_note.append("conflict-zero(%s) 段 %s: %s が %s → %s (時間・混雑に依存する指標なので落としません)"
                                         % (w, st, k, a.get(k), b.get(k)))
                rows.append([w, st,
                             both(a.get("applied"), b.get("applied")),
                             both(a.get("denied"), b.get("denied")),
                             both(a.get("dup_files"), b.get("dup_files")),
                             both(a.get("hunks"), b.get("hunks")),
                             "%d/%d / %d/%d" % (a.get("conflict_merges", 0), a.get("merges", 0),
                                                b.get("conflict_merges", 0), b.get("merges", 0)),
                             mark])
                trows.append([w, st,
                              "%s / %s" % (ms(a.get("wall_edit_ms")), ms(b.get("wall_edit_ms"))),
                              "%s / %s" % (ms(a.get("wall_merge_ms")), ms(b.get("wall_merge_ms"))),
                              "%s / %s" % (ms(a.get("wall_total_ms")), ms(b.get("wall_total_ms"))),
                              "%s / %s" % (ms(a.get("gate_p50")), ms(b.get("gate_p50"))),
                              "%s / %s" % (ms(a.get("gate_max")), ms(b.get("gate_max")))])
                result["conflict_zero"].append(
                    {"writers": int(w), "stage": st,
                     "host": {k: a.get(k) for k in CZ_KEYS},
                     "linux": {k: b.get(k) for k in CZ_KEYS},
                     "diverged": bad})
        body.append("== conflict-zero-bench (値は ホスト / Linux)")
        body.append("   ホスト=%s  Linux=%s" % (hostlab, lxlab if linux_ran else "skip"))
        body.append(table(["体数", "段", "書けた", "断った", "重複file", "ハンク", "衝突マージ", "判定"], rows))
        body.append("")
        body.append("== 壁時計 (**情報。ここで合否を決めない**) 値は ホスト / Linux")
        body.append(table(["体数", "段", "編集", "マージ", "全体", "ゲートp50", "ゲートmax"], trows))
        body.append("")

    # ── coedit ───────────────────────────────────────────────────
    if o["bench"] != "cz":
        rows = []
        for w in ws:
            h = load(os.path.join(out, "host-coedit-%s.json" % w))
            l = load(os.path.join(out, "linux-coedit-%s.json" % w))
            hrc = rc_of(os.path.join(out, "host-coedit-%s.rc" % w))
            lrc = rc_of(os.path.join(out, "linux-coedit-%s.rc" % w))
            if host_ran and hrc not in (0, None):
                hard_bad.append("coedit(%s): ホスト側が rc=%s で終わりました" % (w, hrc))
            if linux_ran and lrc not in (0, None):
                hard_bad.append("coedit(%s): Linux 側が rc=%s で終わりました" % (w, lrc))
            hs = {(s["layout"], s["mode"]): s for s in (h or {}).get("stages", [])}
            ls = {(s["layout"], s["mode"]): s for s in (l or {}).get("stages", [])}
            for key in sorted(set(hs) | set(ls)):
                a, b = hs.get(key), ls.get(key)
                aok = a is not None and a.get("status") != "skip"
                bok = b is not None and b.get("status") != "skip"
                if not aok and not bok:
                    continue
                if not (aok and bok):
                    rows.append([w, key[0], key[1],
                                 both(a.get("completed") if aok else None,
                                      b.get("completed") if bok else None),
                                 "—", "—", "—",
                                 "△ %s のみ実行" % ("ホスト" if aok else "Linux")])
                    result["coedit"].append(
                        {"agents": int(w), "layout": key[0], "mode": key[1],
                         "host": {k: a.get(k) for k in CO_KEYS} if aok else None,
                         "linux": {k: b.get(k) for k in CO_KEYS} if bok else None,
                         "diverged": []})
                    continue
                bad = [k for k in CO_HARD if a.get(k) != b.get(k)]
                mark = "✓" if not bad else "✗ %s が違う" % " ".join(bad)
                if bad:
                    hard_bad.append("coedit(%s) %s/%s: %s"
                                    % (w, key[0], key[1],
                                       " / ".join("%s %s→%s" % (k, a.get(k), b.get(k)) for k in bad)))
                for k in CO_SOFT:
                    if k.endswith("_ms"):
                        continue
                    if a.get(k) != b.get(k):
                        soft_note.append("coedit(%s) %s/%s: %s が %s → %s (落としません)"
                                         % (w, key[0], key[1], k, a.get(k), b.get(k)))
                rows.append([w, key[0], key[1],
                             both(a.get("completed"), b.get("completed")),
                             both(a.get("denied"), b.get("denied")),
                             both(a.get("hunks"), b.get("hunks")),
                             both(a.get("out_of_bounds"), b.get("out_of_bounds")),
                             mark])
                result["coedit"].append(
                    {"agents": int(w), "layout": key[0], "mode": key[1],
                     "host": {k: a.get(k) for k in CO_KEYS},
                     "linux": {k: b.get(k) for k in CO_KEYS},
                     "diverged": bad})
        body.append("== coedit-bench (値は ホスト / Linux)")
        body.append(table(["体数", "並べ方", "段", "完了", "拒否", "ハンク", "範囲外", "判定"], rows))
        body.append("")

    if soft_note:
        body.append("== OS 差として出しただけの食い違い (**落としません**)")
        for s in soft_note[:40]:
            body.append("   " + s)
        if len(soft_note) > 40:
            body.append("   ... 他 %d 件" % (len(soft_note) - 40))
        body.append("")

    if not linux_ran:
        body.append("== 判定: **片側しか測っていません** (Linux は skip)")
        body.append("   理由: %s" % (o["linux-skip"] or "(記録なし)"))
    elif not host_ran:
        body.append("== 判定: **片側しか測っていません** (ホスト側は走らせていません)")
        body.append("   --linux-only を外すと 2 つの OS を並べられます。")
    elif hard_bad:
        body.append("== 判定: **OS で結論が変わりました**")
        for s in hard_bad:
            body.append("   ✗ " + s)
    else:
        body.append("== 判定: 落とす指標は 2 つの OS で 1 つも食い違いませんでした")
    body.append("")

    result["verdict"] = {"hard_divergences": hard_bad, "soft_notes": soft_note,
                         "both_sides": bool(host_ran and linux_ran)}

    text = "\n".join(body)
    print(text, file=(sys.stderr if as_json else sys.stdout))
    if as_json:
        json.dump(result, sys.stdout, ensure_ascii=False, indent=2)
        print()
    return 1 if hard_bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
XPLAT_PY

exit "$verdict"
