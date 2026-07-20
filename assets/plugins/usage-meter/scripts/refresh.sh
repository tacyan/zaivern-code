#!/bin/sh
# 使用量を集計し、set_panel アクションで「使用量」パネルを更新する。
# コマンドからも、5 分ごとのフックからも同じ処理を使う。
set -eu

DIR="${ZV_PLUGIN_DIR:-$(dirname "$0")/..}"
. "$DIR/scripts/common.sh"

zv_data

OUT="$ZV_PLUGIN_DATA/usage.md"
if ! python3 "$DIR/scripts/scan.py" "${ZV_CFG_EXTRA_DIRS:-}" >"$OUT" 2>"$ZV_PLUGIN_DATA/usage.err"; then
  zv_fail "使用量を集計できませんでした: $(head -c 200 "$ZV_PLUGIN_DATA/usage.err" | tr '\n' ' ')"
fi

zv_emit action set_panel panel usage text "@@$OUT"

# 手動実行のときだけ状態表示を出す (フックからは静かに更新する)
if [ -z "${ZV_EVENT:-}" ]; then
  zv_emit action set_status text "使用量を更新しました"
fi
