#!/bin/sh
# 画面の切り取りと、前面タブへ JavaScript を実行できるブラウザの検出をまとめる。
# common.sh を読み込んだ後に . で読み込むこと。

# 範囲を選ばせて PNG を保存する。保存できたらパスを、中止されたら空を返す。
zv_shot() {
  if ! zv_have screencapture; then
    printf ''
    return 0
  fi
  mkdir -p "$ZV_PLUGIN_DATA/shots"
  _png="$ZV_PLUGIN_DATA/shots/$(date +%Y%m%d-%H%M%S)-$$.png"
  screencapture -i -x "$_png" >/dev/null 2>&1 || true
  if [ -s "$_png" ]; then
    printf '%s' "$_png"
  else
    rm -f "$_png"
    printf ''
  fi
}

# 前面タブに JavaScript を実行できる、起動中のブラウザのアプリ名を返す。
# 設定 → キャッシュ → 自動検出 の順で決める。見つからなければ空。
zv_browser_app() {
  if [ -n "${ZV_CFG_BROWSER_APP:-}" ]; then
    printf '%s' "$ZV_CFG_BROWSER_APP"
    return 0
  fi
  _cache="$ZV_PLUGIN_DATA/browser-app.txt"
  if [ -s "$_cache" ]; then
    cat "$_cache"
    return 0
  fi
  _name=$(osascript "$DIR/scripts/find-browser.applescript" 2>/dev/null || true)
  _name=$(printf '%s' "$_name" | tr -d '\r\n')
  if [ -n "$_name" ]; then
    printf '%s' "$_name" >"$_cache"
  fi
  printf '%s' "$_name"
}

# osascript のエラー出力から、よくある失敗の原因を日本語で説明する。
zv_applescript_hint() {
  case "$1" in
    *-2700*|*avaScript*|*JAVASCRIPT*)
      printf '%s' "ブラウザ側で Apple Events からの JavaScript 実行が無効になっています。ブラウザのメニュー「表示 > 開発 / デベロッパー > Apple Events からの JavaScript を許可」を有効にしてから、もう一度お試しください。有効にできない場合は「要素情報を貼り付け」をお使いください。"
      ;;
    *-2741*|*-1708*)
      printf '%s' "このアプリでは前面タブへの JavaScript 実行に対応していません。設定「ブラウザのアプリ名」に、対応するブラウザの名前を指定してください。"
      ;;
    *-1743*|*not\ allowed*|*許可*)
      printf '%s' "自動操作の許可がありません。システム設定 > プライバシーとセキュリティ > オートメーション で、このアプリからブラウザの操作を許可してください。"
      ;;
    *)
      printf '%s' "ブラウザの操作に失敗しました。ブラウザが起動していてウィンドウが開いているか確認してください。詳細: $(printf '%s' "$1" | tr '\n' ' ' | cut -c1-200)"
      ;;
  esac
}
