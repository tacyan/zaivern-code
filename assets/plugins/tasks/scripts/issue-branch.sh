#!/bin/sh
# 課題番号から作業ブランチと worktree を作り、そこでターミナルを開くアクションを出す。
# 番号は選択テキスト ($ZV_SELECTION) を優先し、無ければ設定「既定の課題番号」を使う。
set -eu

DIR="${ZV_PLUGIN_DIR:-$(dirname "$0")/..}"
. "$DIR/scripts/common.sh"
. "$DIR/scripts/gh-common.sh"

zv_data
zv_gh_ready

RAW="${ZV_SELECTION:-}"
[ -n "$RAW" ] || RAW="${ZV_CFG_DEFAULT_ISSUE:-}"
NUM=$(printf '%s' "$RAW" | tr -cd '0-9' | head -c 12)

if [ -z "$NUM" ]; then
  zv_fail "課題番号が指定されていません。番号を選択してから実行するか、設定「既定の課題番号」を入力してください。"
fi

META="$ZV_PLUGIN_DATA/issue-$NUM.json"
if ! gh issue view "$NUM" --json number,title >"$META" 2>"$ZV_PLUGIN_DATA/issue-view.err"; then
  zv_fail "課題 #$NUM を取得できませんでした: $(head -c 300 "$ZV_PLUGIN_DATA/issue-view.err" | tr '\n' ' ')"
fi

PREFIX="${ZV_CFG_BRANCH_PREFIX:-work}"
SLUG=$(python3 - "$META" <<'ZVPY'
import json, re, sys

with open(sys.argv[1], "r", encoding="utf-8") as fh:
    data = json.load(fh)
title = (data.get("title") or "").lower()
slug = re.sub(r"[^a-z0-9]+", "-", title).strip("-")[:40].strip("-")
sys.stdout.write(slug or "issue")
ZVPY
)

BRANCH="$PREFIX/$NUM-$SLUG"
WS="${ZV_WORKSPACE:-$PWD}"
WT="$ZV_PLUGIN_DATA/worktrees/$NUM-$SLUG"

if [ -d "$WT" ]; then
  zv_emit action run_terminal command "git status --short --branch" cwd "$WT"
  zv_notify info "既存の作業ツリーを開きました: $WT"
  exit 0
fi

mkdir -p "$ZV_PLUGIN_DATA/worktrees"
if git -C "$WS" show-ref --verify --quiet "refs/heads/$BRANCH"; then
  ADD_ERR=$(git -C "$WS" worktree add "$WT" "$BRANCH" 2>&1) || {
    zv_fail "作業ツリーを作成できませんでした: $(printf '%s' "$ADD_ERR" | tr '\n' ' ' | head -c 300)"
  }
else
  ADD_ERR=$(git -C "$WS" worktree add -b "$BRANCH" "$WT" 2>&1) || {
    zv_fail "作業ツリーを作成できませんでした: $(printf '%s' "$ADD_ERR" | tr '\n' ' ' | head -c 300)"
  }
fi

zv_emit action run_terminal command "git status --short --branch" cwd "$WT"
zv_emit action set_status text "ブランチ $BRANCH を作成しました"
zv_notify info "課題 #$NUM 用にブランチ $BRANCH と作業ツリー $WT を作成しました。"
