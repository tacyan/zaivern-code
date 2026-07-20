#!/bin/sh
# Zaivern Code ワンライナーインストーラ
#   curl -fsSL https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.sh | sh
#
# やること:
#   1. Rust (cargo) が無ければ rustup を非対話でインストール
#   2. rustc 1.88 未満なら stable を更新
#   3. cargo install --git で GitHub から直接ビルド & ~/.cargo/bin に配置
set -eu

REPO_URL="https://github.com/tacyan/zaivern-code"
REQUIRED_MINOR=88

say() { printf '\033[1;36m[zaivern-code]\033[0m %s\n' "$1"; }
err() { printf '\033[1;31m[zaivern-code]\033[0m %s\n' "$1" >&2; }

# --- 1. Rust ツールチェーンの確認 -------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
    # rustup 導入直後で PATH が未反映のケースを拾う
    if [ -f "$HOME/.cargo/env" ]; then
        . "$HOME/.cargo/env"
    fi
fi
if ! command -v cargo >/dev/null 2>&1; then
    say "Rust (cargo) が見つかりません。rustup をインストールします..."
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
    . "$HOME/.cargo/env"
fi

# --- 2. rustc 1.88+ の確認 ---------------------------------------------------
minor=$(rustc --version 2>/dev/null | sed -En 's/^rustc 1\.([0-9]+).*/\1/p')
if [ -z "${minor:-}" ] || [ "$minor" -lt "$REQUIRED_MINOR" ]; then
    say "rustc 1.$REQUIRED_MINOR+ が必要です(現在: $(rustc --version 2>/dev/null || echo '不明'))。stable を更新します..."
    rustup update stable
fi

# --- 3. Linux の場合はビルド依存のヒントを出す --------------------------------
if [ "$(uname -s)" = "Linux" ] && command -v apt-get >/dev/null 2>&1; then
    if ! dpkg -s libgtk-3-dev >/dev/null 2>&1; then
        say "ヒント: ビルドに失敗する場合は次を実行してください:"
        say "  sudo apt-get install -y build-essential libgtk-3-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev"
    fi
fi

# --- 4. GitHub から直接ビルド & インストール ----------------------------------
say "GitHub からビルド・インストールします(初回は数分かかります)..."
cargo install --git "$REPO_URL" --locked zaivern-code

bin_path=$(command -v zaivern-code 2>/dev/null || echo "$HOME/.cargo/bin/zaivern-code")
say ""
say "✅ インストール完了: $bin_path"
say "   起動: zaivern-code [ワークスペースのパス]"
case ":$PATH:" in
    *":$HOME/.cargo/bin:"*) ;;
    *) say "⚠ ~/.cargo/bin が PATH にありません。シェルの rc に以下を追記してください:"
       say '   export PATH="$HOME/.cargo/bin:$PATH"' ;;
esac
