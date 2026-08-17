#!/usr/bin/env sh
# スマホ画面 (`assets/remote/`) を **本物のブラウザで描いて**検査する。
#
# 使い方:
#   tools/remote-check.sh              # 4 通り (狭い / 空 / 英語 / 旧 PC) を検査
#   tools/remote-check.sh --self-test  # 検査そのものを検査する (わざと壊して赤を確認)
#   tools/remote-check.sh --list       # 仕込める欠陥の一覧
#   tools/remote-check.sh --inject history-css   # 1 つだけ仕込んで赤を見る
#   tools/remote-check.sh --seed 123 --walk 300  # 乱歩を長く回す
#
# ## なぜこれが要るのか
#
# Rust の単体テストは **CSS も DOM も 1 度も評価していない**。だから
# `#alist.mid` が `#alist{display:none}` を打ち消して端末と一覧が同時に
# 見えた事故は、テストが全部緑のまま出荷された。
# **評価していないものは検査できない。** 詳しくは docs/preflight.md。
#
# 中身は `tools/remote-check.js` (Node)。ここでは
#   - Node が無ければ理由を出して `[skip]`
#   - Chromium 系が無ければ理由を出して `[skip]` (js 側が rc=2 を返す)
#   - **どの経路で終わっても最後の 1 行に判定を書く**
# だけを行う。**「確かめられなかったので緑」は書かない。**
#
# Python は使わない (CLAUDE.md)。JS を使うのは、検査対象そのものが
# JS と CSS だからで、道具の言語を増やしているのではない。
set -eu
_LABEL='スマホ画面の検査'

# 呼び出し側が `| tail` を挟むと `$?` はそちらのものになる。終了コードだけを
# 真実にせず、**最後の 1 行**に判定を書く (パイプ越しでも嘘にならない)。
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

cd "$(dirname "$0")/.."

case "${1:-}" in
  -h|--help) sed -n '2,10p' "$0"; _LABEL='使い方 (検査は走らせていない)'; exit 0 ;;
  # 一覧は検査ではない。判定行が「緑」と読めないようにラベルを変える。
  --list) _LABEL='仕込みの一覧 (検査は走らせていない)' ;;
esac

NODE="${ZV_NODE:-node}"
if ! command -v "$NODE" >/dev/null 2>&1; then
    printf '\033[1;33m[skip] node が見つかりません — スマホ画面は 1 度も描いていません\n'
    printf '       Node 18 以上を入れるか、ZV_NODE に実行ファイルを指定してください\033[0m\n'
    _LABEL='スマホ画面の検査 [skip] (node が無いので 1 度も描いていない)'
    exit 0
fi

set +e
"$NODE" tools/remote-check.js "$@"
RC=$?
set -e

# rc=2 は「ブラウザが無いので検査していない」。理由は js 側が 1 行出している。
if [ "$RC" -eq 2 ]; then
    _LABEL='スマホ画面の検査 [skip] (ブラウザが無いので 1 度も描いていない)'
    exit 0
fi
[ "$RC" -eq 0 ] || { _WHY='違反が見つかりました'; exit "$RC"; }
exit 0
