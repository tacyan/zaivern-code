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
#   2. コマンドごとの `timeout` — apt が時限を無視しても外から撃つ
#   3. 試行回数の上限 — 合わせて必ず ATTEMPTS×(U+I) 秒以内に決着する
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

ATTEMPTS=2
UPDATE_TIMEOUT=90    # 2 段目: 索引の取得
INSTALL_TIMEOUT=210  # 2 段目: 取得 + 展開 (GTK 一式は 100MB 超ある)

say() { printf '%s\n' "$*"; }
verdict() { say "$1"; }

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

# $1=表示名 $2=時限 以降=コマンド。ATTEMPTS 回まで試す。
retry() {
  what="$1"; secs="$2"; shift 2
  i=1
  while [ "$i" -le "$ATTEMPTS" ]; do
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
  verdict "✓ 自己検査 緑 — 固まったコマンドを ${el}s で打ち切った"
  exit 0
fi

# shellcheck disable=SC2086  # APT_OPTS / PKGS は意図的に語分割する
if ! retry "apt-get update" "$UPDATE_TIMEOUT" sudo apt-get $APT_OPTS update; then
  verdict "✗ Linux 依存 赤 — apt-get update が $ATTEMPTS 回とも通らなかった"
  exit 1
fi

# shellcheck disable=SC2086
if ! retry "apt-get install" "$INSTALL_TIMEOUT" \
  sudo apt-get $APT_OPTS install -y --no-install-recommends $PKGS; then
  verdict "✗ Linux 依存 赤 — apt-get install が $ATTEMPTS 回とも通らなかった"
  exit 1
fi

# 50MB 超の debug バイナリはリンクが支配的。lld があれば数十秒縮む。
# 見つからない場合は既定のリンカのまま (ビルドを壊さない)。
if [ "$WANT_LLD" = 1 ] && command -v ld.lld >/dev/null 2>&1 && [ -n "${GITHUB_ENV:-}" ]; then
  say "RUSTFLAGS=${RUSTFLAGS:-} -C link-arg=-fuse-ld=lld" >> "$GITHUB_ENV"
  say "· lld を使う (RUSTFLAGS へ追加)"
fi

verdict "✓ Linux 依存 緑"
