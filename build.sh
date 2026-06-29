#!/usr/bin/env bash
# wyj-code 构建脚本
# 用法:
#   ./build.sh           — 构建当前平台 release 版本
#   ./build.sh package   — 打包当前平台二进制到 dist/
#   ./build.sh install   — 安装到 ~/.local/bin/
#
# 交叉编译（需要对应工具链）:
#   ./build.sh cross linux-x86_64
#   ./build.sh cross linux-aarch64

set -euo pipefail

BINARY="wyj-code"
VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*= *"\(.*\)"/\1/')
OUT_DIR="dist"

build_release() {
    echo "构建 release 版本 v${VERSION}..."
    cargo build --release --bin "$BINARY"
    echo "✓ target/release/${BINARY}"
}

package_current() {
    build_release
    mkdir -p "$OUT_DIR"
    local platform
    platform=$(rustc -vV | grep host | awk '{print $2}')
    local out="${OUT_DIR}/${BINARY}-${VERSION}-${platform}"
    cp "target/release/${BINARY}" "$out"
    echo "✓ 已打包: $out ($(du -sh "$out" | cut -f1))"
}

install_local() {
    build_release
    local dest="$HOME/.local/bin/$BINARY"
    mkdir -p "$(dirname "$dest")"
    cp "target/release/$BINARY" "$dest"
    echo "✓ 已安装: $dest"
}

cross_compile() {
    local target="${1:-}"
    case "$target" in
        linux-x86_64)
            RUST_TARGET="x86_64-unknown-linux-musl"
            ;;
        linux-aarch64)
            RUST_TARGET="aarch64-unknown-linux-musl"
            ;;
        macos-x86_64)
            RUST_TARGET="x86_64-apple-darwin"
            ;;
        macos-aarch64)
            RUST_TARGET="aarch64-apple-darwin"
            ;;
        *)
            echo "未知平台: $target"
            echo "支持: linux-x86_64, linux-aarch64, macos-x86_64, macos-aarch64"
            exit 1
            ;;
    esac
    echo "交叉编译 $RUST_TARGET..."
    rustup target add "$RUST_TARGET" 2>/dev/null || true
    cargo build --release --bin "$BINARY" --target "$RUST_TARGET"
    mkdir -p "$OUT_DIR"
    local out="${OUT_DIR}/${BINARY}-${VERSION}-${RUST_TARGET}"
    cp "target/${RUST_TARGET}/release/${BINARY}" "$out"
    echo "✓ $out"
}

CMD="${1:-build}"
shift || true

case "$CMD" in
    build)     build_release ;;
    package)   package_current ;;
    install)   install_local ;;
    cross)     cross_compile "$@" ;;
    *)
        echo "用法: $0 {build|package|install|cross <platform>}"
        exit 1
        ;;
esac
