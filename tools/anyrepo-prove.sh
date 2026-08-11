#!/usr/bin/env sh
# 「zaivern を使えば、**このリポジトリ**でも競合が起きない」を、
# **利用者自身のリポジトリ**に対して証明するハーネス。
#
# ## なぜ別に要るのか (tools/conflict-zero-bench.sh との違い)
#
# `tools/conflict-zero-bench.sh` と `tools/coedit-bench.sh` は、
# **その場で作った合成リポジトリ**でしか測っていない。48 個の同じ形の
# `src/mod_%04d.rs` は、行数も言語構成も履歴も現実には存在しない。
# だから出てくる数字は「この合成物では効く」までしか言えず、
# **「どのリポジトリでも」という主張の裏付けにはならない**。
#
# このハーネスは逆から入る。**利用者が自分のリポジトリのパスを渡す**と、
#
#   * そのリポジトリの**実在のファイル**を (サイズ・言語構成・
#     git log から出したホットスポットを反映して) 標本に取り
#   * 同じ担当表を **2 回**流して
#       ベースライン … 何もしないで N 体が同時に書く
#       zaivern あり … 予約 (`zai lease claim`) + ゲート (`zai hook`) を通す
#   * **同じ順序で直列マージ**して、衝突を数える
#
# ## 「証明」と名乗る前に必ず通す能力検査
#
# 検査を省くと「全員書けて衝突 0」という**静かな嘘**が出る。実際に
# `docs/conflict-zero.md` の計測では、能力検査を入れたことで
#   (1) CLI が retry 版のロックを使っておらず 64 体中 18 体しか通らない
#   (2) EOF を超える予約が縮んで 2 つの担当が重なる
#   (3) `--shift` が CLI へ届いていない
# の 3 件が見つかっている。よってこのハーネスは**測る前に**下記を確かめ、
# **1 つでも落ちたら結論を「証明できず」に落として理由を出す** (数字は出す)。
#
#   V1 版の照合     `zai --version` と このリポジトリの Cargo.toml
#                   **古い zai は知らないサブコマンドをワークスペース指定として
#                   扱い、GUI を起こして落ちる**。版が違えば以後は使わない
#   V2 導線の存在   `zai help` の本文に `zai lease` / `zai hook` があるか
#                   (help に無い語を投げない = 上の事故を構造的に避ける)
#   V3 行域の理解   重なる 2 本の行域予約が**断られる**か / 離れた 2 本が通るか
#                   (0.13.0 は `a.rs#L1-10` をただの文字列として飲んでいた)
#   V4 予約の取得   本番と同じ N 体・同じ担当表で**取れた数 / 要求数**
#   V5 ゲートの阻止 予約の**外**へ書こうとしたら本当に止まるか / 中なら通るか
#   V6 無汚染       対象リポジトリの指紋が実験の前後で 1 バイトも変わらないか
#
# ## 対象リポジトリを絶対に汚さない
#
#   * 実験は必ず `git clone --local --no-hardlinks` した**複製**の上で行う。
#     `--no-hardlinks` は「速さより安全」を選んだ結果 (ハードリンクでも
#     git はオブジェクトを書き換えないが、**証明の道具が元を共有していない**
#     ことを自明にしたい)
#   * `HOME` を一時ディレクトリへ差し替えるので、本物の `~/.zaivern` と
#     `~/.gitconfig` にも触らない
#   * 指紋 (HEAD / 全 ref / 作業ツリーの状態 / オブジェクト数 / worktree 一覧) を
#     前後で突き合わせ、**変わっていたら V6 を落とす**。読むだけの `git status` が
#     索引を書き戻して偽陽性を出さないよう `--no-optional-locks` を通す
#   * **cargo を 1 度も呼ばない**。既にある `zai` を使うだけ。ホストの
#     `target/` は 1 バイトも触らない
#
# ## リポジトリの形ごとの可否 (`--shapes` で実測できる)
#
# 「動かないなら理由を出す」ため、bare / submodule / 連結 worktree /
# sparse-checkout / LFS / shallow / 巨大 / 1 コミット / detached HEAD /
# 未コミットあり / 非 git を**その場で作って実際に通す**モードがある。
#
#   tools/anyrepo-prove.sh --shapes
#
# ## 使い方
#
#   tools/anyrepo-prove.sh                                 カレントのリポジトリを証明
#   tools/anyrepo-prove.sh --repo ~/dev/foo
#   tools/anyrepo-prove.sh --repo ~/dev/foo --writers 8,16,32,64
#   tools/anyrepo-prove.sh --overlap 1.0                   全員が同じ所へ向かう
#   tools/anyrepo-prove.sh --json                          JSON は stdout、表は stderr
#   tools/anyrepo-prove.sh --keep                          複製を残す
#   tools/anyrepo-prove.sh --shapes                        形ごとの可否を実測
#
# 環境変数 ZAIVERN_BIN で使う zai を明示できます。
#
# 終了コード: 0 = 証明できた / 1 = 証明できなかった (理由を出す) / 2 = 使い方の誤り
set -eu

# shellcheck disable=SC1007  # `CDPATH= cd` は「その cd にだけ空の CDPATH を渡す」正しい書き方
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

repo=""
writers="8,16"
overlap=0.5
picks=6
seed=20260811
hot_blocks=3
stride=8
scan_cap=4000
max_bytes=262144
json=0
keep=0
trace=""
shapes=0
want_shift=1
timeout_s=120

usage() {
    cat <<'EOS'
使い方: tools/anyrepo-prove.sh [オプション]

  --repo <パス>       証明する対象リポジトリ (既定: カレントの git ルート)
                      **複製の上でしか実験しません。元は 1 バイトも書きません**
  --writers <並び>    同時に書く人数。カンマ区切りで複数指定できます
                      例: --writers 8,16,32,64  (既定 8,16)
  --overlap <0..1>    書き手の重なり具合 (既定 0.5)
                        0.0 = 完全独立 (各自が自分だけのファイル群を書く)
                        0.5 = 半分はホットスポットへ向かう
                        1.0 = 全員がホットスポットへ向かう
  --picks <N>         1 人あたりの編集箇所 (既定 6)
  --seed <N>          担当表を決める乱数種 (既定 20260811)。同じ種なら同じ表
  --hot-blocks <N>    ホットスポット内で奪い合う行域の数 (既定 3)
                      **小さいほど重なる**。ファイル単位で重ねても行が離れれば
                      git は衝突させないので、行域まで重ねないとベースラインが
                      不自然にゼロになる
  --stride <N>        1 行域の行数 (既定 8)。region::SAFE_BAND=3 より広く取る
  --scan-cap <N>      標本に取るまでに調べるファイルの上限 (既定 4000)
                      巨大リポジトリで全件 stat しないための上限
  --max-bytes <N>     標本に取るファイルの上限バイト数 (既定 262144)
                      `zai hook` の読み取り上限 (1MiB) の内側に置く
  --no-shift          交渉 (`zai lease claim --shift`) を使わない
                      既定は使う (使えるかは能力検査で確かめてから)
  --timeout <秒>      zai / git 1 回あたりの上限 (既定 120)
  --trace <パス>      予約・許可・書き込みを 1 行 1 件の JSONL で書き出す
                      **「証明できず」と出たときに原因へ辿れる唯一の記録**
  --shapes            リポジトリの形ごとの可否をその場で作って実測する
  --json              JSON を stdout へ、人が読む表を stderr へ
  --keep              複製と作業ディレクトリを残す
  -h, --help          この使い方

環境変数 ZAIVERN_BIN で使う zai を明示できます。
EOS
}

die_usage() {
    echo "$1" >&2
    echo "  tools/anyrepo-prove.sh --help で使い方を出します" >&2
    exit 2
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
    --repo)
        repo=${2:-}
        shift 2 || true
        ;;
    --writers)
        writers=${2:-}
        shift 2 || true
        ;;
    --overlap)
        overlap=${2:-}
        shift 2 || true
        ;;
    --picks)
        picks=${2:-}
        shift 2 || true
        ;;
    --seed)
        seed=${2:-}
        shift 2 || true
        ;;
    --hot-blocks)
        hot_blocks=${2:-}
        shift 2 || true
        ;;
    --stride)
        stride=${2:-}
        shift 2 || true
        ;;
    --scan-cap)
        scan_cap=${2:-}
        shift 2 || true
        ;;
    --max-bytes)
        max_bytes=${2:-}
        shift 2 || true
        ;;
    --timeout)
        timeout_s=${2:-}
        shift 2 || true
        ;;
    --trace)
        trace=${2:-}
        shift 2 || true
        ;;
    --no-shift)
        want_shift=0
        shift
        ;;
    --shapes)
        shapes=1
        shift
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
        die_usage "不明なオプションです: $1"
        ;;
    esac
done

is_ratio "$overlap" || die_usage "--overlap は 0.0 〜 1.0 で指定してください: $overlap"
for n in "$picks" "$seed" "$hot_blocks" "$stride" "$scan_cap" "$max_bytes" "$timeout_s"; do
    is_num "$n" || die_usage "数値で指定してください: $n"
done
[ "$picks" -ge 1 ] || die_usage "--picks は 1 以上にしてください"
[ "$stride" -ge 4 ] || die_usage "--stride は 4 以上にしてください (region::SAFE_BAND=3 より広く)"
[ "$hot_blocks" -ge 1 ] || die_usage "--hot-blocks は 1 以上にしてください"
[ "$timeout_s" -ge 5 ] || die_usage "--timeout は 5 以上にしてください"

# 書き手の並びを検算する (**ここで弾かないと、python 側で「0 人の段」が
# 静かに成功して「衝突 0」という嘘が出る**)。
wlist=""
IFS_SAVE=$IFS
IFS=,
for w in $writers; do
    IFS=$IFS_SAVE
    is_num "$w" || die_usage "--writers は数値のカンマ区切りです: $writers"
    [ "$w" -ge 2 ] || die_usage "--writers の各値は 2 以上にしてください: $w"
    wlist="$wlist $w"
    IFS=,
done
IFS=$IFS_SAVE
[ -n "$wlist" ] || die_usage "--writers が空です"

# ── 前提の道具 ────────────────────────────────────────────────────
# python3 が要るのは (a) 種から決まる乱数を**実装非依存**に固定するため
# (POSIX sh の算術は符号付きで桁溢れの挙動が実装依存)、(b) 書き手を
# 実際に並列で走らせるため、(c) JSON を壊れずに出すため。
for need in python3 git; do
    if ! command -v "$need" >/dev/null 2>&1; then
        echo "$need が見つかりません。この計測には $need が要ります。" >&2
        exit 1
    fi
done

# ── 対象リポジトリを決める ────────────────────────────────────────
if [ -z "$repo" ] && [ "$shapes" = 0 ]; then
    repo=$(git rev-parse --show-toplevel 2>/dev/null || true)
    if [ -z "$repo" ]; then
        echo "--repo を指定してください (カレントディレクトリは git リポジトリではありません)" >&2
        exit 2
    fi
fi
if [ -n "$repo" ]; then
    # 相対パスでも受ける。**ここで絶対化しておかないと、複製の cwd 変更で壊れる**
    if [ -d "$repo" ]; then
        # shellcheck disable=SC1007  # `CDPATH= cd` は「その cd にだけ空の CDPATH を渡す」正しい書き方
        repo=$(CDPATH= cd -- "$repo" && pwd)
    fi
fi

# ── 使う zai を探す (**絶対にビルドしない**) ──────────────────────
# 版の照合は python 側でやるが、**help を引けない実行ファイルは候補にしない**
# (古い zai は知らない語をワークスペース指定として扱い GUI を起こす)。
zai_help_ok() {
    [ -x "$1" ] || return 1
    python3 - "$1" <<'EOS'
import subprocess, sys
try:
    p = subprocess.run([sys.argv[1], "help"], capture_output=True, text=True,
                       timeout=20, stdin=subprocess.DEVNULL)
except Exception:
    sys.exit(1)
sys.exit(0 if p.returncode == 0 and "zai lease" in p.stdout else 1)
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

zai=""
zai_pick zai_help_ok || true
zai_identity "$zai" >&2

# ── 使い捨ての作業場 ──────────────────────────────────────────────
work=$(mktemp -d 2>/dev/null || mktemp -d -t anyrepo)
# shellcheck disable=SC2329  # trap から呼ばれる (静的には呼び出しが見えない)
cleanup() {
    if [ "$keep" = 1 ]; then
        echo "== 作業ディレクトリを残しました: $work" >&2
    else
        # 複製の中に git worktree がぶら下がっているので、まとめて消す。
        # (worktree は 1 つあたり独立した checkout を持つ = 放置するとすぐ GB になる)
        rm -rf "$work" 2>/dev/null || true
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
export GIT_LFS_SKIP_SMUDGE=1 # LFS はポインタのまま扱う (実体を引きに行かない)
export GIT_AUTHOR_NAME=anyrepo-prove
export GIT_AUTHOR_EMAIL=anyrepo-prove@example.invalid
export GIT_COMMITTER_NAME=anyrepo-prove
export GIT_COMMITTER_EMAIL=anyrepo-prove@example.invalid

python3 - "$work/config.json" <<EOS
import json, sys
json.dump({
    "root": $(printf '%s' "$root" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'),
    "repo": $(printf '%s' "$repo" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'),
    "zai": $(printf '%s' "$zai" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'),
    "target_dir": $(printf '%s' "$target_dir" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'),
    "zai_note": $(printf '%s' "$zai_note" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'),
    "work": $(printf '%s' "$work" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'),
    "writers": [int(x) for x in "$wlist".split()],
    "overlap": float("$overlap"),
    "picks": $picks,
    "seed": $seed,
    "hot_blocks": $hot_blocks,
    "stride": $stride,
    "scan_cap": $scan_cap,
    "max_bytes": $max_bytes,
    "timeout": $timeout_s,
    "trace": $(printf '%s' "$trace" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'),
    "shift": $want_shift == 1,
    "json": $json == 1,
    "keep": $keep == 1,
    "shapes": $shapes == 1,
}, open(sys.argv[1], "w"))
EOS

cat >"$work/anyrepo.py" <<'ANYREPO_PROVE_PY'
#!/usr/bin/env python3
"""anyrepo-prove の計測エンジン。tools/anyrepo-prove.sh から呼ばれる。

**設計の芯**: 同じ担当表を 2 回流し、違うのは「書く前に予約とゲートを通すか」
だけにする。ここに 1 つでも差を混ぜると A/B が意味を失う。
"""

import json
import os
import re
import signal
import subprocess
import sys
import threading
import time
import hashlib

CFG = json.load(open(sys.argv[1], encoding="utf-8"))
ENV = dict(os.environ)

# 対象リポジトリの指紋に使う git は「読むだけ」に固定する。
# `git status` は既定で索引を書き戻すことがあり、それを「汚した」と
# 誤検出すると **V6 が永久に赤**になる (実際に踏みやすい罠)。
GIT_RO = ["git", "--no-optional-locks"]

# 標本に取るファイルの最低行数。--stride * (--hot-blocks + 1) より短いと
# ホットスポットの行域を切り出せない。
MIN_LINES_FACTOR = 2

# ホットスポット判定に使う履歴の深さ。全履歴を舐めると巨大リポジトリで
# **git だけで分単位**かかる (線形に伸びる)。
CHURN_COMMITS = 400


# ═════════════════════════════════════════════════════════════════════
#  プロセス実行 (**ツリーごと殺す**)
# ═════════════════════════════════════════════════════════════════════


class Res:
    __slots__ = ("rc", "out", "err", "timed_out", "ms")

    def __init__(self, rc, out, err, timed_out, ms):
        self.rc, self.out, self.err = rc, out, err
        self.timed_out, self.ms = timed_out, ms


def run(cmd, cwd=None, stdin=None, timeout=None):
    """1 回のコマンド実行。**タイムアウトしたらプロセスグループごと殺す**。

    直接の子だけを殺すと、孫がパイプを握ったまま残って読み取りが返らない。
    unix では子を独立したプロセスグループで起こし、`os.killpg` で畳む
    (`kill` コマンドへ負の PID を渡さないので、procps-ng の `-1...` を
     短オプションと解釈する事故とも無縁)。
    """
    t0 = time.perf_counter()
    kw = {}
    if os.name == "posix":
        kw["start_new_session"] = True
    p = subprocess.Popen(
        cmd,
        cwd=cwd,
        env=ENV,
        stdin=subprocess.PIPE if stdin is not None else subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        **kw
    )
    try:
        out, err = p.communicate(input=stdin, timeout=timeout or CFG["timeout"])
        timed_out = False
    except subprocess.TimeoutExpired:
        timed_out = True
        if os.name == "posix":
            try:
                os.killpg(os.getpgid(p.pid), signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                p.kill()
        else:
            p.kill()
        out, err = p.communicate()
    return Res(p.returncode, out or "", err or "", timed_out,
               (time.perf_counter() - t0) * 1000.0)


def git(args, cwd, check=True, ro=False):
    r = run((GIT_RO if ro else ["git"]) + args, cwd=cwd)
    if check and r.rc != 0:
        raise RuntimeError(
            "git %s が失敗しました (rc=%d, timeout=%s)\n%s%s"
            % (" ".join(args), r.rc, r.timed_out, r.out, r.err)
        )
    return r


# ═════════════════════════════════════════════════════════════════════
#  再現できる乱数 (64bit LCG)
# ═════════════════════════════════════════════════════════════════════


class Rng:
    """`random` を使わないのは、実装が変わると同じ種でも別の担当表が出て
    「同じ作業を両方の段へ流した」という前提が壊れるため。"""

    M = (1 << 64) - 1
    A = 6364136223846793005
    C = 1442695040888963407
    SCALE = 1000000

    def __init__(self, seed):
        self.s = (seed * self.A + self.C) & self.M

    def next(self):
        self.s = (self.s * self.A + self.C) & self.M
        return (self.s >> 33) & 0xFFFFFFFF

    def below(self, n):
        return self.next() % n if n > 0 else 0


# ═════════════════════════════════════════════════════════════════════
#  対象リポジトリの姿を調べる (**動かないなら理由を出す**)
# ═════════════════════════════════════════════════════════════════════


def survey(path, scan_cap):
    """形を調べて `traits` (性質) と、実験できるかどうかを返す。

    **黙って飛ばさない。** 実験できない形は `skip_reason` を必ず埋める。
    """
    t = {"path": path, "traits": [], "notes": [], "supported": True,
         "skip_reason": None}

    def trait(name, note=None):
        t["traits"].append(name)
        if note:
            t["notes"].append("%s: %s" % (name, note))

    if not path or not os.path.isdir(path):
        t["supported"] = False
        t["skip_reason"] = "ディレクトリがありません: %s" % path
        return t

    r = git(["rev-parse", "--git-dir"], path, check=False, ro=True)
    if r.rc != 0:
        trait("non-git")
        t["supported"] = False
        t["skip_reason"] = (
            "git リポジトリではありません "
            "(このハーネスは複製と直列マージで衝突を数えるので git が要ります)"
        )
        return t

    bare = git(["rev-parse", "--is-bare-repository"], path, check=False,
               ro=True).out.strip() == "true"
    if bare:
        trait("bare", "作業ツリーは複製側で作られます (元は bare のまま)")

    common = git(["rev-parse", "--git-common-dir"], path, check=False, ro=True).out.strip()
    gitdir = r.out.strip()
    if common and os.path.abspath(os.path.join(path, common)) != os.path.abspath(
        os.path.join(path, gitdir)
    ):
        trait("linked-worktree",
              "連結 worktree です。複製は共有リポジトリ側の既定ブランチから作られます")

    commits = git(["rev-list", "--count", "HEAD"], path, check=False, ro=True)
    n_commits = int(commits.out.strip()) if commits.rc == 0 and commits.out.strip() else 0
    if n_commits == 0:
        trait("no-commits")
        t["supported"] = False
        t["skip_reason"] = (
            "コミットが 1 つもありません (複製しても比べる基点がありません)"
        )
        return t
    if n_commits == 1:
        trait("single-commit", "履歴が 1 コミット。ホットスポットは履歴でなく大きさ順になります")
    t["commits"] = n_commits

    if git(["rev-parse", "--is-shallow-repository"], path, check=False,
           ro=True).out.strip() == "true":
        trait("shallow", "履歴が浅いので churn (変更頻度) の情報が乏しくなります")

    if git(["symbolic-ref", "-q", "HEAD"], path, check=False, ro=True).rc != 0:
        trait("detached-head", "複製されるのは既定ブランチです (今の detached な位置ではありません)")

    if not bare:
        st = git(["status", "--porcelain", "-uall"], path, check=False, ro=True)
        if st.rc == 0 and st.out.strip():
            trait("dirty",
                  "未コミットの変更は複製に入りません (git clone はコミット済みだけを運びます)")
        sp = git(["config", "--get", "core.sparseCheckout"], path, check=False, ro=True)
        if sp.out.strip() == "true":
            trait("sparse-checkout",
                  "複製は全ファイルを持ちます (sparse は作業ツリー側の設定なので複製されません)")

    if git(["cat-file", "-e", "HEAD:.gitmodules"], path, check=False, ro=True).rc == 0:
        trait("submodule",
              "サブモジュールは初期化しません (親リポジトリのファイルだけで測ります)")

    attrs = git(["cat-file", "-p", "HEAD:.gitattributes"], path, check=False, ro=True)
    if attrs.rc == 0 and "filter=lfs" in attrs.out:
        trait("lfs", "LFS はポインタのまま扱います (GIT_LFS_SKIP_SMUDGE=1)")

    ls = git(["ls-files"], path, check=False, ro=True)
    n_files = len(ls.out.split("\n")) - 1 if ls.rc == 0 else 0
    t["tracked"] = n_files
    if n_files > scan_cap:
        # **閾値は「調べ切れる件数」そのものにする。** 「10 万件なら巨大」
        # のような外から持ってきた数字は、--scan-cap を変えた瞬間に嘘になる
        trait("large",
              "追跡 %d 件 > --scan-cap %d 件。**全件は調べず**、種で決まる等間隔の"
              "間引きで %d 件だけ標本に取ります (同じ種なら同じ標本)"
              % (n_files, scan_cap, scan_cap))
    return t


def fingerprint(path):
    """**元のリポジトリを 1 バイトも書いていない**ことを裏取りする指紋。

    HEAD だけでは足りない (ref の追加・オブジェクトの増加・作業ツリーの
    変化・worktree の登録は HEAD を動かさない)。読むだけで済ませるため、
    git は必ず `--no-optional-locks` を通す。
    """
    if not os.path.isdir(path):
        return {"exists": False}
    out = {"exists": True}

    def h(s):
        return hashlib.sha256(s.encode("utf-8", "replace")).hexdigest()[:16]

    if git(["rev-parse", "--git-dir"], path, check=False, ro=True).rc != 0:
        # 非 git は (相対パス, サイズ, mtime_ns) の一覧を畳む
        rows = []
        for dirpath, dirnames, filenames in os.walk(path):
            dirnames.sort()
            for fn in sorted(filenames):
                ap = os.path.join(dirpath, fn)
                try:
                    st = os.stat(ap)
                except OSError:
                    continue
                rows.append("%s\t%d\t%d" % (os.path.relpath(ap, path), st.st_size,
                                            st.st_mtime_ns))
            if len(rows) > 20000:
                break
        out["tree"] = h("\n".join(rows))
        return out
    def nz(s):
        return sum(1 for x in s.split("\n") if x.strip())

    # **数も一緒に持つ。** ハッシュだけだと「変わった」としか言えず、
    # 利用者は「何が」「どれだけ」動いたのかを追えない (V6 の弱点だった)
    refs = git(["for-each-ref"], path, check=False, ro=True).out
    status = git(["status", "--porcelain", "-uall"], path, check=False, ro=True).out
    objs = git(["count-objects", "-v"], path, check=False, ro=True).out
    wts = git(["worktree", "list", "--porcelain"], path, check=False, ro=True).out
    conf = git(["config", "--local", "--list"], path, check=False, ro=True).out
    nobj = "?"
    for line in objs.split("\n"):
        if line.startswith("count:"):
            nobj = line.split(":", 1)[1].strip()
    out["head"] = git(["rev-parse", "HEAD"], path, check=False, ro=True).out.strip()
    out["refs"] = "%d 本 %s" % (nz(refs), h(refs))
    out["status"] = "%d 行 %s" % (nz(status), h(status))
    out["objects"] = "loose %s %s" % (nobj, h(objs))
    out["worktrees"] = "%d 個 %s" % (
        sum(1 for x in wts.split("\n") if x.startswith("worktree ")), h(wts))
    out["config"] = "%d 行 %s" % (nz(conf), h(conf))
    return out


def clone_repo(src, dst):
    """複製する。**元へは書かない経路だけを使う。**

    実測 (git 2.47 / macOS): `--local --no-hardlinks` は bare / 連結 worktree /
    shallow / detached HEAD / sparse / submodule 親 / 未コミットありのすべてで
    通った。通らないのは非 git と「コミットが 0 件」だけ。
    それでも 1 手に賭けない — 落ちたら file:// 転送へ落とし、
    **どちらで通ったかを結果に残す**。
    """
    attempts = [
        ("local-no-hardlinks", ["clone", "-q", "--local", "--no-hardlinks", src, dst]),
        ("file-transport", ["clone", "-q", "--no-local", "file://" + os.path.abspath(src), dst]),
    ]
    errors = []
    for name, args in attempts:
        if os.path.exists(dst):
            run(["rm", "-rf", dst])
        r = run(["git"] + args)
        if r.rc == 0 and os.path.isdir(os.path.join(dst, ".git")):
            head = git(["rev-parse", "HEAD"], dst, check=False)
            if head.rc == 0:
                return {"method": name, "ms": r.ms, "errors": errors}
            errors.append("%s: 複製できたが HEAD がありません" % name)
            continue
        errors.append("%s: %s" % (name, (r.err or r.out).strip().replace("\n", " ")[:160]))
    return {"method": None, "ms": 0.0, "errors": errors}


# ═════════════════════════════════════════════════════════════════════
#  複製から「実在のファイル」を標本に取る
# ═════════════════════════════════════════════════════════════════════


# テキストとして通してよい制御文字 (タブ・改行・改頁・垂直タブ・BS・ESC)。
TEXT_CTRL = set(b"\t\n\r\f\v\b\x1b")


def is_probably_text(path, cap=8192):
    """git がテキストとして三方向マージする見込みがあるか。

    **UTF-8 を要求しない。** ハーネスはバイト列で編集するので、Shift_JIS や
    latin-1 のファイルも壊さずに扱える。git 自身の判定も「先頭 8000 バイトに
    NUL があるか」だけなので、UTF-8 を要求すると**レガシー文字コードの
    リポジトリだけ標本から丸ごと消える** (= その利用者には何も証明していない
    のに「証明できた」と出る)。
    """
    try:
        with open(path, "rb") as fh:
            head = fh.read(cap)
    except OSError:
        return False
    if b"\x00" in head:
        return False
    # 制御文字だらけなら本文ではない (NUL を持たないバイナリ除け)
    ctrl = sum(1 for b in head if b < 0x20 and b not in TEXT_CTRL)
    return ctrl * 100 <= len(head)


def is_utf8(data):
    try:
        data.decode("utf-8")
        return True
    except UnicodeDecodeError:
        return False


def churn_map(clone):
    """git log から「よく直っているファイル」を数える = ホットスポット。

    **全履歴は舐めない。** 巨大リポジトリでは git だけで分単位かかり、
    しかも古い履歴は今のホットスポットを説明しない。
    """
    r = git(["log", "--no-merges", "-n", str(CHURN_COMMITS), "--format=", "--name-only"],
            clone, check=False)
    counts = {}
    if r.rc != 0:
        return counts
    for line in r.out.split("\n"):
        line = line.strip()
        if line:
            counts[line] = counts.get(line, 0) + 1
    return counts


def sample_corpus(clone, cfg, rng):
    """複製の**実在のファイル**から、編集できるものを選ぶ。

    返りは (候補リスト, 統計)。候補は churn の多い順 → 大きい順。
    """
    ls = git(["ls-files", "-z"], clone, check=False)
    tracked = [p for p in ls.out.split("\0") if p] if ls.rc == 0 else []
    stats = {"tracked_total": len(tracked)}

    # 巨大リポジトリでは全件 stat しない。**種で決まる間引き**にして、
    # 同じ種なら同じ標本になるようにする。
    scanned = tracked
    if len(tracked) > cfg["scan_cap"]:
        step = len(tracked) / float(cfg["scan_cap"])
        scanned = [tracked[int(i * step)] for i in range(cfg["scan_cap"])]
    stats["scanned"] = len(scanned)

    churn = churn_map(clone)
    min_lines = cfg["stride"] * (cfg["hot_blocks"] + 1) * MIN_LINES_FACTOR
    langs = {}
    cands = []
    eol = {"crlf": 0, "lf": 0, "cr": 0, "mixed": 0, "bom": 0, "non_utf8": 0}
    for rel in scanned:
        ap = os.path.join(clone, rel)
        try:
            st = os.stat(ap)
        except OSError:
            continue
        if not st.st_size or st.st_size > cfg["max_bytes"]:
            continue
        if not is_probably_text(ap):
            continue
        try:
            # **バイト列で数える。** テキストで開くと `\r\n` も孤立 `\r` も
            # `\n` へ畳まれ、行数が改行コードによって変わる
            with open(ap, "rb") as fh:
                data = fh.read()
        except OSError:
            continue
        nl = data.count(b"\n") + 1
        if nl < min_lines:
            continue
        crlf, lonecr, lf, bom = eol_kind(data)
        kinds = sum(1 for v in (crlf, lonecr, lf) if v)
        if bom:
            eol["bom"] += 1
        if not is_utf8(data):
            eol["non_utf8"] += 1
        if kinds > 1:
            eol["mixed"] += 1
        elif crlf:
            eol["crlf"] += 1
        elif lonecr:
            eol["cr"] += 1
        else:
            eol["lf"] += 1
        ext = os.path.splitext(rel)[1].lower() or "(拡張子なし)"
        langs[ext] = langs.get(ext, 0) + 1
        cands.append({
            "path": rel, "lines": nl, "bytes": st.st_size,
            "churn": churn.get(rel, 0), "ext": ext,
            "crlf": crlf, "lonecr": lonecr, "bom": bom,
        })
    cands.sort(key=lambda c: (-c["churn"], -c["lines"], c["path"]))
    stats["eligible"] = len(cands)
    stats["eol"] = eol
    stats["languages"] = dict(sorted(langs.items(), key=lambda kv: -kv[1])[:12])
    stats["min_lines"] = min_lines
    stats["max_bytes"] = cfg["max_bytes"]
    return cands, stats


# ═════════════════════════════════════════════════════════════════════
#  担当表 (**両方の段へ 1 バイト同じものを流す**)
# ═════════════════════════════════════════════════════════════════════


def make_plan(rng, cands, writers, cfg):
    """(書き手 → [(候補の添字, 行域の番号)]) を作る。

    * ホットセット = churn 上位 `picks` 件。全員がここへ向かい得る
    * 専有スライス = 残りを書き手ごとに互いに素へ切ったもの
    * 1 件ごとに確率 `overlap` でホットセットから、残りは専有から選ぶ
    * **ホットセットの中では行域も重ねる** (`--hot-blocks` 個の中から選ぶ)。
      ファイルだけ重ねて行が離れると git は衝突させないので、ここを重ねないと
      ベースラインが不自然にゼロになり、A/B が意味を失う
    """
    picks = cfg["picks"]
    hot_n = min(picks, len(cands))
    hot = list(range(hot_n))
    rest = list(range(hot_n, len(cands)))
    thr = int(round(cfg["overlap"] * Rng.SCALE))
    plan = []
    for i in range(writers):
        mine = []
        for k in range(picks):
            if rest and rng.below(Rng.SCALE) >= thr:
                # 専有スライス。書き手ごとに互いに素になるよう歩幅で切る
                idx = rest[(i * picks + k) % len(rest)]
                nb = max(1, (cands[idx]["lines"] - 1) // cfg["stride"])
                blk = rng.below(nb)
            else:
                idx = hot[rng.below(len(hot))] if hot else 0
                nb = max(1, (cands[idx]["lines"] - 1) // cfg["stride"])
                blk = rng.below(min(nb, cfg["hot_blocks"]))
            mine.append((idx, blk))
        plan.append(mine)
    return plan


def block_line(cand, blk, cfg):
    """行域番号 → 実際に書き換える 1 行 (1 始まり)。

    行域の**真ん中**を取る。端を取ると隣の行域と `SAFE_BAND` 以内で
    触れ合い、「重なっていないのに衝突する」が混ざる。
    """
    n = cand["lines"]
    line = blk * cfg["stride"] + cfg["stride"] // 2 + 1
    return max(1, min(n, line))


def duplicates(sets):
    """「2 人以上が**実際に書き換えた行**」の数。**モデル非依存の一次データ**。

    行域 (ブロック) 単位で数えてはいけない。`--shift` が同じブロックの中の
    別の行へずらすと、**衝突していないのに「重なった」と出る** (実測で
    ブロック単位だと 5 件、行単位だと 0 件になった)。守っている性質は
    「同じ行を 2 人が書かない」なので、そのまま行で数える。

    ここが 0 なら衝突は起こり得ない。逆にここが正なのに衝突 0 なら、
    それは「重なったのに衝突しなかった」= 本物の効き目である。
    """
    count = {}
    for s in sets:
        for k in s:
            count[k] = count.get(k, 0) + 1
    dup = sorted(k for k, v in count.items() if v > 1)
    # **どの行がぶつかったかを必ず出す。** 件数だけでは、赤くなった利用者が
    # 「自分のリポジトリの何が原因か」を追えない (再現に要る唯一の情報)
    return (len(dup), sum(v - 1 for v in count.values() if v > 1),
            ["%s:%d" % k for k in dup[:20]])


# ═════════════════════════════════════════════════════════════════════
#  書き手
# ═════════════════════════════════════════════════════════════════════

# `zai lease claim --shift` の契約: **標準出力の最後の行**が厳密に
# `granted <仕様>`。仕様は `<path>#L<a>-<b>` / `<path>#L<n>` / `<path>#@<n>`。
GRANTED = re.compile(r"^granted\s+(\S+)#(?:L(\d+)(?:-(\d+))?|@(\d+))$")


def parse_granted(out):
    last = ""
    for line in out.split("\n"):
        if line.strip():
            last = line.strip()
    m = GRANTED.match(last)
    if not m:
        return None
    if m.group(4):
        n = int(m.group(4))
        return (m.group(1), n, n)
    s = int(m.group(2))
    e = int(m.group(3)) if m.group(3) else s
    return (m.group(1), s, e)


MARK = b"  /*@ANYREPO-PROVE w%d @*/"


def edited_text(old, line_no, writer):
    """`line_no` (1 始まり) の**末尾に目印を足す**だけの編集。**バイト列で扱う**。

    言語に依らないし、意味も持たない。ビルドはしないので構文は問わない。
    大事なのは「同じ行を 2 人が触れば git が必ず衝突させる」ことだけ。

    ## なぜ str ではなくバイト列なのか (**証明を偽にしていた欠陥**)

    以前は `open(..., "r")` / `open(..., "w")` を `newline=""` 無しで使って
    いた。Python の既定は**汎用改行**なので、読んだ時点で `\\r\\n` が `\\n` へ
    畳まれ、書き戻すと**そのファイルは 1 行残らず LF へ書き換わる**。
    ハーネスが全行を書き換えるのだから、どの段でも必ず衝突する。実測で、
    ある実在リポジトリの失敗 5 件は**すべて CRLF の 4 ファイル**が原因で、
    LF へ正規化した複製では 8 通りの設定すべてが衝突 0 だった。
    **Windows 由来のリポジトリは、守られているのに「証明できず」と言われていた。**

    バイト列で扱えば BOM も混在改行も非 UTF-8 もそのまま通る。CRLF の行は
    末尾の `\\r` の**手前**へ目印を入れて、その行の改行も CRLF のまま残す。
    """
    lines = old.split(b"\n")
    i = min(max(0, line_no - 1), len(lines) - 1)
    body, cr = lines[i], b""
    if body.endswith(b"\r"):
        body, cr = body[:-1], b"\r"
    lines[i] = body + (MARK % writer) + cr
    return b"\n".join(lines), i + 1


def mark_only(old, new, writer, line_no):
    """編集が**`line_no` への目印 1 つの追加だけ**であることの裏取り。

    ここが偽になるのは、改行やエンコードを勝手に書き換えたときだけである
    (それこそが上の欠陥だった)。**測る側が対象を書き換えていないこと**を、
    毎回 1 件ずつ実際に確かめる — 口約束にしない。

    **ファイル全体から目印を 1 つ消して比べてはいけない。** 同じ書き手が
    同じファイルの別の行を 2 回編集すると、消えるのは前回の目印なので
    偽の警報になる (実際に 8 体で 7 件出した)。行ごとに突き合わせること。
    """
    o, n = old.split(b"\n"), new.split(b"\n")
    if len(o) != len(n):
        return False
    i = line_no - 1
    if not 0 <= i < len(o):
        return False
    for k in range(len(o)):
        if k != i and o[k] != n[k]:
            return False
    return n[i].replace(MARK % writer, b"", 1) == o[i]


def eol_kind(data):
    """バイト列の改行の様子。`(crlf, lonecr, lf, bom)` の 4 つ組。"""
    crlf = data.count(b"\r\n")
    lf = data.count(b"\n") - crlf
    cr = data.count(b"\r") - crlf
    return crlf, cr, lf, data.startswith(b"\xef\xbb\xbf")


class Writer(threading.Thread):
    """1 人ぶんの書き手。**段が違っても編集の適用コードは同一**。

    違うのは「書く前に予約とゲートを通すかどうか」の 1 点だけ。
    """

    def __init__(self, stage, idx, wt, picks, cands, cfg, zai=None, shift=False):
        super().__init__(daemon=True)
        self.stage, self.idx, self.wt, self.picks = stage, idx, wt, picks
        self.cands, self.cfg, self.zai, self.shift = cands, cfg, zai, shift
        self.session = "anyrepo-%s-%d" % (stage, idx)
        self.written = set()
        self.applied = 0
        self.claim_req = 0
        self.claim_ok = 0
        self.claim_moved = 0
        self.claim_refused = 0
        self.gate_calls = 0
        self.gate_denied = 0
        self.skipped_out_of_file = 0
        self.mangled = 0
        self.timeouts = 0
        self.gate_ms = []
        self.claim_ms = []
        self.trace = []
        self.error = None

    def note(self, **kw):
        if self.cfg.get("trace"):
            kw.update(stage=self.stage, writer=self.idx + 1, t=time.time())
            self.trace.append(kw)

    # ── zai 経由 ──────────────────────────────────────────────
    def claim(self, rel, line):
        """行域を予約する。`--shift` があるときは**断らずにずらして**もらう。"""
        spec = "%s#L%d-%d" % (rel, line, line)
        cmd = [self.zai, "lease", "claim", "--dir", self.wt, "--agent", "claude"]
        if self.shift:
            cmd.append("--shift")
        cmd.append(spec)
        r = run(cmd)
        self.claim_ms.append(r.ms)
        self.claim_req += 1
        if r.timed_out:
            self.timeouts += 1
            return None
        if r.rc != 0:
            self.claim_refused += 1
            return None
        if self.shift:
            g = parse_granted(r.out)
            if g is None:
                # 契約違反。**「取れた」ことにしない** — ここを緩めると
                # 「ずらしたつもりで元の行に書く」が素通りする
                self.claim_refused += 1
                return None
            self.claim_ok += 1
            if g[1] != line:
                self.claim_moved += 1
            return g[1]
        self.claim_ok += 1
        return line

    def gate(self, abspath, newtext):
        """`zai hook` に判定させる。**`Write` 形 (全文) で投げる**。

        `Edit` 形 (`old_string`) だと同じ行文が 2 度出るファイルで
        置換が一意に決まらず、`lease::applied_text` が `None` を返して
        **ファイル全体扱いへ広がる**。実リポジトリは重複行だらけなので、
        行域の効き目を測るには全文を渡すのが唯一正しい形 (実測で確認済み)。
        """
        # フックの payload は JSON なので、ここだけは文字列にするしかない。
        # **ファイルへ書くのはあくまでバイト列の方**なので、この復号で
        # 対象ファイルの改行やエンコードが変わることはない。
        payload = json.dumps({
            "session_id": self.session,
            "cwd": self.wt,
            "hook_event_name": "PreToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": abspath,
                           "content": newtext.decode("utf-8", "replace")},
        })
        r = run([self.zai, "hook", "--zaivern", "claude", "PreToolUse"], stdin=payload)
        self.gate_ms.append(r.ms)
        self.gate_calls += 1
        if r.timed_out:
            self.timeouts += 1
            return False
        return '"permissionDecision":"deny"' in r.out.replace(" ", "")

    # ── 本体 ──────────────────────────────────────────────────
    def run(self):
        try:
            for idx, blk in self.picks:
                c = self.cands[idx]
                rel, ap = c["path"], os.path.join(self.wt, c["path"])
                line = block_line(c, blk, self.cfg)
                asked = line
                if self.zai:
                    line = self.claim(rel, line)
                    self.note(ev="claim", path=rel, asked=asked, granted=line)
                    if line is None:
                        continue
                try:
                    # **バイト列で読む。** テキストで開くと CRLF が畳まれ、
                    # 書き戻した瞬間にファイル全体が LF へ変わる (= 全行衝突)
                    with open(ap, "rb") as fh:
                        old = fh.read()
                except OSError:
                    continue
                if line > old.count(b"\n") + 1:
                    # ずらした先がファイルの外。**書かずに数える** (縮めて
                    # 重ねると 2 人が同じ行に載る = 保護が消える)
                    self.skipped_out_of_file += 1
                    continue
                new, actual = edited_text(old, line, self.idx + 1)
                if not mark_only(old, new, self.idx + 1, actual):
                    # 目印以外が動いた = ハーネスが対象を壊している。
                    # **黙って続けない** (これを黙認していたのが元の欠陥)
                    self.mangled += 1
                    self.note(ev="mangled", path=rel, asked=asked, line=line)
                    continue
                if self.zai and self.gate(ap, new):
                    self.gate_denied += 1
                    self.note(ev="gate-deny", path=rel, asked=asked, line=line)
                    continue
                with open(ap, "wb") as fh:
                    fh.write(new)
                self.written.add((rel, actual))
                self.note(ev="write", path=rel, asked=asked, line=actual)
                self.applied += 1
        except Exception as e:  # スレッドの例外を握り潰さない
            self.error = e


# ═════════════════════════════════════════════════════════════════════
#  マージと衝突の計数
# ═════════════════════════════════════════════════════════════════════

CONF_OURS, CONF_BASE, CONF_THEIRS = 1, 2, 3


def resolve_union(path):
    """衝突マーカを外して**両側を残す**。返りは (ハンク数, 衝突行数)。

    片側を捨てると先行した書き手の成果が消えて、後続のマージが不自然に
    綺麗になる。両側を残すのが人手の解決にいちばん近い。
    """
    try:
        # **ここもバイト列。** テキストで開くと衝突を片付けるついでに
        # ファイル全体の改行が LF へ変わり、次のマージが必ず衝突する
        with open(path, "rb") as fh:
            src = fh.read().split(b"\n")
    except OSError:
        return 0, 0
    out, hunks, clines, mode = [], 0, 0, 0
    for line in src:
        # 目印の行だけ `\r` を落として見る (git は印を LF で書くが、
        # CRLF のファイルでは印の行にも `\r` が付くことがある)
        bare = line[:-1] if line.endswith(b"\r") else line
        if bare.startswith(b"<<<<<<< "):
            hunks += 1
            mode = CONF_OURS
            continue
        if mode and bare.startswith(b"||||||| "):
            mode = CONF_BASE
            continue
        if mode and bare == b"=======":
            mode = CONF_THEIRS
            continue
        if mode and bare.startswith(b">>>>>>> "):
            mode = 0
            continue
        if mode == CONF_BASE:
            continue
        if mode:
            clines += 1
        out.append(line)
    with open(path, "wb") as fh:
        fh.write(b"\n".join(out))
    return hunks, clines


def merge_all(intdir, branches):
    st = {"merges": len(branches), "conflict_merges": 0, "conflict_files": 0,
          "hunks": 0, "lines": 0, "conflict_files_unique": []}
    uniq = set()
    for b in branches:
        p = git(["-c", "merge.conflictstyle=merge", "merge", "--no-edit", "-q", b],
                intdir, check=False)
        if p.rc == 0:
            continue
        files = git(["diff", "--name-only", "--diff-filter=U"], intdir).out.split("\n")
        files = [f for f in files if f]
        if not files:
            raise RuntimeError("マージが衝突以外の理由で失敗しました: %s%s" % (p.out, p.err))
        st["conflict_merges"] += 1
        st["conflict_files"] += len(files)
        for f in files:
            uniq.add(f)
            h, c = resolve_union(os.path.join(intdir, f))
            st["hunks"] += h
            st["lines"] += c
        git(["add", "-A"], intdir)
        git(["commit", "-q", "--no-edit"], intdir)
    st["conflict_files_unique"] = sorted(uniq)[:20]
    return st


# ═════════════════════════════════════════════════════════════════════
#  段
# ═════════════════════════════════════════════════════════════════════


SPEC = re.compile(r"^(.*?)#(?:L(\d+)(?:-(\d+))?|@(\d+))$")


def ledger_overlaps(zai, clone):
    """台帳そのものが**別々の持ち主へ重なる行域を配っていないか**を見る。

    「衝突が残った」だけでは、原因が (a) zaivern が二重に配った のか
    (b) ハーネスが予約の外へ書いた のか区別が付かない。ここを分けないと、
    赤が出たときに製品とハーネスのどちらを直せばよいか永久に決まらない。
    """
    r = run([zai, "lease", "list", "--dir", clone, "--json"])
    if r.rc != 0:
        return {"checked": False, "reason": (r.err or r.out).strip()[:120]}
    try:
        st = json.loads(r.out)
    except ValueError:
        return {"checked": False, "reason": "lease list --json が JSON ではありません"}
    spans = []
    for l in st.get("leases", []):
        h = l.get("holder", {})
        who = "%s|%s|%s" % (h.get("agent", ""), h.get("session", ""), h.get("cwd", ""))
        for pat in l.get("patterns", []):
            m = SPEC.match(pat)
            if not m:
                # 行域指定でない = ファイル全体。重なり判定は行域どうしに限る
                continue
            if m.group(4):
                a = b = int(m.group(4))
            else:
                a = int(m.group(2))
                b = int(m.group(3)) if m.group(3) else a
            spans.append((m.group(1), a, b, who))
    bad = []
    by_path = {}
    for path, a, b, who in spans:
        by_path.setdefault(path, []).append((a, b, who))
    for path, rows in by_path.items():
        rows.sort()
        for i in range(len(rows)):
            for j in range(i + 1, len(rows)):
                a1, b1, w1 = rows[i]
                a2, b2, w2 = rows[j]
                if a2 > b1:
                    break
                if w1 != w2:
                    bad.append("%s L%d-%d と L%d-%d" % (path, a1, b1, a2, b2))
    return {"checked": True, "spans": len(spans), "overlaps": len(bad),
            "detail": sorted(set(bad))[:10]}


def pct(v, q):
    if not v:
        return 0.0
    s = sorted(v)
    return s[min(len(s) - 1, int(round(q * (len(s) - 1))))]


def worktrees_for(clone, work, name, n, base):
    dirs, branches = [], []
    for i in range(n):
        br = "%s-w-%d" % (name, i + 1)
        wt = os.path.join(work, name, "w-%d" % (i + 1))
        git(["worktree", "add", "-q", "-b", br, wt, base], clone)
        dirs.append(wt)
        branches.append(br)
    return dirs, branches


def drop_worktrees(clone, dirs, branches):
    """**統合が済んだ worktree は即座に消す。**

    1 つあたり独立した checkout を持つので、64 体 × 2 段 × 4 サイズを
    放置するとディスクだけでなくページキャッシュも押し出される。
    """
    for wt in dirs:
        git(["worktree", "remove", "--force", wt], clone, check=False)
    for br in branches:
        git(["branch", "-D", br], clone, check=False)
    git(["worktree", "prune"], clone, check=False)


def run_stage(name, clone, work, base, plan, cands, cfg, zai=None, shift=False):
    t_all = time.perf_counter()
    dirs, branches = worktrees_for(clone, work, name, len(plan), base)

    ws = [Writer(name, i, dirs[i], plan[i], cands, cfg, zai, shift)
          for i in range(len(plan))]
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
        if git(["status", "--porcelain"], wt).out.strip():
            git(["add", "-A"], wt)
            git(["commit", "-q", "-m", "writer-%d" % (i + 1)], wt)

    intdir = os.path.join(work, name, "integration")
    git(["worktree", "add", "-q", "-b", "%s-int" % name, intdir, base], clone)
    t0 = time.perf_counter()
    st = merge_all(intdir, branches)
    t_merge = (time.perf_counter() - t0) * 1000.0

    if cfg.get("trace"):
        with open(cfg["trace"], "a", encoding="utf-8") as fh:
            for w_ in ws:
                for rec in w_.trace:
                    fh.write(json.dumps(rec, ensure_ascii=False) + "\n")
    dup_files, dup_writes, dup_detail = duplicates([w.written for w in ws])
    gate_ms = [x for w in ws for x in w.gate_ms]
    claim_ms = [x for w in ws for x in w.claim_ms]
    out = dict(
        stage=name,
        writers=len(plan),
        planned=sum(len(p) for p in plan),
        applied=sum(w.applied for w in ws),
        claim_requested=sum(w.claim_req for w in ws),
        claim_granted=sum(w.claim_ok for w in ws),
        claim_moved=sum(w.claim_moved for w in ws),
        claim_refused=sum(w.claim_refused for w in ws),
        gate_calls=sum(w.gate_calls for w in ws),
        gate_denied=sum(w.gate_denied for w in ws),
        skipped_out_of_file=sum(w.skipped_out_of_file for w in ws),
        mangled=sum(w.mangled for w in ws),
        timeouts=sum(w.timeouts for w in ws),
        dup_lines=dup_files,
        dup_writes=dup_writes,
        dup_detail=dup_detail,
        gate_p50=pct(gate_ms, 0.50), gate_p95=pct(gate_ms, 0.95),
        claim_p50=pct(claim_ms, 0.50), claim_p95=pct(claim_ms, 0.95),
        wall_edit_ms=t_edit, wall_merge_ms=t_merge,
        wall_total_ms=(time.perf_counter() - t_all) * 1000.0,
        **st
    )
    drop_worktrees(clone, dirs + [intdir], branches + ["%s-int" % name])
    return out


# ═════════════════════════════════════════════════════════════════════
#  能力検査 (**測る前に。1 つでも落ちたら「証明」と言わない**)
# ═════════════════════════════════════════════════════════════════════


def expected_version(root):
    """このリポジトリの Cargo.toml が名乗る版。**バイナリの取り違えの番人**。

    `cargo test --bin zai` は bin を作らないので、`target/*/zai` は前の
    実行の残骸かもしれない。古い版は新しいサブコマンドを知らず、
    **知らない語をワークスペース指定として扱う**ので GUI を起こして落ちる。
    """
    p = os.path.join(root, "Cargo.toml")
    try:
        with open(p, encoding="utf-8") as fh:
            in_pkg = False
            for line in fh:
                s = line.strip()
                if s.startswith("["):
                    in_pkg = s == "[package]"
                    continue
                if in_pkg and s.startswith("version"):
                    m = re.search(r'"([^"]+)"', s)
                    if m:
                        return m.group(1)
    except OSError:
        pass
    return None


def check_version(zai, root, checks):
    want = expected_version(root)
    if not zai:
        checks.append({"id": "V1", "name": "zai の版を照合", "ok": False,
                       "detail": "zai が見つかりません (ZAIVERN_BIN / target/*/zai / PATH のどれにも)"})
        return None
    r = run([zai, "--version"])
    got = r.out.strip().split()[-1] if r.rc == 0 and r.out.strip() else None
    ok = bool(got and want and got == want)
    checks.append({
        "id": "V1", "name": "zai の版を照合", "ok": ok,
        "detail": "実行ファイル %s が名乗る版 %s / Cargo.toml が名乗る版 %s%s"
                  % (zai, got, want,
                     "" if ok else " — **別物を掴んでいます。古い版は知らない語を"
                                    "ワークスペース指定として扱い GUI を起こします**"),
    })
    return got


# ── V7: バイナリの素性 (**`touch` で破れない側の判定**) ─────────────
#
# 元の関所は「ソースの mtime > バイナリの mtime なら古い」だけだった。
# **中身の違うバイナリを stale と弾いた直後に `touch` を 1 回する**と、
# 版は同じ・mtime は新しいので受け入れられ、ハーネスは「証明できた」と出す。
# 端から端まで再現した。
#
# ここで打つ手は「**前回の判定を、バイナリの同一性とソースの内容ハッシュで
# 覚えておく**」こと。`touch` は mtime しか動かさないので、
#   * バイナリが同じ (unix: dev+inode+サイズ / windows: 作成時刻+サイズ) で
#   * ソースの内容ハッシュが前回と同じ
# なら**前回の判定をそのまま繰り返す** (= 一度 stale と判った物は touch では
# 生き返らない)。ソースの内容だけが変わった場合は、バイナリが同じである以上
# 中身は追いついていないので **無条件に stale**。
#
# **正直に書く弱点**: スタンプが 1 つも無い状態での初回だけは mtime しか
# 手掛かりが無い。これを閉じるには build.rs でソースのハッシュをバイナリへ
# 焼き込むしかなく、それは別の担当ファイルなのでここでは出来ない。
STAMP_SUFFIX = ".anyrepo-srcstamp"


def bin_identity(path):
    """`touch` で動かない実行ファイルの同一性。**両 OS を実装する**。"""
    try:
        st = os.stat(path)
    except OSError:
        return None
    if os.name == "nt":
        # Windows に inode は無いが作成時刻がある。`touch` 相当
        # (Set-ItemProperty LastWriteTime) では動かない
        return "win:%d:%d" % (int(getattr(st, "st_birthtime", st.st_ctime)), st.st_size)
    return "unix:%d:%d:%d" % (st.st_dev, st.st_ino, st.st_size)


def source_digest(root):
    """`src/**.rs` + `Cargo.toml` + `build.rs` の**内容**の指紋。

    ハッシュは相対パスを混ぜてから内容を流し込む (名前の入れ替えも拾う)。
    """
    src = os.path.join(root, "src")
    if not os.path.isdir(src):
        return None, 0, 0
    files = []
    for dirpath, dirnames, filenames in os.walk(src):
        dirnames.sort()
        for fn in sorted(filenames):
            if fn.endswith(".rs"):
                files.append(os.path.join(dirpath, fn))
    for extra in ("Cargo.toml", "build.rs"):
        p = os.path.join(root, extra)
        if os.path.isfile(p):
            files.append(p)
    files.sort()
    hsh = hashlib.sha256()
    total = 0
    for p in files:
        try:
            with open(p, "rb") as fh:
                data = fh.read()
        except OSError:
            continue
        rel = os.path.relpath(p, root).replace(os.sep, "/")
        hsh.update(rel.encode("utf-8"))
        hsh.update(b"\0%d\0" % len(data))
        hsh.update(data)
        total += len(data)
    if not files:
        return None, 0, 0
    return hsh.hexdigest(), len(files), total


def newest_src_mtime(root):
    """`src/**.rs` + `Cargo.toml` + `build.rs` のいちばん新しい mtime。"""
    best, who = 0.0, None
    src = os.path.join(root, "src")
    for dirpath, dirnames, filenames in os.walk(src):
        dirnames.sort()
        for fn in filenames:
            if not fn.endswith(".rs"):
                continue
            try:
                m = os.path.getmtime(os.path.join(dirpath, fn))
            except OSError:
                continue
            if m > best:
                best, who = m, fn
    for extra in ("Cargo.toml", "build.rs"):
        p = os.path.join(root, extra)
        try:
            m = os.path.getmtime(p)
        except OSError:
            continue
        if m > best:
            best, who = m, extra
    return (who, best) if who else (None, 0.0)


def judge_candidate(path, digest, src_mtime):
    """1 つの実行ファイルの判定と、次回のための記録。返りは (verdict, 理由)。

    * バイナリが前回と同一 (touch では動かない同一性) で内容ハッシュも同じ
      → **前回の判定を繰り返す** (一度 stale と判った物は touch で生き返らない)
    * バイナリが同一なのにソースの内容が変わった → 無条件に stale
    * それ以外 (初めて見る / 建て直された) → mtime で判定して**記録する**
    """
    ident = bin_identity(path)
    if ident is None:
        return "missing", "実行ファイルがありません"
    stamp_path = path + STAMP_SUFFIX
    prev = None
    try:
        with open(stamp_path, encoding="utf-8") as fh:
            prev = json.load(fh)
    except (OSError, ValueError):
        prev = None
    if prev and prev.get("bin") == ident and prev.get("src") == digest:
        v = prev.get("verdict", "usable")
        return v, ("前回と同じ実行ファイル・同じソース内容なので前回の判定"
                   "「%s」を繰り返します (**`touch` では変わりません**)" % v)
    if prev and prev.get("bin") == ident:
        why = ("実行ファイルは前回と同一なのに**ソースの内容が変わりました**。"
               "中身が追いついていないので使えません")
        v = "stale"
    else:
        try:
            bm = os.path.getmtime(path)
        except OSError:
            return "missing", "実行ファイルがありません"
        if src_mtime > bm:
            v, why = "stale", "ソースの方が新しい (mtime による初回判定)"
        else:
            v, why = "usable", ("この実行ファイルの記録はまだありません。"
                                "**初回は mtime しか手掛かりが無く `touch` で"
                                "騙せます** — 判定を記録したので次回からは"
                                "ソースの内容で見ます")
    try:
        tmp = "%s.%d.tmp" % (stamp_path, os.getpid())
        with open(tmp, "w", encoding="utf-8") as fh:
            json.dump({"bin": ident, "src": digest, "verdict": v}, fh)
        os.replace(tmp, stamp_path)
    except OSError as e:
        why += " (記録は残せませんでした: %s)" % e
    return v, why


def zai_candidates(cfg):
    """スタンプを残す対象。**選ばれなかった候補にも残す**のが要点。

    選ばれなかった (= 古いと弾かれた) 実行ファイルにこそ「stale だった」と
    書き残す必要がある。書き残さないと、`touch` された次の回に
    「初めて見る実行ファイル」として素通りしてしまう。
    """
    out = []
    for p in (os.environ.get("ZAIVERN_BIN"), cfg.get("zai")):
        if p:
            out.append(p)
    td = cfg.get("target_dir") or os.path.join(cfg["root"], "target")
    for prof in ("release", "debug"):
        for name in ("zai", "zai.exe"):
            out.append(os.path.join(td, prof, name))
    seen, uniq = set(), []
    for p in out:
        ap = os.path.abspath(p)
        if ap not in seen and os.path.isfile(ap):
            seen.add(ap)
            uniq.append(ap)
    return uniq


def check_binary_provenance(cfg, checks):
    """V7。**前回の判定を内容ハッシュで覚えて `touch` を無効化する。**"""
    root, zai = cfg["root"], cfg["zai"] or None
    t0 = time.perf_counter()
    digest, nfiles, nbytes = source_digest(root)
    _who, src_mtime = newest_src_mtime(root)
    ms = (time.perf_counter() - t0) * 1000.0
    cost = "ソース %d ファイル / %d バイトの sha256 に %.1f ms" % (nfiles, nbytes, ms)
    if digest is None:
        checks.append({
            "id": "V7", "name": "バイナリの素性 (touch 耐性)", "ok": True,
            "detail": "ソースツリーが隣に無いので内容では測れません "
                      "(**mtime だけが頼りです。`touch` で騙せます**)"})
        return True
    seen = []
    for p in zai_candidates(cfg):
        v, why = judge_candidate(p, digest, src_mtime)
        seen.append((p, v, why))
    if not zai:
        # **選ばれなかった理由をここにも出す。** stderr の 1 行を見落とした
        # 利用者が「なぜ全部 NG なのか」を追えなくなる
        note = (cfg.get("zai_note") or "").strip()
        checks.append({
            "id": "V7", "name": "バイナリの素性 (touch 耐性)", "ok": False,
            "detail": "zai が選ばれていません。見えた候補 %d 件は記録しました (%s)%s"
                      % (len(seen), cost, ("。選定の記録: " + note) if note else "")})
        return False
    mine = [(p, v, why) for p, v, why in seen if os.path.abspath(p) == os.path.abspath(zai)]
    if not mine:
        checks.append({"id": "V7", "name": "バイナリの素性 (touch 耐性)", "ok": True,
                       "detail": "選ばれた実行ファイルを stat できませんでした"})
        return True
    _p, v, why = mine[0]
    others = ["%s=%s" % (os.path.basename(os.path.dirname(p)), vv)
              for p, vv, _ in seen if os.path.abspath(p) != os.path.abspath(zai)]
    checks.append({
        "id": "V7", "name": "バイナリの素性 (touch 耐性)", "ok": v == "usable",
        "detail": "%s: %s (%s)%s" % (zai, why, cost,
                                     ("。他の候補: " + ", ".join(others)) if others else ""),
    })
    return v == "usable"


def check_subcommands(zai, checks):
    if not zai:
        checks.append({"id": "V2", "name": "導線の存在", "ok": False,
                       "detail": "zai が見つかりません"})
        return {}
    r = run([zai, "help"])
    have = {k: ("zai %s" % k) in r.out for k in ("lease", "hook", "guard", "split", "train")}
    ok = have.get("lease") and have.get("hook")
    checks.append({
        "id": "V2", "name": "導線の存在 (zai lease / zai hook)", "ok": bool(ok),
        "detail": "help に載っていた: " + ", ".join(k for k, v in have.items() if v)
                  + (" / 載っていない: " + ", ".join(k for k, v in have.items() if not v)
                     if not all(have.values()) else ""),
    })
    return have


def check_region_aware(zai, work, checks):
    """重なる 2 本が**断られる**か。ここを省くと保護ゼロで「衝突 0」が出る。

    0.13.0 の `zai lease claim` は `a.rs#L1-10` をただの文字列として飲むので、
    重なった 2 本が両方通り、しかも rc=0 で返っていた。
    """
    if not zai:
        checks.append({"id": "V3", "name": "行域を理解しているか", "ok": False,
                       "detail": "zai が見つかりません"})
        return False
    probe = os.path.join(work, "probe-region")
    os.makedirs(probe, exist_ok=True)
    git(["init", "-q", "-b", "main", "."], probe)
    with open(os.path.join(probe, "probe.txt"), "w", encoding="utf-8") as fh:
        fh.write("".join("line %d\n" % i for i in range(1, 201)))
    git(["add", "-A"], probe)
    git(["commit", "-qm", "probe"], probe)
    if run([zai, "lease", "enable", "--dir", probe]).rc != 0:
        checks.append({"id": "V3", "name": "行域を理解しているか", "ok": False,
                       "detail": "zai lease enable が失敗しました"})
        return False
    steps = [
        (["lease", "claim", "--dir", probe, "--agent", "a1", "probe.txt#L10-20"], 0,
         "1 本目の行域予約が通ること"),
        (["lease", "claim", "--dir", probe, "--agent", "a2", "probe.txt#L15-25"], 1,
         "**重なった 2 本目が断られること** (通ったら保護は 1 つも無い)"),
        (["lease", "claim", "--dir", probe, "--agent", "a3", "probe.txt#L120-130"], 0,
         "離れた行域は通ること (断ったら行域判定が壊れている)"),
    ]
    for args, want_fail, label in steps:
        r = run([zai] + args)
        failed = r.rc != 0
        if failed != bool(want_fail):
            checks.append({"id": "V3", "name": "行域を理解しているか", "ok": False,
                           "detail": "落ちた条件: " + label})
            return False
    checks.append({"id": "V3", "name": "行域を理解しているか", "ok": True,
                   "detail": "重なる予約を断り、離れた予約を通した"})
    return True


def check_shift(zai, work, checks):
    """`--shift` が本当に**ずらして**返すか。契約は「最後の行が `granted <仕様>`」。

    0.13.0 の `zai lease claim` は**知らないフラグを黙って確保パターンとして飲む**
    (`--shift` という名前のファイルが持ち主一覧に並ぶ)。rc=0 で返るのに
    ずらしてもいないし `granted` も出ない、という形で静かに嘘をつく。
    """
    if not zai:
        return False, "zai が見つかりません"
    probe = os.path.join(work, "probe-shift")
    os.makedirs(probe, exist_ok=True)
    git(["init", "-q", "-b", "main", "."], probe)
    with open(os.path.join(probe, "probe.txt"), "w", encoding="utf-8") as fh:
        fh.write("".join("line %d\n" % i for i in range(1, 401)))
    git(["add", "-A"], probe)
    git(["commit", "-qm", "probe"], probe)
    if run([zai, "lease", "enable", "--dir", probe]).rc != 0:
        return False, "zai lease enable が失敗しました"
    if run([zai, "lease", "claim", "--dir", probe, "--agent", "s1",
            "probe.txt#L10-20"]).rc != 0:
        return False, "下地の予約が取れませんでした"
    r = run([zai, "lease", "claim", "--dir", probe, "--agent", "s2", "--shift",
             "probe.txt#L12-18"])
    if r.rc != 0:
        return False, "重なった要求を断りました (--shift は断らずにずらす契約です)"
    g = parse_granted(r.out)
    if g is None:
        return False, "最後の行が `granted <仕様>` ではありません: %r" % r.out.strip()[-80:]
    if not (g[2] < 10 or g[1] > 20):
        return False, "ずらした先 (%d-%d) が既存の 10-20 と重なっています" % (g[1], g[2])
    return True, "重なった要求を %d-%d へずらした" % (g[1], g[2])


def check_gate_blocks(zai, work, checks):
    """ゲートが本当に止めているか。**止めていないのに緑に見える**を潰す。

    予約の**外**へ書けば止まり、**中**なら通ることを 1 往復ずつ確かめる。
    """
    if not zai:
        checks.append({"id": "V5", "name": "ゲートが止めているか", "ok": False,
                       "detail": "zai が見つかりません"})
        return False
    probe = os.path.join(work, "probe-gate")
    os.makedirs(probe, exist_ok=True)
    git(["init", "-q", "-b", "main", "."], probe)
    body = b"".join(b"line %d\n" % i for i in range(1, 201))
    with open(os.path.join(probe, "probe.txt"), "wb") as fh:
        fh.write(body)
    git(["add", "-A"], probe)
    git(["commit", "-qm", "probe"], probe)
    a = os.path.join(work, "probe-gate-a")
    b = os.path.join(work, "probe-gate-b")
    git(["worktree", "add", "-q", "-b", "ga", a, "HEAD"], probe)
    git(["worktree", "add", "-q", "-b", "gb", b, "HEAD"], probe)
    run([zai, "lease", "enable", "--dir", probe])
    if run([zai, "lease", "claim", "--dir", a, "--agent", "claude",
            "probe.txt#L30-30"]).rc != 0:
        checks.append({"id": "V5", "name": "ゲートが止めているか", "ok": False,
                       "detail": "下地の予約が取れませんでした"})
        return False

    def ask(wt, line, who):
        new, _ = edited_text(body, line, who)
        payload = json.dumps({
            "session_id": "probe-%s" % who, "cwd": wt,
            "hook_event_name": "PreToolUse", "tool_name": "Write",
            "tool_input": {"file_path": os.path.join(wt, "probe.txt"),
                           "content": new.decode("utf-8", "replace")},
        })
        r = run([zai, "hook", "--zaivern", "claude", "PreToolUse"], stdin=payload)
        return '"permissionDecision":"deny"' in r.out.replace(" ", "")

    own = ask(a, 30, 1)          # 自分の域 → 通らないといけない
    other = ask(b, 30, 2)        # 他人の域 → 止まらないといけない
    free = ask(b, 150, 2)        # 空いている域 → 通らないといけない
    ok = (not own) and other and (not free)
    checks.append({
        "id": "V5", "name": "ゲートが止めているか", "ok": ok,
        "detail": "自分の域=%s / 他人の域=%s / 空き域=%s (期待: 通す / 止める / 通す)"
                  % ("止めた" if own else "通した",
                     "止めた" if other else "**通した**",
                     "止めた" if free else "通した"),
    })
    return ok


# ═════════════════════════════════════════════════════════════════════
#  本体
# ═════════════════════════════════════════════════════════════════════


def prove(cfg, repo, label=None):
    """1 つのリポジトリに対する証明。**戻りは JSON にできる dict**。"""
    work = os.path.join(cfg["work"], "run-" + hashlib.sha256(
        repo.encode("utf-8", "replace")).hexdigest()[:10])
    os.makedirs(work, exist_ok=True)
    res = {"label": label or os.path.basename(os.path.abspath(repo)) or repo,
           "repo": repo, "runs": [], "reasons": []}

    res["target"] = survey(repo, cfg["scan_cap"])
    res["fingerprint_before"] = fingerprint(repo)
    if not res["target"]["supported"]:
        res["verdict"] = "skip"
        res["reasons"].append(res["target"]["skip_reason"])
        res["fingerprint_after"] = fingerprint(repo)
        res["untouched"] = untouched(res)
        return res

    clone = os.path.join(work, "clone")
    cl = clone_repo(repo, clone)
    res["clone"] = cl
    if not cl["method"]:
        res["verdict"] = "skip"
        res["reasons"].append("複製できませんでした: " + " / ".join(cl["errors"]))
        res["fingerprint_after"] = fingerprint(repo)
        res["untouched"] = untouched(res)
        return res

    base = git(["rev-parse", "HEAD"], clone).out.strip()
    git(["config", "user.name", "anyrepo-prove"], clone)
    git(["config", "user.email", "anyrepo-prove@example.invalid"], clone)

    rng = Rng(cfg["seed"])
    cands, stats = sample_corpus(clone, cfg, rng)
    res["corpus"] = stats
    res["corpus"]["hot"] = [{k: c[k] for k in ("path", "churn", "lines")}
                            for c in cands[: cfg["picks"]]]
    need = cfg["picks"]
    if len(cands) < need:
        res["verdict"] = "skip"
        res["reasons"].append(
            "編集できるテキストファイルが %d 件しかありません (--picks %d 件が要ります)。"
            "--max-bytes を上げるか --picks を下げてください" % (len(cands), need)
        )
        res["fingerprint_after"] = fingerprint(repo)
        res["untouched"] = untouched(res)
        return res

    zai = cfg["zai"] or None
    for n in cfg["writers"]:
        plan = make_plan(Rng(cfg["seed"]), cands, n, cfg)
        entry = {"writers": n}
        entry["baseline"] = run_stage("base-%d" % n, clone, work, base, plan, cands, cfg)
        if zai and cfg["cap_ok_for_measure"]:
            run([zai, "lease", "disable", "--dir", clone])
            run([zai, "lease", "enable", "--dir", clone])
            entry["zaivern"] = run_stage("zai-%d" % n, clone, work, base, plan, cands,
                                         cfg, zai=zai, shift=cfg["shift_ok"])
            # **消す前に台帳を見る。** disable すると証拠が消える
            entry["ledger"] = ledger_overlaps(zai, clone)
            run([zai, "lease", "disable", "--dir", clone])
        else:
            entry["zaivern"] = None
        res["runs"].append(entry)

    res["fingerprint_after"] = fingerprint(repo)
    res["untouched"] = untouched(res)
    return res


def untouched(res):
    before, after = res.get("fingerprint_before", {}), res.get("fingerprint_after", {})
    changed = [k for k in set(before) | set(after) if before.get(k) != after.get(k)]
    return {"ok": not changed, "changed": changed, "before": before, "after": after}


def verdict_of(res, cap_all_ok):
    """**「証明」と名乗れるのは全部揃ったときだけ。**

    ## `dup_lines` を見ていなかった欠陥 (直した)

    以前はここが `conflict_files` だけを見ていた。**2 体が同じ行を書いたのに
    git がたまたま衝突させなかった実行**でも `proved` と出てしまう。
    所有の保証 (`dup_lines == 0`) はこの製品の中核の主張なので、
    「git が衝突しなかった」ことで代用してはいけない。**同じ行に 2 人が
    載った時点で、衝突の有無に関わらず証明は失敗**である。
    """
    reasons = list(res.get("reasons", []))
    if res.get("verdict") == "skip":
        return "skip", reasons
    if not cap_all_ok:
        reasons.append("能力検査が通っていません (下の一覧を参照)")
    if not res.get("untouched", {}).get("ok", False):
        reasons.append("対象リポジトリの指紋が変わりました: "
                       + ", ".join(res["untouched"]["changed"])
                       + " (同時に編集している別プロセスが居ると必ずこうなります)")
    proved = True
    for r in res["runs"]:
        z = r.get("zaivern")
        if z is None:
            proved = False
            reasons.append("%d 体: zaivern あり の段を実行できませんでした" % r["writers"])
            continue
        if r["baseline"]["conflict_files"] == 0:
            proved = False
            reasons.append(
                "%d 体: ベースラインの衝突が 0 件でした。"
                "重なりが起きていないので A/B に意味がありません "
                "(--overlap を上げるか --hot-blocks を下げてください)" % r["writers"]
            )
        if z["dup_lines"] > 0:
            # **git の結果に関わらず落とす。** 衝突しなかったのは運であって
            # 保証ではない (同じ行への 2 つの編集が偶然同一文字列だった等)
            proved = False
            reasons.append(
                "%d 体: **2 体以上が同じ行を書きました** (重なった行 %d 件 / "
                "重ね書き %d 回)%s。git が衝突させたかどうかに関わらず、"
                "所有の保証が破れているので証明は成立しません"
                % (r["writers"], z["dup_lines"], z["dup_writes"],
                   ("。例: " + ", ".join(z["dup_detail"])) if z["dup_detail"] else "")
            )
        if z.get("mangled"):
            proved = False
            reasons.append(
                "%d 体: ハーネスの編集が目印の追加だけになっていない箇所が %d 件 "
                "(改行コードやエンコードを壊した疑い)。**測定器の側が壊れている**ので"
                "結果は使えません" % (r["writers"], z["mangled"])
            )
        if z["conflict_files"] > 0:
            proved = False
            lg = r.get("ledger") or {}
            where = ""
            if lg.get("checked"):
                lost = z["claim_granted"] - lg["spans"]
                if lg["overlaps"]:
                    where = ("。台帳が別々の持ち主へ重なる行域を %d 件配っていました "
                             "(= zaivern 側の問題)" % lg["overlaps"])
                elif lost > 0:
                    where = ("。最後の台帳に重なりは無いが、成立した予約 %d 件に対し "
                             "行域は %d 件しか残っていない — 畳まれただけかもしれないが、"
                             "**消えたから重ならないだけ**の可能性がある (--trace で追える)"
                             % (z["claim_granted"], lg["spans"]))
                else:
                    where = ("。台帳に重なりも欠落も無いので、**予約の外へ書かれた**"
                             " (= ゲートをすり抜けた) 疑いが濃い")
            reasons.append("%d 体: zaivern ありでも衝突が %d 件残りました%s"
                           % (r["writers"], z["conflict_files"], where))
        if z["timeouts"]:
            proved = False
            reasons.append("%d 体: zai の呼び出しが %d 回タイムアウトしました"
                           % (r["writers"], z["timeouts"]))
    if not cap_all_ok or not res.get("untouched", {}).get("ok", False):
        proved = False
    res["checks"] = verdict_checks(res, cap_all_ok)
    return ("proved" if proved else "unproved"), reasons


# **「証明できた」と言える条件**。表にも JSON にも同じものを出す。
# 文章で書くと必ず実装とずれるので、**判定に使う値そのもの**を並べる。
CRITERIA = [
    ("C1", "ベースラインで実際に衝突が出た (出なければ A/B に意味がない)"),
    ("C2", "zaivern あり: **2 体以上が同じ行を書いた数が 0**"),
    ("C3", "zaivern あり: git のマージで衝突したファイルが 0"),
    ("C4", "zaivern あり: zai 呼び出しのタイムアウトが 0"),
    ("C5", "ハーネスの編集が目印 1 つの追加だけ (改行・エンコードを壊していない)"),
    ("C6", "能力検査 (V1/V2/V3/V5/V7) が全部 OK"),
    ("C7", "対象リポジトリの指紋が前後で同一"),
]

# **測っていないもの**。書かないと読み手が勝手に広げて読む。
NOT_MEASURED = [
    "編集後にビルドが通るか (目印はコメント風だが構文は検査していない)",
    "編集の意味的な正しさ (同じ行を避けただけで、直したい場所とは限らない)",
    "マージ順序の全通り (枝は作った順に 1 本ずつ統合している)",
    "実在のエージェント (書き手はハーネスのスレッドで、CLI 越しの本物ではない)",
    "改行以外のファイル属性 (mode / symlink / submodule / LFS の中身)",
    "リポジトリ全体 (--scan-cap で間引いた標本の中だけ)",
]


def verdict_checks(res, cap_all_ok):
    """条件ごとの可否。**表と JSON で同じものを出す**ための一次データ。"""
    runs = res.get("runs", [])
    zs = [r.get("zaivern") for r in runs]
    have = [z for z in zs if z is not None]

    def every(fn, default=False):
        return all(fn(z) for z in have) if have else default

    return [
        {"id": "C1", "ok": all(r["baseline"]["conflict_files"] > 0 for r in runs)
         if runs else False,
         "value": "ベースラインの衝突ファイル "
                  + "/".join(str(r["baseline"]["conflict_files"]) for r in runs)},
        {"id": "C2", "ok": every(lambda z: z["dup_lines"] == 0),
         "value": "重なった行 " + "/".join(str(z["dup_lines"]) for z in have)},
        {"id": "C3", "ok": every(lambda z: z["conflict_files"] == 0),
         "value": "衝突ファイル " + "/".join(str(z["conflict_files"]) for z in have)},
        {"id": "C4", "ok": every(lambda z: z["timeouts"] == 0),
         "value": "タイムアウト " + "/".join(str(z["timeouts"]) for z in have)},
        {"id": "C5", "ok": all(s.get("mangled", 0) == 0
                               for r in runs for s in (r["baseline"], r.get("zaivern"))
                               if s),
         "value": "目印以外が動いた編集 "
                  + "/".join(str(s.get("mangled", 0))
                             for r in runs for s in (r["baseline"], r.get("zaivern"))
                             if s)},
        {"id": "C6", "ok": bool(cap_all_ok), "value": "能力検査"},
        {"id": "C7", "ok": bool(res.get("untouched", {}).get("ok")),
         "value": "指紋の一致"},
    ]


# ── 形ごとの可否をその場で作って実測する ─────────────────────────


def build_shapes(work):
    """`--shapes` 用。**説明ではなく実物**を作る。"""
    root = os.path.join(work, "shapes")
    os.makedirs(root, exist_ok=True)
    made = []

    def seed_repo(name, files=24, lines=80, commits=3):
        d = os.path.join(root, name)
        os.makedirs(d, exist_ok=True)
        git(["init", "-q", "-b", "main", "."], d)
        for c in range(commits):
            for i in range(files):
                with open(os.path.join(d, "f%02d.txt" % i), "w", encoding="utf-8") as fh:
                    fh.write("".join("f%02d rev%d line %d\n" % (i, c, j)
                                     for j in range(1, lines + 1)))
            git(["add", "-A"], d)
            git(["commit", "-qm", "c%d" % c], d)
        return d

    plain = seed_repo("plain")
    made.append(("普通のリポジトリ", plain))

    bare = os.path.join(root, "bare.git")
    git(["clone", "-q", "--bare", plain, bare], root)
    made.append(("bare", bare))

    src = seed_repo("wt-src")
    lw = os.path.join(root, "linked-wt")
    git(["worktree", "add", "-q", "-b", "lw", lw, "HEAD"], src)
    made.append(("連結 worktree の中", lw))

    sh = os.path.join(root, "shallow")
    git(["clone", "-q", "--depth", "1", "file://" + seed_repo("shallow-src"), sh], root)
    made.append(("shallow clone", sh))

    made.append(("履歴が 1 コミットだけ", seed_repo("single", commits=1)))

    det = seed_repo("detached")
    git(["checkout", "-q", "--detach", "HEAD~1"], det)
    made.append(("detached HEAD", det))

    dirty = seed_repo("dirty")
    with open(os.path.join(dirty, "f00.txt"), "a", encoding="utf-8") as fh:
        fh.write("未コミットの行\n")
    made.append(("未コミットの変更あり", dirty))

    sp = seed_repo("sparse")
    git(["sparse-checkout", "set", "--no-cone", "f00.txt", "f01.txt"], sp, check=False)
    made.append(("sparse-checkout", sp))

    child = seed_repo("sub-child")
    par = seed_repo("sub-parent")
    git(["-c", "protocol.file.allow=always", "submodule", "add", "-q", child, "child"],
        par, check=False)
    git(["commit", "-qm", "add submodule"], par, check=False)
    made.append(("submodule あり", par))

    lfs = seed_repo("lfs")
    with open(os.path.join(lfs, ".gitattributes"), "w", encoding="utf-8") as fh:
        fh.write("*.bin filter=lfs diff=lfs merge=lfs -text\n")
    with open(os.path.join(lfs, "big.bin"), "w", encoding="utf-8") as fh:
        fh.write("version https://git-lfs.github.com/spec/v1\noid sha256:%s\nsize 1\n"
                 % ("0" * 64))
    git(["add", "-A"], lfs)
    git(["commit", "-qm", "lfs pointer"], lfs)
    made.append(("LFS (ポインタ)", lfs))

    # **--scan-cap (既定 4000) を超える件数**にする。超えないと間引きの経路が
    # 1 度も走らず、「巨大でも動く」が未検証のまま緑になる
    big = seed_repo("huge", files=4200, lines=80, commits=1)
    made.append(("巨大 (追跡 4200 件 = 標本上限 4000 超え)", big))

    nc = os.path.join(root, "no-commit")
    os.makedirs(nc, exist_ok=True)
    git(["init", "-q", "-b", "main", "."], nc)
    made.append(("コミットが 0 件", nc))

    ng = os.path.join(root, "not-a-repo")
    os.makedirs(ng, exist_ok=True)
    with open(os.path.join(ng, "a.txt"), "w", encoding="utf-8") as fh:
        fh.write("a\n")
    made.append(("非 git ディレクトリ", ng))

    return made


# ═════════════════════════════════════════════════════════════════════
#  表示
# ═════════════════════════════════════════════════════════════════════


def w(line=""):
    sys.stderr.write(line + "\n")


def render_criteria():
    """**出力の先頭に「証明できた」と言える条件を置く。**

    言葉を弱めるのではなく正確にする。読み手が結論だけを持ち帰っても、
    何を測って何を測っていないかが必ず一緒に付いてくるようにする。
    """
    w("== 「証明できた」と言える条件 (**全部揃ったときだけ**)")
    for cid, text in CRITERIA:
        w("  %s  %s" % (cid, text))
    w("== このハーネスが**測っていない**もの")
    for text in NOT_MEASURED:
        w("  - %s" % text)
    w("")


def render_caps(checks):
    w("== 能力検査 (**測る前に**。1 つでも落ちたら「証明」と名乗らない)")
    for c in checks:
        w("  [%s] %s  %s" % ("OK" if c["ok"] else "NG", c["id"], c["name"]))
        w("        %s" % c["detail"])
    w("")


def render_res(res):
    t = res["target"]
    w("== 対象: %s" % res["repo"])
    w("   形: %s" % (", ".join(t["traits"]) or "普通の git リポジトリ"))
    for n in t["notes"]:
        w("   注記: %s" % n)
    if res.get("clone", {}).get("method"):
        w("   複製: %s (%.0f ms)" % (res["clone"]["method"], res["clone"]["ms"]))
    if res.get("corpus"):
        c = res["corpus"]
        w("   標本: 追跡 %d 件 → 調べた %d 件 → 編集できた %d 件 "
          "(%d 行以上 / %d バイト以下のテキスト)"
          % (c.get("tracked_total", 0), c.get("scanned", 0), c.get("eligible", 0),
             c.get("min_lines", 0), c.get("max_bytes", 0)))
        langs = c.get("languages", {})
        if langs:
            w("   言語構成: " + ", ".join("%s×%d" % (k, v) for k, v in
                                          list(langs.items())[:8]))
        e = c.get("eol")
        if e:
            # **改行コードを必ず出す。** 以前のハーネスは CRLF を LF へ
            # 書き換えており、Windows 由来のリポジトリは守られているのに
            # 「証明できず」と言われていた。何件混ざっていたのかを見せる
            w("   改行の内訳: LF×%d / CRLF×%d / CR×%d / 混在×%d "
              "(BOM×%d / 非 UTF-8×%d) — **すべてバイト列のまま編集します**"
              % (e.get("lf", 0), e.get("crlf", 0), e.get("cr", 0), e.get("mixed", 0),
                 e.get("bom", 0), e.get("non_utf8", 0)))
        hot = c.get("hot", [])
        if hot:
            w("   ホットスポット (git log の変更回数順): "
              + ", ".join("%s(%d回/%d行)" % (h["path"], h["churn"], h["lines"])
                          for h in hot[:4]))
    if res["verdict"] == "skip":
        w("   >>> skip: %s" % "; ".join(res["reasons"]))
        w("")
        return
    w("")
    w("   書き手  段            予定  適用  重なった行  衝突マージ  衝突ファイル  衝突ハンク")
    for r in res["runs"]:
        for key, label in (("baseline", "ベースライン"), ("zaivern", "zaivern あり")):
            s = r.get(key)
            if s is None:
                w("   %5d   %-12s  (実行できず)" % (r["writers"], label))
                continue
            w("   %5d   %-12s%6d%6d%12d%12d%14d%12d"
              % (r["writers"], label, s["planned"], s["applied"], s["dup_lines"],
                 s["conflict_merges"], s["conflict_files"], s["hunks"]))
        z = r.get("zaivern")
        if z:
            w("           予約 %d/%d 件成立 (ずらした %d 件 / 断られた %d 件) "
              "/ ゲート %d 回中 %d 回拒否 / EOF 外 %d 件"
              % (z["claim_granted"], z["claim_requested"], z["claim_moved"],
                 z["claim_refused"], z["gate_calls"], z["gate_denied"],
                 z["skipped_out_of_file"]))
            w("           予約 p50 %.0f ms / p95 %.0f ms、ゲート p50 %.0f ms / p95 %.0f ms"
              % (z["claim_p50"], z["claim_p95"], z["gate_p50"], z["gate_p95"]))
            if z["dup_detail"]:
                w("           **2 人以上が同じ行を書いた**: " + ", ".join(z["dup_detail"]))
            lg = r.get("ledger") or {}
            if lg.get("checked"):
                # **成立した予約の数と、最後に台帳へ残っていた行域の数を並べる。**
                # 減るのは正常 — 同じ持ち主の隣り合う域は 1 本に畳まれる
                # (32 体・重なり 1.0 で 192 → 132 になったが衝突は 0 だった)。
                # **だから単独では異常の印にしない。**衝突が残ったときだけ、
                # 「消えたから重ならないだけ」を疑う手掛かりとして使う
                w("           台帳の整合: 成立した予約 %d 件 → 最後に残った行域 %d 件"
                  " (同じ持ち主の隣り合う域は 1 本に畳まれるので減る)"
                  % (z["claim_granted"], lg["spans"]))
                w("                       別々の持ち主へ重なって配られたもの %d 件%s"
                  % (lg["overlaps"],
                     (" — " + ", ".join(lg["detail"])) if lg["detail"] else ""))
            elif lg:
                w("           台帳の整合: 確かめられませんでした (%s)" % lg.get("reason"))
            if z["conflict_files_unique"]:
                w("           衝突したファイル: " + ", ".join(z["conflict_files_unique"]))
    u = res.get("untouched", {})
    w("")
    w("   [%s] V6 元のリポジトリは無傷 %s"
      % ("OK" if u.get("ok") else "NG",
         "" if u.get("ok") else "— 変わった項目: " + ", ".join(u.get("changed", []))))
    if not u.get("ok"):
        # **何がどう変わったかを出す。** 「変わった」だけでは、ハーネスが書いたのか
        # 別のプロセスが書いたのか利用者に切り分けられない。実測で、同じリポジトリを
        # 別のエージェントが編集していると refs / objects / worktrees が動く
        for k in u.get("changed", []):
            w("        %-10s %s → %s"
              % (k, str(u["before"].get(k))[:24], str(u["after"].get(k))[:24]))
        w("        ハーネスは複製しか書きません。**対象リポジトリを同時に編集している"
          "プロセスが無いか**を先に確かめてください")
    texts = dict(CRITERIA)
    for c in res.get("checks", []):
        w("   [%s] %s %s — %s" % ("OK" if c["ok"] else "NG", c["id"],
                                  c["value"], texts.get(c["id"], "")))
    w("   結論: %s" % {"proved": "**証明できた**", "unproved": "証明できず",
                       "skip": "skip"}[res["verdict"]])
    for r in res["reasons"]:
        w("     - %s" % r)
    w("")


def main():
    cfg = CFG
    checks = []
    zai = cfg["zai"] or None
    check_version(zai, cfg["root"], checks)
    check_binary_provenance(cfg, checks)
    have = check_subcommands(zai, checks)
    region_ok = check_region_aware(zai, cfg["work"], checks) if have.get("lease") else False
    if not have.get("lease"):
        checks.append({"id": "V3", "name": "行域を理解しているか", "ok": False,
                       "detail": "zai lease が help にありません"})
    # **`--no-shift` は利用者の選択であって能力の不足ではない。**
    # ここで NG を出すと「使わないと決めたもの」が警告として並び、
    # 本当に見るべき赤が埋もれる (この製品が嫌う偽の警告そのもの)
    shift_ok = False
    if cfg["shift"]:
        shift_detail = "V3 が落ちたので実行しませんでした"
        if region_ok:
            shift_ok, shift_detail = check_shift(zai, cfg["work"], checks)
        checks.append({"id": "V4a", "name": "交渉 (--shift) が使えるか", "ok": shift_ok,
                       "detail": shift_detail
                       + ("" if shift_ok else " → 予約は「要求どおりか拒否か」で進めます")})
    gate_ok = check_gate_blocks(zai, cfg["work"], checks) if region_ok else False
    if not region_ok:
        checks.append({"id": "V5", "name": "ゲートが止めているか", "ok": False,
                       "detail": "V3 が落ちたので実行しませんでした"})

    # V4a (交渉) は「あれば強い」であって「無いと嘘になる」検査ではないので
    # 必須から外す。V1/V2/V3/V5/V7 が本番の測定を成立させる最小集合。
    required = {"V1", "V2", "V3", "V5", "V7"}
    cap_all_ok = all(c["ok"] for c in checks if c["id"] in required)
    cfg["shift_ok"] = shift_ok
    cfg["cap_ok_for_measure"] = bool(zai) and region_ok and gate_ok

    out = {
        "tool": "anyrepo-prove", "format": 1,
        "generated_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "config": {k: cfg[k] for k in ("writers", "overlap", "picks", "seed",
                                       "hot_blocks", "stride", "scan_cap",
                                       "max_bytes", "timeout", "shift")},
        "zai": {"path": zai, "shift_used": shift_ok},
        "capability": {"checks": checks, "required": sorted(required),
                       "all_ok": cap_all_ok},
        "results": [],
    }

    targets = []
    if cfg["shapes"]:
        targets = build_shapes(cfg["work"])
    if cfg["repo"]:
        targets.append((os.path.basename(os.path.abspath(cfg["repo"])) or cfg["repo"],
                        cfg["repo"]))
    if not targets:
        w("対象がありません (--repo か --shapes を指定してください)")
        return 2

    out["criteria"] = [{"id": c, "text": t} for c, t in CRITERIA]
    out["not_measured"] = list(NOT_MEASURED)
    render_criteria()
    render_caps(checks)
    worst = 0
    for label, path in targets:
        res = prove(cfg, path, label)
        res["verdict"], res["reasons"] = verdict_of(res, cap_all_ok)
        render_res(res)
        out["results"].append(res)
        if res["verdict"] == "unproved":
            worst = max(worst, 1)

    proved = [r for r in out["results"] if r["verdict"] == "proved"]
    skipped = [r for r in out["results"] if r["verdict"] == "skip"]
    unproved = [r for r in out["results"] if r["verdict"] == "unproved"]
    out["summary"] = {"proved": len(proved), "skipped": len(skipped),
                      "unproved": len(unproved)}
    w("== まとめ: 証明できた %d 件 / 証明できず %d 件 / skip %d 件"
      % (len(proved), len(unproved), len(skipped)))
    for r in skipped:
        w("   skip: %s — %s" % (r["label"], "; ".join(r["reasons"]) or "理由なし"))

    if cfg["json"]:
        json.dump(out, sys.stdout, ensure_ascii=False, indent=1)
        sys.stdout.write("\n")
    return worst


if __name__ == "__main__":
    sys.exit(main())
ANYREPO_PROVE_PY

python3 "$work/anyrepo.py" "$work/config.json"
rc=$?
exit "$rc"
