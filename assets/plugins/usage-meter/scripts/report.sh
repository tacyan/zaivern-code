#!/bin/sh
# 使用量の集計結果を新しいタブへ書き出す。パネルより広い画面で確認したいとき用。
set -eu

DIR="${ZV_PLUGIN_DIR:-$(dirname "$0")/..}"
. "$DIR/scripts/common.sh"

zv_data

OUT="$ZV_PLUGIN_DATA/usage-report.md"
if ! python3 "$DIR/scripts/scan.py" "${ZV_CFG_EXTRA_DIRS:-}" >"$OUT" 2>"$ZV_PLUGIN_DATA/usage.err"; then
  zv_fail "使用量を集計できませんでした: $(head -c 200 "$ZV_PLUGIN_DATA/usage.err" | tr '\n' ' ')"
fi

zv_emit action new_tab title "使用量レポート" text "@@$OUT"
