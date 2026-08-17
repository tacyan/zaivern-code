#!/usr/bin/env sh
# 出荷前の関門 — **リリース前に必ず通す 1 本の入口**。
#
# 使い方:
#   tools/release-gate.sh                 # 全部
#   tools/release-gate.sh --quick         # 遅い段 (全テスト / docker / windows) を省く
#   tools/release-gate.sh --only remote    # 1 段だけ
#   tools/release-gate.sh --skip windows   # 1 段だけ外す (複数可)
#   tools/release-gate.sh --list           # 段の一覧
#
# ## なぜ 1 本にするのか
#
# 道具は揃っていた (`verify.sh` / `gui-smoke.sh` / `linux-test.sh` /
# `windows-check.sh` / `zai i18n missing`)。それでも 0.18 系では
# **スマホ 2 件・PC 1 件・多言語 1 件**が出荷まで生き残った。理由は単純で、
# **どれを走らせるかが人の記憶に委ねられていた**から。
# 覚えていないと走らない検査は、無いのと同じ意味しか持たない。
#
# だから「リリース前にこれを打つ」を 1 つだけにする。段を足したい人は
# 下の `STEPS` に 1 行足す — 入口は増やさない。
#
# ## 出力の約束
#
#   * 段ごとに ✓ / ✗ / [skip] を出し、**最後に表と判定行を必ず 1 行**書く
#   * `[skip]` は「確かめていない」であって緑ではない。理由を必ず添える
#   * `| head` を挟んでも嘘にならない (終了コードだけを真実にしない)
#
# 詳しくは docs/preflight.md。
set -eu
_LABEL='出荷前の関門'
_WHY=''

_verdict() {
    _rc=$?
    # 片付けは**判定を取ってから**。先に走らせると `$?` が rm のものになる。
    if [ -n "${GATE_TMP:-}" ]; then rm -rf "$GATE_TMP" 2>/dev/null || true; fi
    if [ "$_rc" -eq 0 ]; then
        printf '\033[1;32m✓ %s 緑\033[0m\n' "$_LABEL"
    else
        printf '\033[1;31m✗ %s 赤 (rc=%s)%s\033[0m\n' \
            "$_LABEL" "$_rc" "${_WHY:+ — $_WHY}"
    fi
}
trap _verdict EXIT

cd "$(dirname "$0")/.."

# ── 段の定義 ────────────────────────────────────────────────────────
# 名前:速いか:説明 と、実行する中身 (run_<名前>) の対で持つ。
# **新しい段はここへ 1 行足すだけ。** 入口 (このファイルの下半分) は触らない。
STEPS='verify i18n remote gui linux windows'

# shellcheck disable=SC2034  # eval "d=\$desc_$name" で引く
desc_verify='整形 + clippy + 警告ゼロ + 全テスト + 実バイナリ (tools/verify.sh)'
desc_i18n='訳の無い画面文字列がゼロ (zai i18n missing)'
desc_remote='スマホ画面を本物のブラウザで描いて不変条件 (tools/remote-check.sh)'
desc_gui='実バイナリを起こして panic.log が出ない (tools/gui-smoke.sh)'
desc_linux='Linux でのコンパイルとテスト (tools/linux-test.sh, docker)'
desc_windows='Windows 向けのコンパイル (tools/windows-check.sh, cargo-xwin)'

slow_verify=1; slow_i18n=0; slow_remote=0; slow_gui=0; slow_linux=1; slow_windows=1

run_verify() {
    if [ "$QUICK" = 1 ]; then
        tools/verify.sh --lint --bin --quick
    else
        tools/verify.sh --lint --bin --all
    fi
}

run_i18n() {
    ZAI="${CARGO_TARGET_DIR:-target}/debug/zai"
    if [ ! -x "$ZAI" ]; then
        echo "[skip] $ZAI がありません (tools/verify.sh --bin で建ててください)"
        return 0
    fi
    # 実 ~/.zaivern を触らせない (他インスタンスの生きた台帳を壊さない)。
    ZAIVERN_HOME="$GATE_TMP/zaivern-home" "$ZAI" i18n missing
}

run_remote() { tools/remote-check.sh --self-test; }
run_gui()    { tools/gui-smoke.sh; }
run_linux()  { tools/linux-test.sh --check; }
run_windows() { tools/windows-check.sh; }

# ── 入口 ────────────────────────────────────────────────────────────
QUICK=0
ONLY=''
SKIP=''
for a in "$@"; do
    case "$a" in
        --quick) QUICK=1 ;;
        --list)
            for s in $STEPS; do
                eval "d=\$desc_$s"
                printf '  %-8s %s\n' "$s" "$d"
            done
            _LABEL='段の一覧'
            exit 0 ;;
        --only) NEXT=only ;;
        --skip) NEXT=skip ;;
        -h|--help) sed -n '2,10p' "$0"; _LABEL='使い方'; exit 0 ;;
        *)
            case "${NEXT:-}" in
                only) ONLY="$ONLY $a" ;;
                skip) SKIP="$SKIP $a" ;;
                *) echo "知らない引数: $a" >&2; _WHY="知らない引数: $a"; exit 64 ;;
            esac ;;
    esac
done

# 一時ディレクトリは OS から取る (パスの直書き禁止)。
GATE_TMP=$(mktemp -d 2>/dev/null || mktemp -d -t zvgate)
ESC=$(printf '\033')

OK=0; NG=0; SK=0
SUMMARY=''

step() {
    name="$1"
    eval "d=\$desc_$name"
    eval "slow=\$slow_$name"

    if [ -n "$ONLY" ]; then
        case " $ONLY " in
            *" $name "*) ;;
            *) record "$name" 'skip' '--only の対象外'; return 0 ;;
        esac
    fi
    case " $SKIP " in
        *" $name "*) record "$name" 'skip' '--skip で外されました'; return 0 ;;
    esac
    if [ "$QUICK" = 1 ] && [ "$slow" = 1 ] && [ "$name" != verify ]; then
        record "$name" 'skip' '--quick なので走らせていません'
        return 0
    fi

    printf '\n\033[1;36m▸ %s — %s\033[0m\n' "$name" "$d"
    log="$GATE_TMP/$name.log"
    # **溜めずに素通しする。** 全部終わってから `cat` すると、3 分かかる段は
    # そのあいだ画面が真っ白で、固まったのか働いているのか分からない
    # (CI で 15 分間まっさらなログになったのと同じ形)。
    #
    # ただし `| tee` を素で挟むと `$?` は tee のものになり「落ちたのに緑」を
    # 読む (実際に 2 度やった)。**終了コードはパイプの内側で捕まえる。**
    set +e
    { "run_$name" 2>&1; echo "$?" > "$GATE_TMP/$name.rc"; } | tee "$log"
    rc=$(cat "$GATE_TMP/$name.rc" 2>/dev/null || echo 1)
    set -e
    if [ "$rc" -ne 0 ]; then
        record "$name" 'ng' "rc=$rc"
    elif grep -q '\[skip\]' "$log"; then
        record "$name" 'skip' "$(grep -m1 '\[skip\]' "$log" | sed -e 's/^[[:space:]]*//' -e "s/$ESC\[[0-9;]*m//g")"
    else
        record "$name" 'ok' ''
    fi
}

record() {
    case "$2" in
        ok)   OK=$((OK + 1));  mark='✓'; extra='' ;;
        ng)   NG=$((NG + 1));  mark='✗'; extra="  $3" ;;
        skip) SK=$((SK + 1));  mark='-'; extra="  $3" ;;
    esac
    SUMMARY="$SUMMARY
  $mark $1$extra"
}

for s in $STEPS; do
    step "$s"
done

printf '\n\033[1m── 出荷前の関門 ──\033[0m%s\n' "$SUMMARY"
printf '  緑 %s / 赤 %s / 未確認 %s\n' "$OK" "$NG" "$SK"

if [ "$NG" -gt 0 ]; then
    _WHY="$NG 段が赤"
    exit 1
fi
if [ "$SK" -gt 0 ]; then
    _LABEL="出荷前の関門 (緑 $OK / **未確認 $SK** — 確かめていない段があります)"
else
    _LABEL="出荷前の関門 (全 $OK 段)"
fi
exit 0
