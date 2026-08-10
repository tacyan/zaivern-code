#!/usr/bin/env sh
# **アイドル時の CPU コストを数字で出す** — 設計原則 3 のリリースゲート。
#
# ## なぜ要るか
#
# 設計原則 3 は「アイドル時のコストはゼロでなければならない。アイドル時の
# CPU/GPU 使用率は**印象ではなく数値**でリリースゲートにする」と言っている。
# 競合 (orca) の最大の不満がまさに「アイドルで CPU 1 コアの約 40%」であり、
# ここは正面から勝てる領域なのに、**このリポジトリは一度も測っていなかった**。
#
# `ZAIVERN_PERF=1` の `perf::dump()` はフレーム時間と再描画要求の内訳を出すが、
# **プロセスが実際に何秒ぶん CPU を焼いたか**は外からしか分からない。
# このスクリプトがその外側を埋める。
#
# ## 測り方 (と、測らないこと)
#
#   * **絶対の閾値を置かない。** 「アイドル 30 秒で CPU 0.5 秒以下なら合格」の
#     ような線は必ず嘘をつく (このリポジトリでは実測で 3 件落ちた)。
#     ここがするのは (a) 生の数字を出す (b) `--baseline` で「何もしない
#     プロセス」の増分と**比べる** (c) `--json` で機械可読にする、の 3 つだけ。
#     合否はレビューする人間が、同じ機械の 2 つの数字を見て決める。
#   * **プロセスの CPU 時間の増分**を見る。`%CPU` の瞬間値ではなく累積時間の
#     差分なので、サンプリングの運不運に左右されない。
#   * **子プロセスは数えない。** エージェント (PTY) を起こすと交絡するので、
#     測るのは `zai` 自身の user+sys だけ。
#   * 起動できない環境 (ヘッドレスの Linux で winit が上がらない等) は
#     **skip と理由を出して続行**する。黙って飛ばさない。
#
# ## 使い方
#
#   tools/idle-cpu.sh                 # 既定 (20 秒放置して増分を出す)
#   tools/idle-cpu.sh --seconds 60    # 放置する秒数
#   tools/idle-cpu.sh --baseline      # 何もしないプロセスの増分と比べる
#   tools/idle-cpu.sh --json          # 機械可読 (CI / 版間比較)
#   tools/idle-cpu.sh --workspace DIR # 開くワークスペース (既定: 空の一時 dir)
#   tools/idle-cpu.sh --bin PATH      # 測るバイナリを直に指定
#   tools/idle-cpu.sh --help
#
# ## 再描画の内訳 (誰がアイドルで描かせているか)
#
# CPU 時間だけでは「なぜ焼いているか」が分からない。`ZAIVERN_PERF=1` を立てて
# **アイドル中の再描画要求を出所ごとに**取る。`perf::dump()` は普段 `on_exit`
# からしか呼ばれず、SIGTERM ではハンドラが無いので出ない — そこで
# `ZAIVERN_PERF_DUMP_AFTER=<秒>` で**放置しきった時点の 1 回**を予約する。
#
# ## 移植性
#
# パスは 1 つも直書きしない。バイナリは
#   $CARGO_TARGET_DIR → <リポジトリ>/target/{release,debug} → PATH
# の順で探し、見つけた版の `--version` を必ず出す (前の実行の残骸を
# 「新しい版」と読み違えないため)。CPU 時間は Linux では /proc、
# それが無ければ `ps` から取る (macOS はこちら)。

set -eu

# ── 引数 ───────────────────────────────────────────────────────────────

SECONDS_IDLE=20
WARMUP=5
WANT_JSON=0
WANT_BASELINE=0
BIN=""
WORKSPACE=""

usage() {
    sed -n '2,60p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
    -h | --help)
        usage
        exit 0
        ;;
    --seconds)
        [ $# -ge 2 ] || { echo "--seconds に秒数がありません" >&2; exit 2; }
        SECONDS_IDLE=$2
        shift 2
        ;;
    --warmup)
        [ $# -ge 2 ] || { echo "--warmup に秒数がありません" >&2; exit 2; }
        WARMUP=$2
        shift 2
        ;;
    --bin)
        [ $# -ge 2 ] || { echo "--bin にパスがありません" >&2; exit 2; }
        BIN=$2
        shift 2
        ;;
    --workspace)
        [ $# -ge 2 ] || { echo "--workspace にパスがありません" >&2; exit 2; }
        WORKSPACE=$2
        shift 2
        ;;
    --baseline)
        WANT_BASELINE=1
        shift
        ;;
    --json)
        WANT_JSON=1
        shift
        ;;
    *)
        echo "知らない引数: $1 (--help を見てください)" >&2
        exit 2
        ;;
    esac
done

case "$SECONDS_IDLE" in
'' | *[!0-9]*)
    echo "--seconds は 0 以上の整数で指定してください: $SECONDS_IDLE" >&2
    exit 2
    ;;
esac
case "$WARMUP" in
'' | *[!0-9]*)
    echo "--warmup は 0 以上の整数で指定してください: $WARMUP" >&2
    exit 2
    ;;
esac

# ── 後始末 (どの経路で抜けても取りこぼさない) ──────────────────────────

APP_PID=""
BASE_PID=""
TMPDIR_RUN=""

cleanup() {
    for pid in $APP_PID $BASE_PID; do
        kill "$pid" 2>/dev/null || true
    done
    if [ -n "$TMPDIR_RUN" ]; then
        rm -rf "$TMPDIR_RUN"
    fi
    return 0
}
trap cleanup EXIT INT TERM

# JSON の文字列値へ入れられる形にする (アプリのログを素で入れると壊れる)。
json_str() {
    printf '%s' "$1" | tr -d '\n\r\t' | sed 's/\\/\\\\/g; s/"/\\"/g'
}

# skip は「測れなかった」であって「合格」ではない。理由を必ず添える。
skip() {
    if [ "$WANT_JSON" = 1 ]; then
        printf '{"status":"skipped","reason":"%s"}\n' "$(json_str "$1")"
    else
        echo "skip: $1"
    fi
    exit 0
}

# ── バイナリを探す ─────────────────────────────────────────────────────

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH='' cd -- "$script_dir/.." && pwd)

find_bin() {
    # 1. 明示指定
    if [ -n "$BIN" ]; then
        [ -x "$BIN" ] || return 1
        printf '%s\n' "$BIN"
        return 0
    fi
    # 2. CARGO_TARGET_DIR (相対でも効くように pwd 経由で正規化)
    if [ -n "${CARGO_TARGET_DIR:-}" ]; then
        for p in release debug; do
            c=$CARGO_TARGET_DIR/$p/zai
            if [ -x "$c" ]; then
                printf '%s\n' "$c"
                return 0
            fi
        done
    fi
    # 3. リポジトリの target/ (スクリプトの位置から導く。直書きしない)
    for p in release debug; do
        c=$repo_root/target/$p/zai
        if [ -x "$c" ]; then
            printf '%s\n' "$c"
            return 0
        fi
    done
    # 4. PATH
    c=$(command -v zai 2>/dev/null) || return 1
    printf '%s\n' "$c"
}

ZAI=$(find_bin) || skip "zai が見つかりません (CARGO_TARGET_DIR / $repo_root/target/{release,debug} / PATH を見ました。cargo build を先に走らせてください)"

VERSION=$("$ZAI" --version 2>/dev/null | head -1 || true)
[ -n "$VERSION" ] || VERSION="(--version が返らない)"

# ── GUI が上がる見込みがあるか (ヘッドレスは skip) ──────────────────────

OS=$(uname -s)
case "$OS" in
Linux)
    if [ -z "${DISPLAY:-}" ] && [ -z "${WAYLAND_DISPLAY:-}" ]; then
        skip "ヘッドレスの Linux です (DISPLAY も WAYLAND_DISPLAY も無く winit が上がりません)。Xvfb 越しに走らせてください: xvfb-run -a $0"
    fi
    ;;
Darwin) ;;
*)
    # Windows (MSYS/Cygwin) などは CPU 時間の取り方が別なので、
    # 誤った数字を出すより測らないほうがよい。
    skip "$OS では CPU 時間の取り方を実装していません (Linux の /proc と ps だけ)"
    ;;
esac

# ── CPU 時間 (秒。user+sys の累積) ─────────────────────────────────────
#
# Linux: /proc/<pid>/stat の 14/15 番目 (utime/stime) を CLK_TCK で割る。
#        ps より分解能が高く、コンテナでも取れる。
# それ以外 (macOS): ps -o time= の [[DD-]HH:]MM:SS[.ff]。
#        macOS の ps は 1/100 秒まで出す。

CLK_TCK=$(getconf CLK_TCK 2>/dev/null || echo 100)

cpu_seconds() {
    _pid=$1
    if [ -r "/proc/$_pid/stat" ]; then
        # comm に空白や ')' が入りうるので、最後の ')' で切ってから数える。
        awk -v tck="$CLK_TCK" '{
            n = 0
            for (i = NF; i >= 1; i--) if ($i ~ /\)$/) { n = i; break }
            if (n == 0) exit 1
            # $n が comm の末尾。state が n+1、utime は n+12、stime は n+13。
            printf "%.3f\n", ($(n + 12) + $(n + 13)) / tck
        }' "/proc/$_pid/stat" 2>/dev/null && return 0
    fi
    ps -o time= -p "$_pid" 2>/dev/null | awk '
        NF == 0 { next }
        {
            t = $1
            d = 0
            if (index(t, "-") > 0) { split(t, a, "-"); d = a[1]; t = a[2] }
            n = split(t, p, ":")
            s = 0
            for (i = 1; i <= n; i++) s = s * 60 + p[i]
            printf "%.3f\n", s + d * 86400
            exit
        }'
}

alive() { kill -0 "$1" 2>/dev/null; }

# ── 走らせる ───────────────────────────────────────────────────────────

TMPDIR_RUN=$(mktemp -d 2>/dev/null) || skip "一時ディレクトリを作れません"
PERF_OUT=$TMPDIR_RUN/perf.txt
APP_LOG=$TMPDIR_RUN/zai.log

if [ -z "$WORKSPACE" ]; then
    WORKSPACE=$TMPDIR_RUN/ws
    mkdir -p "$WORKSPACE"
fi

[ "$WANT_JSON" = 1 ] || {
    echo "zai      : $ZAI"
    echo "version  : $VERSION"
    echo "os       : $OS"
    echo "workspace: $WORKSPACE"
    echo "測定     : 起動 → ${WARMUP}s ウォームアップ → ${SECONDS_IDLE}s 放置"
}

# 起動から測り終わるまでの秒数。`ZAIVERN_PERF_DUMP_AFTER` にこれを渡すと、
# ちょうど放置しきった時点でレポートが 1 回だけ $PERF_OUT へ出る
# (2 は起動確認ぶん。下の `sleep 2` と合わせてある)。
DUMP_AFTER=$((2 + WARMUP + SECONDS_IDLE))

ZAIVERN_PERF=1 ZAIVERN_PERF_OUT="$PERF_OUT" ZAIVERN_PERF_DUMP_AFTER="$DUMP_AFTER" \
    "$ZAI" "$WORKSPACE" >"$APP_LOG" 2>&1 &
APP_PID=$!

if [ "$WANT_BASELINE" = 1 ]; then
    # 「何もしないプロセス」。同じ機械・同じ瞬間の下限として並べる。
    sleep $((WARMUP + SECONDS_IDLE + 5)) &
    BASE_PID=$!
fi

# 起動に失敗していないか。GUI が上がらない環境はここで落ちる。
sleep 2
if ! alive "$APP_PID"; then
    reason=$(tr -d '\n' <"$APP_LOG" 2>/dev/null | cut -c1-300)
    skip "zai が起動直後に終了しました: ${reason:-(出力なし)}"
fi

# ウォームアップ (フォント読み込み・最初のスキャン・初回描画を測定から外す)。
if [ "$WARMUP" -gt 0 ]; then
    sleep "$WARMUP"
fi

if ! alive "$APP_PID"; then
    reason=$(tr -d '\n' <"$APP_LOG" 2>/dev/null | cut -c1-300)
    skip "ウォームアップ中に zai が終了しました: ${reason:-(出力なし)}"
fi

T0=$(cpu_seconds "$APP_PID")
B0=""
if [ -n "$BASE_PID" ]; then
    B0=$(cpu_seconds "$BASE_PID")
fi
[ -n "$T0" ] || skip "CPU 時間を取れません (/proc も ps も使えない環境)"

sleep "$SECONDS_IDLE"

if ! alive "$APP_PID"; then
    skip "放置中に zai が終了しました (測定区間が成立していません)"
fi

T1=$(cpu_seconds "$APP_PID")
B1=""
if [ -n "$BASE_PID" ]; then
    B1=$(cpu_seconds "$BASE_PID")
fi

RSS_KB=$(ps -o rss= -p "$APP_PID" 2>/dev/null | tr -d ' ' || true)
[ -n "$RSS_KB" ] || RSS_KB=0

# 予約したレポートが落ちてくるのを待つ (書き終わる前に殺すと内訳が消える)。
i=0
while [ "$i" -lt 50 ] && [ ! -s "$PERF_OUT" ] && alive "$APP_PID"; do
    sleep 0.2 2>/dev/null || sleep 1
    i=$((i + 1))
done

# 終了させる (on_exit が走ればもう 1 本レポートが追記される)。
kill "$APP_PID" 2>/dev/null || true
i=0
while [ "$i" -lt 30 ] && alive "$APP_PID"; do
    sleep 0.2 2>/dev/null || sleep 1
    i=$((i + 1))
done
kill -9 "$APP_PID" 2>/dev/null || true
wait "$APP_PID" 2>/dev/null || true
APP_PID=""

# ── 集計 ───────────────────────────────────────────────────────────────

DELTA=$(awk -v a="$T0" -v b="$T1" 'BEGIN { printf "%.3f", b - a }')
# 1 コアぶんを 100% とした割合。**閾値ではなく、読む人のための換算**。
PCT=$(awk -v d="$DELTA" -v s="$SECONDS_IDLE" 'BEGIN { if (s > 0) printf "%.2f", d * 100 / s; else printf "0.00" }')
BDELTA=""
BPCT=""
if [ -n "$B0" ] && [ -n "$B1" ]; then
    BDELTA=$(awk -v a="$B0" -v b="$B1" 'BEGIN { printf "%.3f", b - a }')
    BPCT=$(awk -v d="$BDELTA" -v s="$SECONDS_IDLE" 'BEGIN { if (s > 0) printf "%.2f", d * 100 / s; else printf "0.00" }')
fi
if [ -n "$BASE_PID" ]; then
    kill "$BASE_PID" 2>/dev/null || true
    BASE_PID=""
fi

# 再描画の内訳。$PERF_OUT が書かれていれば読む。
REPAINT_LINES=""
PERF_HEAD=""
PERF_NOTE=""
if [ -s "$PERF_OUT" ]; then
    # 予約ぶんと on_exit ぶんで最大 2 本出る。**最初の 1 本 = 放置しきった
    # 時点**を読む (on_exit ぶんには終了処理のフレームが混ざる)。
    PERF_HEAD=$(grep '^ZAIVERN_PERF ' "$PERF_OUT" | head -1 || true)
    REPAINT_LINES=$(awk '/^ZAIVERN_PERF /{ if (seen++) exit } /^ZAIVERN_PERF_REPAINT /{ print }' "$PERF_OUT" || true)
    [ -n "$REPAINT_LINES" ] || PERF_NOTE="アイドル中の再描画要求は 1 件も記録されなかった (damage 駆動)"
else
    PERF_NOTE="再描画の内訳は取れなかった: ZAIVERN_PERF_DUMP_AFTER の予約が届いていない (perf::frame_start が呼ばれていないか、この版が対応していない)。手順は docs/idle-cost.md"
fi

if [ "$WANT_JSON" = 1 ]; then
    printf '{"status":"measured"'
    printf ',"version":"%s","os":"%s"' "$(json_str "$VERSION")" "$OS"
    printf ',"idle_seconds":%s,"warmup_seconds":%s' "$SECONDS_IDLE" "$WARMUP"
    printf ',"cpu_seconds_delta":%s,"cpu_percent_of_one_core":%s' "$DELTA" "$PCT"
    printf ',"rss_kb":%s' "$RSS_KB"
    if [ -n "$BDELTA" ]; then
        printf ',"baseline_cpu_seconds_delta":%s,"baseline_cpu_percent_of_one_core":%s' "$BDELTA" "$BPCT"
    fi
    printf ',"repaint_sources":['
    if [ -n "$REPAINT_LINES" ]; then
        printf '%s' "$REPAINT_LINES" | awk '
            { src = ""; n = 0
              for (i = 1; i <= NF; i++) {
                  if ($i ~ /^source=/) { src = substr($i, 8) }
                  if ($i ~ /^count=/)  { n = substr($i, 7) }
              }
              if (src != "") { printf "%s{\"source\":\"%s\",\"count\":%s}", (c++ ? "," : ""), src, n }
            }'
    fi
    printf ']'
    if [ -n "$PERF_NOTE" ]; then
        printf ',"note":"%s"' "$(json_str "$PERF_NOTE")"
    fi
    printf '}\n'
else
    echo
    echo "── アイドル ${SECONDS_IDLE}s の実測 ──────────────────────────────"
    echo "CPU 時間の増分 : ${DELTA}s  (= 1 コアの ${PCT}%)"
    echo "RSS            : ${RSS_KB} KB"
    if [ -n "$BDELTA" ]; then
        echo "基準 (sleep)   : ${BDELTA}s  (= 1 コアの ${BPCT}%)  ← 同じ機械・同じ区間の下限"
    fi
    if [ -n "$PERF_HEAD" ]; then
        echo "perf           : $PERF_HEAD"
    fi
    if [ -n "$REPAINT_LINES" ]; then
        echo
        echo "アイドル中の再描画要求 (多い順):"
        printf '%s\n' "$REPAINT_LINES" | sed 's/^ZAIVERN_PERF_REPAINT /  /'
    fi
    if [ -n "$PERF_NOTE" ]; then
        echo
        echo "note: $PERF_NOTE"
    fi
    echo
    echo "※ 合否の線はここでは引かない (絶対時間の閾値は必ず嘘をつく)。"
    echo "   版を替えて同じ機械で 2 回走らせ、増分どうしを比べること。"
fi
