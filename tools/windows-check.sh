#!/usr/bin/env sh
# Windows 側のビルドを macOS / Linux からローカルで検証する。
#
# ## なぜ要るか
#
# `#[cfg(windows)]` のコードは macOS のビルドで**一度もコンパイルされない**。
# src/ の 23 ファイルに 87 箇所の Windows 分岐があるのに、手元では
# `cargo check` が全部緑になる — 型エラーが眠っていても気付けない。
#
# tools/linux-test.sh と同じ理由で、CI 往復 (1 周 5〜6 分) に頼ると
# 切り分けに要る情報が出てこない。これを使えば手元で完結する。
#
# ## 使い方
#
#   tools/windows-check.sh              # 型検査 (--all-targets)。既定・最速
#   tools/windows-check.sh --build      # 実際に zai.exe を作る (リンクまで通す)
#   tools/windows-check.sh --clippy     # CI と同じ債務リストで clippy
#   tools/windows-check.sh --gnu        # mingw 経由の windows-gnu で型検査
#   tools/windows-check.sh --wine       # lease:: と instances:: を **実際に実行**する
#   tools/windows-check.sh --wine git:: # 実行するテストを絞る
#   tools/windows-check.sh keybinds::   # そのテストを Windows バイナリで実行 (--wine と同じ)
#
# ## ビルド置き場 (並列で走らせるときに効く)
#
# 既定では **ワークツリーごとに別の target** を使う。隔離ワークツリーで
# 複数のエージェントが同時に走っても、互いの診断が混ざらない。
#
#   ZAIVERN_WINDOWS_TARGET=<path>   置き場を明示する (最優先)
#   ZAIVERN_WINDOWS_TARGET_SHARED=1 全ワークツリーで 1 つを共有する。
#                                   ツリーが 1 つだけならキャッシュが効いて速い。
#                                   **並列で走らせているときは立てない**
#
# ## 既知の穴 — cargo-xwin 側の競合 (target を分けても残る)
#
# cargo-xwin は**起動のたびに**共有キャッシュの `clang-cl` シンボリックリンクを
# 作り直す (macOS なら `~/Library/Caches/cargo-xwin/clang-cl`)。そのため
# **2 本を完全に同時に起こすと必ず片方が落ちる**:
#
#   Error: Failed to setup clang-cl symlink
#   Caused by: ... File exists (os error 17)
#
# これは CARGO_TARGET_DIR とは無関係で、ここで直せるものではない
# (この版の cargo-xwin にキャッシュ位置を変える引数が無く、SDK は 1.1GB
#  あるのでワークツリーごとに持たせるのも現実的でない)。
# **数秒ずらして起こせば当たらない。** 落ちたら間を空けて撃ち直すこと。
#
# ## どこまで担保できるか (正直な話)
#
#   * **担保できる**: コンパイルとリンク。`#[cfg(windows)]` の型エラー・
#     未使用 import・Windows 限定 API の誤用は全部ここで落ちる。
#     実際に PE32+ の `zai.exe` が macOS 上で出来上がる。
#   * **一部だけ担保できる**: 実行時の挙動。`--wine` が Windows のテスト
#     バイナリを wine で走らせる。ホストに wine が無ければ **docker の
#     コンテナ内の wine** へ自動で落ちる (tools/linux-test.sh と同じ流儀)。
#     ファイル API (`create_new` の排他 / `rename` の置換) や PID の生存確認
#     など、**OS のカーネルの振る舞いに依るものはここで実際に動く**。
#   * **担保できない**: GUI と、wine が実装していない Win32 の細部。
#     wine が緑でも実機 Windows の保証にはならない (CI の windows-latest が本番)。
#   * **担保できない**: build.rs の VERSIONINFO 埋め込み。Cargo は
#     `[target.'cfg(windows)'.build-dependencies]` を**ホスト**で評価するため、
#     macOS からのクロスビルドでは winresource が入らず、build.rs の
#     `#[cfg(windows)]` も無効になる。ここだけは CI の windows-latest が唯一の検証。
#
# ## なぜ `cargo check --target x86_64-pc-windows-msvc` 単体では駄目か
#
# syntect が C の oniguruma (`onig_sys`) を引くので、素の cargo では
# `fatal error: 'stdlib.h' file not found` で build script が落ちる。
# Windows の C ヘッダと import ライブラリが要る。cargo-xwin がそれを
# Microsoft の公式配布から取ってきて clang-cl / lld-link に渡す。
set -eu
_LABEL='Windows'

# ── 判定を「出力そのもの」へ書く ────────────────────────────────────────
#
# 呼び出し側が `| tail` / `| head` を挟むと `$?` はそちらのものになるので、
# **中止したのに rc=0** に見える (実際にこれで「docker が起動していないのに
# 緑」と誤読した)。終了コードだけを真実にしない — どの経路で終わっても
# 最後の 1 行に結果を書き、パイプ越しでも嘘にならないようにする。
_verdict() {
    _rc=$?
    if [ "$_rc" -eq 0 ]; then
        printf '\033[1;32m✓ %s 緑\033[0m\n' "$_LABEL"
    else
        printf '\033[1;31m✗ %s 赤 (rc=%s)%s\033[0m\n' \
            "$_LABEL" "$_rc" "${_WHY:+ — $_WHY}"
    fi
}
_WHY=''
trap _verdict EXIT

# **この先で `exec` を使わないこと。** プロセスを置き換えると上の EXIT トラップが
# 発火せず、判定行が出ないまま終わる (`| tail` を挟むと「中止したのに緑」に見える)。
# 以前ここは `exec env … cargo xwin check` で、実際に判定行が 1 行も出ていなかった。
# 番人は `cli::tests::検証スクリプトは終了時に判定行を出す`。

# プロジェクトのルート (このスクリプトの 1 つ上)。パスを直書きしない。
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

# **ホストの target/ を汚さないのが要点。** 同じディレクトリを使うと Windows の
# 成果物で macOS のビルドが無効化され、戻ったときにフルビルドが走る。
# TMPDIR は macOS では per-user、Linux では /tmp。どちらでも書ける場所になる。
tmp=${TMPDIR:-/tmp}

# **さらに、ワークツリーごとに分ける。** 以前はここが固定パス
# (`$TMPDIR/zaivern-windows-target`) だったため、隔離ワークツリーで複数の
# エージェントが同時に走ると **全員が 1 つの target を共有**していた。
# cargo のビルドロックは target ディレクトリ単位なので直列化するだけでなく、
# **別のツリーの失敗した診断が自分のエラーとして再生される**:
#
#   実測 — 自分のツリーには確かに在る定数が `E0425 cannot find value` で落ちた。
#   `touch build.rs` で通ったので「古いキャッシュ」と誤診したが、真因は
#   **隣のツリーの診断の再生**だった。target を分けたら 31.9 秒で緑になった。
#
# 「自分のコードが悪い」と読める嘘を出すので、既定は必ず分離する。
# 分ける鍵は**ワークツリーの絶対パス**から導く (linux-test.sh と同じ流儀。
# パスを直書きしない)。basename は人が読むため、cksum は衝突を避けるため。
if [ -n "${ZAIVERN_WINDOWS_TARGET:-}" ]; then
    # 明示指定が最優先 (従来どおりの逃げ道)。
    target=$ZAIVERN_WINDOWS_TARGET
elif [ "${ZAIVERN_WINDOWS_TARGET_SHARED:-0}" = 1 ]; then
    # **共有したい人のための明示的な手段。** ワークツリーを 1 つしか動かして
    # いないなら、キャッシュを使い回せて速い。並列で走らせているときに
    # これを立てると上の誤診が戻ってくるので、既定にはしない。
    target="${tmp%/}/zaivern-windows-target"
else
    slug=$(printf '%s' "$root" | cksum | cut -d' ' -f1)
    target="${tmp%/}/zaivern-windows-target-$(basename "$root")-$slug"
fi

# CI の windows-latest は MSVC。既定はそれに合わせる (target_env が食い違わない)。
triple=${ZAIVERN_WINDOWS_TRIPLE:-x86_64-pc-windows-msvc}

# rustup の std が無ければ足す。無言で失敗しないよう、どの三つ組か出す。
ensure_std() {
    if ! rustup target list --installed 2>/dev/null | grep -qx "$1"; then
        echo "== rustup target add $1"
        rustup target add "$1"
    fi
}

need_xwin() {
    if command -v cargo-xwin >/dev/null 2>&1; then
        return 0
    fi
    cat >&2 <<'EOS'
cargo-xwin が見つかりません。Windows の C ヘッダ / import ライブラリを
Microsoft の公式配布から取ってきて clang-cl・lld-link へ渡す道具です。

    cargo install cargo-xwin --locked

初回実行時に Windows SDK と MSVC CRT を ~/.cache/cargo-xwin へ落とします
(数百 MB・1 回だけ)。ライセンスは Microsoft のもので、成果物の再配布はしません。

xwin を入れたくない / ネットワークが塞がっている場合は mingw 経由でも検査できます:

    brew install mingw-w64          # macOS
    sudo apt install gcc-mingw-w64  # Debian / Ubuntu
    tools/windows-check.sh --gnu
EOS
    exit 1
}

# CI の lint ジョブ (.github/workflows/test.yml) と同じ「凍結された債務リスト」。
# ここを CI と揃えておかないと、Windows 限定の lint だけ基準が違うことになる。
# CI の clippy は ubuntu 1 台でしか回らないので、**Windows 限定の警告は
# CI では永久に検出されない**。それを拾えるのがこのモードの存在理由。
clippy_debt() {
    cat <<'EOS'
-Aclippy::cloned_ref_to_slice_refs
-Aclippy::assertions_on_constants
-Aclippy::type_complexity
-Aclippy::manual_contains
-Aclippy::unnecessary_sort_by
-Aclippy::map_identity
-Aclippy::derivable_impls
-Aclippy::manual_inspect
-Aclippy::neg_cmp_op_on_partial_ord
-Aclippy::collapsible_match
-Aclippy::doc_lazy_continuation
-Aclippy::empty_line_after_doc_comments
-Aclippy::int_plus_one
-Aclippy::explicit_counter_loop
-Aclippy::single_range_in_vec_init
-Aclippy::cmp_owned
-Aclippy::manual_div_ceil
-Aclippy::field_reassign_with_default
-Aclippy::needless_return
-Aclippy::manual_repeat_n
-Aclippy::manual_clamp
-Aclippy::if_same_then_else
-Aclippy::question_mark
-Aclippy::single_match
-Aclippy::redundant_pattern_matching
-Aclippy::io_other_error
EOS
}

# ── Windows バイナリを「実際に動かす」ための runner ─────────────────────
#
# ホストの wine が第一候補。macOS では wine-stable の cask が
# **gstreamer-runtime の pkg インストールに sudo を要求する**ため
# 非対話の環境では入らない (実際に入らなかった)。そこで docker の
# コンテナ内 wine へ落ちる。tools/linux-test.sh と同じく
# 「ホストに何も入れずに別 OS を動かす」流儀。
#
# **wine は 9 以上が要る。** Rust の std は 1.78 以降 `bcryptprimitives.dll` の
# ProcessPrng を引く。wine 8 (debian bookworm) にはこの DLL が無く、
# 実行は `err:module:import_dll Library bcryptprimitives.dll ... not found` で
# **出力を 1 行も出さずに終了コード 53** になる (実測)。緑にも赤にも見えない
# ので、最も気付きにくい壊れ方をする。既定を wine 10 の debian:trixie にしてある。
wine_image=${ZAIVERN_WINE_IMAGE:-zaivern-wine10}
wine_base=${ZAIVERN_WINE_BASE_IMAGE:-debian:trixie}
# WINEPREFIX は名前付きボリュームへ置く。--rm を跨いで残るので
# 2 回目以降は初回セットアップ (数秒) を払わずに済み、
# **ホストの ~/.wine も target/ も汚さない**。
wine_prefix_vol=${ZAIVERN_WINE_PREFIX_VOLUME:-zaivern-wineprefix}

# --wine に引数が無いときに走らせる既定のテスト。
# **Windows で実行が 1 行も検証されていなかった 2 つ**を既定にしてある。
default_wine_filters="lease:: instances::"

# ホストの wine が使えるか (バージョン 9 以上か) を見る。
host_wine_ok() {
    command -v "$1" >/dev/null 2>&1 || return 1
    v=$("$1" --version 2>/dev/null | sed -n 's/^wine-\([0-9][0-9]*\).*/\1/p')
    [ -n "$v" ] && [ "$v" -ge 9 ]
}

ensure_wine_image() {
    if docker image inspect "$wine_image" >/dev/null 2>&1; then
        return 0
    fi
    echo "== wine イメージを作ります: $wine_image ($wine_base / 初回だけ・数分)"
    # $wine_base を展開したいので heredoc は非クォート。
    docker build -t "$wine_image" - <<EOS
FROM $wine_base
RUN apt-get update \
 && apt-get install -y --no-install-recommends wine wine64 ca-certificates \
 && rm -rf /var/lib/apt/lists/*
EOS
}

# 引数: 実行ファイル (ホストのパス) と、テストバイナリへ渡す引数。
#
# 出力に出る次の 2 つは**無害**なので気にしないこと:
#   * "it looks like wine32 is missing"      — 64bit の exe には要らない
#   * "failed to open ...\\rundll32.exe"      — prefix 初期化時の探索
# ── wine の中に git が居ないことを**必ず名指しで言う** ────────────────
#
# `tools/linux-test.sh` は「git が無いと無言でスキップされる」を警告するが、
# wine 側はもっと質が悪い: git を要するテストは**スキップではなく赤になる**。
# 実測で `czero_init::` の **40 件**が「git が無い」だけで FAILED になり、
# 3 件の本物 (`lease::span_tests` の 64 体) がその中に埋もれた。
# **環境由来の赤は、regression の赤と見分けが付かないと意味が無い。**
#
# wine のイメージは Linux の debian なので、Windows バイナリから見える
# `git.exe` は最初から存在しない。Git for Windows を wine prefix へ入れるのは
# このスクリプトの担当ではないので、**入れずに、何が赤くなるかを先に出す**。
warn_no_git_in_wine() {
    if docker run --rm --entrypoint sh "$wine_image" \
        -c 'command -v git >/dev/null 2>&1' >/dev/null 2>&1; then
        return 0
    fi
    cat >&2 <<'EOS'
!! wine のイメージに git がありません (Windows バイナリからは git.exe も見えません)。
   git を起こすテストは **スキップではなく FAILED になります**。実測で赤くなるもの:
     features::czero_init::imp::tests::*   (40 件)
   これは環境由来なので **regression ではありません**。本物の赤と混ぜないこと。
   git を要さない経路 (lease:: / instances:: / pathx:: / keybinds:: など) の
   結果だけが、ここで意味を持ちます。
EOS
}

run_with_wine() {
    exe=$1
    shift
    for w in wine64 wine; do
        if host_wine_ok "$w"; then
            echo "== ホストの $w で実行"
            env WINEDEBUG=-all "$w" "$exe" "$@"
        fi
    done
    if ! docker info >/dev/null 2>&1; then
        cat >&2 <<'EOS'
Windows バイナリを実行するには **wine 9 以上**が要ります。ホストにも docker にも
見つかりませんでした (古い wine はバージョンが足りないため使いません)。
次のどちらかを用意してください。

    * Docker Desktop を起動する (このスクリプトが wine 入りイメージを自動で作ります)
    * ホストへ wine 9+ を入れる (Debian/Ubuntu: sudo apt install wine)

型検査だけなら wine も docker も不要です:

    tools/windows-check.sh
EOS
        exit 1
    fi
    ensure_wine_image
    warn_no_git_in_wine
    # 実行ファイルの置き場だけを読み取り専用でマウントする。
    dir=$(CDPATH= cd -- "$(dirname -- "$exe")" && pwd)
    base=$(basename -- "$exe")
    echo "== docker ($wine_image) の wine で実行: $base $*"
    # XDG_RUNTIME_DIR が無いと wine が毎回エラー行を吐くので、
    # コンテナ内に 0700 の一時ディレクトリを作って渡す (/tmp は 1777 なので不可)。
    docker run --rm \
        -v "$dir":/exe:ro \
        -v "$wine_prefix_vol":/wineprefix \
        -e WINEDEBUG=-all \
        -e WINEPREFIX=/wineprefix \
        -w /tmp \
        "$wine_image" \
        sh -c 'mkdir -p /tmp/xdg && chmod 700 /tmp/xdg && XDG_RUNTIME_DIR=/tmp/xdg exec wine "$@"' \
        wine "/exe/$base" "$@"
}

# テストバイナリ (.exe) を作り、そのパスを標準出力へ返す。
build_test_exe() {
    need_xwin
    ensure_std "$triple"
    echo "== Windows ($triple) で実行: cargo xwin test --bin zai --no-run" >&2
    CARGO_TARGET_DIR="$target" cargo xwin test --bin zai --target "$triple" --no-run >&2
    # 2 回目は warm なので即返る。**パスを ls -t で推測しない** —
    # 古い .exe が残っていると別物を動かしてしまう。cargo に聞く。
    # **パイプの終了コードは最後の段のもの**なので、cargo が落ちても sed が
    # 成功して空文字 + rc=0 を返してしまう (= 「.exe が出来た」という嘘)。
    # 一度変数へ受けて cargo 自身の終了コードを見てから解析する。
    _json=$(CARGO_TARGET_DIR="$target" cargo xwin test --bin zai --target "$triple" \
        --no-run --message-format=json 2>/dev/null) || {
        echo "cargo xwin test が失敗しました (実行ファイルの場所を聞けません)" >&2
        return 1
    }
    _exe=$(printf '%s\n' "$_json" \
        | grep -o '"executable":"[^"]*\.exe"' \
        | tail -1 \
        | sed 's/^"executable":"//; s/"$//')
    if [ -z "$_exe" ]; then
        echo "cargo は成功したのに実行ファイルの場所が出てきませんでした" >&2
        return 1
    fi
    printf '%s\n' "$_exe"
}

mode=${1:-}

case "$mode" in
--gnu)
    # xwin を使わない代替路。mingw-w64 の gcc が onig_sys の C を通す。
    # cfg 的には msvc と同じ (src/ に target_env の分岐は 1 つも無い) ので
    # Windows 限定コードの型検査としては等価。ただし CI は MSVC なので
    # リンカ由来の差 (シンボル解決など) はこちらでは出ない。
    triple=x86_64-pc-windows-gnu
    cc=x86_64-w64-mingw32-gcc
    if ! command -v "$cc" >/dev/null 2>&1; then
        cat >&2 <<EOS
$cc が見つかりません。

    brew install mingw-w64          # macOS
    sudo apt install gcc-mingw-w64  # Debian / Ubuntu
EOS
        exit 1
    fi
    ensure_std "$triple"
    # cc-rs / cargo へ mingw のツールチェーンを教える。環境変数名は三つ組の
    # `-` を `_` にしたもの。ここを直書きせず $triple から作る。
    env_key=$(echo "$triple" | tr '-' '_')
    echo "== Windows ($triple, mingw) で実行: cargo check --all-targets"
    env \
        CARGO_TARGET_DIR="${target}-gnu" \
        "CC_${env_key}=$cc" \
        "AR_${env_key}=x86_64-w64-mingw32-ar" \
        "CARGO_TARGET_$(echo "$env_key" | tr '[:lower:]' '[:upper:]')_LINKER=$cc" \
        cargo check --target "$triple" --all-targets
    ;;
--build)
    need_xwin
    ensure_std "$triple"
    echo "== Windows ($triple) で実行: cargo xwin build --bin zai"
    CARGO_TARGET_DIR="$target" cargo xwin build --bin zai --target "$triple"
    exe="$target/$triple/debug/zai.exe"
    echo "== 出来上がり: $exe"
    # file が無い環境 (最小 Linux コンテナ等) でも落とさない。
    command -v file >/dev/null 2>&1 && file "$exe" || true
    ;;
--clippy)
    need_xwin
    ensure_std "$triple"
    echo "== Windows ($triple) で実行: cargo xwin clippy --all-targets"
    # clippy_debt() は 1 行 1 引数。`set --` で位置パラメータへ展開する
    # (配列の無い POSIX sh でも安全に渡せる)。
    set -- $(clippy_debt)
    env CARGO_TARGET_DIR="$target" \
        cargo xwin clippy --target "$triple" --all-targets --locked -- -D warnings "$@"
    ;;
"")
    need_xwin
    ensure_std "$triple"
    echo "== Windows ($triple) で実行: cargo xwin check --all-targets"
    env CARGO_TARGET_DIR="$target" \
        cargo xwin check --target "$triple" --all-targets
    ;;
--wine)
    # 引数が無ければ「Windows で実行が検証されていなかった 2 モジュール」。
    shift 2>/dev/null || true
    if [ "$#" -gt 0 ]; then
        filters=$*
    else
        filters=$default_wine_filters
    fi
    exe=$(build_test_exe)
    if [ -z "$exe" ] || [ ! -f "$exe" ]; then
        echo "テストバイナリのパスを cargo から取得できませんでした" >&2
        exit 1
    fi
    # libtest は複数のフィルタを受け付ける (どれかに一致すれば実行)。
    # --test-threads=1 は付けない: 並列でこそ出る競合 (リースのロック取り合い)
    # を見たいのがこのモードの目的だから。
    # shellcheck disable=SC2086
    run_with_wine "$exe" $filters --nocapture
    ;;
-*)
    echo "不明なオプション: $mode (--build / --clippy / --gnu / --wine のいずれか)" >&2
    exit 2
    ;;
*)
    # 素のフィルタ指定も実行モード。--wine と同じ runner を通す
    # (ホストに wine が無ければ docker のコンテナ内 wine へ落ちる)。
    exe=$(build_test_exe)
    if [ -z "$exe" ] || [ ! -f "$exe" ]; then
        echo "テストバイナリのパスを cargo から取得できませんでした" >&2
        exit 1
    fi
    run_with_wine "$exe" "$mode" --nocapture
    ;;
esac
