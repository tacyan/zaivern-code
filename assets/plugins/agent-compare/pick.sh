#!/bin/sh
# 比較表から選んだブランチを「採用」として現在のブランチへ取り込み、
# 結果を通知しつつ比較パネルへ採用記録を書き戻す。
set -eu
. "$ZV_PLUGIN_DIR/lib.sh"

enter_workspace
require_git

sel=$(selected_word)
sel=$(printf '%s' "$sel" | tr -d '`|' | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')
if [ -z "$sel" ]; then
	die "採用するブランチ名を選択してから実行してください。"
fi

branch=""
if git show-ref --verify --quiet "refs/heads/$sel"; then
	branch=$sel
elif resolve_worktree "$sel"; then
	branch=$WT_BRANCH
	if [ -z "$branch" ]; then
		die "「${sel}」は切り離し HEAD のため取り込めません。"
	fi
else
	die "$WT_ERR"
fi

cur=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")
if [ "$branch" = "$cur" ]; then
	die "「${branch}」は現在のブランチです。別のブランチを選んでください。"
fi
if [ -n "$(git status --porcelain)" ]; then
	die "未コミットの変更があります。コミットまたは退避してから採用してください。"
fi

stat=$(git diff --shortstat "HEAD...$branch" 2>/dev/null | sed -e 's/^[[:space:]]*//')
[ -n "$stat" ] || stat="差分なし"

if out=$(git merge --no-ff -m "採用: $branch を $cur へ取り込み" "$branch" 2>&1); then
	notify "「${branch}」を採用して $cur へ取り込みました（${stat}）"
	printf '{"action":"set_panel","panel":"compare","text":"%s"}\n' \
		"$(js "# 成果比較

採用: \`$branch\` → \`$cur\`（${stat}）
$(date '+%Y-%m-%d %H:%M:%S')

「成果を比較」を実行すると最新の比較表に戻ります。")"
	printf '{"action":"refresh_files"}\n'
else
	notify "採用の取り込みで衝突しました: $out" error
	printf '{"action":"run_terminal","command":"%s","cwd":"%s"}\n' \
		"$(js 'git status')" "$(js "$(git rev-parse --show-toplevel)")"
	printf '{"action":"refresh_files"}\n'
fi
