#!/bin/sh
# ブラウザの前面タブに選択オーバーレイを差し込み、クリックされた要素の
# セレクタ・HTML・主要 CSS・矩形を取り出して、切り取り画像とともに
# エージェントのプロンプトへ渡す。新しいバイナリや埋め込みブラウザは使わない。
set -eu

DIR="${ZV_PLUGIN_DIR:-$(dirname "$0")/..}"
. "$DIR/scripts/common.sh"
. "$DIR/scripts/capture-common.sh"

zv_data

if [ "$(uname -s)" != "Darwin" ]; then
  zv_fail "要素の取り込みは macOS でのみ利用できます。「要素情報を貼り付け」をお使いください。"
fi
zv_have osascript || zv_fail "osascript が見つかりません。「要素情報を貼り付け」をお使いください。"

APP=$(zv_browser_app)
if [ -z "$APP" ]; then
  zv_fail "前面タブを操作できるブラウザが見つかりませんでした。ブラウザを起動してページを開いてから実行するか、設定「ブラウザのアプリ名」を指定してください。"
fi

JS="$DIR/scripts/picker.js"
POLLJS="$DIR/scripts/poll.js"
[ -f "$JS" ] && [ -f "$POLLJS" ] \
  || zv_fail "選択用のスクリプトが見つかりません。プラグインを再展開してください。"

INJECT="$ZV_PLUGIN_DATA/inject.applescript"
POLL="$ZV_PLUGIN_DATA/poll.applescript"
ERR="$ZV_PLUGIN_DATA/osascript.err"

cat >"$INJECT" <<APPLESCRIPT
set jsSource to (do shell script "cat " & quoted form of "$JS")
tell application "$APP"
	activate
	execute javascript jsSource in active tab of front window
end tell
APPLESCRIPT

# JavaScript は必ず変数に入れてから渡す。文字列リテラルを直接書くと
# AppleScript 側で構文を解釈できず失敗する。
cat >"$POLL" <<APPLESCRIPT
set jsSource to (do shell script "cat " & quoted form of "$POLLJS")
tell application "$APP"
	execute javascript jsSource in active tab of front window
end tell
APPLESCRIPT

if ! osascript "$INJECT" >/dev/null 2>"$ERR"; then
  zv_fail "$(zv_applescript_hint "$(cat "$ERR")")"
fi

zv_emit action set_status text "ブラウザで要素をクリックしてください (Esc で中止)"

PICK="$ZV_PLUGIN_DATA/pick.json"
TIMEOUT="${ZV_CFG_PICK_TIMEOUT_SECS:-60}"
case "$TIMEOUT" in ''|*[!0-9]*) TIMEOUT=60 ;; esac

i=0
: >"$PICK"
while [ "$i" -lt "$TIMEOUT" ]; do
  RESULT=$(osascript "$POLL" 2>"$ERR" || true)
  if [ -s "$ERR" ] && [ -z "$RESULT" ]; then
    zv_fail "$(zv_applescript_hint "$(cat "$ERR")")"
  fi
  if [ -n "$RESULT" ]; then
    printf '%s' "$RESULT" >"$PICK"
    break
  fi
  sleep 1
  i=$((i + 1))
done

if [ ! -s "$PICK" ]; then
  zv_fail "時間内に要素が選択されませんでした (${TIMEOUT} 秒)。もう一度お試しください。"
fi

if grep -q '"cancelled"' "$PICK"; then
  zv_notify info "要素の選択を中止しました。"
  exit 0
fi

zv_emit action set_status text "切り取る範囲をドラッグしてください (不要なら Esc)"
SHOT=$(zv_shot)

PROMPT="$ZV_PLUGIN_DATA/prompt.txt"
python3 "$DIR/scripts/build-prompt.py" "$PICK" "$SHOT" >"$PROMPT"

zv_emit action agent_prompt agent "${ZV_AGENT:-}" text "@@$PROMPT" submit false
zv_emit action set_status text "要素の情報をプロンプトへ入れました"
