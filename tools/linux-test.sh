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
#
# ただし以前は `CARGO_TARGET_DIR` をコンテナ内の `/tmp` に置いていたため、
# `--rm` で毎回捨てられて **全実行がコールドビルド**になっていた。隔離
# ワークツリーで 6 本を同時に走らせたところ、6 つのフルビルドがメモリを
# 食い尽くして **OOM kill (signal: 9)** でコンパイル途中に落ちた (実測)。
#
# そこで **docker の名前付きボリュームをワークツリーごとに 1 つ**持たせる:
#   * ボリュームは Docker VM 側の FS なので、macOS のバインドマウントと違い
#     cargo が遅くならない
#   * `--rm` を跨いで残るので 2 回目以降が warm になる
#   * 名前にワークツリー由来のスラッグを混ぜるので、**同時に走る別の
#     ワークツリーと絶対に取り合わない** (cargo のビルドロックは
#     target ディレクトリ単位なので、共有すると直列化する)
# ホスト側の実体を触らせないため、パスではなくボリューム名で分ける。
if [ -n "${ZAIVERN_LINUX_TARGET:-}" ]; then
    # 明示指定があればホストのパスをそのまま使う (従来どおりの逃げ道)
    target_mount="$ZAIVERN_LINUX_TARGET"
    mkdir -p "$target_mount"
else
    # ルートの絶対パスから安定したスラッグを作る (パスの直書きをしない)
    slug=$(printf '%s' "$root" | cksum | cut -d' ' -f1)
    target_mount="zaivern-lx-$(basename "$root")-$slug"
fi

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

# **cargo のレジストリも毎回消えていた。** target だけ残しても、crates.io の
# インデックスと展開済みソースが `--rm` で捨てられるので、2 回目以降も依存の
# 取得からやり直しになる。ここは**全ワークツリーで共有してよい** (読み取りが
# 主で、cargo がファイルロックを持つ) ので 1 つにまとめる。
registry_vol=${ZAIVERN_LINUX_REGISTRY:-zaivern-lx-cargo-registry}

echo "== Linux ($image) で実行: $cmd"
echo "   target:   $target_mount (ワークツリーごとに分離。2 回目以降は warm)"
echo "   registry: $registry_vol (全ワークツリーで共有)"
# `CARGO_PROFILE_TEST_DEBUG=0` — Docker VM の RAM は実測 7.65GiB しか無く、
# 並列エージェントのコンテナが同時に居ると `zai` のテストバイナリを
# `debuginfo=2` でリンクする瞬間に **OOM kill (signal: 9)** される。
# 素の "could not compile" としてしか出ないのでコードの失敗と誤読しやすい。
# Linux 側の目的は**挙動の確認**でデバッガを当てることではないので落とす。
exec docker run --rm \
    -v "$root":/w -w /w \
    -v "$target_mount":/target \
    -v "$registry_vol":/usr/local/cargo/registry \
    -e CARGO_TARGET_DIR=/target \
    -e CARGO_PROFILE_TEST_DEBUG=0 \
    "$image" \
    sh -c "$cmd"
