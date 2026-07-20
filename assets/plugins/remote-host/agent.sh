#!/bin/sh
# 選択テキストを指示文として、リモート作業ディレクトリでエージェントを起動する。
# 対話できるよう ssh -t で擬似端末を割り当てる。
set -eu
. "$ZV_PLUGIN_DIR/lib.sh"
. "$ZV_PLUGIN_DIR/remote.sh"

enter_workspace
require_remote

agent=${ZV_CFG_AGENT_CMD:-}
[ -n "$agent" ] || agent=${ZV_AGENT:-}
if [ -z "$agent" ]; then
	die "リモートのエージェント起動コマンドが未設定です。プラグイン設定で指定してください。"
fi

prompt=${ZV_SELECTION:-}
if [ -z "$prompt" ]; then
	prompt=$(cat 2>/dev/null || true)
fi

if [ -z "$(printf '%s' "$prompt" | tr -d '[:space:]')" ]; then
	remote="$(rh_cd) && $agent"
	msg="$RH_HOST でエージェントを起動します。"
else
	remote="$(rh_cd) && $agent $(shquote "$prompt")"
	msg="$RH_HOST でエージェントへ指示を渡して起動します。"
fi

line=$(rh_ssh_line "$remote" "-t")

printf '{"action":"run_terminal","command":"%s","cwd":"%s"}\n' \
	"$(js "$line")" "$(js "${ZV_WORKSPACE:-.}")"
notify "$msg"
