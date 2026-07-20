#!/bin/sh
# 依存の少ない代替手段。クリップボードにコピーした HTML と、任意の切り取り画像を
# まとめてエージェントのプロンプトへ渡す。ブラウザの自動操作は一切行わない。
set -eu

DIR="${ZV_PLUGIN_DIR:-$(dirname "$0")/..}"
. "$DIR/scripts/common.sh"
. "$DIR/scripts/capture-common.sh"

zv_data

CLIP="$ZV_PLUGIN_DATA/clipboard.txt"
if zv_have pbpaste; then
  pbpaste >"$CLIP" 2>/dev/null || : >"$CLIP"
elif zv_have xclip; then
  xclip -selection clipboard -o >"$CLIP" 2>/dev/null || : >"$CLIP"
else
  : >"$CLIP"
fi

if [ ! -s "$CLIP" ] && [ -n "${ZV_SELECTION:-}" ]; then
  printf '%s' "$ZV_SELECTION" >"$CLIP"
fi

SHOT=""
if [ "$(uname -s)" = "Darwin" ]; then
  zv_emit action set_status text "切り取る範囲をドラッグしてください (不要なら Esc)"
  SHOT=$(zv_shot)
fi

if [ ! -s "$CLIP" ] && [ -z "$SHOT" ]; then
  zv_fail "クリップボードが空で、画像の切り取りも行われませんでした。ブラウザの開発者ツールで要素をコピーしてから、もう一度お試しください。"
fi

PROMPT="$ZV_PLUGIN_DATA/paste-prompt.txt"
{
  printf '%s\n\n' "以下は手元で写し取った UI 要素の情報です。"
  if [ -n "$SHOT" ]; then
    printf -- '- 切り取り画像: %s\n' "$SHOT"
  else
    printf -- '- 切り取り画像: 取得不可\n'
  fi
  printf '\n## クリップボードの内容\n'
  if [ -s "$CLIP" ]; then
    printf '```html\n'
    head -c 8000 "$CLIP"
    printf '\n```\n'
  else
    printf '取得不可\n'
  fi
  printf '\nこの内容を参考に作業を進めてください。\n'
} >"$PROMPT"

zv_emit action agent_prompt agent "${ZV_AGENT:-}" text "@@$PROMPT" submit false
zv_emit action set_status text "要素の情報をプロンプトへ入れました"
