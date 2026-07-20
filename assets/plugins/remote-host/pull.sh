#!/bin/sh
# リモート作業ディレクトリの内容を手元のワークスペースへ rsync で取得する。
# 手元のファイルを消さない設定なので、不要になったファイルは手動で整理すること。
set -eu
. "$ZV_PLUGIN_DIR/lib.sh"
. "$ZV_PLUGIN_DIR/remote.sh"

enter_workspace
require_remote
require_rsync

ws=${ZV_WORKSPACE:-.}
sync="rsync -az --info=stats1 $(rh_excludes)"
if [ -n "$RH_OPTS" ]; then
	sync="$sync -e $(shquote "ssh $RH_OPTS")"
fi
sync="$sync $(shquote "$RH_HOST:$RH_PATH/") $(shquote "$ws/")"

printf '{"action":"run_terminal","command":"%s","cwd":"%s"}\n' "$(js "$sync")" "$(js "$ws")"
notify "$RH_HOST:$RH_PATH から取得します。"
printf '{"action":"refresh_files"}\n'
