#!/bin/sh
# 「使用量」パネルの本文を標準出力へ書き出す (パネルの run は stdout をそのまま表示する)。
set -eu

DIR="${ZV_PLUGIN_DIR:-$(dirname "$0")/..}"

if ! command -v python3 >/dev/null 2>&1; then
  printf '%s\n\n%s\n' "## 使用量の目安" "python3 が見つからないため集計できません: **取得不可**"
  exit 0
fi

python3 "$DIR/scripts/scan.py" "${ZV_CFG_EXTRA_DIRS:-}" 2>/dev/null || {
  printf '%s\n\n%s\n' "## 使用量の目安" "集計に失敗しました: **取得不可**"
}
