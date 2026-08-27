#!/usr/bin/env sh
# Context Engine の削減率を、元にした token-slim-mcp と**同じ入力で**比べる。
#
#   tools/context-bench.sh [<token-slim-mcp の実行ファイル>]
#
# 引数を省くと Zaivern 側だけを測る (比較相手が無い環境でも数字は出る)。
# 出力は 1 行 1 件のタブ区切り:
#
#   case  side  original  optimized  reduction%  reps  total_s
#
# ## なぜシェルなのか
#
# 比較相手は**別プロセスの MCP サーバ** (JSON-RPC over stdio) なので、Rust の
# テストからは呼べない。ここは 2 つのプロセスを同じ入力で回して数字を並べる
# だけの外側の道具なので、シェルで足りる。
# **Zaivern 側の削減率そのものの保証は Rust の番人が持つ**
# (`context::tests::削減率は代表入力で床を下回らない`) ので、相手が居ない
# 環境でも保証は消えない。
#
# ## POSIX sh で書く (bash ではない)
#
# CI は `tools/*.sh` を **`sh -n` と `shellcheck --shell=sh`** に通す。
# 最初 bash で書いて `{ time cmd; }` を使ったら、dash に `time` キーワードが
# 無いので構文エラーで落ちた。`local` も使わない (この道具だけが使っていた)。
#
# ## 時間の読み方 (正直に)
#
# POSIX に**秒未満を測る移植性のある手段が無い** (`date +%s%N` は GNU 限定、
# `time` キーワードは bash 限定)。そこで 1 件あたりの ms ではなく
# **「何回回して合計何秒か」**を出す。分解能は ±1 秒なので、per-op の精密な
# 値ではない — 両側を**同じ回数・同じ測り方**で回した粗い比較として読むこと。
# 回数は環境変数 `REPS` で変えられる (既定 100)。
# 秒には**プロセス起動が入る**。どちらの側も 1 要求 = 1 起動なので同じ形の
# 費用を払っているが、「実装の速さ」ではなく「コマンドとして 1 回使う費用」
# であることは変わらない。
set -u
_LABEL='ベンチ'
_WHY=''
LAB=''

# 判定を出力そのものへ書く (`| tail` を挟んでも嘘にならない)。
_verdict() {
    _rc=$?
    # **後始末は rc を取ったあと。** 先に rm すると `$?` が rm のものになり、
    # 失敗したのに緑と書く (実際にそう書いて、読めない出力のまま緑が出た)。
    [ -n "$LAB" ] && rm -rf "$LAB"
    if [ "$_rc" -eq 0 ]; then
        printf '\033[1;32m✓ %s 緑\033[0m\n' "$_LABEL"
    else
        printf '\033[1;31m✗ %s 赤 (rc=%s)%s\033[0m\n' \
            "$_LABEL" "$_rc" "${_WHY:+ — $_WHY}"
    fi
}
trap _verdict EXIT

ROOT=$(cd "$(dirname "$0")/.." && pwd)
ZAI=${ZAI:-$ROOT/target/release/zai}
[ -x "$ZAI" ] || ZAI=$ROOT/target/debug/zai
PEER=${1:-}
REPS=${REPS:-100}
RC=0

if [ ! -x "$ZAI" ]; then
    _LABEL='ベンチ 未確認'
    _WHY='zai が見つからない (cargo build --release --bin zai)'
    exit 2
fi

LAB=$(mktemp -d "${TMPDIR:-/tmp}/zaivern-ctx-bench.XXXXXX")
OUT=$LAB/.bench-out

# ── 代表入力 (決定的。乱数は使わない) ────────────────────────────────
i=1
while [ "$i" -le 400 ]; do
    printf '/// 関数 %s の説明。outline では落ちる行。\n' "$i"
    printf 'pub fn f%s(a: u32, b: u32) -> u32 {\n' "$i"
    printf '    // 途中の説明\n'
    printf '    let scaled = a.wrapping_mul(%s).wrapping_add(b);\n' "$i"
    printf '    let clamped = scaled.clamp(0, u32::MAX / 2);\n'
    printf '    let mut acc = 0u32;\n'
    printf '    for step in 0..clamped.min(8) {\n'
    printf '        acc = acc.wrapping_add(step).wrapping_mul(3);\n'
    printf '    }\n'
    printf '    acc.wrapping_add(clamped)\n}\n\n'
    i=$((i + 1))
done > "$LAB/big.rs"

PAD=$(awk 'BEGIN { while (n++ < 300) printf "x" }')
{
    printf '{"items":['
    i=1
    while [ "$i" -le 2000 ]; do
        [ "$i" -gt 1 ] && printf ','
        printf '{"id":%s,"name":"item-%s","note":"%s"}' "$i" "$i" "$PAD"
        i=$((i + 1))
    done
    printf ']}'
} > "$LAB/big.json"

LOG_LINE='   INFO   waiting for the lock   '
{
    i=1
    while [ "$i" -le 4000 ]; do
        printf '%s\n' "$LOG_LINE"
        [ $((i % 500)) -eq 0 ] && printf 'STEP %s\n' "$i"
        i=$((i + 1))
    done
} > "$LAB/big.log"

# 同じログを JSON の 1 文字列にしたもの (token-slim の text_slim へ渡す)。
# 引用符もバックスラッシュも含まない行なので `\n` で繋ぐだけで安全。
# シェルの文字列連結は件数の 2 乗で効くので、awk に組ませる。
LOG_JSON=$(awk -v line="$LOG_LINE" 'BEGIN {
    for (i = 1; i <= 4000; i++) {
        printf "%s\\n", line
        if (i % 500 == 0) printf "STEP %d\\n", i
    }
}')

row() { printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" "$5" "$6" "$7"; }

# 出力の 1 行目 `~A→~B tok` から数字を取り、削減率つきで 1 行出す。
report() { # case side total_s
    _orig=$(sed -n 's/.*~\([0-9]*\)→~\([0-9]*\) tok.*/\1/p' "$OUT" | head -1)
    _opt=$(sed -n 's/.*~\([0-9]*\)→~\([0-9]*\) tok.*/\2/p' "$OUT" | head -1)
    if [ -z "$_orig" ] || [ -z "$_opt" ]; then
        printf '%s\t%s\t読めません\n' "$1" "$2"
        RC=1
        _WHY="$1/$2 の出力からトークン数を読めない"
        return
    fi
    _pct=0
    [ "$_orig" -gt 0 ] && [ "$_opt" -lt "$_orig" ] &&
        _pct=$(((_orig - _opt) * 100 / _orig))
    row "$1" "$2" "$_orig" "$_opt" "$_pct" "$REPS" "$3"
}

run_zai() { # case args...
    _name=$1
    shift
    _t0=$(date +%s)
    _i=0
    while [ "$_i" -lt "$REPS" ]; do
        "$ZAI" context "$@" --root "$LAB" > "$OUT" 2>&1
        _i=$((_i + 1))
    done
    report "$_name" zaivern "$(($(date +%s) - _t0))"
}

run_peer() { # case json-params
    [ -n "$PEER" ] && [ -x "$PEER" ] || return 0
    _name=$1
    printf '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":%s}\n' "$2" > "$LAB/.req"
    _t0=$(date +%s)
    _i=0
    while [ "$_i" -lt "$REPS" ]; do
        (cd "$LAB" && "$PEER" < "$LAB/.req" > "$OUT" 2>/dev/null)
        _i=$((_i + 1))
    done
    report "$_name" token-slim "$(($(date +%s) - _t0))"
}

row case side original optimized 'reduction%' reps total_s

run_zai  read-auto    read big.rs
run_peer read-auto    '{"name":"read_slim","arguments":{"path":"big.rs"}}'
run_zai  read-slim    read big.rs --mode slim
run_peer read-slim    '{"name":"read_slim","arguments":{"path":"big.rs","mode":"slim"}}'
run_zai  read-outline read big.rs --mode outline
run_peer read-outline '{"name":"read_slim","arguments":{"path":"big.rs","mode":"outline"}}'
run_zai  json         json big.json
run_peer json         '{"name":"json_slim","arguments":{"path":"big.json"}}'
run_zai  text         text big.log --level aggressive
run_peer text         "{\"name\":\"text_slim\",\"arguments\":{\"text\":\"$LOG_JSON\",\"level\":\"aggressive\"}}"

exit "$RC"
