#!/usr/bin/env bash
# wyj-code 一键安装脚本（macOS / Linux）
# 用法：在解压出的目录下执行 ./install.sh
set -euo pipefail

BINARY="wyj-code"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
SRC="$SCRIPT_DIR/$BINARY"
DEST_DIR="$HOME/.local/bin"
DEST="$DEST_DIR/$BINARY"

if [ ! -f "$SRC" ]; then
    echo "未找到 ${SRC} ，请确认 install.sh 与 ${BINARY} 二进制在同一目录下 (解压是否完整?)"
    exit 1
fi

mkdir -p "$DEST_DIR"

# 原子替换：同目录内建临时文件再 mv 到位，不原地覆盖。
# 原地覆盖正在运行进程占用的可执行文件，在 macOS 上会使内核对该 vnode 的代码签名
# 校验失效，导致后续 exec 该路径的新进程被 SIGKILL（见 build.sh install_local()）。
tmp="$(mktemp "$DEST_DIR/.${BINARY}.XXXXXX")"
cp "$SRC" "$tmp"
chmod +x "$tmp"
mv -f "$tmp" "$DEST"

echo "✓ 已安装: $DEST"
"$DEST" --version 2>/dev/null || true

# 确保 ~/.local/bin 在 PATH 中；未命中则按当前 shell 幂等追加到对应 rc 文件
case ":$PATH:" in
    *":$DEST_DIR:"*)
        ;;
    *)
        MARKER="# added by wyj-code install.sh"
        case "${SHELL:-}" in
            */zsh)  RC="$HOME/.zshrc" ;;
            */bash) RC="$HOME/.bashrc" ;;
            *)      RC="$HOME/.profile" ;;
        esac
        if [ -f "$RC" ] && grep -qF "$MARKER" "$RC"; then
            :
        else
            {
                echo ""
                echo "$MARKER"
                echo "export PATH=\"$DEST_DIR:\$PATH\""
            } >> "$RC"
            echo "已将 ${DEST_DIR} 加入 PATH (写入 ${RC})，重新打开终端或执行: source ${RC}"
        fi
        ;;
esac

echo ""
echo "下一步："
echo "  export WYJ_CODE_API_KEY=sk-...   # 或写入 ~/.wyj-code/config.toml"
echo "  wyj-code                          # 启动 TUI"
