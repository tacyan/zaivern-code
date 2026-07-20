#!/bin/sh
# プロジェクトの種類に応じたコマンドをターミナルで実行する。
# 使い方: run.sh <test|build|fmt|script>
#   script のときは、選択テキストか設定「既定のスクリプト名」を実行対象にする。
set -eu

DIR="${ZV_PLUGIN_DIR:-$(dirname "$0")/..}"
. "$DIR/scripts/common.sh"
. "$DIR/scripts/detect.sh"

WHAT="${1:-script}"
zv_detect

if [ -z "$ZV_KIND" ]; then
  zv_fail "$(zv_undetected_msg)"
fi

WS="${ZV_WORKSPACE:-$PWD}"

if [ "$WHAT" = "script" ]; then
  NAME=$(printf '%s' "${ZV_SELECTION:-}" | tr -d '[:space:]')
  [ -n "$NAME" ] || NAME=$(printf '%s' "${ZV_CFG_DEFAULT_SCRIPT:-}" | tr -d '[:space:]')
  if [ -z "$NAME" ]; then
    LIST=$(zv_scripts | head -20 | tr '\n' ' ')
    if [ -n "$LIST" ]; then
      zv_fail "実行するスクリプト名を選択してから実行するか、設定「既定のスクリプト名」を入力してください。候補: $LIST"
    fi
    zv_fail "実行できるスクリプトが見つかりませんでした。"
  fi
  if ! zv_scripts | grep -qx -- "$NAME"; then
    zv_fail "スクリプト「$NAME」は見つかりませんでした。候補: $(zv_scripts | head -20 | tr '\n' ' ')"
  fi
  CMD=$(zv_cmd_for run "$NAME") || zv_fail "スクリプト「$NAME」の実行方法を決められませんでした。"
else
  case "$WHAT" in
    test) LABEL="テスト" ;;
    build) LABEL="ビルド" ;;
    fmt) LABEL="整形" ;;
    *) LABEL="$WHAT" ;;
  esac
  CMD=$(zv_cmd_for "$WHAT" "") || zv_fail "$ZV_LABEL では「$LABEL」に対応するコマンドが見つかりませんでした。"
fi

zv_emit action run_terminal command "$CMD" cwd "$WS"
zv_emit action set_status text "実行: $CMD"
