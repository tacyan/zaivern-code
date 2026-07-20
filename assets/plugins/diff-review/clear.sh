#!/bin/sh
# 未送信のレビューコメントを破棄する。控えとして discarded-<日時>.txt に退避する。
set -eu
. "$ZV_PLUGIN_DIR/lib.sh"

if ! data=$(plugin_data); then
	die "データ置き場を用意できません。ZV_PLUGIN_DATA を確認してください。"
fi

pending="$data/pending.txt"
if [ ! -s "$pending" ]; then
	notify "破棄するコメントはありません。"
	exit 0
fi

count=$(grep -c '^#@#$' "$pending" 2>/dev/null || true)
[ -n "$count" ] || count=0

stamp=$(date '+%Y%m%d-%H%M%S')
mv "$pending" "$data/discarded-$stamp.txt" 2>/dev/null || rm -f "$pending"

notify "$count 件のコメントを破棄しました。"
printf '{"action":"set_panel","panel":"review","text":"%s"}\n' \
	"$(js "$(sh "$ZV_PLUGIN_DIR/pending.sh")")"
