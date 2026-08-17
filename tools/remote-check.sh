#!/usr/bin/env sh
# スマホ画面 (`assets/remote/`) を **本物のブラウザで描いて**検査する。
#
# 使い方:
#   tools/remote-check.sh              # 4 通り (狭い / 空 / 英語 / 旧 PC) を検査
#   tools/remote-check.sh --self-test  # 検査そのものを検査する (わざと壊して赤を確認)
#   tools/remote-check.sh --list       # 仕込める欠陥の一覧
#   tools/remote-check.sh --inject history-css   # 1 つだけ仕込んで赤を見る
#   tools/remote-check.sh --hang cdp   # わざと固めて、時限が効くことを見る
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
#   - **道具が終わらないときは打ち切る** (rc=3。最後の砦)
#   - **どの経路で終わっても最後の 1 行に判定を書く**
# だけを行う。**「確かめられなかったので緑」は書かない。**
#
# ## なぜ「終わらない」を数えるのか
#
# CI (ubuntu) でこの検査が **15 分間 1 バイトも出さないまま**打ち切られた。
# ランダムに止まる関門は、そのうち誰も見なくなる。だから時限は 3 段ある:
#
#   1. js の中: 1 往復 (CDP) / 握手 / 読み込み / 後始末 — それぞれに予算
#   2. js の中: 進捗が止まってからの猶予と、全体の上限 (rc=3)
#   3. ここ:    js 自体が固まったときの最後の砦 (ZV_WALL_S 秒)
#
# 出力は**溜めずに素通し**する。溜めると、固まったときに何も見えない。
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
  -h|--help) sed -n '2,12p' "$0"; _LABEL='使い方 (検査は走らせていない)'; exit 0 ;;
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

# 最後の砦。js の中の時限 (既定 12 分) より長く、CI のジョブ timeout より短く。
# **道具自身の時限が壊れても、ここで必ず終わる。**
ZV_WALL_S="${ZV_WALL_S:-780}"

# 出力は溜めない (固まったときに何も見えなくなるため) — そのまま素通しする。
set +e
"$NODE" tools/remote-check.js "$@" &
_NPID=$!
_RC=''
_WAITED=0
while kill -0 "$_NPID" 2>/dev/null; do
    if [ "$_WAITED" -ge "$ZV_WALL_S" ]; then
        printf '\033[1;31m✗ 道具が %s 秒たっても終わりません — 打ち切ります\033[0m\n' "$ZV_WALL_S"
        printf '  (js の中の時限が働いていません。ZV_WALL_S で猶予を変えられます)\n'
        # まず TERM。js 側の signal 受けがブラウザをツリーごと畳む。
        kill -TERM "$_NPID" 2>/dev/null
        _G=0
        while kill -0 "$_NPID" 2>/dev/null && [ "$_G" -lt 10 ]; do
            sleep 1
            _G=$((_G + 1))
        done
        kill -KILL "$_NPID" 2>/dev/null
        _RC=3
        break
    fi
    sleep 1
    _WAITED=$((_WAITED + 1))
done
if [ -z "$_RC" ]; then
    wait "$_NPID"
    _RC=$?
fi
set -e

# rc=2 は「ブラウザが無いので検査していない」。理由は js 側が 1 行出している。
if [ "$_RC" -eq 2 ]; then
    _LABEL='スマホ画面の検査 [skip] (ブラウザが無いので 1 度も描いていない)'
    exit 0
fi
# rc=3 は「終わらないので打ち切った」。**違反とは別物**なので言い分を分ける
# (「違反が見つかりました」と書くと、探しても違反が無くて時間を溶かす)。
if [ "$_RC" -eq 3 ]; then
    _WHY='時限切れ — 終わらないので打ち切りました (違反ではありません)'
    exit 3
fi
if [ "$_RC" -eq 64 ]; then
    _WHY='引数が違います'
    exit 64
fi
[ "$_RC" -eq 0 ] || { _WHY='違反が見つかりました'; exit "$_RC"; }
exit 0
