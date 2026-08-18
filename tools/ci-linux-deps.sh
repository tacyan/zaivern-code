#!/usr/bin/env sh
# CI の Linux ランナーへ eframe/egui 0.29 + rfd (GTK3) のビルド依存を入れる。
#
# 使い方:
#   sh tools/ci-linux-deps.sh          # 基本パッケージだけ
#   sh tools/ci-linux-deps.sh --lld    # lld も入れ、あれば RUSTFLAGS へ足す
#
# ## なぜ道具にしたか
#
# 同じ `apt-get update && apt-get install -y …` が **ワークフロー 3 本に
# 計 10 か所** 複製されていた。片方だけ直すと、直っていない側が次に固まる。
#
# ## なぜ時限が要るか — 実測
#
# 2026-08-17 の run 32078628866 で、`msrv` ジョブの apt が **13 分以上
# 無反応**のままジョブの timeout (15 分) に達して cancelled になった。
# **同じ瞬間に走っていた `fast (ubuntu)` の同じ apt は 58 秒**で終わっている。
# つまりミラーの 1 本が黙って詰まっただけで、他は健全だった。
#
# 素の apt は HTTP に既定の時限を持たない。**待てば終わる**という前提で
# 書かれているので、詰まったミラーを掴むと永遠に待つ。しかもログには
# 1 バイトも出ないので、利用者からは「テストが 13 分終わらない」としか
# 見えない (原因の行が最後まで残らない)。
#
# 3 段で守る:
#   1. apt 自身に時限と再試行 (`Acquire::*::Timeout` / `Acquire::Retries`)
#      — 詰まったミラーを掴んでも十数秒で諦め、別のを引く
#   2. 外側の `timeout` — apt が時限を無視しても外から撃つ
#   3. 全体の予算 (BUDGET) — 段ごとの固定待ちにはしない。**遅いだけで
#      進んでいる apt を殺さない**ため、残りを次の試行へ渡す
#
# ## 判定は最後の 1 行に必ず書く
#
# `| head` などを挟むと `$?` はそちらのものになるので、パイプ越しに読んでも
# 嘘にならないよう、どの経路で終わっても最後に 1 行だけ判定を出す
# (`tools/verify.sh` / `linux-test.sh` / `windows-check.sh` と同じ約束)。
set -eu

WANT_LLD=0
SELF_TEST=0
for a in "$@"; do
  case "$a" in
    --lld) WANT_LLD=1 ;;
    --self-test) SELF_TEST=1 ;;
    -h|--help) sed -n '2,8p' "$0"; exit 0 ;;
    *) echo "不明な引数: $a" >&2; exit 2 ;;
  esac
done

PKGS="libgtk-3-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev libssl-dev"
[ "$WANT_LLD" = 1 ] && PKGS="$PKGS lld"

# 1 段目: apt 自身の時限と再試行。これが本命 —
# 詰まったミラーを掴んでも十数秒で諦めて次を引く。
APT_OPTS="-o Acquire::Retries=3
          -o Acquire::http::Timeout=15
          -o Acquire::https::Timeout=15
          -o Acquire::ftp::Timeout=15"

# 2 段目: **段ごとの固定待ちにしない。全体に 1 つの予算を置く。**
#
# 最初は「update に 90 秒 / install に 210 秒」と段ごとに切ったが、
# **進んでいる apt を殺した** — リリースの linux-x86_64 が実際に落ちた。
# ログを読むと azure のミラーが死んで `archive.ubuntu.com` へ切り替わり、
# 索引を落とし始めた 68 秒後に 90 秒の線へ当たっていた。**遅いだけで
# 進んでいた**のに撃った形で、CLAUDE.md の
# 「固定の待ちにすると遅いランナーを誤って殺す」をそのまま踏んだ。
#
# そこで、1 回ごとではなく**全体の予算**を持ち、残りを次の試行へ渡す。
# 遅いランナーは予算を丸ごと使えるし、本当に固まったものは必ず
# BUDGET 秒で決着する (無音の 15 分にはならない)。
# 実測の目安: 健全なら 60 秒 / ミラーが 1 本死んだ日で 90 秒。
BUDGET=360
ATTEMPTS=2

say() { printf '%s\n' "$*"; }
verdict() { say "$1"; }

_started=$(date +%s)
# 残り予算 (秒)。5 秒を下回ったら 0 を返す = もう試さない。
remaining() {
  r=$((BUDGET - ($(date +%s) - _started)))
  [ "$r" -lt 5 ] && r=0
  echo "$r"
}

# 時限つきで 1 回走らせる。時限切れは 124 (timeout(1) と同じ約束)。
#
# `timeout` があればそれを使い、無ければ**素の sh だけ**で同じことをする。
# 外部道具に頼ると、道具が無い環境で時限が黙って消える —
# そして「時限が消えている環境」は、たいてい自己検査も回せない環境なので
# 誰も気付かない。ここは素の sh で書いて、**どこでも証明できる**ようにする。
run_with_timeout() {
  secs="$1"; shift
  if command -v timeout >/dev/null 2>&1; then
    timeout "$secs" "$@"
    return $?
  fi
  "$@" &
  pid=$!
  waited=0
  while [ "$waited" -lt "$secs" ]; do
    kill -0 "$pid" 2>/dev/null || break
    sleep 1
    waited=$((waited + 1))
  done
  if kill -0 "$pid" 2>/dev/null; then
    kill -TERM "$pid" 2>/dev/null || true
    sleep 1
    kill -KILL "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    return 124
  fi
  wait "$pid"
}

# $1=表示名 $2=時限 (0 なら「残り予算」を使う) 以降=コマンド。
# ATTEMPTS 回まで、**残り予算が尽きるまで**試す。
retry() {
  what="$1"; fixed="$2"; shift 2
  i=1
  while [ "$i" -le "$ATTEMPTS" ]; do
    if [ "$fixed" -gt 0 ]; then
      secs="$fixed"
    else
      secs=$(remaining)
      if [ "$secs" = 0 ]; then
        say "⏱ $what — 予算 ${BUDGET}s を使い切った (試行 $i/$ATTEMPTS)"
        return 1
      fi
    fi
    # **`if …; then return 0; fi` の後で `$?` を読まないこと。**
    # 分岐が選ばれなかった `if` は 0 を返すので、時限切れ (124) が
    # 握り潰されて「失敗 rc=0」という意味不明な行になる (実際になった)。
    rc=0
    run_with_timeout "$secs" "$@" || rc=$?
    [ "$rc" = 0 ] && return 0
    # timeout(1) は時限切れを 124 で返す。理由を必ず 1 行残す
    # (ここが無音だと「ただ遅い」と区別が付かない)。
    if [ "$rc" = 124 ]; then
      say "⏱ $what が ${secs}s で時間切れ (試行 $i/$ATTEMPTS) — ミラーが詰まっている"
    else
      say "✗ $what が失敗 rc=$rc (試行 $i/$ATTEMPTS)"
    fi
    i=$((i + 1))
  done
  return 1
}

# ── 自己検査: わざと固まらせて、時限が本当に撃つことを証明する ──────
#
# 空回りする時限を残さないための手。`tools/remote-check.sh --hang` と同じ趣旨で、
# **時限が効かなくなったらここが赤くなる**。apt も sudo も要らないので、
# 手元 (macOS) でも CI でも同じように回せる。
if [ "$SELF_TEST" = 1 ]; then
  ATTEMPTS=2
  t0=$(date +%s)
  if retry "わざと固める" 1 sleep 30; then
    verdict "✗ 自己検査 赤 — 固まったコマンドが成功として返った (時限が効いていない)"
    exit 1
  fi
  el=$(( $(date +%s) - t0 ))
  # 2 回 × 1 秒 = 2 秒が理想。ランナーの遅さを見込んで上限は 10 秒。
  if [ "$el" -gt 10 ]; then
    verdict "✗ 自己検査 赤 — 打ち切りに ${el}s かかった (時限が働いていない)"
    exit 1
  fi
  # ── 2 本目: **予算の経路**も撃つことを証明する ──────────────────
  #
  # 実際に apt を通るのはこちら (`retry … 0 …`)。固定の待ちだけを試して
  # 満足すると、**本番で使う経路が一度も確かめられていない**まま残る。
  # 下限 (5s) より大きくすること — 下回ると「使い切った」へ即落ちて
  # **時限そのものを一度も試さない**まま緑になる (最初にそう書いて気付いた)。
  BUDGET=8
  _started=$(date +%s)
  ATTEMPTS=3
  t0=$(date +%s)
  if retry "わざと固める (予算)" 0 sleep 30; then
    verdict "✗ 自己検査 赤 — 予算の経路で固まったコマンドが成功として返った"
    exit 1
  fi
  el=$(( $(date +%s) - t0 ))
  # 予算 8 秒 + 撃つまでの余裕。使い切ったら**必ず**降りること
  # (降りないと、試行回数ぶんだけ予算が伸びて意味が消える)。
  if [ "$el" -lt 5 ]; then
    verdict "✗ 自己検査 赤 — 予算の経路が ${el}s で終わった (時限を一度も試していない)"
    exit 1
  fi
  if [ "$el" -gt 20 ]; then
    verdict "✗ 自己検査 赤 — 予算 ${BUDGET}s なのに ${el}s かかった (残りを渡していない)"
    exit 1
  fi
  verdict "✓ 自己検査 緑 — 固定の待ちと予算の両方が撃った (${el}s)"
  exit 0
fi

# shellcheck disable=SC2086  # APT_OPTS / PKGS は意図的に語分割する
if ! retry "apt-get update" 0 sudo apt-get $APT_OPTS update; then
  verdict "✗ Linux 依存 赤 — apt-get update が通らなかった (予算 ${BUDGET}s)"
  exit 1
fi

# shellcheck disable=SC2086
if ! retry "apt-get install" 0 \
  sudo apt-get $APT_OPTS install -y --no-install-recommends $PKGS; then
  verdict "✗ Linux 依存 赤 — apt-get install が通らなかった (予算 ${BUDGET}s)"
  exit 1
fi

# 50MB 超の debug バイナリはリンクが支配的。lld があれば数十秒縮む。
# 見つからない場合は既定のリンカのまま (ビルドを壊さない)。
if [ "$WANT_LLD" = 1 ] && command -v ld.lld >/dev/null 2>&1 && [ -n "${GITHUB_ENV:-}" ]; then
  say "RUSTFLAGS=${RUSTFLAGS:-} -C link-arg=-fuse-ld=lld" >> "$GITHUB_ENV"
  say "· lld を使う (RUSTFLAGS へ追加)"
fi

verdict "✓ Linux 依存 緑"
