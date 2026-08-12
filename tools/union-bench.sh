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
# ## 第 2 部: マーカを 1 つも置かない合成リポジトリ (= 他人のリポジトリ)
#
#   .gitignore / package.json / CHANGELOG.md / mod ブロック / code.rs へ
#   N 人が同時に追記し、baseline / union-nomarker / union-auto を比べる。
#   **誤自動解決の件数**も出す (0 でなければならない)。定義は下の方に。
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
# Windows (Git Bash / PowerShell) の既定コードページは UTF-8 ではないので、
# Python が日本語を stdout へ書いた瞬間に
# `UnicodeEncodeError: 'charmap' codec can't encode characters` で落ちる
# (CI の probe (windows-latest) が実際にこれで赤くなった)。
# **どの OS でも同じ出力になるよう UTF-8 を明示する。** 既に設定されていれば尊重する。
export PYTHONUTF8="${PYTHONUTF8:-1}"
export PYTHONIOENCODING="${PYTHONIOENCODING:-utf-8}"

writers_list="8 16 32"
driver=""
json=0
keep=0

usage() {
    sed -n '2,48p' "$0" | sed 's/^# \{0,1\}//'
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

# この計測の適格判定: 実行できれば足りる (マージドライバとしてしか使わない)。
union_capable() { [ -x "$1" ]; }

zai=""
if [ -z "$driver" ]; then
    zai_pick union_capable || true
    zai_identity "$zai" >&2
    if [ -n "$zai" ]; then
        driver="'$zai' merge-driver %O %A %B %L %P"
        driver_whole="'$zai' merge-driver --whole %O %A %B %L %P"
        driver_auto="'$zai' merge-driver --auto %O %A %B %L %P"
        driver_label="zai merge-driver ($zai)"
    else
        echo "使える zai がありません。ZAIVERN_BIN で指定するか --driver を渡してください。" >&2
        echo "  理由: ${zai_note:-(候補なし)}" >&2
        echo "  (cargo は呼びません。ビルド済みバイナリだけを使います)" >&2
        exit 1
    fi
else
    driver_whole="$driver"
    driver_auto="$driver"
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


# ══════════════════════════════════════════════════════════════════
#  第 2 部: **マーカが 1 つも無い**合成リポジトリ
# ══════════════════════════════════════════════════════════════════
#
# 第 1 部はマーカ方式の実測なので、**zaivern 自身のリポジトリでしか
# 再現できない**。ここは「他人のリポジトリ」を模す — マーカを 1 つも
# 置かず、どこにでもある 4 つの一覧だけを置く:
#
#   .gitignore      1 行 1 要素の一覧          (Flat)
#   package.json    依存表 (キーが違えば共存)   (Bracket + JSON 検査)
#   CHANGELOG.md    見出しつきの追記帳          (Journal / 重複を畳まない)
#   mods_block.rs   mod 宣言の連続ブロック      (Imports)
#   code.rs         **一覧ではない**関数の中身   (判定は None = 素の git のまま)
#
# code.rs を必ず入れてあるのは、**効かない条件も数字で出す**ため。ここが
# baseline と同じでなければ「一覧でないものまで混ぜている」ことになる。
#
# 条件:
#   baseline        ドライバ無し
#   union-nomarker  ドライバ有り (--auto 無し) = **マーカが無いので何もしない**
#   union-auto      ドライバ有り (--auto)      = 中身から一覧を見つける
#
# **誤自動解決** の定義 (0 でなければならない):
#   「一度も衝突を出さずに自動でマージされたファイル」のうち、
#     (a) どれかの書き手の追記が消えた / 二重になった
#     (b) 元からあった行が消えた
#     (c) 衝突マーカが残った
#     (d) package.json が JSON として壊れた  (python3 がある環境でだけ検査)
#   のいずれかを起こしたものの数。

PLAIN_KEYS="gi pkg chg mods code"

plain_file() {
    case "$1" in
        gi)   echo '.gitignore' ;;
        pkg)  echo 'package.json' ;;
        chg)  echo 'CHANGELOG.md' ;;
        mods) echo 'mods_block.rs' ;;
        code) echo 'code.rs' ;;
    esac
}

seed_plain() {
    _r=$1
    { echo '# build output'
      echo 'target/'
      echo 'node_modules/'
      echo '*.log'
    } > "$_r/.gitignore"

    { echo '{'
      echo '  "name": "bench",'
      echo '  "version": "0.1.0",'
      echo '  "dependencies": {'
      echo '    "alpha": "^1.0.0",'
      echo '    "zulu": "^9.0.0"'
      echo '  }'
      echo '}'
    } > "$_r/package.json"

    { echo '# Changelog'
      echo ''
      echo '## Unreleased'
      echo ''
      echo '- 最初の項目'
      echo '- 二つ目の項目'
      echo '- 三つ目の項目'
    } > "$_r/CHANGELOG.md"

    { echo '// この行は一覧ではない'
      echo 'fn helper() { }'
      echo ''
      echo 'mod app;'
      echo 'mod git;'
      echo 'mod term;'
      echo ''
      echo 'fn main() {}'
    } > "$_r/mods_block.rs"

    # **一覧ではない**もの。自動判定は降りて、素の git と同じ結果になるはず。
    { echo 'fn run() {'
      echo '    let mut n = 0;'
      echo '    n += 1;'
      echo '    println!("{n}");'
      echo '}'
    } > "$_r/code.rs"
}

# $1 ファイル, $2 目印の文字列, $3 挿し込む行
insert_after() {
    awk -v a="$2" -v ins="$3" '
        { print }
        !done_ && index($0, a) > 0 { print ins; done_ = 1 }
    ' "$1" > "$1.n" && mv "$1.n" "$1"
}

apply_writer_plain() {
    _r=$1; _i=$2
    printf '%s\n' "w_$_i/" >> "$_r/.gitignore"
    insert_after "$_r/package.json"  '"alpha"'  "    \"dep_$_i\": \"^1.0.0\","
    printf '%s\n' "- writer-$_i の変更" >> "$_r/CHANGELOG.md"
    insert_after "$_r/mods_block.rs" 'mod git;' "mod feat_$_i;"
    insert_after "$_r/code.rs" 'let mut n = 0;' "    n += $_i; // w_$_i"
}

# ちょうど 1 回だけ出てくるか。$1 ファイル, $2 文字列 → 0 = 良い / 1 = 違反
once() {
    _c=$(grep -c -F -- "$2" "$1" 2>/dev/null || true)
    [ "${_c:-0}" = 1 ] && return 0
    return 1
}

# $1 key, $2 repo, $3 人数 → 違反の数を標準出力へ
verify_plain_one() {
    _k=$1; _r=$2; _n=$3; _bad=0
    _f=$_r/$(plain_file "$_k")
    grep -q '^<<<<<<<' "$_f" 2>/dev/null && _bad=$((_bad + 1))
    # (b) 元の行がちょうど 1 回ずつ残っている
    case "$_k" in
        gi)   _basepats='target/|node_modules/|*.log' ;;
        pkg)  _basepats='"alpha"|"zulu"|"name"' ;;
        chg)  _basepats='- 最初の項目|- 二つ目の項目|- 三つ目の項目' ;;
        mods) _basepats='mod app;|mod term;|fn main() {}' ;;
        code) _basepats='fn run() {|let mut n = 0;|println!' ;;
    esac
    _rest=$_basepats
    while [ -n "$_rest" ]; do
        _p=${_rest%%|*}
        if [ "$_rest" = "$_p" ]; then _rest=''; else _rest=${_rest#*|}; fi
        once "$_f" "$_p" || _bad=$((_bad + 1))
    done
    # (a) 各書き手の追記がちょうど 1 回ずつ
    _i=1
    while [ "$_i" -le "$_n" ]; do
        case "$_k" in
            gi)   _p="w_$_i/" ;;
            pkg)  _p="\"dep_$_i\"" ;;
            chg)  _p="- writer-$_i の変更" ;;
            mods) _p="mod feat_$_i;" ;;
            code) _p="n += $_i; // w_$_i" ;;
        esac
        once "$_f" "$_p" || _bad=$((_bad + 1))
        _i=$((_i + 1))
    done
    # (d) JSON として妥当か (python3 がある環境でだけ)
    if [ "$_k" = pkg ] && [ "$json_check" = 1 ]; then
        python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$_f" >/dev/null 2>&1 \
            || _bad=$((_bad + 1))
    fi
    echo "$_bad"
}

# 1 条件 1 人数。標準出力に "総ハンク 総衝突行 人手が要ったファイル数 誤自動解決" を返す。
run_plain() {
    _cond=$1; _n=$2
    _r=$tmp/plain-$_cond-$_n
    mkdir -p "$_r"
    git -C "$_r" init -q -b main . >/dev/null 2>&1
    git -C "$_r" config user.email bench@example.invalid
    git -C "$_r" config user.name  bench
    git -C "$_r" config commit.gpgsign false
    seed_plain "$_r"
    if [ "$_cond" != baseline ]; then
        case "$_cond" in
            union-auto) _d=$driver_auto ;;
            *)          _d=$driver ;;
        esac
        git -C "$_r" config merge.zaivern-union-auto.name "bench"
        git -C "$_r" config merge.zaivern-union-auto.driver "$_d"
        : > "$_r/.gitattributes"
        for k in $PLAIN_KEYS; do
            echo "$(plain_file "$k") merge=zaivern-union-auto" >> "$_r/.gitattributes"
        done
    fi
    git -C "$_r" add -A >/dev/null; git -C "$_r" commit -qm base >/dev/null

    _i=1
    while [ "$_i" -le "$_n" ]; do
        git -C "$_r" checkout -q -b "w$_i" main
        apply_writer_plain "$_r" "$_i"
        git -C "$_r" commit -qam "writer-$_i" >/dev/null
        git -C "$_r" checkout -q main
        _i=$((_i + 1))
    done

    for k in $PLAIN_KEYS; do eval "_manual_$k=0"; done
    _total=0; _tlines=0
    _i=1
    while [ "$_i" -le "$_n" ]; do
        if ! git -C "$_r" merge --no-edit -q "w$_i" >/dev/null 2>&1; then
            for f in $(git -C "$_r" diff --name-only --diff-filter=U); do
                _c=$(grep -c '^<<<<<<<' "$_r/$f" 2>/dev/null || echo 0)
                _total=$((_total + _c))
                _tlines=$((_tlines + $(conflict_lines "$_r/$f")))
                for k in $PLAIN_KEYS; do
                    [ "$(plain_file "$k")" = "$f" ] && eval "_manual_$k=1"
                done
                resolve_keeping_both "$_r/$f"
                git -C "$_r" add "$f" >/dev/null
            done
            git -C "$_r" commit -q --no-edit >/dev/null 2>&1 || true
        fi
        _i=$((_i + 1))
    done

    _manual_files=0
    _mis=0
    for k in $PLAIN_KEYS; do
        eval "_m=\$_manual_$k"
        _v=$(verify_plain_one "$k" "$_r" "$_n")
        if [ "$_m" = 1 ]; then
            _manual_files=$((_manual_files + 1))
        elif [ "$_v" -gt 0 ]; then
            # **一度も人手が入っていないのに壊れている = 誤自動解決**
            _mis=$((_mis + 1))
            echo "   ! 誤自動解決: $(plain_file "$k") ($_cond, N=$_n, 違反 $_v 件)" >&2
        fi
    done
    echo "$_total $_tlines $_manual_files $_mis"
}

json_check=0
command -v python3 >/dev/null 2>&1 && json_check=1

printf '== マーカ無しの合成リポジトリ (.gitignore / package.json / CHANGELOG.md / mod ブロック)\n' >&2
[ "$json_check" = 1 ] || printf '   (python3 が無いので JSON の構文検査は省略)\n' >&2
hdr2=$(printf '%-15s %4s | %6s %7s | %11s %11s' condition N hunks c-lines manual-files misresolved)
printf '%s\n' "$hdr2" >&2
printf '%s\n' "$(echo "$hdr2" | tr -c '|\n' '-')" >&2

base_lines=''
for n in $writers_list; do
    for cond in baseline union-nomarker union-auto; do
        out=$(run_plain "$cond" "$n")
        set -- $out
        printf '%-15s %4s | %6s %7s | %11s %11s\n' "$cond" "$n" "$1" "$2" "$3" "$4" >&2
        [ "$cond" = baseline ] && base_lines=$2
        if [ "$cond" = union-auto ] && [ "${base_lines:-0}" -gt 0 ]; then
            printf '  → 衝突行の削減率 %d%% (%s → %s 行)\n' \
                "$(( (base_lines - $2) * 100 / base_lines ))" "$base_lines" "$2" >&2
        fi
        jrows2="${jrows2:-}{\"part\":\"plain\",\"condition\":\"$cond\",\"writers\":$n,\"hunks\":$1,\"conflict_lines\":$2,\"manual_files\":$3,\"misresolved\":$4},"
    done
    printf '\n' >&2
done

cat >&2 <<'NOTE2'
読み方:
  misresolved は **0 でなければならない** (定義はスクリプト冒頭)。
  union-nomarker が baseline と完全一致であることが
  「--auto を付けなければ素の git と 1 バイトも変わらない」の実測です。
  union-auto で残る manual-files は code.rs (一覧ではないもの) だけであるべきで、
  そこは **効かないのが正しい**。効く条件と効かない条件を同じ表に出しています。
NOTE2

if [ "$json" = 1 ]; then
    printf '{"driver":"%s","rows":[%s%s]}\n' "$driver_label" "$jrows" "${jrows2%,}"
fi
