#!/usr/bin/env sh
# Linux 側のテストを Docker でローカル再現する。
#
# ## なぜ要るか
#
# macOS で開発していると、**Linux / Windows でしか出ない不具合が素通りする**。
# 実際に 43,000 行の変更が macOS ローカルでは全部緑なのに、CI の Linux と
# Windows で落ちた。原因は `keybinds::canonical_mods` が **非 macOS では ⌃ を
# ⌘ へ畳む**こと — mac で区別されている `⌃⌘F` (全画面) と `⌘F` (検索) が、
# Linux / Windows では**同じ打鍵**になり片方が永久に効かなくなっていた。
#
# CI 往復は 1 周 5〜6 分かかるうえ、原因の切り分けに必要な情報が出てこない。
# これを使えば **1 周 30 秒**で回せる。
#
# ## 使い方
#
#   tools/linux-test.sh                 # 全テスト
#   tools/linux-test.sh keybinds::      # モジュール指定
#   tools/linux-test.sh --check         # cargo check だけ (最速)
#
# ## 既知の「Docker でだけ落ちる」テスト (CI の ubuntu では通る)
#
# 以下はコンテナ環境の差であって不具合ではない。**追いかけないこと。**
#
#   * `app::glyph_tests::*`                    slim イメージにフォントが無い
#   * `cli::tests::instance_current_uses_own_pid`  PID 名前空間が違う
#   * `terminal::reap_pty_tests::*`            コンテナ内の PTY / プロセスツリー
#
# 判断に迷ったら、その 1 件を CI の ubuntu ジョブで確認する。
set -eu

# プロジェクトのルート (このスクリプトの 1 つ上)。パスを直書きしない。
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

# rustc 1.88+ が要る (Cargo.toml の記述)。イメージは環境変数で上書きできる。
image=${ZAIVERN_LINUX_IMAGE:-rust:1.90-slim}

# **ホストの target/ を汚さないのが要点。** 同じディレクトリを使うと Linux の
# 成果物で macOS のビルドが無効化され、戻ったときにフルビルドが走る。
target=${ZAIVERN_LINUX_TARGET:-/tmp/zaivern-linux-target}

if ! docker info >/dev/null 2>&1; then
    echo "docker が動いていません。Docker Desktop を起動してください。" >&2
    exit 1
fi

if [ "${1:-}" = "--check" ]; then
    cmd="cargo check --all-targets"
else
    filter=${1:-}
    cmd="cargo test --bin zai ${filter}"
fi

echo "== Linux ($image) で実行: $cmd"
exec docker run --rm \
    -v "$root":/w -w /w \
    -e CARGO_TARGET_DIR="$target" \
    "$image" \
    sh -c "$cmd"
