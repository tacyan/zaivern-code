#!/bin/sh
# 現在のリポジトリのワークツリーを列挙し、ブランチ・未コミット数・最新コミットを
# markdown でパネルへ出力する。エラー時もパネルに読める日本語を表示する。
set -eu
. "$ZV_PLUGIN_DIR/lib.sh"

ERR_MODE=text
enter_workspace
require_git

here=$(git rev-parse --show-toplevel)

printf '## ワークツリー\n\n'

git worktree list --porcelain 2>/dev/null | {
	wt=""
	br=""
	total=0

	show() {
		[ -n "$wt" ] || return 0
		total=$((total + 1))
		mark="  "
		if [ "$wt" = "$here" ]; then mark="→ "; fi
		dirty=$(git -C "$wt" status --porcelain 2>/dev/null | wc -l | tr -d ' ')
		if [ "$dirty" = "0" ]; then
			state="変更なし"
		else
			state="未コミット $dirty 件"
		fi
		last=$(git -C "$wt" log -1 --pretty='%h %s' 2>/dev/null || true)
		[ -n "$last" ] || last="コミットなし"
		printf -- '- %s**%s** — %s\n' "$mark" "${br:-(不明)}" "$state"
		printf -- '  - `%s`\n' "$wt"
		printf -- '  - %s\n' "$last"
	}

	while IFS= read -r line || [ -n "$line" ]; do
		case "$line" in
		"worktree "*)
			show
			wt=${line#worktree }
			br=""
			;;
		"branch "*)
			br=${line#branch }
			br=${br#refs/heads/}
			;;
		detached) br="(切り離し HEAD)" ;;
		esac
	done
	show

	printf '\n合計 %s 件（→ が現在のワークツリー）\n' "$total"
}
