#!/usr/bin/env sh
# `tools/verify.sh` の**判定そのもの**を検査する。
#
# ## なぜ要るか
#
# verify.sh は「テストを走らせたか」を人へ報告する道具なので、ここが嘘を
# つくと**全部の緑が信用できなくなる**。実際に嘘をついていた:
# 変更ファイルの basename だけをフィルタにしていたため、
# `src/team/panel.rs` を触ると `panel::` が渡り、**`context::panel` の
# テストが走っただけで緑**になっていた (team の panel が 0 件でも)。
#
# cargo を実際に回すと遅いうえ、リポジトリの作業ツリーに結果が左右される
# (何を触っているかで走るテストが変わる)。そこで:
#
#   * **偽の cargo を PATH の先頭に置く** — canned な `--list` と結果を返す
#   * **使い捨ての git リポジトリ**を作り、そこへ verify.sh を写す
#     (verify.sh は `cd "$(dirname "$0")/.."` するので、写した先が根になる)
#
# これで「どのフィルタで cargo を呼んだか」「終了コード」「一時ファイルの
# 後始末」を**決定的に**確かめられる。
#
# 使い方: tools/verify-selftest.sh
set -eu
_LABEL='verify.sh の自己検査'
_verdict() {
    _rc=$?
    [ -n "${WORK:-}" ] && rm -rf "$WORK"
    if [ "$_rc" -eq 0 ]; then
        printf '\033[1;32m✓ %s 緑\033[0m\n' "$_LABEL"
    else
        printf '\033[1;31m✗ %s 赤 (rc=%s)%s\033[0m\n' \
            "$_LABEL" "$_rc" "${_WHY:+ — $_WHY}"
    fi
}
_WHY=''
trap _verdict EXIT
cd "$(dirname "$0")/.."
REPO=$(pwd)

command -v git >/dev/null 2>&1 || { _WHY='git がありません'; exit 1; }

WORK=$(mktemp -d "${TMPDIR:-/tmp}/zv-verify-selftest.XXXXXX")
# 一時ファイルの後始末を見るため、verify.sh の TMPDIR は**専用の空フォルダ**へ
# 向ける (共有の /tmp だと他プロセスの残骸と区別が付かない)。
TMPD="$WORK/tmp"
BIN="$WORK/bin"
FIX="$WORK/fixture"
mkdir -p "$TMPD" "$BIN" "$FIX/tools" "$FIX/src/team" "$FIX/src/context"

# ── 偽の cargo ────────────────────────────────────────────────────────
#
# `--list` は仕込んだ一覧をそのまま返す。フィルタ付きの `test` は、その
# 一覧の**部分一致**で件数を数えて `running N tests` を出す (本物と同じ
# 意味論)。呼ばれたフィルタは `calls` へ 1 行ずつ残す。
cat > "$BIN/cargo" <<'FAKE'
#!/usr/bin/env sh
sub=${1:-}
case "$sub" in
  fmt|clippy|build) exit 0 ;;
  test) ;;
  *) exit 0 ;;
esac
shift
# 素通しする呼び出し (コンパイルだけ / 一覧) を先に見分ける。**記録しない**。
for a in "$@"; do
  case "$a" in
    --no-run) echo '    Finished `test` profile'; exit 0 ;;
    --list) cat "$ZV_FAKE_LIST"; exit 0 ;;
  esac
done
# ここまで来たら実行。フィルタは `--bin zai` を除いた最初の引数 (無ければ全件)。
filter=''
skip=0
for a in "$@"; do
  if [ "$skip" = 1 ]; then skip=0; continue; fi
  case "$a" in
    --bin) skip=1 ;;
    --*) ;;
    *) [ -z "$filter" ] && filter=$a ;;
  esac
done
printf '%s\n' "${filter:-<all>}" >> "$ZV_FAKE_CALLS"
if [ -n "${ZV_FAKE_FAIL:-}" ] && [ "$ZV_FAKE_FAIL" = "$filter" ]; then
  echo 'error: 仕込んだ失敗'
  exit 101
fi
if [ "${ZV_FAKE_SILENT:-0}" = 1 ]; then
  echo 'test result: ok.'
  exit 0
fi
if [ -z "$filter" ]; then
  n=$(grep -c ': test$' "$ZV_FAKE_LIST" || true)
else
  n=$(grep ': test$' "$ZV_FAKE_LIST" | grep -c -- "$filter" || true)
fi
echo "running $n tests"
echo 'test result: ok.'
exit 0
FAKE
chmod +x "$BIN/cargo"

# ── 使い捨てのリポジトリ ──────────────────────────────────────────────
cp "$REPO/tools/verify.sh" "$FIX/tools/verify.sh"
chmod +x "$FIX/tools/verify.sh"
: > "$FIX/src/team/panel.rs"
: > "$FIX/src/context/panel.rs"
: > "$FIX/src/team/mod.rs"
: > "$FIX/src/nothing.rs"
( cd "$FIX" \
  && git init -q . \
  && git config user.email zv@example.invalid \
  && git config user.name zv \
  && git add -A \
  && git commit -qm base ) >/dev/null 2>&1 \
  || { _WHY='使い捨てリポジトリを作れません'; exit 1; }

# **実際のテスト名の形**をそのまま置く。`features::…::imp::…` の途中要素と、
# basename が同じ別モジュール (`context::panel`) が肝。
cat > "$WORK/list" <<'LIST'
features::team::imp::panel::tests::runを閉じると全セッションの停止が実行側へ届く: test
features::team::imp::panel::tests::outbox_delivery::改名で公開した報告は一度だけ取り込む: test
context::panel::tests::幅で畳む: test
context::panel::tests::空でも落ちない: test
app::team_glue::tests::配線: test
3 tests, 0 benchmarks
LIST

export ZV_FAKE_LIST="$WORK/list"

# `$1` を変更済みにして verify.sh を回す。出力は `$WORK/out`、cargo に渡った
# フィルタは `$WORK/calls`。戻りは verify.sh の終了コード。
run_verify() {
  : > "$WORK/calls"
  ( cd "$FIX" && printf 'touched\n' > "$1" )
  set +e
  ( cd "$FIX" \
    && PATH="$BIN:$PATH" TMPDIR="$TMPD" ZV_FAKE_CALLS="$WORK/calls" \
       ZV_FAKE_FAIL="${FAKE_FAIL:-}" ZV_FAKE_SILENT="${FAKE_SILENT:-0}" \
       sh tools/verify.sh ) > "$WORK/out" 2>&1
  _rc=$?
  set -e
  ( cd "$FIX" && git checkout -q -- . )
  return $_rc
}

ok=0
ng=0
check() { # check <説明> <条件の真偽 (0/1)>
  if [ "$2" = 0 ]; then
    ok=$((ok + 1))
    printf '  \033[1;32m✓\033[0m %s\n' "$1"
  else
    ng=$((ng + 1))
    printf '  \033[1;31m✗ %s\033[0m\n' "$1"
    sed -e 's/^/      /' "$WORK/out" | tail -25
  fi
}
has_call() { grep -qx -- "$1" "$WORK/calls"; }

# ── 0. この検査が空振りしないことの証明 ────────────────────────────────
#
# **わざと壊して赤になることを確かめる。** 検査を足しただけで「守れている」
# と思い込まないための段 (この repo で 3 版続けて空回りした前科がある)。
# 旧実装 (basename をフィルタにする) を作り直して回し、下の検査が実際に
# それを捕まえることを見る。
make_old() {
  # 対象の決め方だけを旧実装へ差し替える (前半はそのまま使う)。
  awk '/^# 引数が無ければ/ { exit } { print }' "$REPO/tools/verify.sh" > "$FIX/tools/verify-old.sh"
  cat >> "$FIX/tools/verify-old.sh" <<'OLD'
MODS=$(git status --porcelain -- 'src/*.rs' 2>/dev/null | awk '{print $NF}' \
       | sed -e 's|.*/||' -e 's|\.rs$||' | sort -u)
[ -n "$MODS" ] || exit 0
_ran_total=0
for m in $MODS; do
  step "テスト: ${m}::"
  ( cargo test --bin zai "${m}::" 2>&1; echo $? > "$_ZV_RC" ) | tee "$_ZV_LOG"
  [ "$(cat "$_ZV_RC")" = 0 ] || exit 1
  _n=$(sed -n 's/^running \([0-9]*\) tests*$/\1/p' "$_ZV_LOG" | head -1)
  _ran_total=$((_ran_total + ${_n:-0}))
done
[ "$_ran_total" != 0 ] || exit 1
OLD
  chmod +x "$FIX/tools/verify-old.sh"
}

printf '\n▸ 0. 旧実装 (basename) を、この検査が捕まえる\n'
make_old
: > "$WORK/calls"
( cd "$FIX" && printf 'touched\n' > src/team/panel.rs )
set +e
( cd "$FIX" && PATH="$BIN:$PATH" TMPDIR="$TMPD" ZV_FAKE_CALLS="$WORK/calls" \
    ZV_FAKE_SILENT=0 sh tools/verify-old.sh ) > "$WORK/out" 2>&1
old_rc=$?
set -e
( cd "$FIX" && git checkout -q -- . )
check '旧実装は basename の panel:: を渡す (＝別モジュールに当たる)' \
  "$(grep -qx 'panel::' "$WORK/calls" && echo 0 || echo 1)"
check '旧実装はそれでも緑を出す (＝これが直したかった嘘)' \
  "$([ "$old_rc" = 0 ] && echo 0 || echo 1)"

printf '\n▸ 1. 別モジュールの同名テストで緑にしない\n'
run_verify src/team/panel.rs && rc=0 || rc=$?
check 'src/team/panel.rs は features::team::imp::panel:: を走らせる' \
  "$(has_call 'features::team::imp::panel::' && echo 0 || echo 1)"
check 'basename の panel:: を渡さない (context::panel まで巻き込まない)' \
  "$(grep -qx 'panel::' "$WORK/calls" && echo 1 || echo 0)"
check 'context::panel:: は走らせない' \
  "$(has_call 'context::panel::' && echo 1 || echo 0)"
check '成功で終わる' "$([ "$rc" = 0 ] && echo 0 || echo 1)"

printf '\n▸ 2. 同じ basename の別モジュールを区別する\n'
run_verify src/context/panel.rs && rc=0 || rc=$?
check 'src/context/panel.rs は context::panel:: を走らせる' \
  "$(has_call 'context::panel::' && echo 0 || echo 1)"
check 'team 側は走らせない' \
  "$(has_call 'features::team::imp::panel::' && echo 1 || echo 0)"
check '成功で終わる' "$([ "$rc" = 0 ] && echo 0 || echo 1)"

printf '\n▸ 3. 対象テスト 0 件を検出し、安全側へ倒れる\n'
run_verify src/nothing.rs && rc=0 || rc=$?
check 'テストを持たないファイルとして名指しする' \
  "$(grep -q 'テストを持たない変更ファイル' "$WORK/out" && echo 0 || echo 1)"
check '一部だけ走らせて緑にせず、全テストへ倒れる' \
  "$(has_call '<all>' && echo 0 || echo 1)"
check '成功で終わる (全テストが緑なので)' "$([ "$rc" = 0 ] && echo 0 || echo 1)"

printf '\n▸ 4. mod.rs はその下の一族をまとめて走らせる\n'
run_verify src/team/mod.rs && rc=0 || rc=$?
check 'src/team/mod.rs は features::team:: を走らせる (部分木ごと)' \
  "$(has_call 'features::team::' && echo 0 || echo 1)"
check '無関係な context::panel:: は走らせない' \
  "$(has_call 'context::panel::' && echo 1 || echo 0)"
check '成功で終わる' "$([ "$rc" = 0 ] && echo 0 || echo 1)"

printf '\n▸ 5. cargo が落ちたらスクリプトも落ちる\n'
FAKE_FAIL='features::team::imp::panel::'
run_verify src/team/panel.rs && rc=0 || rc=$?
FAKE_FAIL=''
check '非 0 で終わる' "$([ "$rc" != 0 ] && echo 0 || echo 1)"
check '判定行に赤が出る' \
  "$(grep -q '✗ .* 赤' "$WORK/out" && echo 0 || echo 1)"

printf '\n▸ 6. 件数を読めなければ緑にしない\n'
FAKE_SILENT=1
run_verify src/team/panel.rs && rc=0 || rc=$?
FAKE_SILENT=0
check '非 0 で終わる (走ったか確かめられないので)' \
  "$([ "$rc" != 0 ] && echo 0 || echo 1)"
check '理由を出す' \
  "$(grep -q '件数を読めない' "$WORK/out" && echo 0 || echo 1)"

printf '\n▸ 7. 一時ファイルを残さない (成功・失敗の両方)\n'
run_verify src/team/panel.rs || true
left_ok=$(find "$TMPD" -name 'zv-verify-*' 2>/dev/null | wc -l | tr -d ' ')
FAKE_FAIL='features::team::imp::panel::'
run_verify src/team/panel.rs || true
FAKE_FAIL=''
left_ng=$(find "$TMPD" -name 'zv-verify-*' 2>/dev/null | wc -l | tr -d ' ')
check "成功のあとに残らない (残 $left_ok)" "$([ "$left_ok" = 0 ] && echo 0 || echo 1)"
check "失敗のあとに残らない (残 $left_ng)" "$([ "$left_ng" = 0 ] && echo 0 || echo 1)"

printf '\n%s 件成功 / %s 件失敗\n' "$ok" "$ng"
if [ "$ng" != 0 ]; then
  _WHY="$ng 件の検査が落ちた"
  exit 1
fi
