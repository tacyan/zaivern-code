#!/bin/sh
# 画面の一部をドラッグで切り取り、その画像パスだけをエージェントのプロンプトへ渡す。
set -eu

DIR="${ZV_PLUGIN_DIR:-$(dirname "$0")/..}"
. "$DIR/scripts/common.sh"
. "$DIR/scripts/capture-common.sh"

zv_data

if [ "$(uname -s)" != "Darwin" ] || ! zv_have screencapture; then
  zv_fail "画面の切り取りは macOS でのみ利用できます。"
fi

zv_emit action set_status text "切り取る範囲をドラッグしてください"
SHOT=$(zv_shot)

if [ -z "$SHOT" ]; then
  zv_notify info "切り取りを中止しました。"
  exit 0
fi

NOTE="${ZV_SELECTION:-}"
PROMPT="$ZV_PLUGIN_DATA/region-prompt.txt"
{
  printf '%s\n\n' "画面を切り取った画像を添えます。"
  printf -- '- 画像: %s\n' "$SHOT"
  if [ -n "$NOTE" ]; then
    printf '\n## 補足\n%s\n' "$NOTE"
  fi
  printf '\nこの画像を参考に作業を進めてください。\n'
} >"$PROMPT"

zv_emit action agent_prompt agent "${ZV_AGENT:-}" text "@@$PROMPT" submit false
zv_emit action set_status text "切り取り画像をプロンプトへ入れました"
