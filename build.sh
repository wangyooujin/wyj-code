#!/usr/bin/env bash
# wyj-code 构建脚本
# 用法:
#   ./build.sh                    — 构建当前平台 release 版本
#   ./build.sh package            — 打包当前平台二进制到 dist/
#   ./build.sh install            — 安装到 ~/.local/bin/（已安装则直接覆盖，即为升级）
#   ./build.sh uninstall          — 卸载二进制，保留 ~/.wyj-code/ 下的配置与记忆
#   ./build.sh uninstall --purge — 卸载二进制，并在二次确认后彻底删除 ~/.wyj-code/
#   ./build.sh release            — 交互式确认版本号后自动 bump Cargo.toml + commit + tag + push，触发 GitHub Actions Release
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
    # 原子替换：写临时文件（新 inode）再 mv 到位，避免原地覆盖正在运行的旧进程所占用的
    # 可执行文件——原地覆盖会导致 macOS 内核对该 vnode 的代码签名校验失效，
    # 使后续任何新进程 exec 该路径时被内核 SIGKILL（"zsh: killed"）。
    local tmp
    tmp="$(mktemp "$(dirname "$dest")/.${BINARY}.XXXXXX")"
    cp "target/release/$BINARY" "$tmp"
    chmod +x "$tmp"
    mv -f "$tmp" "$dest"
    echo "✓ 已安装: $dest (v${VERSION})"
    case ":$PATH:" in
        *":$HOME/.local/bin:"*) ;;
        *)
            echo "⚠ ~/.local/bin 不在 PATH 中，请将以下内容加入你的 shell 配置文件（如 ~/.zshrc）："
            echo "  export PATH=\"\$HOME/.local/bin:\$PATH\""
            ;;
    esac
}

uninstall_local() {
    local dest="$HOME/.local/bin/$BINARY"
    if [ -e "$dest" ] || [ -L "$dest" ]; then
        rm -f "$dest"
        echo "✓ 已卸载: $dest"
    else
        echo "未安装（$dest 不存在），无需卸载"
    fi

    if [ "${1:-}" = "--purge" ]; then
        local data_dir="$HOME/.wyj-code"
        if [ -d "$data_dir" ]; then
            read -r -p "确定要彻底删除 $data_dir （配置、记忆、历史等全部数据，不可恢复）？输入 yes 确认: " confirm
            if [ "$confirm" = "yes" ]; then
                rm -rf "$data_dir"
                echo "✓ 已删除: $data_dir"
            else
                echo "已取消，保留 $data_dir"
            fi
        else
            echo "$data_dir 不存在，无需清除"
        fi
    fi
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

release_version() {
    # 阶段一：前置检查
    local current_branch
    current_branch=$(git rev-parse --abbrev-ref HEAD)
    if [ "$current_branch" != "master" ]; then
        echo "✗ 当前分支为 ${current_branch}，release 只能在 master 上执行"
        exit 1
    fi

    if [ -n "$(git status --porcelain)" ]; then
        echo "✗ 工作区有未提交的改动，请先提交或 stash"
        exit 1
    fi

    echo "运行 cargo test..."
    cargo test --locked
    echo "运行 cargo clippy..."
    cargo clippy --all-targets --locked

    # 阶段二：版本号建议 + 交互确认
    local suggested_version
    suggested_version=$(echo "$VERSION" | awk -F. '{print $1"."$2"."$3+1}')

    echo ""
    echo "当前版本: ${VERSION}"
    echo "建议发布版本: ${suggested_version}"
    echo ""
    echo "确认发布流程："
    echo "  1. 更新 Cargo.toml 版本号并提交 (chore: bump version to <version>)"
    echo "  2. 打 tag v<version>"
    echo "  3. push 当前分支 + push tag 到 origin（触发 GitHub Actions Release）"
    echo ""
    local reply
    read -r -p "回车使用建议版本 ${suggested_version}，或输入自定义版本号（如 1.1.0），输入其他内容取消: " reply

    local new_version
    if [ -z "$reply" ]; then
        new_version="$suggested_version"
    elif [[ "$reply" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
        new_version="$reply"
    else
        echo "已取消"
        exit 0
    fi

    # 阶段三：版本号唯一性校验
    if git rev-parse "v${new_version}" >/dev/null 2>&1; then
        echo "✗ 本地已存在 tag v${new_version}"
        exit 1
    fi
    if git ls-remote --exit-code --tags origin "refs/tags/v${new_version}" >/dev/null 2>&1; then
        echo "✗ 远端已存在 tag v${new_version}"
        exit 1
    fi

    # 阶段四：写入版本号 + 提交
    sed -i.bak "s/^version = \".*\"/version = \"${new_version}\"/" Cargo.toml
    rm -f Cargo.toml.bak
    cargo check
    git add Cargo.toml Cargo.lock
    git commit -m "chore: bump version to ${new_version}"

    # 阶段五：打 tag + push
    git tag -a "v${new_version}" -m "v${new_version}"
    git push origin "$current_branch"
    git push origin "v${new_version}"
    echo "✓ 已推送 v${new_version}，GitHub Actions Release workflow 即将触发"
}

CMD="${1:-build}"
shift || true

case "$CMD" in
    build)     build_release ;;
    package)   package_current ;;
    install)   install_local ;;
    uninstall) uninstall_local "$@" ;;
    cross)     cross_compile "$@" ;;
    release)   release_version ;;
    *)
        echo "用法: $0 {build|package|install|uninstall [--purge]|cross <platform>|release}"
        exit 1
        ;;
esac
