#!/bin/sh
# 「使用量」パネルの本文を標準出力へ書き出す (パネルの run は stdout をそのまま表示する)。
set -eu

DIR="${ZV_PLUGIN_DIR:-$(dirname "$0")/..}"
. "$DIR/scripts/common.sh"

"$ZV_ZAI" plugin usage-scan "${ZV_CFG_EXTRA_DIRS:-}" 2>/dev/null || {
  printf '%s\n\n%s\n' "## 使用量の目安" "集計に失敗しました: **取得不可**"
}
