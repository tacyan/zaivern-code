#!/bin/sh
# 手元のワークスペースをリモート作業ディレクトリへ rsync で送る。
# 既定では .git や生成物を除外し、リモート側の余分なファイルは消さない。
set -eu
. "$ZV_PLUGIN_DIR/lib.sh"
. "$ZV_PLUGIN_DIR/remote.sh"

enter_workspace
require_remote
require_rsync

ws=${ZV_WORKSPACE:-.}
mk=$(rh_ssh_line "mkdir -p $(rh_path_arg)")
sync="rsync -az --info=stats1 $(rh_excludes)"
if [ -n "$RH_OPTS" ]; then
	sync="$sync -e $(shquote "ssh $RH_OPTS")"
fi
sync="$sync $(shquote "$ws/") $(shquote "$RH_HOST:$RH_PATH/")"

printf '{"action":"run_terminal","command":"%s","cwd":"%s"}\n' \
	"$(js "$mk && $sync")" "$(js "$ws")"
notify "$RH_HOST:$RH_PATH へ同期します。"
