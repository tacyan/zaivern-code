#!/usr/bin/env sh
# GUI E2E の **層 2** — 実バイナリを起こして、生きていることを確かめる。
#
# 使い方:
#   tools/gui-smoke.sh              # 既定 (5 秒生かして観察)
#   tools/gui-smoke.sh --seconds 12 # 観察時間を変える
#   tools/gui-smoke.sh --build      # 先に cargo build --bin zai する
#
# ## 何を見るか
#
#   1. `zai <一時ワークスペース>` が起動して**指定秒数のあいだ生きている**
#   2. その間に `panic.log` が 1 バイトも作られない
#   3. 終わったら**プロセスツリーごと**片付く (孫が残らない)
#
# 画面は見ない (ヘッドレスでは見えない)。CLAUDE.md の
# 「GUI の動作検証はプロセス生存確認で行う」に従う。
#
# ## 環境を汚さない
#
#   - `ZAIVERN_HOME` を一時ディレクトリへ向ける。**実 `~/.zaivern` は
#     1 バイトも読み書きしない** (他のインスタンスの生きたリースを壊さない)
#   - ワークスペースも一時ディレクトリ。`$TMPDIR` / `TMP` から取るので
#     どの OS でも動く (パスの直書き禁止)
#
# ## 開けない環境では「静かに緑」にしない
#
# CI の Linux コンテナには X も Wayland も無い。そこで GUI を起こすと
# winit が `neither WAYLAND_DISPLAY nor DISPLAY is set` で落ちる。
# **これを合格にすると、このスクリプトは永久に何も見なくなる**ので、
# 理由を出して `[skip]` で降りる (終了コード 0、ただし理由は必ず 1 行出す)。
set -eu

cd "$(dirname "$0")/.."

SECONDS_ALIVE=5
DO_BUILD=0
while [ $# -gt 0 ]; do
  case "$1" in
    --seconds) SECONDS_ALIVE="${2:?--seconds には秒数が要る}"; shift 2 ;;
    --build) DO_BUILD=1; shift ;;
    -h|--help) sed -n '2,32p' "$0"; exit 0 ;;
    *) echo "不明な引数: $1" >&2; exit 2 ;;
  esac
done

say() { printf '%s\n' "$*"; }
skip() { say "[skip] $*"; exit 0; }

# ── 1. GUI を開ける環境か ──────────────────────────────────────────────
case "$(uname -s 2>/dev/null || echo unknown)" in
  Linux)
    if [ -z "${DISPLAY:-}" ] && [ -z "${WAYLAND_DISPLAY:-}" ]; then
      skip "DISPLAY も WAYLAND_DISPLAY も無い (ヘッドレス Linux)。
       GUI を開けないので層 2 は回せない。層 1 (cargo test --bin zai e2e::) は
       この環境でも全部回る。X を用意するなら xvfb-run 経由で呼ぶこと:
         xvfb-run -a tools/gui-smoke.sh"
    fi
    ;;
  Darwin) : ;;              # macOS は常にウィンドウサーバがある
  MINGW*|MSYS*|CYGWIN*) : ;; # Git Bash 越しの Windows
  *) skip "未知の OS。GUI を開けるか判断できない" ;;
esac

# ── 2. バイナリを用意する ──────────────────────────────────────────────
BIN="target/debug/zai"
case "$(uname -s 2>/dev/null || echo unknown)" in
  MINGW*|MSYS*|CYGWIN*) BIN="target/debug/zai.exe" ;;
esac

if [ "$DO_BUILD" = 1 ]; then
  say "== cargo build --bin zai =="
  cargo build --bin zai
fi
if [ ! -x "$BIN" ]; then
  skip "$BIN が無い。先に建てること: cargo build --bin zai (または --build)"
fi
# **版が同じまま中身だけ古いバイナリ**がいちばん質が悪い (CLAUDE.md)。
# src/ の最新更新より古ければ、測っているのは今のコードではない。
NEWEST_SRC="$(find src -name '*.rs' -newer "$BIN" -print -quit 2>/dev/null || true)"
if [ -n "$NEWEST_SRC" ]; then
  skip "$BIN が src/ より古い (例: $NEWEST_SRC)。
       このまま測ると「直したのに直っていない」という嘘の結果が出る。
       建て直すこと: cargo build --bin zai"
fi

# ── 3. 使い捨ての置き場 ────────────────────────────────────────────────
TMPROOT="${TMPDIR:-${TMP:-/tmp}}"
STAMP="$$-$(date +%s 2>/dev/null || echo 0)"
WORK="$TMPROOT/zaivern-gui-smoke-$STAMP"
export ZAIVERN_HOME="$WORK/home"
WS="$WORK/ws"
mkdir -p "$ZAIVERN_HOME" "$WS"
printf 'fn main() {}\n' > "$WS/main.rs"

PID=""
cleanup() {
  if [ -n "$PID" ] && kill -0 "$PID" 2>/dev/null; then
    # **ツリーごと**殺す。直接の子だけだと孫がパイプを握って残る。
    kill -TERM -- "-$PID" 2>/dev/null || kill -TERM "$PID" 2>/dev/null || true
    sleep 1
    kill -KILL -- "-$PID" 2>/dev/null || kill -KILL "$PID" 2>/dev/null || true
  fi
  rm -rf "$WORK" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# ── 4. 起こして観察する ────────────────────────────────────────────────
say "== zai を $SECONDS_ALIVE 秒起こす =="
say "   ZAIVERN_HOME=$ZAIVERN_HOME"
say "   workspace=$WS"
LOG="$WORK/stdout.log"
# 自分のプロセスグループで起こす (上の kill -- -PID が効くように)
if command -v setsid >/dev/null 2>&1; then
  setsid "$BIN" "$WS" >"$LOG" 2>&1 &
else
  "$BIN" "$WS" >"$LOG" 2>&1 &
fi
PID=$!

i=0
while [ "$i" -lt "$SECONDS_ALIVE" ]; do
  sleep 1
  i=$((i + 1))
  if ! kill -0 "$PID" 2>/dev/null; then
    say "!! $i 秒で落ちた。出力:"
    sed -n '1,60p' "$LOG" 2>/dev/null || true
    # ヘッドレスで窓を開けなかっただけなら skip (静かに緑にはしない)
    if grep -qi 'WAYLAND_DISPLAY\|DISPLAY is not set\|NoDisplay\|failed to create window' "$LOG" 2>/dev/null; then
      skip "窓を開けられない環境だった (上の出力を見よ)"
    fi
    exit 1
  fi
done

# ── 5. 判定 ────────────────────────────────────────────────────────────
rc=0
if [ -s "$ZAIVERN_HOME/panic.log" ]; then
  say "!! panic.log ができた:"
  sed -n '1,40p' "$ZAIVERN_HOME/panic.log"
  rc=1
else
  say "ok: $SECONDS_ALIVE 秒生存 / panic.log 無し"
fi

cleanup
PID=""
trap - EXIT INT TERM

# 片付けたあとも残っている子がいないか (孫の取り残しは UI の固まりの元)
if pgrep -f "$WS" >/dev/null 2>&1; then
  say "!! 片付けたのに残っているプロセスがある"
  rc=1
fi

exit "$rc"
