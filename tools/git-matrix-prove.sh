#!/usr/bin/env sh
# **git の版だけ**を振って「競合ゼロ」を測り直す。
#
# ## なぜ要ったのか
#
# `docs/conflict-zero.md` / `docs/anyrepo-proof.md` / `docs/czero-repo-shapes.md`
# の数字は **macOS・git 2.47.1 の 1 点**でしか測っていなかった。
# `tools/xplat-bench.sh` が OS の軸 (macOS / Linux) を埋めたが、
# **git の版の軸は 1 点のまま**だった。ところが中核の判定は git の版で変わる:
#
#   * `git merge-tree --write-tree` は **2.38** で入った。それ未満では
#     `conflict.rs` が行範囲だけの判定へ縮退し、`coedit` の一撃統合の検算も落ちる
#   * `git merge` の既定戦略 ort は **2.34** から。それ未満は recursive で
#     diff が myers になり、`docs/conflict-zero.md` の
#     「ort は diff-algorithm=histogram 固定」という考察の前提が消える
#
# ## OS ではなく git だけを振る理由
#
# ディストリを替えると glibc も FS も python も一緒に変わり、
# 「git の版のせい」と言えなくなる。**変える変数を 1 つにする**ため、
# 同じ土台イメージ (`rust:1.90-slim` = Debian trixie) の上で
# **git だけをソースから入れ替える**。
#
# ## 使い方
#
#   tools/git-matrix-prove.sh                     既定 (stock と 2.30.2)
#   tools/git-matrix-prove.sh --git stock,2.30.2,2.39.5
#   tools/git-matrix-prove.sh --mode probe        素の git の挙動だけ (zai 不要・速い)
#   tools/git-matrix-prove.sh --mode shapes       リポジトリの形ごとの可否 (zai が要る)
#   tools/git-matrix-prove.sh --out <ディレクトリ>  JSON の置き場
#
# ## 触らないもの
#
#   * **ホストの `target/`**。コンテナ側は名前付きボリューム
#     (`CARGO_TARGET_DIR=/target`)。混ぜると Linux の成果物で macOS の
#     ビルドが無効化され、戻るたびにフルビルドが走る
#   * 本物の `~/.zaivern`。コンテナ内の一時ディレクトリへ `ZAIVERN_HOME` を向ける
#   * **docker build に context を渡さない** (`docker build -`)。
#     実際に 920MB の作業ディレクトリを context に取ってしまい、
#     イメージ 1 枚に 15 分以上かかった。Dockerfile は stdin から渡す
set -eu
# Windows (Git Bash / PowerShell) の既定コードページは UTF-8 ではないので、
# Python が日本語を stdout へ書いた瞬間に
# `UnicodeEncodeError: 'charmap' codec can't encode characters` で落ちる
# (CI の probe (windows-latest) が実際にこれで赤くなった)。
# **どの OS でも同じ出力になるよう UTF-8 を明示する。** 既に設定されていれば尊重する。
export PYTHONUTF8="${PYTHONUTF8:-1}"
export PYTHONIOENCODING="${PYTHONIOENCODING:-utf-8}"

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

gits=stock,2.30.2
mode=both
out=

while [ $# -gt 0 ]; do
    case "$1" in
    --git)
        gits=${2:?--git に版の並びが要ります}
        shift 2
        ;;
    --mode)
        mode=${2:?--mode に probe|shapes|both が要ります}
        shift 2
        ;;
    --out)
        out=${2:?--out にディレクトリが要ります}
        shift 2
        ;;
    -h | --help)
        sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "不明な引数: $1" >&2
        exit 2
        ;;
    esac
done

if ! docker info >/dev/null 2>&1; then
    echo "docker が動いていません。Docker Desktop を起動してください。" >&2
    exit 2
fi

if [ -z "$out" ]; then
    out=$(mktemp -d "${TMPDIR:-/tmp}/zai-gitmx.XXXXXX")
fi
mkdir -p "$out"

base_image=${ZAIVERN_LINUX_IMAGE:-rust:1.90-slim}

# 名前付きボリュームはワークツリーごとに分ける (同時に走る別のワークツリーと
# cargo のビルドロックを取り合わないため。tools/linux-test.sh と同じ流儀)。
slug=$(printf '%s' "$root" | cksum | cut -d' ' -f1)
target_vol="zaivern-gitmx-target-$slug"
registry_vol=${ZAIVERN_LINUX_REGISTRY:-zaivern-lx-cargo-registry}

image_for() {
    case "$1" in
    stock) echo "zaivern-gitmx-stock" ;;
    *) echo "zaivern-gitmx-$(printf '%s' "$1" | tr -d '.')" ;;
    esac
}

# ── イメージを 1 度だけ作る ──────────────────────────────────────
ensure_image() {
    ver=$1
    img=$(image_for "$ver")
    if docker image inspect "$img" >/dev/null 2>&1; then
        return 0
    fi
    echo "== イメージを作ります: $img (git=$ver / 初回だけ)" >&2
    if [ "$ver" = stock ]; then
        printf 'FROM %s\nRUN apt-get update && apt-get install -y --no-install-recommends git python3 ca-certificates file && rm -rf /var/lib/apt/lists/*\n' \
            "$base_image" | docker build -q -t "$img" - >/dev/null
    else
        # NO_OPENSSL / NO_CURL / NO_EXPAT で依存を削る。ここで測るのは
        # **ローカルのマージ**だけなので、HTTPS の remote も expat も要らない
        # (古い git を新しい OpenSSL 3 で通そうとすると余計な失敗を拾う)。
        #
        # shellcheck disable=SC2016
        # `${GITVER}` と `$PATH` は**docker 側で展開させる**ので、
        # ここでシェルに展開されては困る (単引用符は意図的)。
        printf 'FROM %s\nARG GITVER\nRUN apt-get update && apt-get install -y --no-install-recommends python3 ca-certificates curl make gcc libc6-dev zlib1g-dev perl file && rm -rf /var/lib/apt/lists/*\nRUN curl -fsSL "https://mirrors.edge.kernel.org/pub/software/scm/git/git-${GITVER}.tar.gz" -o /tmp/git.tgz && mkdir -p /usr/src && tar xzf /tmp/git.tgz -C /usr/src && rm /tmp/git.tgz && make -C "/usr/src/git-${GITVER}" -j"$(nproc)" prefix=/usr/local NO_TCLTK=1 NO_GETTEXT=1 NO_OPENSSL=1 NO_CURL=1 NO_EXPAT=1 install && rm -rf "/usr/src/git-${GITVER}"\nENV PATH=/usr/local/bin:$PATH\n' \
            "$base_image" | docker build -q -t "$img" --build-arg "GITVER=$ver" - >/dev/null
    fi
}

# ── zai を 1 度だけ作る (形ごとの可否に要る) ──────────────────────
ensure_zai() {
    echo "== Linux 向けに zai を作ります (target は名前付きボリューム $target_vol)" >&2
    docker run --rm \
        -v "$root":/w -w /w \
        -v "$target_vol":/target \
        -v "$registry_vol":/usr/local/cargo/registry \
        -e CARGO_TARGET_DIR=/target \
        -e CARGO_PROFILE_DEV_DEBUG=0 \
        "$(image_for stock)" \
        cargo build --bin zai --locked >&2
}

run_in() {
    ver=$1
    shift
    docker run --rm \
        -v "$root":/w -w /w \
        -v "$target_vol":/target \
        -e CARGO_TARGET_DIR=/target \
        -e ZAIVERN_HOME=/tmp/zaivern-home \
        -e ZAIVERN_BIN=/target/debug/zai \
        "$(image_for "$ver")" \
        sh -c "$*"
}

echo "$gits" | tr ',' '\n' | while IFS= read -r ver; do
    [ -n "$ver" ] || continue
    ensure_image "$ver"
done

case "$mode" in
shapes | both) ensure_zai ;;
esac

echo "$gits" | tr ',' '\n' | while IFS= read -r ver; do
    [ -n "$ver" ] || continue
    echo "" >&2
    echo "══════ git=$ver ══════" >&2
    run_in "$ver" 'git --version' >&2

    case "$mode" in
    probe | both)
        run_in "$ver" "sh tools/git-portability-probe.sh --json" \
            >"$out/probe-linux-git$ver.json" || echo "  !! probe が赤 (git=$ver)" >&2
        run_in "$ver" "sh tools/merge-band-probe.sh --mode pairs" >&2 ||
            echo "  !! merge-band が赤 (git=$ver)" >&2
        ;;
    esac

    case "$mode" in
    shapes | both)
        # `--shapes` は**書ける**場所が要る (/w は読み取り専用にしない)。
        # ハーネス自身が HOME を一時ディレクトリへ差し替えるので、
        # 本物の ~/.zaivern と ~/.gitconfig には触らない。
        run_in "$ver" "mkdir -p /tmp/zaivern-home && sh tools/anyrepo-prove.sh --shapes --writers 4 --overlap 1.0 --json" \
            >"$out/shapes-linux-git$ver.json" ||
            echo "  !! shapes が赤 (git=$ver)" >&2
        ;;
    esac
done

echo "" >&2
echo "== JSON: $out" >&2
ls -1 "$out" >&2
