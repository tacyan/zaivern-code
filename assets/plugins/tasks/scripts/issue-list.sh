#!/bin/sh
# 課題の一覧を取得し、set_panel アクションで「課題」パネルへ Markdown を流し込む。
set -eu

DIR="${ZV_PLUGIN_DIR:-$(dirname "$0")/..}"
. "$DIR/scripts/common.sh"
. "$DIR/scripts/gh-common.sh"

zv_data
zv_gh_ready

OUT="$ZV_PLUGIN_DATA/issue-list.md"
JSON="$ZV_PLUGIN_DATA/issue-list.json"

if ! gh issue list --limit "$(zv_limit)" \
    --json number,title,author,labels,state,url \
    >"$JSON" 2>"$ZV_PLUGIN_DATA/issue-list.err"; then
  zv_fail "課題一覧を取得できませんでした: $(head -c 300 "$ZV_PLUGIN_DATA/issue-list.err" | tr '\n' ' ')"
fi

zv_render "課題一覧" issue "$JSON" >"$OUT"
zv_emit action set_panel panel tasks text "@@$OUT"
zv_emit action set_status text "課題一覧を更新しました"
