#!/bin/sh
# 選択テキスト（無ければ日時）を作業名として、専用ブランチ付きの git ワークツリーを
# リポジトリの隣に作成し、そのディレクトリでターミナルを開くアクションを出力する。
set -eu
. "$ZV_PLUGIN_DIR/lib.sh"

enter_workspace
require_git

task=${ZV_SELECTION:-}
if [ -z "$task" ]; then
	task=$(cat 2>/dev/null || true)
fi
task=$(printf '%s' "$task" | tr '\n\t' '  ' | sed -e 's/^ *//' -e 's/ *$//')

slug=$(slugify "$task")
[ -n "$slug" ] || slug="task-$(date +%Y%m%d-%H%M%S)"

repo=$(git rev-parse --show-toplevel)
name=$(basename "$repo")
root=$(worktree_root)
dir="$root/$name-wt-$slug"
branch="wt/$slug"

if [ -e "$dir" ]; then
	die "同名のディレクトリがすでにあります: $dir"
fi
if git show-ref --verify --quiet "refs/heads/$branch"; then
	branch="$branch-$(date +%H%M%S)"
fi

mkdir -p "$root" 2>/dev/null || die "ワークツリー置き場を作成できません: $root"

if ! out=$(git worktree add -b "$branch" "$dir" 2>&1); then
	die "ワークツリーを作成できません: $out"
fi

notify "ワークツリーを作成しました: ${dir}（ブランチ ${branch}）"
printf '{"action":"run_terminal","command":"%s","cwd":"%s"}\n' "$(js 'git status -sb')" "$(js "$dir")"
printf '{"action":"refresh_files"}\n'
