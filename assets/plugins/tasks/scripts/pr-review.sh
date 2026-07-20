#!/bin/sh
# PR にレビュー依頼のコメントを投稿する。
# 本文は選択テキストがあればそれを、無ければ設定「レビュー依頼の本文」を使う。
set -eu

DIR="${ZV_PLUGIN_DIR:-$(dirname "$0")/..}"
. "$DIR/scripts/common.sh"
. "$DIR/scripts/gh-common.sh"

zv_data
zv_gh_ready

SEL="${ZV_SELECTION:-}"
NUM=$(printf '%s' "$SEL" | tr -cd '0-9' | head -c 12)
BODY="${ZV_CFG_REVIEW_MESSAGE:-レビューをお願いします。}"

# 選択が数字だけなら PR 番号として扱う。文字を含むなら本文として扱う。
REST=$(printf '%s' "$SEL" | tr -d '0-9[:space:]')
if [ -n "$REST" ]; then
  BODY="$SEL"
  NUM=''
fi

ERR="$ZV_PLUGIN_DATA/pr-review.err"
if [ -n "$NUM" ]; then
  gh pr comment "$NUM" --body "$BODY" >/dev/null 2>"$ERR" \
    || zv_fail "PR #$NUM にコメントできませんでした: $(head -c 300 "$ERR" | tr '\n' ' ')"
  zv_notify info "PR #$NUM にレビュー依頼を投稿しました。"
else
  gh pr comment --body "$BODY" >/dev/null 2>"$ERR" \
    || zv_fail "現在のブランチに対応する PR が見つかりません。PR 番号を選択してから実行してください。"
  zv_notify info "現在のブランチの PR にレビュー依頼を投稿しました。"
fi
