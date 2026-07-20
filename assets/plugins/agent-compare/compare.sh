#!/bin/sh
# 各ワークツリーのブランチを比較元ブランチと突き合わせ、変更ファイル数・追加行・削除行・
# コミット数・未コミット数を表にまとめた markdown を出力する（新規タブ／パネル用）。
set -eu
. "$ZV_PLUGIN_DIR/lib.sh"

ERR_MODE=text
enter_workspace
require_git

base=$(base_branch)
if [ -z "$base" ]; then
	die "比較元ブランチを判定できません。プラグイン設定の「比較元ブランチ」を指定してください。"
fi
if ! git rev-parse --verify --quiet "$base" >/dev/null; then
	die "比較元ブランチ「${base}」が存在しません。プラグイン設定を見直してください。"
fi

tab=$(printf '\t')
detail=${ZV_CFG_DETAIL:-true}

printf '# 成果比較\n\n'
printf -- '- 比較元: `%s`\n' "$base"
printf -- '- 作成日時: %s\n\n' "$(date '+%Y-%m-%d %H:%M:%S')"

rows=""
details=""
n=0

while IFS="$tab" read -r p b; do
	if [ -z "$p" ] || [ -z "$b" ]; then continue; fi
	if [ "$b" = "$base" ]; then continue; fi

	sums=$(git diff --numstat "$base...$b" 2>/dev/null | awk '
		{ if ($1 != "-") a += $1; if ($2 != "-") d += $2; f++ }
		END { printf "%d %d %d", f + 0, a + 0, d + 0 }
	')
	# shellcheck disable=SC2086
	set -- $sums
	files=$1
	add=$2
	del=$3

	commits=$(git rev-list --count "$base..$b" 2>/dev/null || echo 0)
	dirty=$(git -C "$p" status --porcelain 2>/dev/null | wc -l | tr -d ' ')
	last=$(git log -1 --pretty='%h %s' "$b" 2>/dev/null || true)
	[ -n "$last" ] || last="コミットなし"

	n=$((n + 1))
	rows="$rows| \`$b\` | $files | +$add | -$del | $commits | $dirty |
"
	details="$details
## \`$b\`

- ワークツリー: \`$p\`
- 最新コミット: $last
- 変更 $files ファイル / +$add / -$del / コミット $commits 件 / 未コミット $dirty 件
"
	if [ "$detail" = "true" ] && [ "$files" != "0" ]; then
		flist=$(git diff --numstat "$base...$b" 2>/dev/null | head -n 20 |
			awk '{ printf "- `%s` (+%s / -%s)\n", $3, $1, $2 }')
		details="$details
$flist
"
		if [ "$files" -gt 20 ]; then
			details="$details- ほか $((files - 20)) ファイル
"
		fi
	fi
done <<EOF
$(worktree_pairs)
EOF

if [ "$n" -eq 0 ]; then
	printf '比較できるブランチがありません。「ワークツリーを作成」で作業用のワークツリーを用意してください。\n'
	exit 0
fi

printf '| ブランチ | 変更 | 追加 | 削除 | コミット | 未コミット |\n'
printf '|---|---:|---:|---:|---:|---:|\n'
printf '%s' "$rows"
printf '%s\n' "$details"
printf -- '---\n\n採用するブランチ名を選択して「採用して取り込む」を実行してください。\n'
