#!/bin/sh
# 共通ヘルパ。JSON Lines のアクション出力と依存コマンドの確認をまとめる。
# 各スクリプトの先頭から . (ドット) で読み込んで使う。

if ! command -v python3 >/dev/null 2>&1; then
  printf '%s\n' '{"action":"notify","level":"error","message":"python3 が見つかりません。この機能には python3 が必要です。"}'
  exit 0
fi

# zv_emit キー 値 [キー 値 ...]
# 値が @@ で始まる場合は、続くパスのファイル内容を読み込んで値にする。
zv_emit() {
  python3 - "$@" <<'ZVPY'
import json, sys

args = sys.argv[1:]
obj = {}
for i in range(0, len(args) - 1, 2):
    key, val = args[i], args[i + 1]
    if val.startswith("@@"):
        with open(val[2:], "r", encoding="utf-8", errors="replace") as fh:
            val = fh.read()
    if key == "submit":
        obj[key] = val.lower() in ("1", "true", "yes")
    elif key in ("line", "column"):
        obj[key] = int(val)
    else:
        obj[key] = val
sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
ZVPY
}

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
