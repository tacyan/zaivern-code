#!/bin/sh
# 共通ヘルパ。JSON Lines のアクション出力と依存コマンドの確認をまとめる。
# 各スクリプトの先頭から . (ドット) で読み込んで使う。
#
# JSON の組み立ては zai 本体 (`zai plugin emit`) に任せる。以前は python3 で
# 組んでいたが、Windows では Microsoft Store の「アプリ実行エイリアス」が
# python3 として PATH に居座るため、`command -v python3` は成功するのに
# 実行すると `Python` の 1 行を出して exit 49 で死ぬ — 存在確認を通り抜けて
# 失敗する壊れ方だった。zai は今このスクリプトを動かしている本体なので、
# 「在るか分からない別の実行環境」を増やさずに済む。

# zai 本体。ZV_BIN は仕様 3 章の環境変数 (実行中の zai の実体)。
ZV_ZAI="${ZV_BIN:-zai}"
if ! command -v "$ZV_ZAI" >/dev/null 2>&1; then
  printf '%s\n' '{"action":"notify","level":"error","message":"zai 本体が見つかりません。プラグインは Zaivern から実行してください。"}'
  exit 0
fi

# zv_emit キー 値 [キー 値 ...]
# 値が @@ で始まる場合は、続くパスのファイル内容を値にする。
zv_emit() { "$ZV_ZAI" plugin emit "$@"; }

# zv_notify レベル メッセージ
zv_notify() { zv_emit action notify level "$1" message "$2"; }

# 失敗時: 日本語のエラー通知を出して正常終了する (アプリ側を落とさない)
zv_fail() { zv_notify error "$1"; exit 0; }

zv_have() { command -v "$1" >/dev/null 2>&1; }

# 書き込み先を用意する。$ZV_PLUGIN_DATA が無い場合のみ一時領域へ退避する。
zv_data() {
  : "${ZV_PLUGIN_DATA:=${TMPDIR:-/tmp}/zaivern-plugin-data}"
  mkdir -p "$ZV_PLUGIN_DATA"
}
