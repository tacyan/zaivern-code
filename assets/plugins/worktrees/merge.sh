#!/bin/sh
# 選択したワークツリー（またはブランチ）の変更を、いま開いているブランチへ取り込む。
# 衝突した場合は中断せず、解決用のターミナルを開くアクションを出力する。
set -eu
. "$ZV_PLUGIN_DIR/lib.sh"

enter_workspace
require_git

sel=$(selected_word)
branch=""
if resolve_worktree "$sel"; then
	branch=$WT_BRANCH
	if [ -z "$branch" ]; then
		die "「${sel}」は切り離し HEAD のため取り込めません。"
	fi
elif git show-ref --verify --quiet "refs/heads/$sel"; then
	branch=$sel
else
	die "$WT_ERR"
fi

cur=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")
if [ "$branch" = "$cur" ]; then
	die "「${branch}」は現在のブランチです。取り込む対象を変えてください。"
fi

if [ -n "$(git status --porcelain)" ]; then
	die "未コミットの変更があります。コミットまたは退避してから取り込んでください。"
fi

stat=$(git diff --stat "HEAD...$branch" 2>/dev/null | tail -n 1 | sed -e 's/^[[:space:]]*//')
[ -n "$stat" ] || stat="差分なし"

if out=$(git merge --no-ff "$branch" 2>&1); then
	notify "$branch を $cur へ取り込みました（${stat}）"
	printf '{"action":"refresh_files"}\n'
else
	notify "取り込みで衝突しました: $out" error
	printf '{"action":"run_terminal","command":"%s","cwd":"%s"}\n' \
		"$(js 'git status')" "$(js "$(git rev-parse --show-toplevel)")"
	printf '{"action":"refresh_files"}\n'
fi
