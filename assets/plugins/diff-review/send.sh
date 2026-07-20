#!/bin/sh
# 溜まったレビューコメントを 1 つの指示文へ整形してエージェントへ渡し、
# 送信済みとして退避してから未送信リストを空にする。
set -eu
. "$ZV_PLUGIN_DIR/lib.sh"

enter_workspace

if ! data=$(plugin_data); then
	die "データ置き場を用意できません。ZV_PLUGIN_DATA を確認してください。"
fi

pending="$data/pending.txt"
if [ ! -s "$pending" ]; then
	die "送信できるコメントがありません。先に「コメントを追加」で指摘を溜めてください。"
fi

count=$(grep -c '^#@#$' "$pending" 2>/dev/null || true)
[ -n "$count" ] || count=0

body=$(awk -F'\t' '
	BEGIN { state = 0 }
	state == 0 { printf "\n## %s:%s\n", $1, $2; state = 1; next }
	$0 == "#@#" { state = 0; next }
	{ printf "%s\n", $0 }
' "$pending")

prompt="以下はコードレビューの指摘です。ファイルと行番号を確認し、それぞれに対応してください。
対応が不要と判断した場合は理由を添えてください。

ブランチ: ${ZV_GIT_BRANCH:-不明}
指摘件数: $count
$body"

agent=${ZV_CFG_AGENT:-}
[ -n "$agent" ] || agent=${ZV_AGENT:-}

submit=${ZV_CFG_SUBMIT:-false}
case "$submit" in
true | 1 | yes) submit=true ;;
*) submit=false ;;
esac

printf '{"action":"agent_prompt","agent":"%s","text":"%s","submit":%s}\n' \
	"$(js "$agent")" "$(js "$prompt")" "$submit"

stamp=$(date '+%Y%m%d-%H%M%S')
mv "$pending" "$data/sent-$stamp.txt" 2>/dev/null || rm -f "$pending"

notify "$count 件のコメントをエージェントへ渡しました（控え: $data/sent-$stamp.txt）"
printf '{"action":"set_panel","panel":"review","text":"%s"}\n' \
	"$(js "$(sh "$ZV_PLUGIN_DIR/pending.sh")")"
