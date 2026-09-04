#!/usr/bin/env sh
# ローカル検証を **1 回のコンパイル**で終わらせる。
#
# 使い方:
#   tools/verify.sh                 # 整形 + 警告 + 触ったモジュールのテスト
#   tools/verify.sh git:: lsp::     # モジュールを指定して走らせる
#   tools/verify.sh --all           # 全テスト (cargo test。CI の nextest とは別)
#   tools/verify.sh --quick         # 整形と警告だけ (テストは走らせない)
#   tools/verify.sh --lint          # CI と同じ clippy (**push する前に必ず**)
#   tools/verify.sh --bin           # 実バイナリ (target/debug/zai) も建てる
#
# ## なぜこのスクリプトが要るのか (実測)
#
# `cargo check` と `cargo test` は **成果物を共有しない**。check は .rmeta しか
# 作らず、test はコード生成まで要るので、両方走らせると同じクレートを 2 回
# コンパイルすることになる。実測 (warm, 1 ファイル変更):
#
#   cargo check --bin zai --all-targets   13s
#   そのあと cargo test                   18s
#   ------------------------------------------
#   合計                                  31s
#
#   cargo test --bin zai --no-run のみ    18s   ← 警告も全部ここで出る
#
# **`cargo test` は test コードも含めて全部コンパイルするので、警告の検出漏れが
# 無い。** 逆に `cargo check --bin zai` (--all-targets 無し) は `#[cfg(test)]` を
# コンパイルしないため、テストコードの警告を**取りこぼす**
# (実際に non_snake_case を 2 回見落とした)。
#
# だから検証は「test を 1 回コンパイルして、そのバイナリでテストを走らせる」に
# 一本化する。
set -eu
_LABEL='検証'

# ── 判定を「出力そのもの」へ書く ────────────────────────────────────────
#
# 呼び出し側が `| tail` / `| head` を挟むと `$?` はそちらのものになるので、
# **中止したのに rc=0** に見える (実際にこれで「docker が起動していないのに
# 緑」と誤読した)。終了コードだけを真実にしない — どの経路で終わっても
# 最後の 1 行に結果を書き、パイプ越しでも嘘にならないようにする。
_verdict() {
    _rc=$?
    rm -f "${_ZV_LOG:-}" "${_ZV_RC:-}" "${_ZV_LIST:-}"
    if [ "$_rc" -eq 0 ]; then
        printf '\033[1;32m✓ %s 緑\033[0m\n' "$_LABEL"
    else
        printf '\033[1;31m✗ %s 赤 (rc=%s)%s\033[0m\n' \
            "$_LABEL" "$_rc" "${_WHY:+ — $_WHY}"
    fi
}
# テストの控え置き場。**trap は増やさない** — `trap ... EXIT` をもう 1 つ
# 置くと `_verdict` が置き換わって、最後の判定行が出なくなる。
_ZV_LOG="${TMPDIR:-/tmp}/zv-verify-$$.log"
_ZV_RC="${TMPDIR:-/tmp}/zv-verify-$$.rc"
# `cargo test -- --list` の控え (変更ファイル → テスト名の接頭辞、の照合に使う)。
_ZV_LIST="${TMPDIR:-/tmp}/zv-verify-$$.list"
_WHY=''
trap _verdict EXIT

cd "$(dirname "$0")/.."

QUICK=0
ALL=0
LINT=0
BIN=0
FILTERS=""
for a in "$@"; do
  case "$a" in
    --quick) QUICK=1 ;;
    --lint) LINT=1 ;;
    --all) ALL=1 ;;
    --bin) BIN=1 ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) FILTERS="$FILTERS $a" ;;
  esac
done

step() { printf '\n\033[1;36m▸ %s\033[0m\n' "$1"; }

# `target/<profile>/zai` より新しいソースの一覧 (空なら zai の方が新しい)。
# **mtime を数値で取らない** (GNU は `stat -c %Y`、BSD は `stat -f %m` で
# 引数が違う)。`find -newer` は POSIX にあるのでどちらの OS でも動く。
newer_than_bin() {
  [ -e "$1" ] || return 0
  find src -name '*.rs' -newer "$1" -print 2>/dev/null || true
  find Cargo.toml build.rs -newer "$1" -print 2>/dev/null || true
}

step "整形 (cargo fmt --all --check)"
cargo fmt --all --check

# CI の lint は **ubuntu 1 台でしか回らない**うえ、clippy は rustc の警告より
# 広い。ローカルの `cargo test` が緑でも clippy だけ赤くなることが実際にある
# (doc コメントのインデント 1 つで CI が落ちた)。
# 既定で走らせないのは、clippy-driver が別の fingerprint を持つため
# **もう一度フルコンパイルが走る**から。push の直前だけ払う。
if [ "$LINT" = 1 ]; then
  step "clippy (CI と同じ債務リスト)"
  # ## 手元の版を必ず出す — **緑が誰の緑かを言えるようにする**
  #
  # CI は `dtolnay/rust-toolchain` の **stable** を引くので、手元が古いと
  # 「新しい lint がまだ無い」だけで緑になる。実測: rustc 1.94 で
  # `tools/verify.sh --lint` が緑だったコミットが、CI (1.98) では
  # `float_literal_f32_fallback` 4 件と `unnecessary_min_or_max` 1 件で赤に
  # なった。**版を出していれば「手元が古い」と気付けた**ので、判定と一緒に
  # 必ず 1 行出す。
  #
  # 版そのものを検査にはしない (どの版が CI に居るかは、ここからは
  # 見えない)。`rustup check` が使えるときだけ、更新があることを添える。
  printf '  %s\n' "$(cargo clippy --version 2>/dev/null || echo 'clippy 版不明')"
  if command -v rustup >/dev/null 2>&1; then
    STALE=$(rustup check 2>/dev/null | grep -E '^stable-.*Update available' || true)
    if [ -n "$STALE" ]; then
      printf '\033[1;33m  ! stable に更新があります。CI は最新の stable を引くので、\n'
      printf '    手元だけ緑になることがあります (rustup update stable)\033[0m\n'
    fi
  fi
  # 債務リストは **ワークフローが唯一の出所**。ここへ写経すると必ずずれる。
  DEBT=$(sed -n '/DEBT=(/,/^ *)/p' .github/workflows/test.yml \
         | grep -oE -- '-A clippy::[a-z_]+' | tr '\n' ' ')
  # shellcheck disable=SC2086
  cargo clippy --bin zai --all-targets -- -D warnings $DEBT
  printf '\033[1;32m✓ clippy 緑\033[0m\n'
fi

# ## 実バイナリ (`target/debug/zai`) の扱い — **静かな嘘の温床**
#
# `cargo test --bin zai --no-run` も `cargo test` も **bin を作らない**。
# だから `target/debug/zai` は前の実行の残骸のまま残り、実バイナリを使う
# テスト (`crate::test_util::real_zai` を通るもの) は黙って skip されるか、
# **版が同じまま中身だけ古いバイナリ**で走る。実際にこれで
# `guard` の実フック試験が「はみ出したのに通った」で赤くなり、
# **`--version` は両方 0.14.0 だった** (照合をすり抜けた)。
#
# 既定でここを建てないのは、bin が `#[cfg(test)]` 抜きの別成果物で
# **もう一度コード生成とリンクが走る**から。実測 (1 ファイル touch、warm):
#
#   cargo test --bin zai --no-run      44s / 94s
#   そのあと cargo build --bin zai     +6s / +22s   ← --bin の追加費用
#
# 2 回で 3 倍ばらついたのは同居している他のビルドと取り合ったため。
# **「Compiling」行とバイナリの mtime が動いたことを確認済み**で、
# どちらも本当にリンクし直している (空振りではない)。要るときだけ払う。
if [ "$BIN" = 1 ]; then
  step "実バイナリ (cargo build --bin zai)"
  cargo build --bin zai
fi

# **1 回だけコンパイルする。** ここで bin もテストコードも全部通るので、
# 警告の検出はこれで完結する (別途 cargo check は走らせない = 二重払いしない)。
step "コンパイルと警告 (cargo test --no-run。テストコードも含む)"
OUT=$(cargo test --bin zai --no-run --color always 2>&1) || { printf '%s\n' "$OUT"; exit 1; }
printf '%s\n' "$OUT" | grep -E '^(warning|error)' -A 6 || true
if printf '%s\n' "$OUT" | grep -qE '^warning'; then
  printf '\n\033[1;31m✗ 警告が残っている (この repo は警告ゼロが約束)\033[0m\n'
  exit 1
fi
printf '\033[1;32m✓ 警告ゼロ\033[0m\n'

# **実バイナリが古ければ黙らない。** ここを黙ると、実バイナリを使うテストが
# 全部 skip されたまま「全部緑」に見える (= 静かな嘘)。
ZAI_BIN="${CARGO_TARGET_DIR:-target}/debug/zai"
STALE=$(newer_than_bin "$ZAI_BIN")
if [ ! -e "$ZAI_BIN" ]; then
  printf '\033[1;33m! %s が無い → 実バイナリを使うテストは skip されます (tools/verify.sh --bin で建てる)\033[0m\n' "$ZAI_BIN"
elif [ -n "$STALE" ]; then
  printf '\033[1;33m! %s がソースより古い (%s 件が新しい。例: %s)\n  → 実バイナリを使うテストは skip されます (tools/verify.sh --bin で建て直す)\033[0m\n' \
    "$ZAI_BIN" "$(printf '%s\n' "$STALE" | grep -c . || true)" "$(printf '%s\n' "$STALE" | sed -n '1p')"
else
  printf '\033[1;32m✓ 実バイナリも最新 (%s)\033[0m\n' "$ZAI_BIN"
fi

[ "$QUICK" = 1 ] && {
  printf '\n--quick なのでテストは走らせない\n'
  _LABEL='整形と警告だけ (テストは走らせていない)'
  exit 0
}

# ここから先は **再コンパイルしない**。上で作ったバイナリをそのまま使う。
if [ "$ALL" = 1 ]; then
  step "全テスト"
  cargo test --bin zai
  exit 0
fi

if [ -n "$FILTERS" ]; then
  for f in $FILTERS; do
    step "テスト: $f"
    cargo test --bin zai "$f"
  done
  exit 0
fi

# 引数が無ければ **git が見ている変更から対象のテストを決める**。
#
# ## basename で探さない (**静かな嘘の温床だった**)
#
# 以前はファイル名だけを取って `panel::` のようなフィルタにしていた。
# `cargo test` のフィルタは**テスト名の部分一致**なので、これは
# `context::panel::…` にも当たる。つまり **`src/team/panel.rs` のテストが
# 1 件も走らなくても、別モジュールの `panel` が走れば「緑」と出る**。
# しかも Rust のテスト名はファイルの道ではなく*モジュール*の道で、
# `src/features/` の下は `#[path]` で引き込まれるため
# `features::team::imp::panel::…` のように**途中に要素が挟まる**。
# 道をそのまま `::` に変換しても当たらない。
#
# ## 実際のテスト名と突き合わせて決める
#
# 真実は `cargo test -- --list` が出す**実際のテスト名**しかないので、
# そこから対応付ける。規則は 1 つ:
#
#   ファイルの道の要素 (`src/` と `.rs` と `mod` を除く) が、テスト名の
#   要素として**その順で**現れること。接頭辞は最後に一致した要素まで。
#
#   src/team/panel.rs → [team, panel]
#     features::team::imp::panel::tests::x → team(2) < panel(4)  ✓ → features::team::imp::panel::
#     context::panel::tests::x             → team が無い          ✗
#
# 間に挟まる `imp` のような要素は何個あってもよい (順序だけ見る)。
# 一致が 1 つも無ければ、そのファイルは**このバイナリにテストを持たない** —
# 一覧が真実なので推測ではない。
#
# **広めに当たるのは許す。狭く外すのは許さない。** `src/cli.rs` のように
# 要素が 1 つだけのファイルは `cli::` のほか `features::team::imp::cli::` にも
# 当たる (どれが「本物」かは一覧からは決められない)。多く走るぶんには
# 嘘にならないが、走らせずに緑と言うのは嘘になる。迷ったら広い側へ倒す。
CHANGED=$(git status --porcelain -- 'src/*.rs' 2>/dev/null | awk '{print $NF}' | sort -u)
if [ -z "$CHANGED" ]; then
  printf '\n変更された src/*.rs が無いので、テストは走らせない (--all で全部)\n'
  _LABEL='検証 (テストは 1 件も走っていない)'
  exit 0
fi

# 全テスト名の一覧を 1 回だけ取る。**取れなければ全部走らせる** —
# 対応付けられないまま一部だけ走らせて緑にするより、遅くても正しく赤にする。
run_all_and_exit() {
  printf '\n\033[1;33m! %s → 安全側で全テストを走らせます\033[0m\n' "$1"
  step "全テスト (安全側のフォールバック)"
  ( cargo test --bin zai 2>&1; echo $? > "$_ZV_RC" ) | tee "$_ZV_LOG"
  if [ "$(cat "$_ZV_RC")" != 0 ]; then
    _WHY='全テストが赤'
    exit 1
  fi
  _n=$(sed -n 's/^running \([0-9][0-9]*\) tests*$/\1/p' "$_ZV_LOG" | head -1)
  if [ -z "$_n" ] || [ "$_n" = 0 ]; then
    _WHY='全テストを走らせたのに件数を読めない (テストが 1 件も走っていない)'
    exit 1
  fi
  printf '\n\033[1;32m✓ %s 件のテストが実際に走った\033[0m\n' "$_n"
  exit 0
}

cargo test --bin zai -- --list > "$_ZV_LIST" 2>/dev/null \
  || run_all_and_exit 'cargo test -- --list を取れない'
[ -s "$_ZV_LIST" ] || run_all_and_exit 'テスト一覧が空'

# ファイルの道 → 要素列 (`src/` を落とし、`mod.rs` は親フォルダの名前にする)。
components_of() {
  printf '%s\n' "$1" | sed -e 's|^\./||' -e 's|^src/||' -e 's|\.rs$||' \
    | awk -F/ '{ for (i = 1; i <= NF; i++) if ($i != "mod") printf "%s%s", (i>1?" ":""), $i; print "" }'
}

# 要素列 → 実際のテスト名の接頭辞 (無ければ何も出さない)。
prefix_for() {
  awk -v want="$1" '
    BEGIN { wn = split(want, W, " ") }
    {
      line = $0
      sub(/:[ \t]*test[s]?[ \t]*$/, "", line)
      if (line !~ /::/ || wn == 0) next
      cn = split(line, C, "::")
      wi = 1; last = 0
      for (ci = 1; ci <= cn && wi <= wn; ci++) {
        if (C[ci] == W[wi]) { last = ci; wi++ }
      }
      if (wi <= wn) next
      p = C[1]
      for (ci = 2; ci <= last; ci++) p = p "::" C[ci]
      p = p "::"
      if (!(p in seen)) { seen[p] = 1; print p }
    }
  ' "$_ZV_LIST"
}

_TARGETS=''
NOTESTS=''
for f in $CHANGED; do
  _comp=$(components_of "$f")
  _pfx=$(prefix_for "$_comp")
  if [ -z "$_pfx" ]; then
    NOTESTS="${NOTESTS}${NOTESTS:+, }${f}"
    continue
  fi
  for p in $_pfx; do
    case " $_TARGETS " in
      *" $p "*) ;;
      *) _TARGETS="${_TARGETS}${_TARGETS:+ }${p}" ;;
    esac
  done
done

if [ -n "$NOTESTS" ]; then
  printf '\n\033[1;33m! テストを持たない変更ファイル: %s\033[0m\n' "$NOTESTS"
  printf '  (一覧に 1 件も現れないファイルです。持っているはずならモジュールの繋ぎ方を確かめる)\n'
fi
# **全部が「テスト無し」なら、この実行は 1 件も走らせないことになる。**
# そこで緑を出すのが以前の嘘だったので、安全側で全部走らせる。
[ -n "$_TARGETS" ] || run_all_and_exit '変更ファイルに対応するテストが 1 件も見つからない'

_ran_total=0
for m in $_TARGETS; do
  step "テスト: $m"
  # **溜めずに素通しする。** `_out=$(…)` で溜めると、固まったときに
  # 1 バイトも出ないまま待つことになる。`tee` で流しながら控えを取る。
  #
  # パイプを挟むと `$?` は `tee` のものになるので (この script の冒頭に
  # 書いてある罠と同じ)、cargo の終了コードは**別に受け取る**。
  # `PIPESTATUS` は bash 専用で、ここは `#!/usr/bin/env sh` なので使えない。
  ( cargo test --bin zai "$m" 2>&1; echo $? > "$_ZV_RC" ) | tee "$_ZV_LOG"
  if [ "$(cat "$_ZV_RC")" != 0 ]; then
    _WHY="$m が赤"
    exit 1
  fi
  # **件数を読めないのは緑にしない。** 読めたのに 0 なら、一覧から起こした
  # 接頭辞が実際には当たっていない = 対応付けが壊れている。どちらも赤。
  _n=$(sed -n 's/^running \([0-9][0-9]*\) tests*$/\1/p' "$_ZV_LOG" | head -1)
  if [ -z "$_n" ]; then
    _WHY="$m の実行件数を読めない (テストが走ったか確かめられない)"
    exit 1
  fi
  if [ "$_n" = 0 ]; then
    _WHY="$m は一覧に載っているのに 0 件しか走らない (対応付けが壊れている)"
    exit 1
  fi
  _ran_total=$((_ran_total + _n))
done
printf '\n\033[1;32m✓ %s 件のテストが実際に走った\033[0m\n' "$_ran_total"
