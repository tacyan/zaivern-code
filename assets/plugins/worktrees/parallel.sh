#!/bin/sh
# 選択テキストを共通の指示文として受け取り、設定した数だけワークツリーを作成して、
# それぞれのディレクトリで同じ指示文を渡したエージェントを起動する。
set -eu
. "$ZV_PLUGIN_DIR/lib.sh"

enter_workspace
require_git

prompt=${ZV_SELECTION:-}
if [ -z "$prompt" ]; then
	prompt=$(cat 2>/dev/null || true)
fi
if [ -z "$(printf '%s' "$prompt" | tr -d '[:space:]')" ]; then
	die "指示文がありません。エディタで指示文を選択してから実行してください。"
fi

agent=$(agent_command)
if [ -z "$agent" ]; then
	die "エージェント起動コマンドが未設定です。プラグイン設定の「エージェント起動コマンド」を入力してください。"
fi

count=${ZV_CFG_COUNT:-3}
case "$count" in
'' | *[!0-9]*) count=3 ;;
esac
[ "$count" -ge 1 ] || count=1
[ "$count" -le 8 ] || count=8

repo=$(git rev-parse --show-toplevel)
name=$(basename "$repo")
root=$(worktree_root)
mkdir -p "$root" 2>/dev/null || die "ワークツリー置き場を作成できません: $root"

slug=$(slugify "$(printf '%s' "$prompt" | head -n 1)")
[ -n "$slug" ] || slug="para"
stamp=$(date +%m%d-%H%M%S)

created=0
i=1
while [ "$i" -le "$count" ]; do
	dir="$root/$name-wt-$slug-$stamp-$i"
	branch="wt/$slug-$stamp-$i"
	if out=$(git worktree add -b "$branch" "$dir" 2>&1); then
		created=$((created + 1))
		printf '{"action":"run_terminal","command":"%s","cwd":"%s"}\n' \
			"$(js "$agent $(shquote "$prompt")")" "$(js "$dir")"
	else
		notify "$i 番目のワークツリーを作成できません: $out" warn
	fi
	i=$((i + 1))
done

if [ "$created" -eq 0 ]; then
	die "ワークツリーを 1 つも作成できませんでした。"
fi

notify "$created 個のワークツリーでエージェントを起動しました。"
printf '{"action":"refresh_files"}\n'
