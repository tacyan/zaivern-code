#!/bin/sh
# 起動時フック。プロジェクトを判定して「実行」パネルを一度だけ埋める。
set -eu

DIR="${ZV_PLUGIN_DIR:-$(dirname "$0")/..}"
. "$DIR/scripts/common.sh"

zv_data

OUT="$ZV_PLUGIN_DATA/panel.md"
sh "$DIR/scripts/render.sh" >"$OUT" 2>"$ZV_PLUGIN_DATA/panel.err" || {
  zv_fail "プロジェクトの判定に失敗しました: $(head -c 200 "$ZV_PLUGIN_DATA/panel.err" | tr '\n' ' ')"
}

zv_emit action set_panel panel runner text "@@$OUT"
