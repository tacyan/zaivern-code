#!/usr/bin/env sh
# ローカル検証を **1 回のコンパイル**で終わらせる。
#
# 使い方:
#   tools/verify.sh                 # 整形 + 警告 + 触ったモジュールのテスト
#   tools/verify.sh git:: lsp::     # モジュールを指定して走らせる
#   tools/verify.sh --all           # 全テスト (cargo test。CI の nextest とは別)
#   tools/verify.sh --quick         # 整形と警告だけ (テストは走らせない)
#   tools/verify.sh --lint          # CI と同じ clippy (**push する前に必ず**)
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

cd "$(dirname "$0")/.."

QUICK=0
ALL=0
LINT=0
FILTERS=""
for a in "$@"; do
  case "$a" in
    --quick) QUICK=1 ;;
    --lint) LINT=1 ;;
    --all) ALL=1 ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) FILTERS="$FILTERS $a" ;;
  esac
done

step() { printf '\n\033[1;36m▸ %s\033[0m\n' "$1"; }

step "整形 (cargo fmt --all --check)"
cargo fmt --all --check

# CI の lint は **ubuntu 1 台でしか回らない**うえ、clippy は rustc の警告より
# 広い。ローカルの `cargo test` が緑でも clippy だけ赤くなることが実際にある
# (doc コメントのインデント 1 つで CI が落ちた)。
# 既定で走らせないのは、clippy-driver が別の fingerprint を持つため
# **もう一度フルコンパイルが走る**から。push の直前だけ払う。
if [ "$LINT" = 1 ]; then
  step "clippy (CI と同じ債務リスト)"
  # 債務リストは **ワークフローが唯一の出所**。ここへ写経すると必ずずれる。
  DEBT=$(sed -n '/DEBT=(/,/^ *)/p' .github/workflows/test.yml \
         | grep -oE -- '-A clippy::[a-z_]+' | tr '\n' ' ')
  # shellcheck disable=SC2086
  cargo clippy --bin zai --all-targets -- -D warnings $DEBT
  printf '\033[1;32m✓ clippy 緑\033[0m\n'
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

[ "$QUICK" = 1 ] && { printf '\n--quick なのでテストは走らせない\n'; exit 0; }

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
# src/foo.rs を触ったなら foo:: を走らせる (どの環境でも同じ導出)。
MODS=$(git status --porcelain -- 'src/*.rs' 2>/dev/null | awk '{print $NF}' \
       | sed -e 's|^src/||' -e 's|\.rs$||' | sort -u)
if [ -z "$MODS" ]; then
  printf '\n変更された src/*.rs が無いので、テストは走らせない (--all で全部)\n'
  exit 0
fi
for m in $MODS; do
  step "テスト: ${m}::"
  cargo test --bin zai "${m}::"
done
