#!/bin/sh
# 選択した名前に一致するワークツリーを 1 件だけ特定して取り外す。
# 取り込み済みのブランチであれば併せて削除し、未取り込みなら残して警告する。
set -eu
. "$ZV_PLUGIN_DIR/lib.sh"

enter_workspace
require_git

sel=$(selected_word)
if ! resolve_worktree "$sel"; then
	die "$WT_ERR"
fi

here=$(git rev-parse --show-toplevel)
if [ "$WT_PATH" = "$here" ]; then
	die "現在開いているワークツリーは削除できません: $WT_PATH"
fi

if ! out=$(git worktree remove "$WT_PATH" 2>&1); then
	if ! out=$(git worktree remove --force "$WT_PATH" 2>&1); then
		die "ワークツリーを削除できません: $out"
	fi
	notify "未コミットの変更ごと削除しました: $WT_PATH" warn
fi

if [ -n "$WT_BRANCH" ]; then
	if git branch -d "$WT_BRANCH" >/dev/null 2>&1; then
		notify "ワークツリーとブランチ $WT_BRANCH を削除しました。"
	else
		notify "ワークツリーを削除しました。ブランチ $WT_BRANCH は未取り込みのため残しています。" warn
	fi
else
	notify "ワークツリーを削除しました: $WT_PATH"
fi

printf '{"action":"refresh_files"}\n'
