#!/bin/sh
# 選択テキストを、いま開いているファイルと行番号に紐づくレビューコメントとして
# データ置き場の pending.txt へ追記する。
set -eu
. "$ZV_PLUGIN_DIR/lib.sh"

enter_workspace

if ! data=$(plugin_data); then
	die "データ置き場を用意できません。ZV_PLUGIN_DATA を確認してください。"
fi

text=${ZV_SELECTION:-}
if [ -z "$text" ]; then
	text=$(cat 2>/dev/null || true)
fi
if [ -z "$(printf '%s' "$text" | tr -d '[:space:]')" ]; then
	die "コメント本文がありません。指摘したい内容を選択してから実行してください。"
fi

file=${ZV_FILE:-}
if [ -z "$file" ]; then
	file="(ファイル未指定)"
else
	ws=${ZV_WORKSPACE:-}
	case "$file" in
	"$ws"/*) file=${file#"$ws"/} ;;
	esac
fi

line=${ZV_LINE:-0}
case "$line" in
'' | *[!0-9]*) line=0 ;;
esac

pending="$data/pending.txt"
{
	printf '%s\t%s\t%s\n' "$file" "$line" "$(date '+%Y-%m-%d %H:%M:%S')"
	printf '%s\n' "$text"
	printf '#@#\n'
} >>"$pending" || die "コメントを保存できません: $pending"

count=$(grep -c '^#@#$' "$pending" 2>/dev/null || true)
[ -n "$count" ] || count=0
notify "コメントを追加しました（未送信 $count 件）: $file:$line"
printf '{"action":"set_panel","panel":"review","text":"%s"}\n' \
	"$(js "$(sh "$ZV_PLUGIN_DIR/pending.sh")")"
