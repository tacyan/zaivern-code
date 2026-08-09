#!/usr/bin/env sh
# fish / PowerShell のシェル統合シムを**実インタプリタで**検証する。
#
# ## なぜ要るか
#
# `src/shellint.rs` の `FISH_SHIM` / `PWSH_SHIM` は、Rust から見れば
# **ただの文字列**である。`cargo test` は「二重発行の門番という語が入っている」
# 程度しか言えない。実際に fish / pwsh に読ませるまで、
#
#   * 構文が通るか
#   * OSC 633 が **A → B → E → C → D** の順で出るか
#   * 同じ境界を 2 回出していないか (二重発行)
#   * 終了コードが**本当に**その値で届くか
#
# のどれも分からない。実測でこの 4 つとも壊れていたことがある:
#
#   * pwsh: `$LASTEXITCODE` はネイティブコマンドが置いた値が残り続けるので、
#     `/bin/sh -c "exit 42"` の**次に成功した** `echo AFTER` まで `D;42` になり、
#     状態ラダーが「異常終了」で貼り付いた
#   * pwsh: 素の Enter でも `E;;<nonce>` + `C` + `D` が出て、空のコマンドが
#     1 件ずつ履歴に積まれた
#   * fish: command substitution が改行で分割するので、複数行のコマンドが
#     `for i in 1 2echo $iend` に潰れた (`\n` → `\x0a` の置換は一度も効かない)
#   * fish: プロンプトを包む前に printf を撃っていたので `$status` が壊れ、
#     終了コードを色で出す本人のプロンプト (fish 既定を含む) が常に 0 を見た
#   * fish: 門番が `fish_prompt` の本文しか見ておらず、iTerm2 の fish 統合
#     (環境変数で名乗る) と**二重発行**した
#
# ## 使い方
#
#   tools/shell-verify.sh              # fish と pwsh の両方
#   tools/shell-verify.sh fish         # fish だけ
#   tools/shell-verify.sh pwsh         # pwsh だけ
#   tools/shell-verify.sh --trace      # 判定に加えて OSC 633 の列を全部出す
#
# 実測 2 分半かかる (両方)。時間のほとんどは**わざと入れた待ち**で、
# 1 行ずつ「打鍵」してプロンプトが返るのを待っている。固まっているのではない。
#
# ## ホストを汚さないこと
#
#   * **cargo を 1 度も呼ばない。** シムは `src/shellint.rs` の raw 文字列から
#     awk で切り出す。よって `target/` に触れず、他のエージェントが回している
#     cargo のビルドロックとも取り合わない (1 回 1 分で終わる)。
#     切り出し規則が本体とずれないことは
#     `shellint::tests::シムはソースから機械的に取り出せる` が守る。
#   * インタプリタは Docker の中だけに置く。ホストへ fish / pwsh を入れない。
#   * 作業ディレクトリは `mktemp -d` (= TMPDIR 由来)。パスを直書きしない。
#
# ## 担保できないもの (正直な話)
#
#   * **Windows の powershell.exe (5.1)**。ここで動かすのは Linux の pwsh 7 で、
#     PSReadLine の版も違う。5.1 は CI の windows-latest でも動かしていない。
#   * **実端末**。`script(1)` の擬似端末は `ESC[6n` (カーソル位置問い合わせ) に
#     誰も答えないので、PSReadLine の描画は実機と同じにならない
#     (端末サイズを明示しないと ReallyRender で例外を吐くほど脆い)。
#     ここで見ているのは**アプリが読む OSC のバイト列**であって、見た目ではない。
#   * **fish 4.x / pwsh 5.1 以外の版**。既定のイメージ以外は環境変数で差せる。
set -eu

# プロジェクトのルート (このスクリプトの 1 つ上)。パスを直書きしない。
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
src="$root/src/shellint.rs"

# 版を固定できるよう環境変数で差せる。既定は「手に入りやすい」側に寄せる。
fish_base=${ZAIVERN_FISH_IMAGE:-alpine:3.21}
fish_image=${ZAIVERN_FISH_TAG:-zaivern-shellverify-fish}
pwsh_image=${ZAIVERN_PWSH_IMAGE:-mcr.microsoft.com/powershell:latest}

# 検証用の nonce。実行ごとに変える必要は無い (シムが素通しすることの確認)。
nonce=zvtest

want=${1:-both}
trace_all=0
case "$want" in
--trace)
    want=both
    trace_all=1
    ;;
fish | pwsh | both) ;;
--*)
    echo "不明なオプション: $want (fish / pwsh / --trace のいずれか)" >&2
    exit 2
    ;;
*)
    echo "不明な引数: $want (fish / pwsh / --trace のいずれか)" >&2
    exit 2
    ;;
esac

no_docker() {
    cat <<EOS
docker が見つからない (または動いていない) ため、実インタプリタでの検証は
飛ばしました。**失敗ではありません。**

このスクリプトは fish / pwsh を Docker の中だけで動かします
(ホストへ入れないため)。Docker Desktop を起動するか、次を入れてください:

    https://www.docker.com/products/docker-desktop/

どうしてもホストへ入れたい場合は、次で入ります (このスクリプトは使いません):

    brew install fish                  # macOS
    brew install --cask powershell     # macOS
    sudo apt install fish powershell   # Debian / Ubuntu

Rust 側だけの検証 (実インタプリタ不要) は次で回ります:

    cargo test --bin zai shellint::
EOS
}

if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
    no_docker
    exit 0
fi

# ---------------------------------------------------------------------------
# シムを src/shellint.rs から切り出す (cargo を呼ばない)
# ---------------------------------------------------------------------------

work=$(mktemp -d "${TMPDIR:-/tmp}/zaivern-shellverify.XXXXXX")
trap 'rm -rf "$work"' EXIT INT TERM

# `const NAME: &str = r#"` の行から `"#;` だけの行までが本体。raw 文字列なので
# エスケープは無く、切り出したバイト列がそのまま `write_shims` の出力と一致する。
extract_shim() {
    awk -v pfx="const $1: &str = r#\"" '
        !inside { if (index($0, pfx) == 1) { inside = 1; print substr($0, length(pfx) + 1) } ; next }
        $0 == "\"#;" { exit }
        { print }
    ' "$src"
}

extract_shim FISH_SHIM >"$work/zaivern.fish"
extract_shim PWSH_SHIM >"$work/zaivern.ps1"
for f in zaivern.fish zaivern.ps1; do
    if [ ! -s "$work/$f" ]; then
        echo "$f を $src から切り出せませんでした (raw 文字列の書き方が変わった?)" >&2
        exit 1
    fi
done

# ---------------------------------------------------------------------------
# 判定の道具
# ---------------------------------------------------------------------------

pass=0
fail=0

ok() {
    pass=$((pass + 1))
    printf '  \033[32mOK\033[0m   %s\n' "$1"
}

ng() {
    fail=$((fail + 1))
    printf '  \033[31mNG\033[0m   %s\n' "$1"
    if [ $# -ge 2 ]; then
        printf '       %s\n' "$2"
    fi
}

# 生の PTY 出力から OSC 633 のマーカーだけを 1 行 1 件で取り出す。
#
# BEL で切ってから ESC]633; の後ろを拾う。`grep -o` や sed の `\033` は
# BSD / GNU / busybox で挙動が割れるので、POSIX awk の index/substr だけで書く。
osc_trace() {
    LC_ALL=C tr '\007' '\n' <"$1" | LC_ALL=C awk -v m="$(printf '\033]633;')" '
        { i = index($0, m); if (i > 0) print substr($0, i + length(m)) }
    '
}

# A と B が厳密に交互か = 同じ境界を 2 回出していないか (二重発行の検出)。
alternates_ab() {
    LC_ALL=C awk '
        $0 == "A" { if (last == "A") { print "A が続けて 2 回出た (行 " NR ")"; bad = 1 } ; last = "A" }
        $0 == "B" { if (last == "B") { print "B が続けて 2 回出た (行 " NR ")"; bad = 1 } ; last = "B" }
        END { exit bad ? 1 : 0 }
    ' "$1"
}

# 入力ファイルを 1 行ずつ「打鍵」する駆動スクリプト。空行はそのまま Enter。
# 改行コードはシェルによって違う (pwsh の PSReadLine は CR しか Enter と見ない)。
write_driver() {
    cat >"$work/drive.sh" <<'EOS'
#!/bin/sh
# 引数: <入力ファイル> <改行コード: lf|cr> <1 打鍵あたりの待ち秒> <起動コマンド>
set -eu
input=$1
eol=$2
step=$3
shift 3
(
    # 最初のプロンプトが出るまで待つ (待たずに撃つと入力が食われる)
    sleep 4
    while IFS= read -r line; do
        if [ "$eol" = cr ]; then
            printf '%s\r' "$line"
        else
            printf '%s\n' "$line"
        fi
        sleep "$step"
    done <"$input"
    # 最後の 1 行が処理されるまで PTY を開けておく
    sleep 5
) | timeout -s KILL 120 script -qec "stty rows 40 cols 200; $*" /dev/null
EOS
    chmod +x "$work/drive.sh"
}

write_driver

# ---------------------------------------------------------------------------
# fish
# ---------------------------------------------------------------------------

verify_fish() {
    echo "== fish ($fish_base) でシェル統合シムを検証"
    printf 'FROM %s\nRUN apk add --no-cache fish util-linux\n' "$fish_base" >"$work/fish.Dockerfile"
    if ! docker build -q -t "$fish_image" -f "$work/fish.Dockerfile" "$work" >"$work/build.log" 2>&1; then
        echo "fish イメージのビルドに失敗しました:" >&2
        cat "$work/build.log" >&2
        exit 1
    fi
    ver=$(docker run --rm "$fish_image" fish --version)
    echo "   $ver"

    # ── 本番の一連 ───────────────────────────────────────────────
    # `;` を含む行と複数行の行を必ず入れる (どちらも実測で壊れていた)。
    cat >"$work/in-fish.txt" <<'EOS'
echo hello
false
for i in 1 2
echo $i
end
echo a; echo b
exit
EOS
    docker run --rm -i -v "$work":/zv:ro -e TERM=xterm-256color \
        -e "ZAIVERN_SHELL_NONCE=$nonce" "$fish_image" \
        /zv/drive.sh /zv/in-fish.txt lf 1 \
        "fish -l -C 'source /zv/zaivern.fish'" >"$work/fish.raw" 2>&1 || true
    osc_trace "$work/fish.raw" >"$work/fish.trace"
    [ "$trace_all" = 1 ] && sed 's/^/       /' "$work/fish.trace"

    if [ ! -s "$work/fish.trace" ]; then
        ng "OSC 633 が 1 件も出ていない" "生の出力: $(head -c 200 "$work/fish.raw" | tr -d '\000')"
        return
    fi

    # 期待するコマンド行と終了コードの列。`exit` 以降は fish が落ちる途中で
    # 出方が揺れるので、確定している 8 件だけを突き合わせる。
    cat >"$work/fish.want" <<EOS
E;echo hello;$nonce
D;0
E;false;$nonce
D;1
E;for i in 1 2\\x0aecho \$i\\x0aend;$nonce
D;0
E;echo a\\x3b echo b;$nonce
D;0
EOS
    LC_ALL=C grep -E '^(E|D);' "$work/fish.trace" | head -8 >"$work/fish.got" || true
    if diff -u "$work/fish.want" "$work/fish.got" >"$work/fish.diff" 2>&1; then
        ok "コマンド行と終了コードが順番どおり届く (複数行・セミコロンを含む)"
    else
        ng "コマンド行 / 終了コードの列が期待と違う" "$(sed -n '3,20p' "$work/fish.diff" | tr '\n' '|')"
    fi

    if alternates_ab "$work/fish.trace" >"$work/fish.ab" 2>&1; then
        ok "プロンプト境界 A / B が厳密に交互 (二重発行なし)"
    else
        ng "プロンプト境界が二重発行されている" "$(cat "$work/fish.ab")"
    fi

    if LC_ALL=C grep -q '^P;Cwd=/$' "$work/fish.trace"; then
        ok "作業ディレクトリ (P;Cwd) が届く"
    else
        ng "P;Cwd が届いていない"
    fi

    # ── 本人のプロンプトが $status を見られるか ──────────────────
    # 終了コードを色で出すプロンプト (fish 既定を含む) が壊れないことの番人。
    cat >"$work/status-probe.sh" <<EOS
#!/bin/sh
set -eu
mkdir -p "\$HOME/.config/fish"
printf 'function fish_prompt\n echo -n "[st=\$status]# "\nend\n' >"\$HOME/.config/fish/config.fish"
printf 'false\ntrue\nexit\n' >/tmp/in.txt
/zv/drive.sh /tmp/in.txt lf 1 "fish -l -C 'source /zv/zaivern.fish'"
EOS
    chmod +x "$work/status-probe.sh"
    docker run --rm -i -v "$work":/zv:ro -e TERM=xterm-256color \
        -e "ZAIVERN_SHELL_NONCE=$nonce" "$fish_image" \
        /zv/status-probe.sh >"$work/fish-status.raw" 2>&1 || true
    got=$(LC_ALL=C tr -c '[:print:]\n' '\n' <"$work/fish-status.raw" |
        LC_ALL=C sed -n 's/.*\[st=\([0-9]*\)\].*/\1/p' | tr '\n' ' ')
    case "$got" in
    "0 1 0 "*)
        ok "本人のプロンプトが直前の終了コードを見られる (0 → 1 → 0)"
        ;;
    *)
        ng "プロンプトから見える \$status が壊れている" "見えた列: [$got] (期待: 0 1 0)"
        ;;
    esac

    # ── 二重発行の門番 (環境変数) ────────────────────────────────
    printf 'echo X\nexit\n' >"$work/in-tiny.txt"
    docker run --rm -i -v "$work":/zv:ro -e TERM=xterm-256color \
        -e ITERM_SHELL_INTEGRATION_INSTALLED=Yes \
        -e "ZAIVERN_SHELL_NONCE=$nonce" "$fish_image" \
        /zv/drive.sh /zv/in-tiny.txt lf 1 \
        "fish -l -C 'source /zv/zaivern.fish'" >"$work/fish-iterm.raw" 2>&1 || true
    n=$(osc_trace "$work/fish-iterm.raw" | LC_ALL=C grep -c . || true)
    if [ "$n" = 0 ] && LC_ALL=C grep -q 'X' "$work/fish-iterm.raw"; then
        ok "iTerm2 が居るときは何も出さない (シェルは普通に動く)"
    else
        ng "iTerm2 と二重発行している" "OSC 633 が $n 件出た"
    fi

    # ── 二重発行の門番 (自動読み込みされる fish_prompt) ──────────
    # `~/.config/fish/functions/fish_prompt.fish` は**最初に必要になるまで
    # 読まれない**。source 時点で覗いても空振りすることの実証でもある。
    cat >"$work/autoload-probe.sh" <<'EOS'
#!/bin/sh
set -eu
mkdir -p "$HOME/.config/fish/functions"
printf 'function fish_prompt\n printf "\\033]133;A\\007"\n echo -n "# "\nend\n' \
    >"$HOME/.config/fish/functions/fish_prompt.fish"
printf 'echo X\nexit\n' >/tmp/in.txt
/zv/drive.sh /tmp/in.txt lf 1 "fish -l -C 'source /zv/zaivern.fish'"
EOS
    chmod +x "$work/autoload-probe.sh"
    docker run --rm -i -v "$work":/zv:ro -e TERM=xterm-256color \
        -e "ZAIVERN_SHELL_NONCE=$nonce" "$fish_image" \
        /zv/autoload-probe.sh >"$work/fish-autoload.raw" 2>&1 || true
    n=$(osc_trace "$work/fish-autoload.raw" | LC_ALL=C grep -c . || true)
    if [ "$n" = 0 ] && LC_ALL=C grep -q 'X' "$work/fish-autoload.raw"; then
        ok "既に 133 を出すプロンプトが居るときは降りる (自動読み込みでも)"
    else
        ng "133 を出すプロンプトと二重発行している" "OSC 633 が $n 件出た"
    fi
}

# ---------------------------------------------------------------------------
# PowerShell
# ---------------------------------------------------------------------------

verify_pwsh() {
    echo "== pwsh ($pwsh_image) でシェル統合シムを検証"
    ver=$(docker run --rm --entrypoint pwsh "$pwsh_image" --version)
    echo "   $ver"

    # 本人のプロンプトを先に定義してからシムを読む。$LASTEXITCODE が
    # こちらの副作用で汚れていないことを、プロンプトの出力そのもので見る
    # (oh-my-posh / starship はこれを読んで色を変える)。
    cat >"$work/probe.ps1" <<'EOS'
function Global:Prompt() { "[lec=$($global:LASTEXITCODE)]PS> " }
. '/zv/zaivern.ps1'
EOS

    # 3 行目は**空の Enter**。何も実行していないのに 1 件積まれる事故の番人。
    cat >"$work/in-pwsh.txt" <<'EOS'
/bin/sh -c "exit 42"
echo AFTER

echo two
exit
EOS
    docker run --rm -i -v "$work":/zv:ro -e TERM=xterm-256color \
        -e "ZAIVERN_SHELL_NONCE=$nonce" --entrypoint /zv/drive.sh "$pwsh_image" \
        /zv/in-pwsh.txt cr 2 \
        "pwsh -NoLogo -NoExit -Command \". '/zv/probe.ps1'\"" >"$work/pwsh.raw" 2>&1 || true
    osc_trace "$work/pwsh.raw" >"$work/pwsh.trace"
    [ "$trace_all" = 1 ] && sed 's/^/       /' "$work/pwsh.trace"

    if [ ! -s "$work/pwsh.trace" ]; then
        ng "OSC 633 が 1 件も出ていない" "生の出力: $(head -c 200 "$work/pwsh.raw" | tr -d '\000')"
        return
    fi

    cat >"$work/pwsh.want" <<EOS
E;/bin/sh -c "exit 42";$nonce
D;42
E;echo AFTER;$nonce
D;0
E;echo two;$nonce
D;0
EOS
    LC_ALL=C grep -E '^(E|D);' "$work/pwsh.trace" | head -6 >"$work/pwsh.got" || true
    if diff -u "$work/pwsh.want" "$work/pwsh.got" >"$work/pwsh.diff" 2>&1; then
        ok "終了コードが漏れない (42 の次の成功が 0 で届く) / 空 Enter を数えない"
    else
        ng "コマンド行 / 終了コードの列が期待と違う" "$(sed -n '3,20p' "$work/pwsh.diff" | tr '\n' '|')"
    fi

    if LC_ALL=C grep -q "^E;;$nonce\$" "$work/pwsh.trace"; then
        ng "空の Enter でコマンド行 (E;;) を出している"
    else
        ok "空の Enter では E も C も出さない"
    fi

    if alternates_ab "$work/pwsh.trace" >"$work/pwsh.ab" 2>&1; then
        ok "プロンプト境界 A / B が厳密に交互 (二重発行なし)"
    else
        ng "プロンプト境界が二重発行されている" "$(cat "$work/pwsh.ab")"
    fi

    if LC_ALL=C grep -q '^P;Cwd=/$' "$work/pwsh.trace"; then
        ok "作業ディレクトリ (P;Cwd) が届く"
    else
        ng "P;Cwd が届いていない"
    fi

    if LC_ALL=C grep -q '\[lec=42\]' "$work/pwsh.raw"; then
        ok "本人のプロンプトが \$LASTEXITCODE を見られる (42 が見えた)"
    else
        ng "プロンプトから見える \$LASTEXITCODE が壊れている" \
            "見えた列: $(LC_ALL=C sed -n 's/.*\(\[lec=[0-9]*\]\).*/\1/p' "$work/pwsh.raw" | tr '\n' ' ')"
    fi
}

case "$want" in
fish) verify_fish ;;
pwsh) verify_pwsh ;;
both)
    verify_fish
    verify_pwsh
    ;;
esac

echo
echo "== 合計: $pass 件 OK / $fail 件 NG"
[ "$fail" = 0 ] || exit 1
