#!/usr/bin/env bash
# Context Engine の削減率を、元にした token-slim-mcp と**同じ入力で**比べる。
#
#   tools/context-bench.sh [<token-slim-mcp の実行ファイル>]
#
# 引数を省くと Zaivern 側だけを測る (比較相手が無い環境でも数字は出る)。
# 出力は 1 行 1 件のタブ区切り: case / side / original / optimized / reduction% / ms
#
# ## なぜシェルなのか
#
# 比較相手は**別プロセスの MCP サーバ** (JSON-RPC over stdio) なので、Rust の
# テストからは呼べない。ここは 2 つのプロセスを同じ入力で回して数字を並べる
# だけの外側の道具なので、シェルで足りる。
# **Zaivern 側の削減率そのものの保証は Rust の番人が持つ**
# (`context::bench::tests::*`) ので、相手が居ない環境でも保証は消えない。
#
# ## 数字の読み方 (正直に)
#
# ms には**プロセス起動**が入る。Zaivern 側は 1 回の起動で 1 件、
# token-slim 側は MCP のハンドシェイク無しで 1 要求 = 1 起動なので、
# どちらも同じ形の費用を払っている。それでも「実装の速さ」ではなく
# 「この道具をコマンドとして 1 回使う費用」であることは変わらない。
set -u
_LABEL='ベンチ'
_WHY=''

# 判定を出力そのものへ書く (`| tail` を挟んでも嘘にならない)。
_verdict() {
    _rc=$?
    # **後始末は rc を取ったあと。** 先に rm すると `$?` が rm のものになり、
    # 失敗したのに緑と書く (実際にそう書いて、読めない出力のまま緑が出た)。
    [ -n "${LAB:-}" ] && rm -rf "$LAB"
    if [ "$_rc" -eq 0 ]; then
        printf '\033[1;32m✓ %s 緑\033[0m\n' "$_LABEL"
    else
        printf '\033[1;31m✗ %s 赤 (rc=%s)%s\033[0m\n' \
            "$_LABEL" "$_rc" "${_WHY:+ — $_WHY}"
    fi
}
trap _verdict EXIT

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ZAI="${ZAI:-$ROOT/target/release/zai}"
[ -x "$ZAI" ] || ZAI="$ROOT/target/debug/zai"
PEER="${1:-}"
RC=0

if [ ! -x "$ZAI" ]; then
    _LABEL='ベンチ 未確認'
    _WHY='zai が見つからない (cargo build --release --bin zai)'
    exit 2
fi

LAB="$(mktemp -d "${TMPDIR:-/tmp}/zaivern-ctx-bench.XXXXXX")"

# ── 代表入力 (決定的。乱数は使わない) ────────────────────────────────
{
    for i in $(seq 1 400); do
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
    done
} > "$LAB/big.rs"

PAD="$(printf 'x%.0s' $(seq 1 300))"
{
    printf '{"items":['
    for i in $(seq 1 2000); do
        [ "$i" -gt 1 ] && printf ','
        printf '{"id":%s,"name":"item-%s","note":"%s"}' "$i" "$i" "$PAD"
    done
    printf ']}'
} > "$LAB/big.json"

LOG_LINE='   INFO   waiting for the lock   '
{
    for i in $(seq 1 4000); do
        printf '%s\n' "$LOG_LINE"
        [ $((i % 500)) -eq 0 ] && printf 'STEP %s\n' "$i"
    done
} > "$LAB/big.log"
# 同じログを JSON の 1 文字列にしたもの (token-slim の text_slim へ渡す)。
# 引用符もバックスラッシュも含まない行なので、`\n` で繋ぐだけで安全。
LOG_JSON=""
for i in $(seq 1 4000); do
    LOG_JSON="$LOG_JSON$LOG_LINE\\n"
    [ $((i % 500)) -eq 0 ] && LOG_JSON="${LOG_JSON}STEP $i\\n"
done

row() { printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" "$5" "$6"; }

# 出力の 1 行目 `~A→~B tok` から数字を取り、削減率つきで 1 行出す。
report() { # case side output ms
    local orig opt pct=0
    orig=$(printf '%s' "$3" | sed -n 's/.*~\([0-9]*\)→~\([0-9]*\) tok.*/\1/p' | head -1)
    opt=$(printf '%s' "$3" | sed -n 's/.*~\([0-9]*\)→~\([0-9]*\) tok.*/\2/p' | head -1)
    if [ -z "$orig" ] || [ -z "$opt" ]; then
        printf '%s\t%s\t読めません\n' "$1" "$2"
        RC=1
        _WHY="$1/$2 の出力からトークン数を読めない"
        return
    fi
    [ "$orig" -gt 0 ] && [ "$opt" -lt "$orig" ] && pct=$(( (orig - opt) * 100 / orig ))
    row "$1" "$2" "$orig" "$opt" "$pct" "$4"
}

TIMEFORMAT='%3R'

# 出力はファイルへ、時間は `time` の stderr から取る。
# **`$( … )` の中で変数へ代入しない** — 部分シェルなので外から見えず、
# `set -u` で「未定義」として落ちる (実際にそう書いて落ちた)。
OUT="$LAB/.bench-out"

run_zai() { # case args...
    local name=$1; shift
    local ms
    ms=$( { time "$ZAI" context "$@" --root "$LAB" >"$OUT" 2>&1 ; } 2>&1 )
    report "$name" zaivern "$(cat "$OUT")" "$ms"
}

run_peer() { # case json-params
    [ -n "$PEER" ] && [ -x "$PEER" ] || return 0
    local name=$1 params=$2 ms
    printf '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":%s}\n' "$params" >"$LAB/.req"
    ms=$( { time (cd "$LAB" && "$PEER" <"$LAB/.req" >"$OUT" 2>/dev/null) ; } 2>&1 )
    report "$name" token-slim "$(cat "$OUT")" "$ms"
}

row case side original optimized 'reduction%' 秒

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
