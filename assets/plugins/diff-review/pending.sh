#!/bin/sh
# 未送信のレビューコメントを、ファイル・行つきの markdown 一覧として出力する（パネル用）。
set -eu
. "$ZV_PLUGIN_DIR/lib.sh"

ERR_MODE=text

if ! data=$(plugin_data); then
	die "データ置き場を用意できません。ZV_PLUGIN_DATA を確認してください。"
fi

pending="$data/pending.txt"

printf '## レビューコメント\n\n'

if [ ! -s "$pending" ]; then
	printf '未送信のコメントはありません。\n\n'
	printf '気になる箇所を選択して「コメントを追加」を実行すると、ここに溜まります。\n'
	exit 0
fi

count=$(grep -c '^#@#$' "$pending" 2>/dev/null || true)
[ -n "$count" ] || count=0
printf '未送信 %s 件\n' "$count"

awk -F'\t' '
	BEGIN { state = 0 }
	state == 0 { printf "\n### `%s`:%s\n\n_%s_\n\n", $1, $2, $3; state = 1; next }
	$0 == "#@#" { state = 0; next }
	{ printf "> %s\n", $0 }
' "$pending"
