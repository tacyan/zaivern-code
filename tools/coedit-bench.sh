#!/usr/bin/env sh
# 「**同じファイルでも、違う行なら 2 人が同時に書ける**」を実測するハーネス。
#
# ## なぜ別に要るのか (tools/conflict-zero-bench.sh との違い)
#
# 既存のハーネスは **ファイルを N 個** 置いて「誰がどのファイルを取るか」を測る。
# そこではファイル単位の所有が自然に効くので、**行域オーナーシップの価値は
# 1 ミリも見えない**。この製品が新しく主張したいのは
#
#   > 同じファイルの違う行なら、2 人が同時に書ける
#
# なので、測る土俵は逆にしないといけない。**ファイルは 1 個だけ**置く。
# ファイル単位の所有だと並列度が構造的に 1 に潰れる状況で、
# 行域オーナーシップが何を取り戻すのかを数字にする。
#
# ## 中核の問い
#
#   1. N 体が「同じファイルの違う行」を同時に書いたとき、衝突は本当に 0 か
#   2. ファイル単位の所有と比べて、**拒否された書き込みは何回減ったのか**
#   3. **完了した書き込み**は保存されているか (0 件は「書かせない」でも買える。
#      総作業量を測らないと「衝突 0」の主張は片手落ちになる)
#
# ## 段 (--mode)
#
#   a       素の git。所有の仕組みを 1 つも通さない。**対照群**
#   b       ファイル単位の所有 (`zai lease claim <file>`)。従来モード
#   c       行域オーナーシップ (`zai lease claim '<file>#L10-40'`)。出荷物での実測。
#           **出荷物が行域を理解していなければ skip して「未測定」と出す**
#   cref    行域オーナーシップの**参照ゲート**。`src/region.rs` の
#           `spans_too_close` (gap < band なら近すぎる) をハーネス内で再実装したもの。
#           出荷物と独立に「行域所有で到達できる上限」を出す。
#           docs/conflict-zero.md 3.5 が参照マージドライバでやったのと同じ作法
#   c+      **交渉あり** (`zai lease claim --shift '<file>#L10-40'`)。
#           段 c と**同じ担当表**を使い、重なったら断らずに**ずらして**取る。
#           ずらされたら**実際に取れた行域へ書く** (要求した行ではなく)。
#           出荷物に `--shift` が無ければ skip
#   c+ref   交渉の**参照ゲート**。要求位置から外側へ 1 行ずつ探して、
#           安全帯を満たす最も近い空きへ置く (最近傍あてはめ)。
#           出荷物と独立に「交渉で到達できる上限」と**ずらした距離**を出す
#   ins     **挿入点** (`zai lease claim '<file>#@120'`)。幅 0 で既存行を
#           1 行も占有しない予約。各体は自分の点へ行を**足す**。
#           出荷物が `#@N` を点として理解していなければ skip
#   insref  挿入点の**参照ゲート**。点を `[n,n]` として `spans_too_close` に掛ける
#           (`Span::probe` と同じ扱い)
#
# ## 並べ方 (--layout)
#
#   disjoint  安全帯 (band) 以上離した行域を配る。**衝突は出ないはず**
#   crowded   わざと域を近づける・重ねる。**衝突が出ることの裏取り**と、
#             安全帯がどれだけ保守的か (git なら通ったのに断った数) を測る
#
# ## 「なぜ断られたのか」を表に出す (供給と需要)
#
# 完了 / 拒否だけを見ても **「空きが無いから断った」のか「空いているのに
# 断った」のかが分からない**。走らせるたびに、担当表から
#
#   要求行数計 / 要求の幅 / 互いに素に置くのに要る行数 / ファイル行数 / 空き
#
# を出す。`必要行数 <= ファイル行数` なのに拒否が出ていれば、
# それは**容量の問題ではなく「誰もずらしていない」問題**である。
#
# ## 使い方
#
#   tools/coedit-bench.sh                                   既定 (8 体 / 800 行 / 全段 / 両方の並べ方)
#   tools/coedit-bench.sh --agents 8 --lines 500 --mode all
#   tools/coedit-bench.sh --agents 1,4,8,16,32,64 --lines 2000
#   tools/coedit-bench.sh --agents 64 --lines 2000 --layout disjoint
#   tools/coedit-bench.sh --mode a,cref --json
#   tools/coedit-bench.sh --seed 12345                      乱数の種 (既定 20260810)
#   tools/coedit-bench.sh --band 3                          安全帯 (既定は region::SAFE_BAND と同じ 3)
#   tools/coedit-bench.sh --keep                            一時リポジトリを残す
#
# 環境変数 ZAIVERN_BIN で使う zai を明示できます (無くても a / cref は必ず出ます)。
#
# ## 終了コード
#
#   0  保護のある段 (a 以外すべて) が 1 ハンクも衝突を残さなかった。
#      あるいはその段が skip された (理由は必ず表示される)
#   1  保護のある段が次のいずれかをやった。**主張が壊れている**
#        * 衝突ハンクを残した
#        * **ファイルの外**の行域を配った (交渉がずらしすぎた)
#        * `--shift` の契約 (`granted <spec>` を最後の行に出す) を破った
#   2  引数の指定ミス、または行数が足りず行域を配れない
#   3  前提が無い (git / awk が見つからない)
#
# ## 副作用を持たない作り
#
#   * 一時リポジトリは `mktemp -d` (= `$TMPDIR` 由来)。パスを直書きしない
#   * `HOME` を段ごとの一時ディレクトリへ差し替えるので、**本物の `~/.zaivern` と
#     `~/.gitconfig` には一切触らない**
#   * **cargo を呼ばない。** 既にある `zai` を使うだけで、無ければ段を skip する。
#     ホストの `target/` は 1 バイトも触らない
#   * **python を呼ばない。** POSIX sh + awk だけで完結する
#     (Windows の git 同梱 sh でも走る。`[[` / `local` / 配列を使わない)
#   * 後始末は trap。`--keep` を付けたときだけ残す
set -eu

agents=8
lines=800
modes=all
layouts=both
seed=20260810
band=3
json=0
keep=0
selfcheck=0

usage() {
    cat <<'EOS'
使い方: tools/coedit-bench.sh [オプション]

  --agents N[,N...]   同時に走らせる体数。カンマ区切りで掃引できる (既定 8)
  --lines N           合成ファイルの行数 (既定 800)
  --mode a|b|c|cref|c+|c+ref|ins|insref|all
                      段。カンマ区切り可
                      (既定 all = a,b,c,cref,c+,c+ref,ins,insref)
                      c+ / c+ref は交渉あり (ずらして取る)
                      ins / insref は挿入点 (幅 0・既存行を占有しない)
  --layout disjoint|crowded|both
                      行域の並べ方 (既定 both)
  --seed N            乱数の種。同じ種なら同じ担当表になる (既定 20260810)
  --band N            安全帯の行数。region::SAFE_BAND と揃える (既定 3)
  --json              JSON を stdout へ、表を stderr へ
  --keep              一時リポジトリを消さない
  --self-check        契約どおりに振る舞う**代役の zai** を作って全段を走らせる。
                      出荷物が --shift / #@N を持つ前に、ハーネス側 (能力検査・
                      granted の解釈・ずらした先への書き込み) が正しいかを確かめる
  -h, --help          これ

終了コード: 0=保護段は健全 / 1=保護段が衝突・範囲外配分・契約違反のいずれか
            2=引数か行数の誤り / 3=前提不足
EOS
}

while [ $# -gt 0 ]; do
    case "$1" in
    --agents)
        [ $# -ge 2 ] || { echo "--agents に値がありません" >&2; exit 2; }
        agents=$2; shift 2 ;;
    --lines)
        [ $# -ge 2 ] || { echo "--lines に値がありません" >&2; exit 2; }
        lines=$2; shift 2 ;;
    --mode)
        [ $# -ge 2 ] || { echo "--mode に値がありません" >&2; exit 2; }
        modes=$2; shift 2 ;;
    --layout)
        [ $# -ge 2 ] || { echo "--layout に値がありません" >&2; exit 2; }
        layouts=$2; shift 2 ;;
    --seed)
        [ $# -ge 2 ] || { echo "--seed に値がありません" >&2; exit 2; }
        seed=$2; shift 2 ;;
    --band)
        [ $# -ge 2 ] || { echo "--band に値がありません" >&2; exit 2; }
        band=$2; shift 2 ;;
    --json) json=1; shift ;;
    --keep) keep=1; shift ;;
    --self-check) selfcheck=1; shift ;;
    -h | --help) usage; exit 0 ;;
    *)
        echo "知らない引数です: $1" >&2
        usage >&2
        exit 2 ;;
    esac
done

# ── 引数の検算 (数字でないものを黙って 0 として扱わない) ─────────────
num_ok() {
    case "$1" in
    '' | *[!0-9]*) return 1 ;;
    *) return 0 ;;
    esac
}
for v in $(printf '%s' "$agents" | tr ',' ' '); do
    num_ok "$v" && [ "$v" -ge 1 ] || { echo "--agents は 1 以上の整数です: $v" >&2; exit 2; }
done
num_ok "$lines" && [ "$lines" -ge 1 ] || { echo "--lines は 1 以上の整数です: $lines" >&2; exit 2; }
num_ok "$seed" || { echo "--seed は整数です: $seed" >&2; exit 2; }
num_ok "$band" || { echo "--band は 0 以上の整数です: $band" >&2; exit 2; }

case "$modes" in all) modes="a,b,c,cref,c+,c+ref,ins,insref" ;; esac
for m in $(printf '%s' "$modes" | tr ',' ' '); do
    case "$m" in
    a | b | c | cref | 'c+' | 'c+ref' | ins | insref) ;;
    *)
        echo "--mode は a / b / c / cref / c+ / c+ref / ins / insref のいずれかです: $m" >&2
        exit 2 ;;
    esac
done
case "$layouts" in
both) layouts="disjoint,crowded" ;;
disjoint | crowded) ;;
*) echo "--layout は disjoint / crowded / both です: $layouts" >&2; exit 2 ;;
esac

# ── 前提 ──────────────────────────────────────────────────────────
for need in git awk; do
    command -v "$need" >/dev/null 2>&1 || {
        echo "$need が見つかりません。この計測には $need が要ります。" >&2
        exit 3
    }
done

# ── ミリ秒時計。無ければ綺麗に粗くなる ────────────────────────────
# `date +%s%N` は GNU coreutils だけ。BSD (macOS) は末尾に literal な "N" を返す。
# 巨大な整数を POSIX sh の算術に入れない (桁溢れの挙動が実装依存) ので、
# 文字列として末尾 6 桁を落とす。
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
        # 秒精度しか無い環境。時間の列は「粗い」と明記して出す
        printf '%s000\n' "$(date +%s)" ;;
    esac
}

# ── git のバージョン (merge-tree --write-tree は 2.38 から) ─────────
git_version=$(git --version 2>/dev/null | awk '{print $3}')
has_merge_tree=$(printf '%s\n' "$git_version" | awk -F. '{
    maj = $1 + 0; min = $2 + 0
    print (maj > 2 || (maj == 2 && min >= 38)) ? 1 : 0
}')
[ -n "$has_merge_tree" ] || has_merge_tree=0

# ── 使う zai を探す。**絶対にビルドしない** ────────────────────────
# 古い zai は知らないサブコマンドを GUI 起動として扱って固まるので、
# `zai help` の中身に載っているものだけを叩く (既存ハーネスと同じ作法)。
# shellcheck disable=SC1007  # `CDPATH= cd` は「その cd にだけ空の CDPATH を渡す」正しい書き方
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

# この計測の適格判定: `zai help` が動き、その中身を段の判定に使う。
zai_help_text=""
coedit_capable() {
    zai_help_text=$("$1" help 2>/dev/null </dev/null) || return 1
    return 0
}
zai=""
zai_pick coedit_capable || true
has_lease=0
if [ -n "$zai" ] && printf '%s\n' "$zai_help_text" | grep -q 'zai lease'; then
    has_lease=1
fi

# ── 使い捨ての作業場 ──────────────────────────────────────────────
work=$(mktemp -d 2>/dev/null || mktemp -d -t coeditbench)
# shellcheck disable=SC2329  # trap から呼ばれる
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

export GIT_CONFIG_NOSYSTEM=1
export GIT_TERMINAL_PROMPT=0
export GIT_AUTHOR_NAME=coedit-bench
export GIT_AUTHOR_EMAIL=coedit-bench@example.invalid
export GIT_COMMITTER_NAME=coedit-bench
export GIT_COMMITTER_EMAIL=coedit-bench@example.invalid
export GIT_ADVICE=0
# 本物の ~/.zaivern と ~/.gitconfig に触らせない (段ごとに更に差し替える)
mkdir -p "$work/home"
HOME="$work/home"
USERPROFILE="$work/home"
export HOME USERPROFILE

# ── --self-check: 契約どおりに振る舞う「代役の zai」 ────────────────
#
# 出荷物が `--shift` / `#@N` を持つまで、段 c+ と ins は skip される。
# それは正しい報告だが、**skip されている間ハーネス側のコードは 1 度も走らない**
# (能力検査の成功側・`granted <spec>` の解釈・**ずらした先**への書き込み)。
# 実装が届いた日に初めて動かして壊れているのでは、計測器として使えない。
#
# そこで契約だけを満たす最小の代役を作り、`--self-check` で全段を通す。
# **これは製品の測定値ではない。** 見出しに毎回そう出す。
if [ "$selfcheck" = 1 ]; then
    mkdir -p "$work/stub"
    cat >"$work/stub/zai" <<'ZAISTUB'
#!/usr/bin/env sh
# coedit-bench --self-check の代役。台帳は $HOME に置く (段ごとに隔離される)。
set -eu
band=${ZAI_STUB_BAND:-3}
lg="$HOME/.zai-stub-ledger"
[ $# -gt 0 ] || exit 2
cmd=$1
shift
case "$cmd" in
help)
    echo "lease (ファイル所有 — 並列エージェントの衝突を「起こさせない」):"
    echo "  zai lease claim <パターン...> [--agent 名前]"
    exit 0 ;;
lease) ;;
*) exit 2 ;;
esac
[ $# -gt 0 ] || exit 2
sub=$1
shift
spec=""
dir="."
want_shift=0
while [ $# -gt 0 ]; do
    case "$1" in
    --shift) want_shift=1; shift ;;
    --agent) shift; [ $# -gt 0 ] || exit 2; shift ;;
    --dir) shift; [ $# -gt 0 ] || exit 2; dir=$1; shift ;;
    --*) echo "知らない引数です: $1" >&2; exit 2 ;;
    *) spec=$1; shift ;;
    esac
done
case "$sub" in
enable) : >"$lg"; exit 0 ;;
claim) ;;
*) exit 2 ;;
esac
[ -n "$spec" ] || exit 2
[ -f "$lg" ] || exit 2

path=$spec
kind=whole
s=0
e=0
case "$spec" in
*'#'*)
    path=${spec%%#*}
    frag=${spec##*#}
    case "$frag" in
    '@'*) kind=point; s=${frag#@}; e=$s ;;
    L* | l*) kind=range; body=${frag#?}; s=${body%%-*}; e=${body##*-} ;;
    *) path=$spec ;;
    esac ;;
esac
total=$(awk 'END { print NR + 0 }' "$dir/$path" 2>/dev/null || echo 0)

tries=0
while ! mkdir "$lg.lock" 2>/dev/null; do
    tries=$((tries + 1))
    [ "$tries" -lt 200000 ] || exit 1
done

if [ "$kind" = whole ]; then
    # ファイル全体は「同じパスの何かがあれば断る」。台帳では 0 0 で表す
    if awk -v p="$path" '$1 == p { bad = 1; exit } END { exit(bad ? 1 : 0) }' "$lg"; then
        printf '%s 0 0\n' "$path" >>"$lg"
        rmdir "$lg.lock"
        [ "$want_shift" = 0 ] || printf 'granted %s\n' "$path"
        exit 0
    fi
    rmdir "$lg.lock"
    exit 1
fi

# ずらすときだけファイル末尾で頭打ちにする。ずらさないなら「要求どおりか否か」
# しかないので、行数を超えた指定を代役の都合で断らない
# (能力検査の土台は 1 行のファイルなので、ここを固くすると段 C が skip になる)
maxd=0
bound=0
if [ "$want_shift" = 1 ] && [ "$total" -gt 0 ]; then
    maxd=$total
    bound=$total
fi
got=$(awk -v p="$path" -v s="$s" -v e="$e" -v bound="$bound" -v band="$band" -v maxd="$maxd" '
    BEGIN { n = 0 }
    $1 == p { hs[n] = $2 + 0; he[n] = $3 + 0; n++ }
    END {
        len = e - s + 1
        if (len < 1) len = 1
        for (d = 0; d <= maxd; d++) {
            for (k = 0; k < 2; k++) {
                if (k == 1 && d == 0) continue
                cs = (k == 0) ? s + d : s - d
                ce = cs + len - 1
                if (cs < 1) continue
                if (bound > 0 && ce > bound) continue
                ok = 1
                for (i = 0; i < n; i++) {
                    if (hs[i] == 0 && he[i] == 0) { ok = 0; break }
                    if (cs <= hs[i]) { lo_e = ce; hi_s = hs[i] }
                    else { lo_e = he[i]; hi_s = cs }
                    if (hi_s - lo_e - 1 < band) { ok = 0; break }
                }
                if (ok) { printf "%d %d\n", cs, ce; exit }
            }
        }
    }' "$lg")
if [ -z "$got" ]; then
    rmdir "$lg.lock"
    exit 1
fi
gs=${got% *}
ge=${got#* }
printf '%s %s %s\n' "$path" "$gs" "$ge" >>"$lg"
rmdir "$lg.lock"
if [ "$want_shift" = 1 ]; then
    if [ "$kind" = point ]; then
        printf 'granted %s#@%s\n' "$path" "$gs"
    else
        printf 'granted %s#L%s-%s\n' "$path" "$gs" "$ge"
    fi
fi
exit 0
ZAISTUB
    chmod +x "$work/stub/zai"
    zai="$work/stub/zai"
    has_lease=1
    ZAI_STUB_BAND=$band
    export ZAI_STUB_BAND
fi

# ── 出荷物が「行域」を理解しているかの能力検査 ─────────────────────
#
# **ここを省くと静かな嘘になる。** 0.13.0 の `zai lease claim` は
# `a.rs#L1-10` を**ただの文字列**として受け取るので、
#   * `a.rs#L1-10` と `a.rs#L5-15` (重なっている) が **両方通る**
#   * `a.rs` を誰かが持っていても `a.rs#L1-10` が通る
# つまり保護が 1 つも無いのに「全員書けて衝突 0」という綺麗な数字が出る。
# 重なった 2 つを投げて**断られること**を確かめてから段 c を実行する。
region_aware=0
region_skip_reason="zai が見つかりません"
if [ "$has_lease" = 1 ]; then
    region_skip_reason=""
    probe="$work/probe"
    mkdir -p "$probe/repo" "$probe/home"
    probe_rc=0
    (
        HOME="$probe/home"
        USERPROFILE="$probe/home"
        export HOME USERPROFILE
        cd "$probe/repo"
        git init -q -b main . >/dev/null 2>&1
        printf 'x\n' >a.rs
        git add -A >/dev/null 2>&1
        git commit -qm probe >/dev/null 2>&1
        "$zai" lease enable --dir "$probe/repo" >/dev/null 2>&1 || exit 20
        # 1 本目は通らないといけない
        "$zai" lease claim 'a.rs#L1-10' --agent probe1 --dir "$probe/repo" >/dev/null 2>&1 || exit 21
        # 重なっている 2 本目は**断られないといけない**
        if "$zai" lease claim 'a.rs#L5-15' --agent probe2 --dir "$probe/repo" >/dev/null 2>&1; then
            exit 22
        fi
        # 安全帯より離れた 3 本目は通らないといけない
        "$zai" lease claim 'a.rs#L100-110' --agent probe3 --dir "$probe/repo" >/dev/null 2>&1 || exit 23
        exit 0
    ) || probe_rc=$?
    case "$probe_rc" in
    0) region_aware=1 ;;
    20) region_skip_reason="zai lease enable が失敗しました" ;;
    21) region_skip_reason="zai lease claim が行域指定を受け付けません" ;;
    22) region_skip_reason="重なった 2 つの行域が両方通りました (行域を理解していない = 保護がありません)" ;;
    23) region_skip_reason="離れた行域を断りました (行域の判定が壊れています)" ;;
    *) region_skip_reason="能力検査そのものが失敗しました" ;;
    esac
fi

# ── `granted <spec>` を読む (段 c+ の契約) ─────────────────────────
#
# 契約は「**標準出力の最後の行**が厳密に `granted <spec>`。空白 1 つ区切り、装飾なし」。
# 余分な語や飾りが付いていたら**読めたことにしない** — そこを緩めると
# 「ずらしたつもりで元の行に書く」という一番危ない誤りが素通りする。
#
# 立てる変数: pg_start / pg_end (`Span::probe` と同じ扱い。挿入点は [n, n]) と pg_ins。
pg_start=0
pg_end=0
pg_ins=0
parse_granted() { # $1=最後の行 -> 0 で解釈成功 / 1 で契約違反
    pg_start=0; pg_end=0; pg_ins=0
    case "$1" in
    'granted '*) ;;
    *) return 1 ;;
    esac
    _spec=${1#granted }
    # 空白が残っていたら「空白 1 つ区切り」ではない
    case "$_spec" in '' | *' '* | *'	'*) return 1 ;; esac
    case "$_spec" in *'#'*) ;; *) return 1 ;; esac
    _frag=${_spec##*#}
    case "$_frag" in
    '@'*)
        _n=${_frag#@}
        num_ok "$_n" && [ "$_n" -ge 1 ] || return 1
        pg_ins=1; pg_start=$_n; pg_end=$_n ;;
    L* | l*)
        _body=${_frag#?}
        case "$_body" in *-*) ;; *) return 1 ;; esac
        _s=${_body%%-*}; _e=${_body##*-}
        num_ok "$_s" && num_ok "$_e" || return 1
        [ "$_s" -ge 1 ] && [ "$_e" -ge "$_s" ] || return 1
        pg_start=$_s; pg_end=$_e ;;
    *) return 1 ;;
    esac
    return 0
}

# ── 出荷物が「交渉 (--shift)」を持っているかの能力検査 ─────────────
#
# **ここを省くと段 c と同じ数字が「交渉できました」の顔で出る。**
# 0.13.0 の `zai lease claim` は**知らないフラグを黙って確保パターンとして飲む**
# (実測: `--shift` が持ち主一覧に `--shift` という名前の「ファイル」として並ぶ)。
# つまり rc=0 で返るのに、ずらしてもいないし `granted` も出ない。
# 「最後の行が `granted <spec>`」まで確かめてから段 c+ を実行する。
shift_aware=0
shift_skip_reason="zai が見つかりません"
if [ "$has_lease" = 1 ]; then
    shift_skip_reason=""
    sprobe="$work/sprobe"
    mkdir -p "$sprobe/repo" "$sprobe/home"
    sprobe_rc=0
    (
        HOME="$sprobe/home"
        USERPROFILE="$sprobe/home"
        export HOME USERPROFILE
        cd "$sprobe/repo"
        git init -q -b main . >/dev/null 2>&1
        awk 'BEGIN { for (i = 1; i <= 200; i++) print "x" i }' >a.rs
        git add -A >/dev/null 2>&1
        git commit -qm probe >/dev/null 2>&1
        "$zai" lease enable --dir "$sprobe/repo" >/dev/null 2>&1 || exit 30
        "$zai" lease claim 'a.rs#L1-10' --agent s1 --dir "$sprobe/repo" >/dev/null 2>&1 || exit 30
        # (1) ぶつからない要求。--shift でも rc=0 で、最後の行は要求どおりの granted
        _o=$("$zai" lease claim 'a.rs#L50-60' --shift --agent s2 --dir "$sprobe/repo" 2>/dev/null </dev/null) || exit 31
        _last=$(printf '%s\n' "$_o" | awk 'NF { l = $0 } END { print l }')
        [ "$_last" = 'granted a.rs#L50-60' ] || exit 32
        # (2) ぶつかる要求。断らずに**ずらして** rc=0 で返らないといけない
        _o=$("$zai" lease claim 'a.rs#L5-15' --shift --agent s3 --dir "$sprobe/repo" 2>/dev/null </dev/null) || exit 33
        _last=$(printf '%s\n' "$_o" | awk 'NF { l = $0 } END { print l }')
        parse_granted "$_last" || exit 35
        # (3) 配られた域は、既に持たれている 2 つから安全帯ぶん離れていること。
        #     ここを見ないと「granted と言いながら重ねて配る」を通してしまう
        printf 'a.rs 1 10\na.rs 50 60\n' >"$sprobe/held"
        awk -v s="$pg_start" -v e="$pg_end" -v band="$band" '
            {
                if (s <= $2) { lo_e = e; hi_s = $2 } else { lo_e = $3; hi_s = s }
                if (hi_s - lo_e - 1 < band) bad = 1
            }
            END { exit(bad ? 1 : 0) }' "$sprobe/held" || exit 34
        exit 0
    ) || sprobe_rc=$?
    case "$sprobe_rc" in
    0) shift_aware=1 ;;
    30) shift_skip_reason="下ごしらえ (lease enable / 1 本目の確保) が失敗しました" ;;
    31) shift_skip_reason="ぶつからない要求に --shift を付けたら rc≠0 になりました" ;;
    32) shift_skip_reason="--shift が 'granted <spec>' を最後の行に出しません (**未実装**。知らないフラグが黙って確保パターンとして飲まれています)" ;;
    33) shift_skip_reason="重なった要求を断りました (--shift がずらしていません)" ;;
    34) shift_skip_reason="ずらした先が既存の域と近すぎます (granted と言いながら重ねて配っています)" ;;
    35) shift_skip_reason="最後の行を 'granted <spec>' として読めませんでした" ;;
    *) shift_skip_reason="能力検査そのものが失敗しました" ;;
    esac
fi

# ── 出荷物が「挿入点 (#@N)」を持っているかの能力検査 ───────────────
#
# `a.rs#@120` は**ただのパス文字列としても受理されてしまう**
# (同じ文字列なら 2 人目は断られるので、浅く見ると「効いている」ように見える)。
# 点として理解しているかは、次の 2 つでしか分からない:
#   * 安全帯より**近い別の点**を断るか
#   * その点を**覆う行域**とぶつかるか
insert_aware=0
insert_skip_reason="zai が見つかりません"
if [ "$has_lease" = 1 ]; then
    insert_skip_reason=""
    iprobe="$work/iprobe"
    mkdir -p "$iprobe/repo" "$iprobe/home"
    near_pt=$((120 + band))
    far_pt=$((120 + band + 1))
    iprobe_rc=0
    (
        HOME="$iprobe/home"
        USERPROFILE="$iprobe/home"
        export HOME USERPROFILE
        cd "$iprobe/repo"
        git init -q -b main . >/dev/null 2>&1
        awk 'BEGIN { for (i = 1; i <= 200; i++) print "x" i }' >a.rs
        git add -A >/dev/null 2>&1
        git commit -qm probe >/dev/null 2>&1
        "$zai" lease enable --dir "$iprobe/repo" >/dev/null 2>&1 || exit 40
        "$zai" lease claim 'a.rs#@120' --agent n1 --dir "$iprobe/repo" >/dev/null 2>&1 || exit 41
        # 安全帯より近い点は断られないといけない
        if "$zai" lease claim "a.rs#@$near_pt" --agent n2 --dir "$iprobe/repo" >/dev/null 2>&1; then
            exit 42
        fi
        # 安全帯ぶん離れた点は通らないといけない (ここを断ると並列度が上がらない)
        "$zai" lease claim "a.rs#@$far_pt" --agent n3 --dir "$iprobe/repo" >/dev/null 2>&1 || exit 43
        # 点を**覆う**行域とはぶつかる (上の点から十分離れた場所で試す)
        "$zai" lease claim 'a.rs#L60-70' --agent n4 --dir "$iprobe/repo" >/dev/null 2>&1 || exit 44
        if "$zai" lease claim 'a.rs#@65' --agent n5 --dir "$iprobe/repo" >/dev/null 2>&1; then
            exit 45
        fi
        exit 0
    ) || iprobe_rc=$?
    case "$iprobe_rc" in
    0) insert_aware=1 ;;
    40) insert_skip_reason="zai lease enable が失敗しました" ;;
    41) insert_skip_reason="zai lease claim が挿入点 (#@N) を受け付けません" ;;
    42) insert_skip_reason="安全帯より近い 2 つの挿入点が両方通りました (**点として理解していない** = 保護がありません)" ;;
    43) insert_skip_reason="安全帯ぶん離れた挿入点を断りました (並列度の天井が外れていません)" ;;
    44) insert_skip_reason="下ごしらえの行域確保が失敗しました" ;;
    45) insert_skip_reason="行域が覆っている挿入点が通りました (点を行域と突き合わせていません)" ;;
    *) insert_skip_reason="能力検査そのものが失敗しました" ;;
    esac
fi

# ── 担当表を作る ──────────────────────────────────────────────────
#
# disjoint: ファイルを N 等分し、各枠の中に「後ろへ band 行の空きを残した」行域を置く。
#           隣との隙間は必ず band 行以上 (= region::spans_too_close が false)。
# crowded : 幅 band+3 の行域を stride 2 で並べる。隣同士は**重なり**、
#           3 つ先とは「重なってはいないが band 行未満しか離れていない」組になる。
#           前者は git でも衝突し、後者は git なら通るが安全帯は断る。
plan_regions() {
    awk -v n="$1" -v total="$2" -v band="$3" -v layout="$4" -v seed="$5" 'BEGIN {
        srand(seed)
        if (layout == "disjoint") {
            slot = int(total / n)
            for (i = 1; i <= n; i++) {
                slot_start = (i - 1) * slot + 1
                avail = slot - band          # 枠の末尾に band 行の空きを残す
                if (avail < 1) avail = 1
                rl = int(avail / 2)
                if (rl < 1) rl = 1
                maxjit = avail - rl
                if (maxjit < 0) maxjit = 0
                jit = int(rand() * (maxjit + 1))
                s = slot_start + jit
                e = s + rl - 1
                if (e > total) e = total
                printf "%d %d %d\n", i, s, e
            }
        } else {
            rl = band + 3
            stride = 2
            base = int((total - (n - 1) * stride - rl) / 2)
            if (base < 1) base = 1
            for (i = 1; i <= n; i++) {
                s = base + (i - 1) * stride
                e = s + rl - 1
                if (e > total) e = total
                if (s > total) s = total
                printf "%d %d %d\n", i, s, e
            }
        }
    }'
}

# 担当表そのものが「一撃マージできる」条件を満たしているか (region::is_disjoint 相当)。
# 出力: "近すぎる組の数"
count_close_pairs() {
    awk -v band="$band" '
        { s[NR] = $2; e[NR] = $3; n = NR }
        END {
            c = 0
            for (i = 1; i <= n; i++)
                for (j = i + 1; j <= n; j++) {
                    if (s[i] <= s[j]) { lo_e = e[i]; hi_s = s[j] } else { lo_e = e[j]; hi_s = s[i] }
                    gap = hi_s - lo_e - 1
                    if (gap < band) c++
                }
            print c
        }' "$1"
}

# ── 参照ゲート (段 cref) ──────────────────────────────────────────
# `src/region.rs` の spans_too_close をそのまま写したもの。
# 台帳は 1 本のテキストファイル。mkdir で相互排他する (並列の体が本当に競る)。
ref_claim() { # $1=台帳 $2=path $3=start $4=end -> 0 で確保 / 1 で拒否
    _tries=0
    while ! mkdir "$1.lock" 2>/dev/null; do
        _tries=$((_tries + 1))
        [ "$_tries" -lt 200000 ] || return 1
    done
    if awk -v p="$2" -v s="$3" -v e="$4" -v band="$band" '
            $1 == p {
                if (s <= $2) { lo_e = e; hi_s = $2 } else { lo_e = $3; hi_s = s }
                gap = hi_s - lo_e - 1
                if (gap < band) { bad = 1; exit }
            }
            END { exit(bad ? 1 : 0) }' "$1"; then
        printf '%s %s %s\n' "$2" "$3" "$4" >>"$1"
        rmdir "$1.lock"
        return 0
    fi
    rmdir "$1.lock"
    return 1
}

# ── 交渉の参照ゲート (段 c+ref) ───────────────────────────────────
#
# 要求位置から**外側へ 1 行ずつ**探して、安全帯を満たす最も近い空きへ置く
# (最近傍あてはめ)。同じ長さのまま動かすだけで、分割はしない。
# `src/negotiate.rs` の `offer` が出す提案の**下限**にあたる保守的な実装で、
# 「ずらせばどこまで行けるのか」を出荷物と独立に出すためにある。
#
# 出力: 確保できたら "開始 終了" を stdout へ。できなければ rc=1。
#
# **順番に依存する。** 並列に走らせるので、誰が先にロックを取ったかで
# 配られる場所が変わる。担当表 (誰が何を要求するか) は --seed で固定できるが、
# **ずらした距離は走るたびに揺れる**。ここは正直に書いておく。
ref_shift_claim() { # $1=台帳 $2=path $3=start $4=end $5=ファイル行数
    _lg=$1
    _tries=0
    while ! mkdir "$_lg.lock" 2>/dev/null; do
        _tries=$((_tries + 1))
        [ "$_tries" -lt 200000 ] || return 1
    done
    _got=$(awk -v p="$2" -v s="$3" -v e="$4" -v total="$5" -v band="$band" '
        # **`n` を BEGIN で 0 にしないと 1 件目が消える。**
        # awk の未初期化変数を添字に使うと `hs[""]` になり (`hs[0]` ではない)、
        # `for (i = 0; i < n; i++)` が読むのは値の入っていない `hs[0]`。
        # 台帳の 1 件目だけが判定から抜け落ちて、**重なった域を 2 人に配る**。
        # 実際にこれで 240-245 と 242-247 を同時に配り、統合で衝突した
        BEGIN { n = 0 }
        $1 == p { hs[n] = $2 + 0; he[n] = $3 + 0; n++ }
        END {
            len = e - s + 1
            if (len < 1) len = 1
            for (d = 0; d <= total; d++) {
                for (k = 0; k < 2; k++) {
                    if (k == 1 && d == 0) continue
                    cs = (k == 0) ? s + d : s - d
                    ce = cs + len - 1
                    if (cs < 1 || ce > total) continue
                    ok = 1
                    for (i = 0; i < n; i++) {
                        if (cs <= hs[i]) { lo_e = ce; hi_s = hs[i] }
                        else { lo_e = he[i]; hi_s = cs }
                        if (hi_s - lo_e - 1 < band) { ok = 0; break }
                    }
                    if (ok) { printf "%d %d\n", cs, ce; exit }
                }
            }
        }' "$_lg")
    if [ -n "$_got" ]; then
        printf '%s %s\n' "$2" "$_got" >>"$_lg"
        rmdir "$_lg.lock"
        printf '%s\n' "$_got"
        return 0
    fi
    rmdir "$_lg.lock"
    return 1
}

# ── 供給と需要 ────────────────────────────────────────────────────
#
# 「完了 11 / 拒否 53」だけを見ても、**空きが無くて断ったのか、
# 空いているのに断ったのか**が分からない。担当表から次を出す:
#
#   要求行数計   sum(len)                    実際に書き換えたい行の総数
#   要求の幅     max(end) - min(start) + 1   要求が集まっている範囲
#   必要行数     sum(len) + (n-1)*band       互いに素に置くのに要る最小の行数
#   点なら必要   1 + (n-1)*(band+1)          幅 0 の挿入点として置くなら
#
# `必要行数 <= ファイル行数` なのに拒否が出ていたら、それは容量ではなく
# **誰もずらしていない**ことの証拠になる。
demand_of() { # $1=担当表 -> "要求行数計 要求の幅 必要行数 点なら必要 空き 判定"
    awk -v band="$band" -v total="$2" '
        {
            n++
            len = $3 - $2 + 1
            if (len < 1) len = 1
            sum += len
            if (lo == 0 || $2 < lo) lo = $2
            if ($3 > hi) hi = $3
        }
        END {
            need = sum + (n - 1) * band
            need_pt = 1 + (n - 1) * (band + 1)
            # **タブ区切り**で返す。空白区切りにすると呼び出し側の
            # `read -r a b c` が 1 列に潰す (実際にそれで表が崩れた)
            printf "%d\t%d\t%d\t%d\t%d\t%s\n", sum, hi - lo + 1, need, need_pt,
                total - sum, (need <= total ? "入る" : "入らない")
        }' "$1"
}

# ── 統計 (p50 / p95 / max / mean)。1 行 1 数値のファイルを食う ──────
pct() {
    awk '
        { v[n++] = $1 + 0; sum += $1 + 0 }
        END {
            if (n == 0) { print "0 0 0 0"; exit }
            for (i = 0; i < n; i++)
                for (j = i + 1; j < n; j++)
                    if (v[j] < v[i]) { t = v[i]; v[i] = v[j]; v[j] = t }
            p50 = v[int(n * 0.50)] ; if (int(n * 0.50) >= n) p50 = v[n-1]
            p95 = v[int(n * 0.95)] ; if (int(n * 0.95) >= n) p95 = v[n-1]
            printf "%d %d %d %d\n", p50, p95, v[n-1], sum / n
        }' "$1"
}

# ── 1 段を走らせる ────────────────────────────────────────────────
# 結果はグローバルへ置く (POSIX sh に戻り値の構造体は無い)。
run_stage() { # $1=mode $2=layout $3=n
    st_mode=$1
    st_layout=$2
    st_n=$3
    st_status=ok
    st_reason=""
    st_planned=$st_n
    st_done=0
    st_denied=0
    st_confl_br=0
    st_confl_files=0
    st_hunks=0
    st_confl_lines=0
    st_human=0
    st_survived=0
    st_edit_ms=0
    st_merge_ms=0
    st_gate_p50=0; st_gate_p95=0; st_gate_max=0; st_gate_mean=0
    st_merge_p50=0; st_merge_p95=0; st_merge_max=0; st_merge_mean=0
    st_pred_checked=0
    st_pred_hit=0
    st_close_pairs=0
    st_asreq=0
    st_shifted=0
    st_d_p50=0; st_d_p95=0; st_d_max=0; st_d_mean=0
    st_oob=0
    st_badproto=0
    st_bad_pairs=0

    if [ "$st_mode" = b ] && [ "$has_lease" != 1 ]; then
        st_status=skip
        st_reason="zai lease が見つかりません"
        return 0
    fi
    if [ "$st_mode" = c ] && [ "$region_aware" != 1 ]; then
        st_status=skip
        st_reason=$region_skip_reason
        return 0
    fi
    if [ "$st_mode" = 'c+' ] && [ "$shift_aware" != 1 ]; then
        st_status=skip
        st_reason=$shift_skip_reason
        return 0
    fi
    if [ "$st_mode" = ins ] && [ "$insert_aware" != 1 ]; then
        st_status=skip
        st_reason=$insert_skip_reason
        return 0
    fi

    stage="$work/$st_mode-$st_layout-$st_n"
    repo="$stage/repo"
    mkdir -p "$repo" "$stage/res" "$stage/home"

    # 段ごとに HOME を作り直す。前の段のリース台帳を引きずらない
    HOME="$stage/home"
    USERPROFILE="$stage/home"
    export HOME USERPROFILE

    # 1 個だけの合成ファイル。ここが肝 —— ファイル単位の所有だと並列度 1 に潰れる
    mkdir -p "$repo/src"
    awk -v n="$lines" 'BEGIN { for (i = 1; i <= n; i++) printf "let value_%06d = %d;\n", i, i }' \
        >"$repo/src/wide.rs"
    if ! (
        cd "$repo"
        git init -q -b main . >/dev/null 2>&1 ||
            { git init -q . && git symbolic-ref HEAD refs/heads/main; }
        git add -A
        git commit -qm "coedit-bench: 合成ファイル $lines 行"
    ) >/dev/null 2>&1; then
        st_status=skip
        st_reason="合成リポジトリを作れませんでした"
        return 0
    fi

    # 担当表は (layout, 体数) ごとに 1 度だけ作って全段で共有する。
    # 段ごとに引き直すと「同じ担当表を使った」と言えなくなる
    cp "$work/plan-$st_layout-$st_n" "$stage/plan"
    st_close_pairs=$(count_close_pairs "$stage/plan")

    if [ "$st_mode" = b ] || [ "$st_mode" = c ] || [ "$st_mode" = 'c+' ] || [ "$st_mode" = ins ]; then
        if ! "$zai" lease enable --dir "$repo" >/dev/null 2>&1; then
            st_status=skip
            st_reason="zai lease enable が失敗しました"
            return 0
        fi
    fi
    : >"$stage/ledger"
    : >"$stage/granted"

    # 体ごとの隔離 worktree を先に作る (worktree の生成そのものは計測対象外)
    i=1
    while [ "$i" -le "$st_n" ]; do
        git -C "$repo" worktree add -q -b "w$i" "$stage/wt$i" main >/dev/null 2>&1
        i=$((i + 1))
    done

    # ── 書き込みフェーズ (ここだけ本当に並列で走らせる) ────────────
    t0=$(now_ms)
    while read -r idx rstart rend; do
        (
            agent=$(printf 'a%03d' "$idx")
            wt="$stage/wt$idx"
            res="$stage/res/$idx"
            g0=$(now_ms)
            granted=1
            # ws/we = **実際に配られた**域。ずらされたらここが要求とずれる
            ws=$rstart
            we=$rend
            isins=0
            badproto=0
            dist=0
            oob=0
            case "$st_mode" in
            a) granted=1 ;;
            b) "$zai" lease claim 'src/wide.rs' --agent "$agent" --dir "$repo" >/dev/null 2>&1 || granted=0 ;;
            c) "$zai" lease claim "src/wide.rs#L$rstart-$rend" --agent "$agent" --dir "$repo" >/dev/null 2>&1 || granted=0 ;;
            cref) ref_claim "$stage/ledger" 'src/wide.rs' "$rstart" "$rend" || granted=0 ;;
            'c+')
                # rc=0 でも**最後の行**が `granted <spec>` でなければ契約違反。
                # 読めないまま要求した行へ書くのが一番危ない (ずらされたのに
                # 元の場所を潰す) ので、読めなければ「書かない」側に倒す
                out=$("$zai" lease claim "src/wide.rs#L$rstart-$rend" --shift \
                    --agent "$agent" --dir "$repo" 2>/dev/null </dev/null) || granted=0
                if [ "$granted" = 1 ]; then
                    last=$(printf '%s\n' "$out" | awk 'NF { l = $0 } END { print l }')
                    if parse_granted "$last"; then
                        ws=$pg_start; we=$pg_end; isins=$pg_ins
                    else
                        badproto=1
                        granted=0
                    fi
                fi ;;
            'c+ref')
                got=$(ref_shift_claim "$stage/ledger" 'src/wide.rs' "$rstart" "$rend" "$lines") || granted=0
                if [ "$granted" = 1 ]; then
                    ws=${got% *}
                    we=${got#* }
                fi ;;
            ins)
                isins=1; ws=$rstart; we=$rstart
                "$zai" lease claim "src/wide.rs#@$rstart" --agent "$agent" --dir "$repo" >/dev/null 2>&1 || granted=0 ;;
            insref)
                # 挿入点は `Span::probe` と同じく [n, n] として安全帯に掛ける
                isins=1; ws=$rstart; we=$rstart
                ref_claim "$stage/ledger" 'src/wide.rs' "$rstart" "$rstart" || granted=0 ;;
            esac
            g1=$(now_ms)
            if [ "$granted" = 1 ]; then
                dist=$((ws - rstart))
                [ "$dist" -ge 0 ] || dist=$((0 - dist))
                hi=$lines
                [ "$isins" = 0 ] || hi=$((lines + 1))   # 末尾への挿入は行 n+1 まで正当
                if [ "$ws" -lt 1 ] || [ "$we" -gt "$hi" ]; then
                    oob=1
                fi
                if [ "$isins" = 1 ]; then
                    # 挿入点: 既存の行を 1 行も潰さず、点の手前へ書き足す
                    ok=0
                    if awk -v n="$ws" -v a="$idx" '
                        NR == n { for (k = 1; k <= 3; k++) printf "fn added_%03d_%d() {} // a%03d\n", a, k, a }
                        { print }
                        END { if (n > NR) for (k = 1; k <= 3; k++) printf "fn added_%03d_%d() {} // a%03d\n", a, k, a }
                    ' "$wt/src/wide.rs" >"$wt/src/wide.new"; then ok=1; fi
                else
                    ok=0
                    if awk -v s="$ws" -v e="$we" -v a="$idx" '{
                        if (NR >= s && NR <= e)
                            printf "let value_%06d = %d; // a%03d\n", NR, NR * 1000 + a, a
                        else print
                    }' "$wt/src/wide.rs" >"$wt/src/wide.new"; then ok=1; fi
                fi
                if [ "$ok" = 1 ]; then
                    mv "$wt/src/wide.new" "$wt/src/wide.rs"
                fi
                # **配られた域そのものを残す。** 統合が偶然きれいに通っても、
                # ゲートが重なった域を配っていたら主張は壊れている。
                # (実際に参照ゲートの awk バグをこの検査で特定した)
                case "$st_mode" in
                a | b) ;;
                *) printf '%s %s %s\n' "$idx" "$ws" "$we" >>"$stage/granted" ;;
                esac
                # ref のロック競合で稀に失敗するので数回だけ粘る (長い sleep は書かない)
                tries=0
                while [ "$tries" -lt 5 ]; do
                    if git -C "$wt" commit -qam "$agent: L$rstart-$rend" >/dev/null 2>&1; then
                        break
                    fi
                    tries=$((tries + 1))
                done
            fi
            printf '%s %s %s %s %s\n' "$granted" "$((g1 - g0))" "$dist" "$oob" "$badproto" >"$res"
        ) &
    done <"$stage/plan"
    wait || true
    t1=$(now_ms)
    st_edit_ms=$((t1 - t0))

    : >"$stage/gate_ms"
    : >"$stage/dist"
    i=1
    while [ "$i" -le "$st_n" ]; do
        if [ -f "$stage/res/$i" ]; then
            read -r g_ok g_ms g_dist g_oob g_bad <"$stage/res/$i" ||
                { g_ok=0; g_ms=0; g_dist=0; g_oob=0; g_bad=0; }
        else
            g_ok=0; g_ms=0; g_dist=0; g_oob=0; g_bad=0
        fi
        if [ "$g_ok" = 1 ]; then
            st_done=$((st_done + 1))
            if [ "${g_dist:-0}" -gt 0 ]; then
                st_shifted=$((st_shifted + 1))
                printf '%s\n' "$g_dist" >>"$stage/dist"
            else
                st_asreq=$((st_asreq + 1))
            fi
        else
            st_denied=$((st_denied + 1))
        fi
        st_oob=$((st_oob + ${g_oob:-0}))
        st_badproto=$((st_badproto + ${g_bad:-0}))
        [ "$st_mode" = a ] || printf '%s\n' "$g_ms" >>"$stage/gate_ms"
        i=$((i + 1))
    done
    # shellcheck disable=SC2046  # pct は空白区切りの 4 数値を返す。分割させたい
    set -- $(pct "$stage/gate_ms")
    st_gate_p50=$1; st_gate_p95=$2; st_gate_max=$3; st_gate_mean=$4
    # ずらした距離は**ずらした件だけ**で取る。取れた 64 件のうち 2 件しか
    # 動いていないのに「p50 は 0 行」と出すと、払った代償が消えてしまう
    # shellcheck disable=SC2046
    set -- $(pct "$stage/dist")
    st_d_p50=$1; st_d_p95=$2; st_d_max=$3; st_d_mean=$4
    # ゲートの自己検査: 実際に配られた域が互いに素か (region::is_disjoint 相当)
    st_bad_pairs=$(count_close_pairs "$stage/granted")
    [ -n "$st_bad_pairs" ] || st_bad_pairs=0

    # ── 統合フェーズ (作成順に直列マージ。衝突したら abort して次へ) ──
    git -C "$repo" worktree add -q -b integ "$stage/integ" main >/dev/null 2>&1
    integ="$stage/integ"
    : >"$stage/merge_ms"
    t0=$(now_ms)
    i=1
    while [ "$i" -le "$st_n" ]; do
        nc=$(git -C "$repo" rev-list --count "main..w$i" 2>/dev/null || echo 0)
        if [ "$nc" -gt 0 ]; then
            if [ "$has_merge_tree" = 1 ]; then
                head_sha=$(git -C "$integ" rev-parse HEAD)
                if git -C "$repo" merge-tree --write-tree "$head_sha" "w$i" >/dev/null 2>&1; then
                    pred=clean
                else
                    pred=conflict
                fi
                st_pred_checked=$((st_pred_checked + 1))
            else
                pred=unknown
            fi
            m0=$(now_ms)
            if git -C "$integ" merge --no-edit -q "w$i" >/dev/null 2>&1; then
                actual=clean
            else
                actual=conflict
                st_confl_br=$((st_confl_br + 1))
                st_human=$((st_human + 1))
                git -C "$integ" diff --name-only --diff-filter=U >"$stage/unmerged" 2>/dev/null || : >"$stage/unmerged"
                nf=$(awk 'END { print NR + 0 }' "$stage/unmerged")
                st_confl_files=$((st_confl_files + nf))
                if [ "$nf" -gt 0 ]; then
                    # shellcheck disable=SC2046  # 衝突ファイル名の並びを awk の引数へ展開したい
                    set -- $(cd "$integ" && awk '
                        /^<<<<<<< / { h++; ins = 1; next }
                        /^=======$/ { if (ins) next }
                        /^>>>>>>> / { ins = 0; next }
                        { if (ins) l++ }
                        END { printf "%d %d\n", h + 0, l + 0 }' $(cat "$stage/unmerged"))
                    st_hunks=$((st_hunks + $1))
                    st_confl_lines=$((st_confl_lines + $2))
                fi
                git -C "$integ" merge --abort >/dev/null 2>&1 || true
            fi
            m1=$(now_ms)
            printf '%s\n' "$((m1 - m0))" >>"$stage/merge_ms"
            if [ "$pred" != unknown ] && [ "$pred" = "$actual" ]; then
                st_pred_hit=$((st_pred_hit + 1))
            fi
        fi
        i=$((i + 1))
    done
    t1=$(now_ms)
    st_merge_ms=$((t1 - t0))
    # shellcheck disable=SC2046
    set -- $(pct "$stage/merge_ms")
    st_merge_p50=$1; st_merge_p95=$2; st_merge_max=$3; st_merge_mean=$4

    # ── 総作業量は保存されたか ──────────────────────────────────
    # 統合後のファイルに、何体ぶんの印が残っているかを数える。
    # 「拒否も衝突も 0」でも、統合後に消えていたら意味が無い
    st_survived=$(grep -o '// a[0-9][0-9][0-9]' "$integ/src/wide.rs" 2>/dev/null |
        sort -u | awk 'END { print NR + 0 }')
    [ -n "$st_survived" ] || st_survived=0

    HOME="$work/home"
    USERPROFILE="$work/home"
    export HOME USERPROFILE
    return 0
}

# ── 実行 ──────────────────────────────────────────────────────────
rows="$work/rows"
: >"$rows"
: >"$work/demand"
fail=0

mode_label() {
    case "$1" in
    a) echo "A 素の git" ;;
    b) echo "B ファイル所有" ;;
    c) echo "C 行域(出荷)" ;;
    cref) echo "C 行域(参照)" ;;
    'c+') echo "C+ 交渉(出荷)" ;;
    'c+ref') echo "C+ 交渉(参照)" ;;
    ins) echo "I 挿入点(出荷)" ;;
    insref) echo "I 挿入点(参照)" ;;
    esac
}

# 段 c+ / c+ref だけが「ずらす」。表を分けるための判定
shifty() {
    case "$1" in
    'c+' | 'c+ref') return 0 ;;
    *) return 1 ;;
    esac
}

for lay in $(printf '%s' "$layouts" | tr ',' ' '); do
    for n in $(printf '%s' "$agents" | tr ',' ' '); do
        # 行域を配れるかを先に検算する。配れないまま走らせて「衝突 0」を
        # 出すのが一番たちが悪い
        if [ "$lay" = disjoint ]; then
            need=$((n * (band + 2)))
            if [ "$lines" -lt "$need" ]; then
                echo "行数が足りません: --agents $n --band $band には --lines $need 以上が要ります (いまは $lines)" >&2
                exit 2
            fi
        fi
        # 担当表は段より先に、(layout, 体数) ごとに 1 度だけ作る。
        # 「段 c と段 c+ は同じ担当表を使った」を作りで保証する
        plan_regions "$n" "$lines" "$band" "$lay" "$seed" >"$work/plan-$lay-$n"
        printf '%s\t%s\t%s\t%s\n' "$lay" "$n" "$lines" "$(demand_of "$work/plan-$lay-$n" "$lines")" \
            >>"$work/demand"

        for m in $(printf '%s' "$modes" | tr ',' ' '); do
            run_stage "$m" "$lay" "$n"
            printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
                "$lay" "$m" "$n" "$st_status" "$st_planned" "$st_done" "$st_denied" \
                "$st_confl_br" "$st_confl_files" "$st_hunks" "$st_confl_lines" "$st_human" \
                "$st_survived" "$st_edit_ms" "$st_merge_ms" \
                "$st_gate_p50" "$st_gate_p95" "$st_gate_max" "$st_gate_mean" \
                "$st_merge_p50" "$st_merge_p95" "$st_merge_max" "$st_merge_mean" \
                "${st_close_pairs:-0}" "$st_pred_checked" "$st_pred_hit" \
                "$st_asreq" "$st_shifted" \
                "$st_d_p50" "$st_d_p95" "$st_d_max" "$st_d_mean" \
                "$st_oob" "$st_badproto" "$st_bad_pairs" >>"$rows"
            if [ "$st_status" = skip ]; then
                printf 'skip\t%s\t%s\t%s\t%s\n' "$lay" "$m" "$n" "$st_reason" >>"$work/skips"
            elif [ "$m" != a ]; then
                # 衝突だけでなく「ファイルの外へ配った」「契約を破った」も主張の破れ
                if [ "$st_hunks" -gt 0 ] || [ "$st_oob" -gt 0 ] ||
                    [ "$st_badproto" -gt 0 ] || [ "$st_bad_pairs" -gt 0 ]; then
                    fail=1
                fi
            fi
        done
    done
done

# ── 報告 ──────────────────────────────────────────────────────────
out=/dev/stdout
[ "$json" = 0 ] || out=/dev/stderr

{
    echo "== coedit-bench: 同じファイルの違う行を N 体で同時に書く =="
    echo "   行数: $lines / 安全帯: $band 行 / 種: $seed / 体数: $agents"
    echo "   git: $git_version (merge-tree --write-tree: $([ "$has_merge_tree" = 1 ] && echo あり || echo 'なし — 予測の検算は skip'))"
    # **使ったバイナリの絶対パスと版、そして落とした候補の理由を必ず出す。**
    zai_identity "$zai" | sed 's/^/   /'
    if [ "$selfcheck" = 1 ]; then
        echo "   ** --self-check: 出荷物ではなく代役の zai です。**"
        echo "   ** ここの数字は製品の測定値ではありません (ハーネス自身の検査です) **"
    fi
    if [ "$region_aware" = 1 ]; then
        echo "   行域対応: あり (段 C を出荷物で実測します)"
    else
        echo "   行域対応: **なし** — 段 C は未測定。理由: $region_skip_reason"
    fi
    if [ "$shift_aware" = 1 ]; then
        echo "   交渉 (--shift): あり (段 C+ を出荷物で実測します)"
    else
        echo "   交渉 (--shift): **なし** — 段 C+ は未測定。理由: $shift_skip_reason"
    fi
    if [ "$insert_aware" = 1 ]; then
        echo "   挿入点 (#@N): あり (段 I を出荷物で実測します)"
    else
        echo "   挿入点 (#@N): **なし** — 段 I は未測定。理由: $insert_skip_reason"
    fi
    [ "$clock" != none ] || echo "   ** 時計が秒精度しかありません。ms 列は粗い値です **"
    echo
    echo "-- 供給と需要 (「入るのに断った」のか「そもそも入らない」のか) --"
    echo "   要求行数=書き換えたい行の総数 / 要求の幅=要求が集まっている範囲"
    echo "   必要行数=互いに素に置く最小 (sum+({体数}-1)x{安全帯}) / 点必要=幅 0 の挿入点として置くなら"
    printf '%-9s %6s %8s %8s %8s %8s %8s %8s  %s\n' \
        layout agents req_ln req_wd need_ln need_pt file_ln free 判定
    while IFS='	' read -r d_lay d_n d_total d_sum d_width d_need d_needpt d_free d_verdict; do
        printf '%-9s %6s %8s %8s %8s %8s %8s %8s  %s\n' \
            "$d_lay" "$d_n" "$d_sum" "$d_width" "$d_need" "$d_needpt" "$d_total" "$d_free" "$d_verdict"
    done <"$work/demand"
    echo
    printf '%-9s %-15s %4s %6s %5s %5s %5s %5s %6s %6s %6s %5s %6s %8s %8s\n' \
        layout 段 体数 状態 計画 完了 拒否 衝突枝 衝突F ハンク 衝突行 人手 生存 編集ms 統合ms
    while IFS='	' read -r r_lay r_mode r_n r_st r_plan r_done r_deny r_br r_cf r_hunk r_cl r_hum r_surv r_ems r_mms r_gp50 r_gp95 r_gmax r_gmean r_mp50 r_mp95 r_mmax r_mmean r_close r_pc r_ph r_asreq r_shifted r_dp50 r_dp95 r_dmax r_dmean r_oob r_bad r_badpair; do
        if [ "$r_st" = skip ]; then
            printf '%-9s %-15s %4s %6s %s\n' "$r_lay" "$(mode_label "$r_mode")" "$r_n" skip "(未測定)"
        else
            printf '%-9s %-15s %4s %6s %5s %5s %5s %5s %6s %6s %6s %5s %6s %8s %8s\n' \
                "$r_lay" "$(mode_label "$r_mode")" "$r_n" ok \
                "$r_plan" "$r_done" "$r_deny" "$r_br" "$r_cf" "$r_hunk" "$r_cl" "$r_hum" "$r_surv" "$r_ems" "$r_mms"
        fi
    done <"$rows"
    echo
    echo "-- ゲート待ち / 統合 1 回 (ms, p50/p95/max/mean)。max と mean の乖離が停止のサイン --"
    echo "   近_要求 = 担当表の時点で安全帯を割っている組 / 近_配布 = **実際に配られた**域で割っている組"
    echo "   近_配布 が 0 でなければゲートのバグ。統合が偶然きれいに通っても主張は壊れている"
    printf '%-9s %-15s %6s %22s %22s %8s %8s %10s\n' \
        layout 段 agents ゲートms 統合1回ms 近_要求 近_配布 予測的中
    while IFS='	' read -r r_lay r_mode r_n r_st r_plan r_done r_deny r_br r_cf r_hunk r_cl r_hum r_surv r_ems r_mms r_gp50 r_gp95 r_gmax r_gmean r_mp50 r_mp95 r_mmax r_mmean r_close r_pc r_ph r_asreq r_shifted r_dp50 r_dp95 r_dmax r_dmean r_oob r_bad r_badpair; do
        if [ "$r_st" = skip ]; then continue; fi
        printf '%-9s %-15s %6s %22s %22s %8s %8s %10s\n' \
            "$r_lay" "$(mode_label "$r_mode")" "$r_n" \
            "$r_gp50/$r_gp95/$r_gmax/$r_gmean" "$r_mp50/$r_mp95/$r_mmax/$r_mmean" \
            "$r_close" "$r_badpair" "$r_ph/$r_pc"
    done <"$rows"
    if awk -F'\t' '$2 ~ /^c\+/ { f = 1 } END { exit(f ? 0 : 1) }' "$rows"; then
        echo
        echo "-- ずらした距離 (交渉の代償)。64/0 だけを見ない — **何を払って買ったのか** --"
        printf '%-9s %-15s %6s %8s %8s %18s %8s\n' \
            layout 段 agents as_req shifted dist_p50/p95/max 範囲外
        # shellcheck disable=SC2034  # read は位置で受けるので、使わない列も名前が要る
        while IFS='	' read -r r_lay r_mode r_n r_st r_plan r_done r_deny r_br r_cf r_hunk r_cl r_hum r_surv r_ems r_mms r_gp50 r_gp95 r_gmax r_gmean r_mp50 r_mp95 r_mmax r_mmean r_close r_pc r_ph r_asreq r_shifted r_dp50 r_dp95 r_dmax r_dmean r_oob r_bad r_badpair; do
            shifty "$r_mode" || continue
            if [ "$r_st" = skip ]; then
                printf '%-9s %-15s %6s %8s\n' "$r_lay" "$(mode_label "$r_mode")" "$r_n" skip
                continue
            fi
            _d="$r_dp50/$r_dp95/$r_dmax"
            [ "$r_shifted" != 0 ] || _d="-"
            printf '%-9s %-15s %6s %8s %8s %18s %8s\n' \
                "$r_lay" "$(mode_label "$r_mode")" "$r_n" "$r_asreq" "$r_shifted" "$_d" "$r_oob"
        done <"$rows"
        echo "   範囲外 = 配られた行域がファイル (1〜$lines 行) の外に出た件数。0 でなければ主張が壊れている"
    fi
    if [ -f "$work/skips" ]; then
        echo
        echo "-- skip した段 (黙って飛ばさない) --"
        while IFS='	' read -r _s s_lay s_mode s_n s_reason; do
            printf '   %s / %s / %s 体: %s\n' "$s_lay" "$(mode_label "$s_mode")" "$s_n" "$s_reason"
        done <"$work/skips"
    fi
    echo
    if [ "$fail" = 0 ]; then
        echo "== 保護のある段は、衝突ハンク 0・範囲外配分 0・契約違反 0 でした =="
    else
        echo "== 保護のある段が壊れています (衝突 / 範囲外配分 / 契約違反 のいずれか) =="
    fi
} >"$out"

if [ "$json" = 1 ]; then
    awk -F'\t' -v lines="$lines" -v band="$band" -v seed="$seed" \
        -v gitver="$git_version" -v zaipath="${zai:-}" -v ra="$region_aware" \
        -v rr="$region_skip_reason" -v sa="$shift_aware" -v sr="$shift_skip_reason" \
        -v ia="$insert_aware" -v ir="$insert_skip_reason" \
        -v clk="$clock" -v fail="$fail" '
        BEGIN {
            printf "{\n  \"lines\": %d,\n  \"band\": %d,\n  \"seed\": %d,\n", lines, band, seed
            printf "  \"git\": \"%s\",\n  \"zai\": \"%s\",\n", gitver, zaipath
            printf "  \"region_aware\": %s,\n  \"region_skip_reason\": \"%s\",\n", (ra == 1 ? "true" : "false"), rr
            printf "  \"shift_aware\": %s,\n  \"shift_skip_reason\": \"%s\",\n", (sa == 1 ? "true" : "false"), sr
            printf "  \"insert_aware\": %s,\n  \"insert_skip_reason\": \"%s\",\n", (ia == 1 ? "true" : "false"), ir
            printf "  \"clock\": \"%s\",\n  \"claim_holds\": %s,\n  \"stages\": [\n", clk, (fail == 0 ? "true" : "false")
        }
        {
            if (NR > 1) printf ",\n"
            printf "    {\"layout\": \"%s\", \"mode\": \"%s\", \"agents\": %d, \"status\": \"%s\"", $1, $2, $3, $4
            if ($4 != "skip")
                printf ", \"planned\": %d, \"completed\": %d, \"denied\": %d, \"conflicted_branches\": %d, \"conflicted_files\": %d, \"hunks\": %d, \"conflict_lines\": %d, \"human_touches\": %d, \"survived_agents\": %d, \"edit_ms\": %d, \"merge_ms\": %d, \"gate_ms\": {\"p50\": %d, \"p95\": %d, \"max\": %d, \"mean\": %d}, \"merge_one_ms\": {\"p50\": %d, \"p95\": %d, \"max\": %d, \"mean\": %d}, \"too_close_pairs\": %d, \"predict_checked\": %d, \"predict_hit\": %d, \"as_requested\": %d, \"shifted\": %d, \"shift_dist\": {\"p50\": %d, \"p95\": %d, \"max\": %d, \"mean\": %d}, \"out_of_bounds\": %d, \"protocol_violations\": %d, \"granted_too_close_pairs\": %d", $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28, $29, $30, $31, $32, $33, $34, $35
            printf "}"
        }
        END { printf "\n  ],\n" }' "$rows"
    # 供給と需要。**「入るのに断った」を機械でも判定できるようにする**
    awk -F'\t' '
        BEGIN { printf "  \"demand\": [\n" }
        {
            if (NR > 1) printf ",\n"
            printf "    {\"layout\": \"%s\", \"agents\": %d, \"file_lines\": %d, \"requested_lines\": %d, \"request_width\": %d, \"need_lines\": %d, \"need_lines_as_points\": %d, \"free_lines\": %d, \"fits\": %s}",
                $1, $2, $3, $4, $5, $6, $7, $8, ($9 == "入る" ? "true" : "false")
        }
        END { printf "\n  ]\n}\n" }' "$work/demand"
fi

exit "$fail"
