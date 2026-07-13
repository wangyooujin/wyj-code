#!/bin/sh
# wyj-code 一键安装引导脚本（macOS / Linux）—— curl -fsSL <url>/install.sh | sh
#
# 与仓库根目录的 install.sh 不是同一个文件：那个脚本随 release 压缩包分发，假定
# wyj-code 二进制已经和它解压在同一目录，只负责"装到 ~/.local/bin + 配置 PATH"。
# 这个脚本运行在用户还什么都没有的机器上，只做前置工作——探测平台、从 GitHub
# Releases 重定向解析最新版本、下载对应压缩包、校验 sha256、解压——解压完成后直接
# exec 执行包内那个 install.sh，把安装这一步完全交给它，不重复实现一遍。
#
# 用 POSIX sh 语法编写（不能假设 curl | sh 时解释器是 bash）。
set -eu

REPO_OWNER="wangyooujin"
REPO_NAME="wyj-code"
BINARY="wyj-code"

log() {
    printf '%s\n' "$*" >&2
}

fail() {
    log "错误：$*"
    log "可以去 https://github.com/${REPO_OWNER}/${REPO_NAME}/releases 手动下载安装。"
    exit 1
}

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || fail "缺少依赖命令：$1"
}

require_cmd curl
require_cmd tar
require_cmd uname
require_cmd mktemp

# 1. 探测平台 -> target triple（映射规则与 crates/store/src/self_update.rs::target_for 一致）
os_name="$(uname -s)"
arch_name="$(uname -m)"

case "$os_name" in
    Darwin)
        case "$arch_name" in
            x86_64) target="x86_64-apple-darwin" ;;
            arm64|aarch64) target="aarch64-apple-darwin" ;;
            *) fail "不支持的 macOS 架构：$arch_name" ;;
        esac
        ;;
    Linux)
        case "$arch_name" in
            x86_64) target="x86_64-unknown-linux-musl" ;;
            aarch64|arm64) target="aarch64-unknown-linux-musl" ;;
            *) fail "不支持的 Linux 架构：$arch_name" ;;
        esac
        ;;
    *)
        fail "不支持的操作系统：$os_name（Windows 请用 install.ps1）"
        ;;
esac

log "==> 探测到平台：${os_name}/${arch_name} -> ${target}"

# 2. 版本：默认查最新 release，可用 WYJ_CODE_VERSION 环境变量指定具体版本回滚/固定
if [ -n "${WYJ_CODE_VERSION:-}" ]; then
    tag="v${WYJ_CODE_VERSION}"
    version="${WYJ_CODE_VERSION}"
    log "==> 使用指定版本：${version}（来自 WYJ_CODE_VERSION）"
else
    log "==> 查询最新版本..."
    latest_url="https://github.com/${REPO_OWNER}/${REPO_NAME}/releases/latest"
    latest_location="$(curl -fsSL -o /dev/null -w '%{url_effective}' "$latest_url")" \
        || fail "查询最新 release 失败：$latest_url"
    case "$latest_location" in
        */releases/tag/*)
            tag="${latest_location##*/releases/tag/}"
            tag="${tag%%\?*}"
            tag="${tag%%#*}"
            ;;
        *)
            fail "无法从 GitHub Releases 重定向地址解析版本：$latest_location"
            ;;
    esac
    case "$tag" in
        v[0-9]*) ;;
        *) fail "GitHub Releases 返回了无效版本 tag：$tag" ;;
    esac
    version="${tag#v}"
    log "==> 最新版本：${version}"
fi

# 3. 拼资产名（命名规则与 .github/workflows/release.yml / asset_names() 一致）
archive="${BINARY}-${version}-${target}.tar.gz"
archive_url="https://github.com/${REPO_OWNER}/${REPO_NAME}/releases/download/${tag}/${archive}"
sha256_url="${archive_url}.sha256"
sums_url="https://github.com/${REPO_OWNER}/${REPO_NAME}/releases/download/${tag}/SHA256SUMS"

# 4. 下载 + 校验 + 解压
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

log "==> 下载 ${archive_url}"
curl -fsSL "$archive_url" -o "${tmp_dir}/${archive}" || fail "下载安装包失败：$archive_url"
if ! curl -fsSL "$sha256_url" -o "${tmp_dir}/${archive}.sha256"; then
    log "==> 单独校验和不存在，尝试 SHA256SUMS"
    curl -fsSL "$sums_url" -o "${tmp_dir}/SHA256SUMS" || fail "下载校验和文件失败：$sums_url"
    awk -v archive="$archive" '
        {
            file = $2
            sub(/^\*/, "", file)
            if (file == archive) {
                print $1 "  " archive
                found = 1
            }
        }
        END { exit found ? 0 : 1 }
    ' "${tmp_dir}/SHA256SUMS" > "${tmp_dir}/${archive}.sha256" || fail "SHA256SUMS 中未找到 ${archive}"
fi

log "==> 校验 sha256"
(
    cd "$tmp_dir"
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 -c "${archive}.sha256" >/dev/null
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c "${archive}.sha256" >/dev/null
    else
        fail "缺少 shasum/sha256sum，无法校验安装包完整性"
    fi
) || fail "sha256 校验失败，安装包可能已损坏或被篡改"

log "==> 解压"
tar xzf "${tmp_dir}/${archive}" -C "$tmp_dir"

extracted_dir="${tmp_dir}/${BINARY}-${version}-${target}"
[ -f "${extracted_dir}/install.sh" ] || fail "压缩包内未找到 install.sh（归档结构异常）"

# 5. 交给压缩包里自带的 install.sh 完成实际安装（装入用户目录 + 配置 PATH）
# 注意：这里不用 exec —— exec 会替换掉当前 shell 进程，导致上面注册的
# EXIT trap（清理 tmp_dir）永远不会触发，留下垃圾临时目录。
log "==> 执行安装..."
cd "$extracted_dir"
chmod +x install.sh
./install.sh
