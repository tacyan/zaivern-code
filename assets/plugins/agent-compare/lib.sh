#!/bin/sh
# 共通ヘルパー。JSON Lines アクションの組み立てと、git 前提条件のチェックを担う。
# 各コマンドスクリプトの先頭で . （ドット）読み込みして使う。

# 制御文字を落としつつ、JSON 文字列の中身として安全な形へエスケープする。
# 改行は \n に畳み、全体を 1 行として出力する。
json_escape() {
	_tab=$(printf '\t')
	_cr=$(printf '\r')
	LC_ALL=C tr -d '\001-\010\013\014\016-\037\177' |
		sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e "s/${_tab}/\\\\t/g" -e "s/${_cr}/\\\\r/g" |
		awk '{ printf "%s%s", sep, $0; sep = "\\n" }'
}

# 引数 1 つを JSON 文字列の中身へ変換する。
js() {
	printf '%s' "$1" | json_escape
}

# シェルコマンドへ埋め込むための単一引用符クォート。
shquote() {
	printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

# 通知アクションを 1 行出力する。level は info / warn / error。
notify() {
	printf '{"action":"notify","message":"%s","level":"%s"}\n' "$(js "$1")" "${2:-info}"
}

# 失敗を日本語で伝えて正常終了する（スタックトレースを出さないため exit 0）。
# ERR_MODE=text を設定した場合は、パネル表示用にそのままの文章を出す。
die() {
	if [ "${ERR_MODE:-json}" = "text" ]; then
		printf '%s\n' "$1"
	else
		notify "$1" error
	fi
	exit 0
}

# git が使えて、かつ作業ディレクトリがリポジトリ内であることを確かめる。
require_git() {
	command -v git >/dev/null 2>&1 || die "git が見つかりません。git を導入してから再実行してください。"
	git rev-parse --is-inside-work-tree >/dev/null 2>&1 ||
		die "ここは git リポジトリではありません。リポジトリを開いてから実行してください。"
}

# 任意の文字列を、ブランチ名・ディレクトリ名に使える短い識別子へ変換する。
# 日本語などで空になった場合は呼び出し側で代替名を用意すること。
slugify() {
	printf '%s' "$1" |
		LC_ALL=C tr -c 'A-Za-z0-9._-' '-' |
		sed -e 's/--*/-/g' -e 's/^-//' -e 's/-*$//' |
		cut -c1-40
}

# 作業ディレクトリをワークスペースへ揃える。
enter_workspace() {
	cd "${ZV_WORKSPACE:-.}" 2>/dev/null || die "ワークスペースへ移動できません: ${ZV_WORKSPACE:-.}"
}

# ワークツリーを置く親ディレクトリ。設定が空ならリポジトリの隣。
worktree_root() {
	_root=${ZV_CFG_ROOT:-}
	if [ -n "$_root" ]; then
		printf '%s' "$_root"
	else
		printf '%s' "$(dirname "$(git rev-parse --show-toplevel)")"
	fi
}

# 「パス<TAB>ブランチ」形式でワークツリー一覧を出力する。
worktree_pairs() {
	git worktree list --porcelain 2>/dev/null | awk '
		/^worktree /{ if (p != "") print p "\t" b; p = substr($0, 10); b = "" }
		/^branch /{ b = substr($0, 8); sub(/^refs\/heads\//, "", b) }
		END { if (p != "") print p "\t" b }
	'
}

# 語に一致するワークツリーを 1 件だけ特定して WT_PATH / WT_BRANCH に入れる。
# 見つからない・複数一致のときは WT_ERR にメッセージを入れて 1 を返す。
WT_PATH=""
WT_BRANCH=""
WT_ERR=""
resolve_worktree() {
	_sel=$1
	WT_PATH=""
	WT_BRANCH=""
	WT_ERR=""
	if [ -z "$_sel" ]; then
		WT_ERR="ワークツリー名かブランチ名を選択してから実行してください。"
		return 1
	fi
	_tab=$(printf '\t')
	_pairs=$(worktree_pairs)
	_n=0
	_list=""
	while IFS="$_tab" read -r _p _b; do
		if [ -z "$_p" ]; then continue; fi
		_hit=0
		case "$_p" in
		*"$_sel"*) _hit=1 ;;
		esac
		if [ -n "$_b" ] && [ "$_b" = "$_sel" ]; then _hit=1; fi
		if [ "$(basename "$_p")" = "$_sel" ]; then _hit=1; fi
		if [ "$_hit" = 1 ]; then
			_n=$((_n + 1))
			WT_PATH=$_p
			WT_BRANCH=$_b
			_list="$_list / $_p"
		fi
	done <<EOF
$_pairs
EOF
	if [ "$_n" -eq 0 ]; then
		WT_ERR="「${_sel}」に一致するワークツリーが見つかりません。"
		return 1
	fi
	if [ "$_n" -gt 1 ]; then
		WT_ERR="「${_sel}」に複数のワークツリーが一致します:$_list"
		return 1
	fi
	return 0
}

# 選択テキスト（無ければ標準入力）から 1 行の語を取り出す。
selected_word() {
	_s=${ZV_SELECTION:-}
	if [ -z "$_s" ]; then
		_s=$(cat 2>/dev/null || true)
	fi
	printf '%s' "$_s" | head -n 1 | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//'
}

# 起動するエージェントのコマンド。設定が空なら使用中のエージェント名を使う。
agent_command() {
	_a=${ZV_CFG_AGENT_CMD:-}
	[ -n "$_a" ] || _a=${ZV_AGENT:-}
	printf '%s' "$_a"
}

# 比較元ブランチ。設定が空なら origin の既定ブランチ → main → master →
# 現在のブランチ、の順に自動判定する。
base_branch() {
	_b=${ZV_CFG_BASE:-}
	if [ -n "$_b" ]; then
		printf '%s' "$_b"
		return 0
	fi
	_o=$(git symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null | sed 's|^origin/||' || true)
	for _c in "$_o" main master; do
		if [ -n "$_c" ] && git show-ref --verify --quiet "refs/heads/$_c"; then
			printf '%s' "$_c"
			return 0
		fi
	done
	git rev-parse --abbrev-ref HEAD 2>/dev/null || printf ''
}

# プラグイン専用のデータ置き場を用意してパスを返す。失敗したら 1 を返す。
# ここと作業ディレクトリ以外へは書き込まないこと。
plugin_data() {
	_d=${ZV_PLUGIN_DATA:-}
	if [ -z "$_d" ]; then
		return 1
	fi
	mkdir -p "$_d" 2>/dev/null || return 1
	printf '%s' "$_d"
}
