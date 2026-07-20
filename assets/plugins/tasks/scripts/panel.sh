#!/bin/sh
# 「課題」パネルの本文を標準出力へ書き出す (パネルの run は stdout をそのまま表示する)。
# パネルを開いたとき (refresh="on_open") に呼ばれる。
set -eu

DIR="${ZV_PLUGIN_DIR:-$(dirname "$0")/..}"
. "$DIR/scripts/common.sh"

if ! zv_have gh; then
  printf '%s\n' "## 課題"
  printf '\n%s\n' "gh コマンドが見つかりません。GitHub CLI を導入し、ターミナルで 'gh auth login' を実行してください。"
  exit 0
fi
if ! gh auth status >/dev/null 2>&1; then
  printf '%s\n' "## 課題"
  printf '\n%s\n' "GitHub の認証が済んでいません。ターミナルで 'gh auth login' を実行してください。"
  exit 0
fi
if ! git -C "${ZV_WORKSPACE:-.}" rev-parse --git-dir >/dev/null 2>&1; then
  printf '%s\n' "## 課題"
  printf '\n%s\n' "このワークスペースは git リポジトリではありません。"
  exit 0
fi

. "$DIR/scripts/gh-common.sh"
zv_data
TMP="$ZV_PLUGIN_DATA/panel.json"

if ! gh issue list --limit "$(zv_limit)" --json number,title,author,labels,state,url >"$TMP" 2>/dev/null; then
  rm -f "$TMP"
  printf '%s\n' "## 課題"
  printf '\n%s\n' "課題一覧を取得できませんでした。リポジトリの接続設定を確認してください。"
  exit 0
fi

zv_render "課題" issue "$TMP"
rm -f "$TMP"
