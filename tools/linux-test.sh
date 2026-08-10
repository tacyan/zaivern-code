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

# ── git を持たせる。**「git が無いので飛ばしました」を無音にしない。** ────
#
# `rust:*-slim` には git が入っていない。src/ の 11 ファイル・34 箇所が
# 「git が無い環境ではスキップ」というガードを持っていて、その中身は
# `println!` なので **既定のテストハーネスでは出力ごと飲まれる**。
# つまり画面には緑しか出ないのに、git / worktree / conflict / lease /
# guard / train / union — **「競合ゼロ」の中核がまるごと未検証**になる。
# 「無言でスキップ」は緑に見えて検証されていない典型なので、
#   1. まずイメージに git があるかを確かめ、
#   2. 無ければ git を足した派生イメージを**1 度だけ**作って使い、
#   3. それも作れなければ**何が検証されないかを名指しで警告する**。
git_skip_sites=34
image_has_git() {
    docker run --rm --entrypoint sh "$1" -c 'command -v git >/dev/null 2>&1' >/dev/null 2>&1
}

if [ "${ZAIVERN_LINUX_SKIP_GIT_IMAGE:-0}" = 1 ]; then
    :
elif image_has_git "$image"; then
    :
else
    # イメージ名から安定した名前を作る (パスもタグも直書きしない)
    derived="zaivern-lx-git-$(printf '%s' "$image" | cksum | cut -d' ' -f1)"
    if docker image inspect "$derived" >/dev/null 2>&1; then
        image=$derived
    else
        echo "== $image に git がありません。git を足した派生イメージを 1 度だけ作ります: $derived"
        # apt (debian 系) と apk (alpine 系) の**両方**を試す。片側だけ書かない。
        if printf 'FROM %s
RUN (command -v apt-get >/dev/null 2>&1 && apt-get update && apt-get install -y --no-install-recommends git && rm -rf /var/lib/apt/lists/*) || (command -v apk >/dev/null 2>&1 && apk add --no-cache git)
' "$image" |
            docker build -q -t "$derived" - >/dev/null 2>&1 && image_has_git "$derived"; then
            image=$derived
        else
            echo "!! git を入れられませんでした。以下は **検証されないまま緑になります**:" >&2
            echo "   git:: / worktree:: / conflict:: / lease:: / guard:: / train:: / union:: /" >&2
            echo "   spec:: / race:: / checkpoint:: / git_panel::" >&2
            echo "   (src/ の 11 ファイル・$git_skip_sites 箇所が「git が無い環境ではスキップ」を持ちます)" >&2
            echo "   git を含むイメージを ZAIVERN_LINUX_IMAGE で指定してください。" >&2
        fi
    fi
fi

if [ "${1:-}" = "--check" ]; then
    cmd="cargo check --all-targets"
elif [ "$#" -le 1 ]; then
    filter=${1:-}
    cmd="cargo test --bin zai ${filter}"
else
    # **フィルタを複数受け取る。** 以前は `filter=${1:-}` で 1 つ目しか見ず、
    # `tools/linux-test.sh guard:: train:: union::` と打つと **guard:: だけが
    # 走って緑**になっていた (実際に踏んだ)。「実行されていないのに緑」は
    # このリポジトリで繰り返し出ている壊れ方なので、ここで潰す。
    #
    # libtest の位置引数は 1 つだけなので、**1 つのコンテナの中で順に回す**
    # (コンテナ起動を N 回繰り返すと、その分だけ待たされる)。
    # `&&` で繋ぐので最初に落ちたところで止まり、終了コードが伝わる。
    cmd=""
    for f in "$@"; do
        [ -n "$cmd" ] && cmd="$cmd && "
        # `$f` はここで展開してコマンド文字列へ焼き込む。エスケープして
        # コンテナ側へ渡すと、向こうに変数が無いので**見出しが空になる**
        # (「どれが走ったか分からない」= 実行の証拠にならない)。
        cmd="${cmd}echo \"== $f\" && cargo test --bin zai $f"
    done
fi

# **cargo のレジストリも毎回消えていた。** target だけ残しても、crates.io の
# インデックスと展開済みソースが `--rm` で捨てられるので、2 回目以降も依存の
# 取得からやり直しになる。ここは**全ワークツリーで共有してよい** (読み取りが
# 主で、cargo がファイルロックを持つ) ので 1 つにまとめる。
registry_vol=${ZAIVERN_LINUX_REGISTRY:-zaivern-lx-cargo-registry}

echo "== Linux ($image) で実行: $cmd"
echo "   target:   $target_mount (ワークツリーごとに分離。2 回目以降は warm)"
echo "   registry: $registry_vol (全ワークツリーで共有)"
if image_has_git "$image"; then
    echo "   git:      あり (git を要するテストも実行されます)"
else
    echo "   git:      **なし — git を要するテストは無言でスキップされます**"
fi
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
