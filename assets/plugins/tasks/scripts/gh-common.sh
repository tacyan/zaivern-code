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

# gh が出した JSON 配列のファイルを Markdown へ整形する。
# 引数1: 見出し, 引数2: 種別 (pr | issue), 引数3: JSON ファイルのパス
#
# JSON の読み取りは `zai plugin json ... rows` に任せ (1 行 1 件のタブ区切り)、
# 整形はシェルで行う。python3 に頼ると Windows で動かない (common.sh の説明)。
zv_render() {
  _heading="$1"; _kind="$2"; _file="$3"
  printf '## %s\n\n' "$_heading"
  _rows=$("$ZV_ZAI" plugin json "$_file" rows number title author.login "labels[].name" state headRefName)
  if [ -z "$_rows" ]; then
    printf '%s\n' "該当する項目はありません。"
    unset _heading _kind _file _rows
    return 0
  fi
  # タブのままでは read が空欄を潰す (IFS の空白文字は連続を 1 個と数えるため、
  # 作成者が空の行で状態がずれて「作成者 @MERGED」になった)。
  # 空白でない区切り (US = 0x1f) へ寄せると、空欄がそのまま空欄で届く。
  _us=$(printf '\037')
  printf '%s\n' "$_rows" | tr '\t' "$_us" | while IFS="$_us" read -r num title author labels state branch; do
    [ -n "$num" ] || continue
    # 表組みと衝突しないよう | はエスケープする
    title=$(printf '%s' "$title" | sed 's/|/\\|/g')
    printf -- '- **#%s** %s\n' "$num" "$title"
    _detail="作成者 @${author:-不明}"
    [ -n "$state" ] && _detail="$_detail / 状態 $state"
    [ -n "$labels" ] && _detail="$_detail / ラベル $labels"
    [ "$_kind" = "pr" ] && [ -n "$branch" ] && _detail="$_detail / ブランチ $branch"
    printf '  - %s\n' "$_detail"
  done
  printf '\n計 %d 件\n' "$(printf '%s\n' "$_rows" | grep -c '')"
  unset _heading _kind _file _rows _us
}
