#!/bin/sh
# PR の差分を取得し、新しいタブへ表示する。
# 番号は選択テキストを優先し、無ければ現在のブランチに紐づく PR を使う。
set -eu

DIR="${ZV_PLUGIN_DIR:-$(dirname "$0")/..}"
. "$DIR/scripts/common.sh"
. "$DIR/scripts/gh-common.sh"

zv_data
zv_gh_ready

NUM=$(printf '%s' "${ZV_SELECTION:-}" | tr -cd '0-9' | head -c 12)

DIFF="$ZV_PLUGIN_DATA/pr-diff.patch"
ERR="$ZV_PLUGIN_DATA/pr-diff.err"

if [ -n "$NUM" ]; then
  TITLE="PR #$NUM の差分"
  gh pr diff "$NUM" >"$DIFF" 2>"$ERR" || zv_fail "PR #$NUM の差分を取得できませんでした: $(head -c 300 "$ERR" | tr '\n' ' ')"
else
  TITLE="現在のブランチの PR 差分"
  gh pr diff >"$DIFF" 2>"$ERR" || zv_fail "現在のブランチに対応する PR が見つかりません。PR 番号を選択してから実行してください。"
fi

if [ ! -s "$DIFF" ]; then
  zv_fail "差分は空でした。"
fi

zv_emit action new_tab title "$TITLE" text "@@$DIFF"
