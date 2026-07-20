#!/bin/sh
# Zaivern Code ワンライナーインストーラ
#   curl -fsSL https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.sh | sh
#
# やること:
#   1. OS/CPU を判定し、GitHub Releases のビルド済みバイナリを ~/.local/bin へ配置
#   2. ビルド済みが無い環境や取得失敗時はソースからビルド
#      (Rust が無ければ rustup ごと非対話でセットアップ)
#
# 環境変数:
#   ZAI_INSTALL_DIR    ビルド済みバイナリの配置先 (既定: ~/.local/bin)
#   ZAI_FROM_SOURCE=1  常にソースビルドする
set -eu

REPO="tacyan/zaivern-code"
REPO_URL="https://github.com/$REPO"
REQUIRED_MINOR=88
INSTALL_DIR="${ZAI_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '\033[1;36m[zaivern-code]\033[0m %s\n' "$1"; }
err() { printf '\033[1;31m[zaivern-code]\033[0m %s\n' "$1" >&2; }

path_hint() {
    case ":$PATH:" in
        *":$1:"*) ;;
        *) say "⚠ $1 が PATH にありません。シェルの rc に以下を追記してください:"
           say "   export PATH=\"$1:\$PATH\"" ;;
    esac
}

# --- ビルド済みバイナリのインストール ----------------------------------------
install_prebuilt() {
    case "$(uname -s)" in
        Darwin) os=macos ;;
        Linux)  os=linux ;;
        *) return 1 ;;
    esac
    case "$(uname -m)" in
        arm64|aarch64) arch=arm64 ;;
        x86_64|amd64)  arch=x86_64 ;;
        *) return 1 ;;
    esac
    # Rosetta 配下のシェルは uname -m が x86_64 になるため実 CPU で補正
    if [ "$os" = "macos" ] && [ "$arch" = "x86_64" ] \
        && [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = "1" ]; then
        arch=arm64
    fi
    tag=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null \
        | sed -En 's/.*"tag_name": *"([^"]+)".*/\1/p' | head -n1) || return 1
    [ -n "$tag" ] || return 1
    name="zai-$tag-$os-$arch"
    url="$REPO_URL/releases/download/$tag/$name.tar.gz"
    tmp=$(mktemp -d) || return 1
    trap 'rm -rf "$tmp"' EXIT
    say "ビルド済みバイナリを取得します: $url"
    curl -fsSL "$url" -o "$tmp/$name.tar.gz" || return 1
    tar xzf "$tmp/$name.tar.gz" -C "$tmp" || return 1
    mkdir -p "$INSTALL_DIR" || return 1
    install -m 755 "$tmp/$name/zai" "$INSTALL_DIR/zai" || return 1
    say ""
    say "✅ インストール完了: $INSTALL_DIR/zai ($tag)"
    say "   起動: zai [ワークスペースのパス]"
    path_hint "$INSTALL_DIR"
    return 0
}

if [ "${ZAI_FROM_SOURCE:-0}" != "1" ] && install_prebuilt; then
    exit 0
fi

# --- ソースビルド (フォールバック) -------------------------------------------
say "ソースからビルド・インストールします..."

# 1. Rust ツールチェーンの確認
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

# 2. rustc 1.88+ の確認
minor=$(rustc --version 2>/dev/null | sed -En 's/^rustc 1\.([0-9]+).*/\1/p')
if [ -z "${minor:-}" ] || [ "$minor" -lt "$REQUIRED_MINOR" ]; then
    say "rustc 1.$REQUIRED_MINOR+ が必要です(現在: $(rustc --version 2>/dev/null || echo '不明'))。stable を更新します..."
    rustup update stable
fi

# 3. Linux の場合はビルド依存のヒントを出す
if [ "$(uname -s)" = "Linux" ] && command -v apt-get >/dev/null 2>&1; then
    if ! dpkg -s libgtk-3-dev >/dev/null 2>&1; then
        say "ヒント: ビルドに失敗する場合は次を実行してください:"
        say "  sudo apt-get install -y build-essential libgtk-3-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev"
    fi
fi

# 4. GitHub から直接ビルド & インストール
say "GitHub からビルド・インストールします(初回は数分かかります)..."
cargo install --git "$REPO_URL" --locked zaivern-code

bin_path=$(command -v zai 2>/dev/null || echo "$HOME/.cargo/bin/zai")
say ""
say "✅ インストール完了: $bin_path"
say "   起動: zai [ワークスペースのパス]"
path_hint "$HOME/.cargo/bin"
