#!/bin/sh
# 「実行」パネルの本文を標準出力へ書き出す (パネルの run は stdout をそのまま表示する)。
set -eu

DIR="${ZV_PLUGIN_DIR:-$(dirname "$0")/..}"
exec sh "$DIR/scripts/render.sh"
