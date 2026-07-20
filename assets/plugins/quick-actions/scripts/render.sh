#!/bin/sh
# 「実行」パネルに出す Markdown を組み立てて標準出力へ書き出す。
# panel.sh と startup.sh の両方から使う。
set -eu

DIR="${ZV_PLUGIN_DIR:-$(dirname "$0")/..}"
. "$DIR/scripts/common.sh"
. "$DIR/scripts/detect.sh"

zv_detect

printf '%s\n\n' "## 実行"

if [ -z "$ZV_KIND" ]; then
  printf '%s\n' "$(zv_undetected_msg)"
  exit 0
fi

printf '%s\n\n' "判定: **$ZV_LABEL**"

printf '%s\n' "### すぐ実行できるもの"
for what in test build fmt; do
  case "$what" in
    test) label="テストを実行" ;;
    build) label="ビルド" ;;
    fmt) label="整形" ;;
  esac
  if cmd=$(zv_cmd_for "$what" ""); then
    printf -- '- %s — `%s`\n' "$label" "$cmd"
  else
    printf -- '- %s — 取得不可 (対応するコマンドがありません)\n' "$label"
  fi
done

printf '\n%s\n' "### スクリプト / ターゲット"
LIST=$(zv_scripts | head -40)
if [ -n "$LIST" ]; then
  printf '%s\n' "$LIST" | while IFS= read -r name; do
    [ -n "$name" ] || continue
    printf -- '- `%s`\n' "$name"
  done
  printf '\n%s\n' "名前を選択して「スクリプトを実行」を呼ぶと、そのまま実行できます。"
else
  printf '%s\n' "取得不可 (実行できるスクリプトが見つかりません)"
fi
