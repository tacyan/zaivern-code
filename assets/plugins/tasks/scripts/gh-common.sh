#!/bin/sh
# gh CLI の存在確認・認証確認と、一覧を Markdown へ整形する処理をまとめる。
# common.sh を読み込んだ後に . で読み込むこと。

# gh が使えない場合は日本語で理由を通知して終了する。
zv_gh_ready() {
  if ! zv_have gh; then
    zv_fail "gh コマンドが見つかりません。GitHub CLI を導入し、ターミナルで 'gh auth login' を実行してください。"
  fi
  if ! gh auth status >/dev/null 2>&1; then
    zv_fail "GitHub の認証が済んでいません。ターミナルで 'gh auth login' を実行してください。"
  fi
  if ! git -C "${ZV_WORKSPACE:-.}" rev-parse --git-dir >/dev/null 2>&1; then
    zv_fail "このワークスペースは git リポジトリではありません。課題や PR を取得できません。"
  fi
}

zv_limit() {
  case "${ZV_CFG_LIST_LIMIT:-}" in
    ''|*[!0-9]*) printf '20' ;;
    *) printf '%s' "$ZV_CFG_LIST_LIMIT" ;;
  esac
}

# gh が出した JSON 配列のファイルを Markdown 表へ整形する。
# 引数1: 見出し, 引数2: 種別 (pr | issue), 引数3: JSON ファイルのパス
zv_render() {
  python3 - "$1" "$2" "$3" <<'ZVPY'
import json, sys

heading, kind, path = sys.argv[1], sys.argv[2], sys.argv[3]
try:
    with open(path, "r", encoding="utf-8") as fh:
        rows = json.load(fh)
except (OSError, json.JSONDecodeError):
    rows = []
if not isinstance(rows, list):
    rows = []

out = ["## " + heading, ""]
if not rows:
    out.append("該当する項目はありません。")
else:
    for row in rows:
        num = row.get("number", "?")
        title = (row.get("title") or "").replace("|", "\\|").strip()
        author = (row.get("author") or {}).get("login") or "不明"
        labels = ", ".join(l.get("name", "") for l in (row.get("labels") or []))
        state = row.get("state") or ""
        out.append("- **#%s** %s" % (num, title))
        detail = ["作成者 @%s" % author]
        if state:
            detail.append("状態 %s" % state)
        if labels:
            detail.append("ラベル %s" % labels)
        if kind == "pr" and row.get("headRefName"):
            detail.append("ブランチ %s" % row["headRefName"])
        out.append("  - " + " / ".join(detail))
    out.append("")
    out.append("計 %d 件" % len(rows))
sys.stdout.write("\n".join(out) + "\n")
ZVPY
}
