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
    rm -f "${_ZV_LOG:-}" "${_ZV_RC:-}"
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

# 引数が無ければ **git が見ている変更から対象モジュールを推測する**。
#
# `src/foo.rs` は `foo::`、`src/team/spec_writer.rs` は `spec_writer::`。
# **ディレクトリ名を混ぜない** — Rust のテスト名は*モジュール*の道で、
# ファイルの道ではない。`src/features/` の下は `#[path]` で引き込まれて
# いるので、実際の名前は `features::team::imp::spec_writer::…` になる。
# ここを `team/spec_writer::` のまま渡すと**1 件も一致せず**、それでも
# `cargo test` は成功で返るので、**0 件走らせて「緑」と出る**
# (実際に team/ の 4 モジュールを触った回で起きた)。
MODS=$(git status --porcelain -- 'src/*.rs' 2>/dev/null | awk '{print $NF}' \
       | sed -e 's|.*/||' -e 's|\.rs$||' | sort -u)
if [ -z "$MODS" ]; then
  printf '\n変更された src/*.rs が無いので、テストは走らせない (--all で全部)\n'
  _LABEL='検証 (テストは 1 件も走っていない)'
  exit 0
fi
# **1 件も走らなかったフィルタは黙って見逃さない。**
#
# `src/team/mod.rs` のように**テストを持たないファイル**は普通にあるので、
# 0 件そのものは赤ではない。赤にするのは「全部のフィルタが 0 件」= この
# 実行で 1 件もテストが走っていないのに緑と出る場合だけ。それ以外は
# **どのフィルタが空振りしたかを名指しで出す** — 空回りしていることが
# 画面に出ていれば、名前の付け間違いはその場で気付ける。
_ran_total=0
_empty=''
for m in $MODS; do
  step "テスト: ${m}::"
  # **溜めずに素通しする。** `_out=$(…)` で溜めると、固まったときに
  # 1 バイトも出ないまま待つことになる。`tee` で流しながら控えを取る。
  #
  # パイプを挟むと `$?` は `tee` のものになるので (この scriptの冒頭に
  # 書いてある罠と同じ)、cargo の終了コードは**別に受け取る**。
  # `PIPESTATUS` は bash 専用で、ここは `#!/usr/bin/env sh` なので使えない。
  ( cargo test --bin zai "${m}::" 2>&1; echo $? > "$_ZV_RC" ) | tee "$_ZV_LOG"
  if [ "$(cat "$_ZV_RC")" != 0 ]; then
    exit 1
  fi
  _n=$(sed -n 's/^running \([0-9]*\) tests*$/\1/p' "$_ZV_LOG" | head -1)
  _n=${_n:-0}
  _ran_total=$((_ran_total + _n))
  if [ "$_n" = 0 ]; then
    _empty="${_empty}${_empty:+, }${m}::"
  fi
done
if [ -n "$_empty" ]; then
  printf '\n\033[1;33m! 一致するテストが無かったフィルタ: %s\033[0m\n' "$_empty"
  printf '  (テストを持たないファイルなら正常。持っているはずなら名前の付け方を確かめる —\n'
  printf '   Rust のテスト名は*モジュール*の道で、ファイルの道ではない)\n'
fi
if [ "$_ran_total" = 0 ]; then
  _WHY="どのフィルタも一致せず、テストが 1 件も走っていない (${_empty})"
  exit 1
fi
printf '\n\033[1;32m✓ %s 件のテストが実際に走った\033[0m\n' "$_ran_total"
