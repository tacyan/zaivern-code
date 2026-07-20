#!/bin/sh
# プロジェクトの種類を判定し、テスト/ビルド/整形のコマンドを決める処理をまとめる。
# common.sh を読み込んだ後に . で読み込むこと。
#
# zv_detect を呼ぶと以下を設定する:
#   ZV_KIND  … rust | node | make | python | ""
#   ZV_LABEL … 画面表示用の名前
#   ZV_PM    … node のときのパッケージ実行コマンド

ZV_KIND=""
ZV_LABEL=""
ZV_PM=""

zv_detect() {
  _ws="${ZV_WORKSPACE:-$PWD}"
  if [ -f "$_ws/Cargo.toml" ]; then
    ZV_KIND="rust"
    ZV_LABEL="Rust プロジェクト (Cargo.toml)"
  elif [ -f "$_ws/package.json" ]; then
    ZV_KIND="node"
    ZV_LABEL="Node プロジェクト (package.json)"
    if [ -f "$_ws/pnpm-lock.yaml" ] && zv_have pnpm; then
      ZV_PM="pnpm"
    elif [ -f "$_ws/yarn.lock" ] && zv_have yarn; then
      ZV_PM="yarn"
    elif [ -f "$_ws/bun.lockb" ] && zv_have bun; then
      ZV_PM="bun"
    else
      ZV_PM="npm"
    fi
  elif [ -f "$_ws/pyproject.toml" ]; then
    ZV_KIND="python"
    ZV_LABEL="Python プロジェクト (pyproject.toml)"
  elif [ -f "$_ws/Makefile" ] || [ -f "$_ws/makefile" ]; then
    ZV_KIND="make"
    ZV_LABEL="Makefile プロジェクト"
  else
    ZV_KIND=""
    ZV_LABEL=""
  fi
}

# 実行できるスクリプト/ターゲットの名前を 1 行 1 件で出す。
zv_scripts() {
  _ws="${ZV_WORKSPACE:-$PWD}"
  case "$ZV_KIND" in
    node)
      python3 - "$_ws/package.json" <<'ZVPY'
import json, sys
try:
    with open(sys.argv[1], "r", encoding="utf-8") as fh:
        data = json.load(fh)
except Exception:
    sys.exit(0)
for name in (data.get("scripts") or {}):
    print(name)
ZVPY
      ;;
    make)
      _mk="$_ws/Makefile"
      [ -f "$_mk" ] || _mk="$_ws/makefile"
      grep -E '^[a-zA-Z0-9][a-zA-Z0-9_.-]*:' "$_mk" 2>/dev/null \
        | sed 's/:.*//' | sort -u | head -40
      ;;
    rust)
      printf '%s\n' build test fmt clippy run check
      ;;
    python)
      printf '%s\n' test build fmt
      ;;
    *) : ;;
  esac
}

# zv_cmd_for <test|build|fmt|run> <引数>
# 実行すべきコマンド文字列を出す。決められない場合は何も出さずに 1 を返す。
zv_cmd_for() {
  _what="$1"
  _arg="${2:-}"
  _ws="${ZV_WORKSPACE:-$PWD}"
  case "$ZV_KIND:$_what" in
    rust:test)  printf 'cargo test' ;;
    rust:build) printf 'cargo build' ;;
    rust:fmt)   printf 'cargo fmt' ;;
    rust:run)   [ -n "$_arg" ] && printf 'cargo %s' "$_arg" || printf 'cargo run' ;;

    node:test)  printf '%s test' "$ZV_PM" ;;
    node:build) printf '%s run build' "$ZV_PM" ;;
    node:fmt)
      if zv_scripts | grep -qx 'fmt'; then printf '%s run fmt' "$ZV_PM"
      elif zv_scripts | grep -qx 'format'; then printf '%s run format' "$ZV_PM"
      elif zv_scripts | grep -qx 'lint'; then printf '%s run lint' "$ZV_PM"
      else return 1
      fi
      ;;
    node:run)   [ -n "$_arg" ] || return 1; printf '%s run %s' "$ZV_PM" "$_arg" ;;

    make:test)  zv_scripts | grep -qx 'test' || return 1; printf 'make test' ;;
    make:build)
      if zv_scripts | grep -qx 'build'; then printf 'make build'
      elif zv_scripts | grep -qx 'all'; then printf 'make all'
      else return 1
      fi
      ;;
    make:fmt)
      if zv_scripts | grep -qx 'fmt'; then printf 'make fmt'
      elif zv_scripts | grep -qx 'format'; then printf 'make format'
      else return 1
      fi
      ;;
    make:run)   [ -n "$_arg" ] || return 1; printf 'make %s' "$_arg" ;;

    python:test)
      if zv_have pytest; then printf 'pytest'
      else printf 'python3 -m pytest'
      fi
      ;;
    python:build) printf 'python3 -m build' ;;
    python:fmt)
      if zv_have ruff; then printf 'ruff format .'
      elif zv_have black; then printf 'black .'
      else return 1
      fi
      ;;
    python:run) [ -n "$_arg" ] || return 1; printf 'python3 -m %s' "$_arg" ;;

    *) return 1 ;;
  esac
  unset _what _arg _ws
  return 0
}

# 判定できなかったときの共通メッセージ
zv_undetected_msg() {
  printf '%s' "実行できるプロジェクトを判別できませんでした。ワークスペース直下に Cargo.toml / package.json / Makefile / pyproject.toml のいずれかがあるか確認してください。"
}
