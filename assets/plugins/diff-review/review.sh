#!/bin/sh
# 現在の作業ツリーの差分を取得してプラグインのデータ置き場へ保存し、
# 新規タブとして開くアクションを出力する。
set -eu
. "$ZV_PLUGIN_DIR/lib.sh"

enter_workspace
require_git

if ! data=$(plugin_data); then
	die "データ置き場を用意できません。ZV_PLUGIN_DATA を確認してください。"
fi

if git rev-parse --verify --quiet HEAD >/dev/null; then
	diff=$(git diff HEAD 2>/dev/null || true)
else
	diff=$(git diff 2>/dev/null || true)
fi

if [ -z "$diff" ]; then
	notify "差分はありません。変更を加えてから実行してください。"
	exit 0
fi

stamp=$(date '+%Y%m%d-%H%M%S')
out="$data/diff-$stamp.diff"
printf '%s\n' "$diff" >"$out" || die "差分を保存できません: $out"
printf '%s\n' "$diff" >"$data/latest.diff" || true

max=${ZV_CFG_MAX_LINES:-4000}
case "$max" in
'' | *[!0-9]*) max=4000 ;;
esac
[ "$max" -ge 100 ] || max=100

total=$(printf '%s\n' "$diff" | wc -l | tr -d ' ')
body=$(printf '%s\n' "$diff" | head -n "$max")
if [ "$total" -gt "$max" ]; then
	body="$body
… 以降 $((total - max)) 行を省略しました（全文: ${out}）"
fi

header="# 差分レビュー $stamp
ブランチ: ${ZV_GIT_BRANCH:-不明} / 全 $total 行
保存先: $out
行を選んで「コメントを追加」、溜まったら「コメントをエージェントへ送る」。

"

printf '{"action":"new_tab","title":"%s","text":"%s"}\n' \
	"$(js "差分レビュー $stamp")" "$(js "$header$body")"
notify "差分を保存しました: $out"
