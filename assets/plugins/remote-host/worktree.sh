#!/bin/sh
# 選択テキストを作業名として、リモート作業ディレクトリのリポジトリに
# ブランチ付きワークツリーを作り、そこでシェルを開く。
set -eu
. "$ZV_PLUGIN_DIR/lib.sh"
. "$ZV_PLUGIN_DIR/remote.sh"

enter_workspace
require_remote

task=$(selected_word)
slug=$(slugify "$task")
[ -n "$slug" ] || slug="task-$(date +%Y%m%d-%H%M%S)"

name=$(basename "$RH_PATH")
branch="wt/$slug"
dir="../$name-wt-$slug"

remote="$(rh_cd) && git worktree add -b \"$branch\" \"$dir\" && cd \"$dir\" && git status -sb && exec \${SHELL:-sh} -l"
line=$(rh_ssh_line "$remote" "-t")

printf '{"action":"run_terminal","command":"%s","cwd":"%s"}\n' \
	"$(js "$line")" "$(js "${ZV_WORKSPACE:-.}")"
notify "$RH_HOST にワークツリー $name-wt-${slug}（ブランチ ${branch}）を作成します。"
