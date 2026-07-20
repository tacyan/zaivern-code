#!/bin/sh
# リモート接続まわりの共通処理。設定の検証と、ssh / rsync のコマンド組み立てを担う。
# lib.sh を読み込んだあとに . （ドット）読み込みして使う。

RH_HOST=""
RH_PATH=""
RH_OPTS=""

# 設定を読み、ssh が使えることまで確認する。不足があれば日本語で通知して終了する。
require_remote() {
	command -v ssh >/dev/null 2>&1 || die "ssh が見つかりません。ssh を導入してから再実行してください。"

	RH_HOST=${ZV_CFG_HOST:-}
	if [ -z "$RH_HOST" ]; then
		die "リモートホストが未設定です。プラグイン設定の「リモートホスト」に user@example.com の形式で入力してください。"
	fi

	RH_PATH=${ZV_CFG_PATH:-}
	if [ -z "$RH_PATH" ]; then
		RH_PATH="~/$(basename "${ZV_WORKSPACE:-workspace}")"
	fi

	RH_OPTS=${ZV_CFG_SSH_OPTS:-}
}

# rsync が使えることを確認する。
require_rsync() {
	command -v rsync >/dev/null 2>&1 ||
		die "rsync が見つかりません。同期には rsync が必要です。導入してから再実行してください。"
}

# リモート側で使う cd 部分を組み立てる（~ で始まる場合は展開させるため引用しない）。
rh_cd() {
	printf 'cd %s' "$(rh_path_arg)"
}

# リモート側でパスを指す引数を組み立てる（~ で始まる場合は展開させるため引用しない）。
rh_path_arg() {
	case "$RH_PATH" in
	"~"*) printf '%s' "$RH_PATH" ;;
	*) printf '"%s"' "$RH_PATH" ;;
	esac
}

# リモートで実行するシェル文字列を受け取り、端末へ流す ssh コマンド行を組み立てる。
# $2 に -t を渡すと擬似端末を割り当てる（対話的なコマンド向け）。
rh_ssh_line() {
	_script=$1
	_tty=${2:-}
	_line="ssh"
	if [ -n "$_tty" ]; then
		_line="$_line $_tty"
	fi
	if [ -n "$RH_OPTS" ]; then
		_line="$_line $RH_OPTS"
	fi
	printf '%s %s %s' "$_line" "$(shquote "$RH_HOST")" "$(shquote "$_script")"
}

# 同期から外すパス。作業に不要で重いものだけを既定で落とす。
rh_excludes() {
	printf '%s' "--exclude .git --exclude target --exclude node_modules --exclude .DS_Store"
}
