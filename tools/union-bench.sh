#!/usr/bin/env sh
# 🧬 union マージドライバの効果を **実際の共有面を模したフィクスチャ**で測る。
#
# ## なぜ別に要るのか (tools/conflict-zero-bench.sh との違い)
#
# 合成ベンチの中身は `let value = N;` の**書き換え**ばかりで、union の守備範囲
# (**追記どうし**) の外にある。実測でも 8/16/32 人すべてでベースラインと同一
# (7/19/56 ハンク) = 効果ゼロだった。**これは配管の失敗ではなく、安全側に
# 倒した設計どおりの正しい「解決しない」**である。
#
# ただしその数字だけが残ると「union は効かない」と読めてしまう。そこで
# CLAUDE.md がこのリポジトリに残っていると認めている共有面そのものを
# フィクスチャにして、**効く条件と効かない条件の両方**を数字で出す。
#
# ## フィクスチャ (実際の共有面の写し)
#
#   config     設定一覧への追記            (config.rs 型)     … 効くはず
#   i18n       翻訳テーブルへの追記                            … 効くはず
#   mods       mod 宣言一覧への追記                            … 効くはず
#   changelog  CHANGELOG への追記                              … 効くはず
#   kb_same    [BindAction; N] + カウント検査 (keybinds.rs 型)
#              全員が同じ N+1 を書く → **衝突は出ないが数が合わなくなる**
#   kb_diff    同上だが各自が別の値を書く → **union でも解決できない**
#
# kb_* を必ず入れてあるのは、**できないことをできないと数字で示す**ため。
#
# ## 条件
#
#   baseline      ドライバ無し (素の git)
#   union-marked  ドライバ有り + フィクスチャにマーカ有り
#   union-plain   ドライバ有り + マーカ無し (= 素の git へ委譲するはず)
#   union-whole   ドライバ (--whole) + マーカ無し
#
# ## 使い方 (再現は 1 行)
#
#   tools/union-bench.sh                       既定 (8 16 32 人)
#   tools/union-bench.sh --writers "4 8"
#   tools/union-bench.sh --json                JSON は stdout、表は stderr
#   tools/union-bench.sh --keep                一時リポジトリを残す
#   tools/union-bench.sh --driver 'git merge-file --union %A %O %B'
#                                              参照ドライバで配管を検算する
#
# 環境変数 ZAIVERN_BIN で使う zai を明示できます。
#
# ## 副作用を持たない作り
#
#   * 一時リポジトリは `mktemp -d` (= $TMPDIR 由来)。パスを直書きしない
#   * HOME を一時ディレクトリへ差し替えるので、本物の ~/.gitconfig に触らない
#   * **cargo を呼ばない。** 既にある zai を使うだけで、ホストの target/ は
#     1 バイトも触らない
# shellcheck disable=SC1007  # `CDPATH= cd` は「その cd にだけ空の CDPATH を渡す」正しい書き方
# shellcheck disable=SC2154  # `_h_<fixture>` は eval で作る動的変数 (静的解析からは見えない)
# shellcheck disable=SC2086  # `set -- $out` / `for w in $writers_list` は意図的な語分割
set -eu

writers_list="8 16 32"
driver=""
json=0
keep=0

usage() {
    sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
}

while [ $# -gt 0 ]; do
    case "$1" in
        --writers) writers_list=${2:-}; shift 2 ;;
        --driver)  driver=${2:-}; shift 2 ;;
        --json)    json=1; shift ;;
        --keep)    keep=1; shift ;;
        -h | --help) usage ;;
        *) echo "不明な引数: $1 (--help で使い方)" >&2; exit 2 ;;
    esac
done

for w in $writers_list; do
    case "$w" in
        ''|*[!0-9]*) echo "--writers は正の整数の並びで指定してください: $w" >&2; exit 2 ;;
    esac
    [ "$w" -ge 2 ] || { echo "--writers は 2 以上 (1 人では衝突が定義できません): $w" >&2; exit 2; }
done

command -v git >/dev/null 2>&1 || { echo "git がありません。" >&2; exit 1; }

# ── ドライバの決定 ────────────────────────────────────────────────
zai=${ZAIVERN_BIN:-}
if [ -z "$driver" ]; then
    if [ -z "$zai" ]; then
        here=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
        for cand in "$here/target/debug/zai" "$here/target/release/zai"; do
            [ -x "$cand" ] && { zai=$cand; break; }
        done
        [ -n "$zai" ] || zai=$(command -v zai 2>/dev/null || true)
    fi
    if [ -n "$zai" ] && [ -x "$zai" ]; then
        driver="'$zai' merge-driver %O %A %B %L %P"
        driver_whole="'$zai' merge-driver --whole %O %A %B %L %P"
        driver_label="zai merge-driver ($zai)"
    else
        echo "zai が見つかりません。ZAIVERN_BIN で指定するか --driver を渡してください。" >&2
        echo "  (cargo は呼びません。ビルド済みバイナリだけを使います)" >&2
        exit 1
    fi
else
    driver_whole="$driver"
    driver_label="$driver"
fi

tmp=$(mktemp -d "${TMPDIR:-/tmp}/zv-union-bench.XXXXXX")
cleanup() { [ "$keep" = 1 ] || rm -rf "$tmp"; }
trap cleanup EXIT INT TERM
mkdir -p "$tmp/home"
HOME=$tmp/home
export HOME
GIT_CONFIG_NOSYSTEM=1; export GIT_CONFIG_NOSYSTEM
GIT_TERMINAL_PROMPT=0; export GIT_TERMINAL_PROMPT

FIXTURES="config i18n mods changelog kb_same kb_diff"

# ── フィクスチャの生成 ────────────────────────────────────────────
# $1 = リポジトリ, $2 = markers(1/0)
seed_fixtures() {
    _r=$1; _m=$2
    _b='// zaivern:union-begin'
    _e='// zaivern:union-end'
    [ "$_m" = 1 ] || { _b=''; _e=''; }

    { echo '// 設定一覧 (config.rs 型)'
      echo 'pub struct Config {'
      [ -n "$_b" ] && echo "    $_b"
      echo '    pub theme: String,'
      echo '    pub font_size: f32,'
      [ -n "$_e" ] && echo "    $_e"
      echo '}'
    } > "$_r/config.rs"

    { echo '// 翻訳テーブル (i18n 型)'
      echo 'pub const JA: &[(&str, &str)] = &['
      [ -n "$_b" ] && echo "    $_b"
      echo '    ("save", "保存"),'
      [ -n "$_e" ] && echo "    $_e"
      echo '];'
    } > "$_r/i18n.rs"

    { echo '// mod 宣言一覧'
      [ -n "$_b" ] && echo "$_b"
      echo 'mod app;'
      echo 'mod git;'
      [ -n "$_e" ] && echo "$_e"
      echo '// end of mods'
    } > "$_r/mods.rs"

    { echo '# Changelog'
      [ -n "$_b" ] && echo "<!-- zaivern:union-begin -->"
      echo '- 0.1.0 最初のリリース'
      [ -n "$_e" ] && echo "<!-- zaivern:union-end -->"
      echo ''
    } > "$_r/CHANGELOG.md"

    # keybinds.rs 型: **固定長配列 + カウント検査**。配列長の数値は
    # 領域の外にあり、union の守備範囲ではない。
    for f in kb_same kb_diff; do
      { echo '// キーバインド (keybinds.rs 型) — 固定長配列 + カウント検査'
        echo 'pub const ALL_ACTIONS: [BindAction; 2] = ['
        [ -n "$_b" ] && echo "    $_b"
        echo '    BindAction::Save,'
        echo '    BindAction::Open,'
        [ -n "$_e" ] && echo "    $_e"
        echo '];'
        echo ''
        echo '#[test]'
        echo 'fn count() { assert_eq!(ALL_ACTIONS.len(), 2); }'
      } > "$_r/$f.rs"
    done
}

# 挿入位置 = 終了マーカの直前 / 無ければ閉じ記号の直前。
# $1 ファイル, $2 挿入する行
append_entry() {
    _f=$1; _line=$2
    awk -v ins="$_line" '
        BEGIN { done_ = 0 }
        {
            if (!done_ && (index($0, "zaivern:union-end") > 0 ||
                           $0 == "];" || $0 == "}" || $0 == "// end of mods" || $0 == "")) {
                print ins; done_ = 1
            }
            print
        }
        END { if (!done_) print ins }
    ' "$_f" > "$_f.new" && mv "$_f.new" "$_f"
}

# 書き手 i の変更を作業ツリーへ適用する。
apply_writer() {
    _r=$1; _i=$2; _n=$3
    append_entry "$_r/config.rs"    "    pub opt_$_i: bool,"
    append_entry "$_r/i18n.rs"      "    (\"key_$_i\", \"訳_$_i\"),"
    append_entry "$_r/mods.rs"      "mod feat_$_i;"
    append_entry "$_r/CHANGELOG.md" "- 0.1.$_i writer-$_i の変更"
    # kb_same: 全員が「今の長さ + 1」= 3 を書く。両側が**同じ値**を書くので
    #          git は綺麗にマージするが、実際の要素数とはずれる。
    append_entry "$_r/kb_same.rs"   "    BindAction::Act$_i,"
    sed -e 's/\[BindAction; 2\]/[BindAction; 3]/' -e 's/len(), 2)/len(), 3)/' \
        "$_r/kb_same.rs" > "$_r/kb_same.rs.n" && mv "$_r/kb_same.rs.n" "$_r/kb_same.rs"
    # kb_diff: 各自が別の値を書く (取り込んだ時期が違う想定)。
    #          既存行の**書き換え**なので union は解決しない = 衝突として残る。
    _v=$((2 + _i))
    append_entry "$_r/kb_diff.rs"   "    BindAction::Act$_i,"
    sed -e "s/\[BindAction; 2\]/[BindAction; $_v]/" -e "s/len(), 2)/len(), $_v)/" \
        "$_r/kb_diff.rs" > "$_r/kb_diff.rs.n" && mv "$_r/kb_diff.rs.n" "$_r/kb_diff.rs"
}

# 衝突を「両側を残す (theirs 側の重複は畳む)」で解決する。
# **人が一覧に対してやる自明な解決の写し**で、ベースラインと union で
# 同じ解決規則を使わないと数字が比べられない (素朴に両側を連結すると
# ベースラインだけ行が二重になり、比較が壊れる)。
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
        inb { if (side == 1) { o[++no] = $0; seen[$0] = 1 } else { t[++nt] = $0 } ; next }
        { print }
    ' "$1" > "$1.r" && mv "$1.r" "$1"
}

# 衝突ブロックの中身の行数 (ハンク数だけでは「分割されて増えた」のか
# 「本当に人が読む量が増えた」のか区別が付かない)。
conflict_lines() {
    awk '
        /^<<<<<<< / { inb = 1; next }
        /^=======$/ { next }
        /^>>>>>>> / { inb = 0; next }
        inb { n++ }
        END { print n + 0 }
    ' "$1"
}

g() { git -C "$1" "$@" 2>/dev/null; }

# 1 条件 1 人数ぶんの実験。標準出力に "総ハンク数 <fixture:hunks>… kb_same_ok kb_diff_ok" を返す。
run_case() {
    _cond=$1; _n=$2
    _r=$tmp/$_cond-$_n
    mkdir -p "$_r"
    git -C "$_r" init -q -b main . >/dev/null 2>&1
    git -C "$_r" config user.email bench@example.invalid
    git -C "$_r" config user.name  bench
    git -C "$_r" config commit.gpgsign false
    _markers=0
    case "$_cond" in union-marked) _markers=1 ;; esac
    seed_fixtures "$_r" "$_markers"
    if [ "$_cond" != baseline ]; then
        case "$_cond" in
            union-whole) _d=$driver_whole ;;
            *)           _d=$driver ;;
        esac
        git -C "$_r" config merge.zaivern-union.name "bench"
        git -C "$_r" config merge.zaivern-union.driver "$_d"
        : > "$_r/.gitattributes"
        for f in $FIXTURES; do
            case "$f" in
                changelog) echo "CHANGELOG.md merge=zaivern-union" >> "$_r/.gitattributes" ;;
                *)         echo "$f.rs merge=zaivern-union"        >> "$_r/.gitattributes" ;;
            esac
        done
    fi
    git -C "$_r" add -A >/dev/null; git -C "$_r" commit -qm base >/dev/null

    _i=1
    while [ "$_i" -le "$_n" ]; do
        git -C "$_r" checkout -q -b "w$_i" main
        apply_writer "$_r" "$_i" "$_n"
        git -C "$_r" commit -qam "writer-$_i" >/dev/null
        git -C "$_r" checkout -q main
        _i=$((_i + 1))
    done

    # 直列マージ。衝突したら「両側を残す」で解決して続ける (人の作業の写し)。
    _total=0
    _tlines=0
    for f in $FIXTURES; do eval "_h_$f=0"; done
    _i=1
    while [ "$_i" -le "$_n" ]; do
        if ! git -C "$_r" merge --no-edit -q "w$_i" >/dev/null 2>&1; then
            for f in $(git -C "$_r" diff --name-only --diff-filter=U); do
                _c=$(grep -c '^<<<<<<<' "$_r/$f" 2>/dev/null || echo 0)
                _total=$((_total + _c))
                _tlines=$((_tlines + $(conflict_lines "$_r/$f")))
                _key=$(echo "$f" | sed -e 's/\.rs$//' -e 's/^CHANGELOG\.md$/changelog/')
                eval "_h_$_key=\$((\${_h_$_key:-0} + _c))"
                resolve_keeping_both "$_r/$f"
                git -C "$_r" add "$f" >/dev/null
            done
            git -C "$_r" commit -q --no-edit >/dev/null 2>&1 || true
        fi
        _i=$((_i + 1))
    done

    # カウント検査 (keybinds 型の回帰テストの代わり)
    _kb_ok=""
    for f in kb_same kb_diff; do
        _entries=$(grep -c '^    BindAction::' "$_r/$f.rs" || true)
        _decl=$(sed -n 's/.*\[BindAction; \([0-9]*\)\].*/\1/p' "$_r/$f.rs" | head -1)
        if [ "$_entries" = "$_decl" ]; then _kb_ok="$_kb_ok OK"; else _kb_ok="$_kb_ok NG($_decl/$_entries)"; fi
    done

    # 追記 4 種 (config / i18n / mods / changelog) はまとめて 1 列にする。
    _appends=$((_h_config + _h_i18n + _h_mods + _h_changelog))
    echo "$_total $_tlines $_appends $_h_kb_same $_h_kb_diff$_kb_ok"
}

# ── 実行 ──────────────────────────────────────────────────────────
printf '== union フィクスチャ・ベンチ\n' >&2
printf '   ドライバ: %s\n' "$driver_label" >&2
printf '   人数    : %s\n' "$writers_list" >&2
printf '   一時repo: %s%s\n\n' "$tmp" "$([ "$keep" = 1 ] && echo ' (--keep)')" >&2

# 見出しは ASCII で揃える (日本語は端末で全角幅になり printf の桁と合わない)。
hdr=$(printf '%-13s %4s | %6s %7s | %8s %8s %8s | %10s %10s' \
    condition N hunks c-lines appends kb_same kb_diff same_chk diff_chk)
printf '%s\n' "$hdr" >&2
printf '%s\n' "$(echo "$hdr" | tr -c '|\n' '-')" >&2

jrows=""
for n in $writers_list; do
    for cond in baseline union-marked union-plain union-whole; do
        out=$(run_case "$cond" "$n")
        set -- $out
        printf '%-13s %4s | %6s %7s | %8s %8s %8s | %10s %10s\n' \
            "$cond" "$n" "$1" "$2" "$3" "$4" "$5" "$6" "$7" >&2
        jrows="$jrows{\"condition\":\"$cond\",\"writers\":$n,\"hunks\":$1,\"conflict_lines\":$2,\"appends_hunks\":$3,\"kb_same_hunks\":$4,\"kb_diff_hunks\":$5,\"kb_same_count_check\":\"$6\",\"kb_diff_count_check\":\"$7\"},"
    done
    printf '\n' >&2
done

cat >&2 <<'NOTE'
列: hunks=総衝突ハンク c-lines=総衝突行 appends=追記4種の合計ハンク
    same_chk/diff_chk = カウント検査 (宣言長/実要素数)

読み方:
  追記4種 (config / i18n / mods / changelog) — union の守備範囲。ここが 0 になる。
  kb_*    (keybinds.rs 型)                   — 配列長の数値を**両側が書き換える**ので
                                               守備範囲の外。**減らない**。
  kb_same 全員が同じ値 (N+1) を書く → 衝突は出ないが、要素数と宣言長がずれる。
          **衝突ゼロ = 安全ではない**ことが count 検査 NG として出る (素の git も同じ)。
  kb_diff 各自が別の値を書く → union でも衝突は残る。ハンク数はむしろ**増える**
          (領域が解決するぶん、残る衝突が数値行だけに分割されるため)。
          人が読む量は「総衝突行」で見ること。
  union-plain がベースラインと**完全一致**であることが、
  「マーカが無いファイルでは素の git と 1 バイトも変わらない」の実測です。
NOTE

if [ "$json" = 1 ]; then
    printf '{"driver":"%s","rows":[%s]}\n' "$driver_label" "${jrows%,}"
fi
