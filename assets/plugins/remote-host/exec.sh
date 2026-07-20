#!/bin/sh
# 選択テキストをコマンドとみなし、リモート作業ディレクトリで実行するターミナルを開く。
# 出力やパスワード入力をそのまま扱えるよう、待たずに端末へ流す。
set -eu
. "$ZV_PLUGIN_DIR/lib.sh"
. "$ZV_PLUGIN_DIR/remote.sh"

enter_workspace
require_remote

cmd=${ZV_SELECTION:-}
if [ -z "$cmd" ]; then
	cmd=$(cat 2>/dev/null || true)
fi
cmd=$(printf '%s' "$cmd" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')
if [ -z "$cmd" ]; then
	die "実行するコマンドがありません。コマンドを選択してから実行してください。"
fi

line=$(rh_ssh_line "$(rh_cd) && $cmd")

printf '{"action":"run_terminal","command":"%s","cwd":"%s"}\n' \
	"$(js "$line")" "$(js "${ZV_WORKSPACE:-.}")"
notify "$RH_HOST で実行します: $cmd"
