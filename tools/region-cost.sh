#!/usr/bin/env sh
# 「**行域判定そのものの費用**」を、ハーネスの費用と分けて測る。
#
# ## なぜ別に要るのか
#
# `tools/coedit-bench.sh` / `tools/conflict-zero-bench.sh` は一時リポジトリを
# 作り、git を起こし、`zai` を何十回も起動する。そこで出る数字の**大半は
# ハーネスの費用**で、「行域判定が速いのか遅いのか」は 1 ミリも見えない。
# 実際、行域判定の側は総当たりで二次に伸びていたのに、**その事実は既存の
# 数字のどこにも現れていなかった**。
#
# ここは外部プロセス・git・ファイル I/O を 1 つも含まない。`src/region.rs` の
# `cost` モジュール (プロセス内マイクロベンチ) を `cargo test -- --nocapture`
# で起こし、同じ入力の生成を「判定あり」と「空 (0 件) の判定」の両方で回して
# 差を取る:
#
#   total   = 入力を作る + 判定する
#   harness = 入力を作る + 空を判定する
#   judge   = total - harness      ← これが知りたかった数字
#
# ## 合否は時間で決めない
#
# 表に出る時間は**数字として出すだけ**。赤にするかどうかは
# 「件数を 2 倍にしたとき**判定の呼び出し回数**が 2 倍を超えないか」で決める。
# 絶対時間の線は Docker の仮想 FS でも他テストとの同時実行でも必ず嘘をつく。
#
# ## 使い方
#
#   tools/region-cost.sh                       表を stdout へ
#   tools/region-cost.sh --json                JSON は stdout、表は stderr
#   tools/region-cost.sh --sizes 100,200,400,800
#   tools/region-cost.sh --lines 2000,4000,8000,16000
#   tools/region-cost.sh --iters 15            1 点あたりの繰り返し (最小値を採る)
#   tools/region-cost.sh --release             最適化ありで測る
#
# 環境変数 CARGO でコンパイラを明示できます (既定は PATH の cargo)。
#
# ## 副作用を持たない作り
#
#   * パスを 1 つも直書きしない (リポジトリ位置は $0 から導く)
#   * 一時ファイルは `mktemp` (= $TMPDIR 由来)
#   * `| head` を挟まない (終了コードが head のものになってしまう)
# shellcheck disable=SC1007  # `CDPATH= cd` は「その cd にだけ空の CDPATH を渡す」正しい書き方
set -eu

sizes=""
lines=""
iters=""
json=0
profile=""

usage() {
    sed -n '2,42p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
}

while [ $# -gt 0 ]; do
    case "$1" in
        --sizes)   sizes=${2:-}; shift 2 ;;
        --lines)   lines=${2:-}; shift 2 ;;
        --iters)   iters=${2:-}; shift 2 ;;
        --json)    json=1; shift ;;
        --release) profile="--release"; shift ;;
        -h | --help) usage ;;
        *) echo "不明な引数: $1 (--help で使い方)" >&2; exit 2 ;;
    esac
done

for v in "$sizes" "$lines" "$iters"; do
    [ -z "$v" ] && continue
    case "$v" in
        *[!0-9,]* | ,* | *,) echo "数字とカンマだけで指定してください: $v" >&2; exit 2 ;;
    esac
done

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cargo=${CARGO:-cargo}

# cargo が無い環境 (配布物だけの箱・最小コンテナ) では **理由を書いて続行**する。
if ! command -v "$cargo" >/dev/null 2>&1; then
    echo "skip: cargo が見つからないので計測しません (CARGO で明示できます)" >&2
    if [ "$json" = 1 ]; then
        printf '{"skipped": true, "reason": "cargo not found"}\n'
    fi
    exit 0
fi

out=$(mktemp "${TMPDIR:-/tmp}/zv-region-cost.XXXXXX")
trap 'rm -f "$out"' EXIT INT TERM

ZAIVERN_REGION_COST=1; export ZAIVERN_REGION_COST
[ -n "$sizes" ] && { ZAIVERN_REGION_COST_SIZES=$sizes; export ZAIVERN_REGION_COST_SIZES; }
[ -n "$lines" ] && { ZAIVERN_REGION_COST_LINES=$lines; export ZAIVERN_REGION_COST_LINES; }
[ -n "$iters" ] && { ZAIVERN_REGION_COST_ITERS=$iters; export ZAIVERN_REGION_COST_ITERS; }

echo "▸ 計測中 (cargo test region::cost -- --nocapture)…" >&2
# **`| head` を挟まない。** 挟むと $? が head のものになり、
# 「コンパイルに失敗したのに成功した」と読み違える。
set +e
(
    CDPATH= cd -- "$root" || exit 1
    # shellcheck disable=SC2086  # $profile は「無し or --release」の意図的な語分割
    "$cargo" test --bin zai $profile region::cost:: -- --nocapture --test-threads=1
) >"$out" 2>&1
rc=$?
set -e

if [ "$rc" -ne 0 ]; then
    echo "計測に失敗しました (cargo の終了コード $rc)。出力:" >&2
    cat "$out" >&2
    exit "$rc"
fi

if grep -q 'REGION-COST-SKIP' "$out"; then
    echo "skip: 計測が飛ばされました (環境変数が届いていない)" >&2
    exit 1
fi

table=$(sed -n '/REGION-COST-TABLE-BEGIN/,/REGION-COST-TABLE-END/p' "$out" \
        | sed '1d;$d')
body=$(sed -n '/REGION-COST-JSON-BEGIN/,/REGION-COST-JSON-END/p' "$out" \
       | sed '1d;$d')

if [ -z "$body" ]; then
    echo "JSON が出力に見つかりません。生の出力:" >&2
    cat "$out" >&2
    exit 1
fi

if [ "$json" = 1 ]; then
    printf '%s\n' "$table" >&2
    printf '%s\n' "$body"
else
    printf '%s\n' "$table"
    echo ""
    echo "判定そのもの = total - harness。harness は同じ入力を作って**空を判定**した費用。"
    echo "JSON が要るなら --json。"
fi
