#!/bin/sh
# 安装仓库自带的 git hooks（目前只有 pre-commit 密钥扫描）。
#
# 为什么不用 core.hooksPath：那会整目录接管 .git/hooks，把用户/工具链装的其他
# 钩子一并屏蔽。这里逐个复制，不覆盖已有的同名钩子（存在时明确告知并退出）。

set -e

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

hooks_dir=$(git rev-parse --git-path hooks)
mkdir -p "$hooks_dir"

src="$repo_root/scripts/pre-commit"
dst="$hooks_dir/pre-commit"

if [ -e "$dst" ] && ! cmp -s "$src" "$dst"; then
  echo "已存在不同的 pre-commit 钩子：$dst"
  echo "请先备份/删除它，或手动把 scripts/pre-commit 的内容合并进去。"
  exit 1
fi

cp "$src" "$dst"
chmod +x "$dst"
echo "已安装 pre-commit 钩子 -> $dst"

if ! command -v gitleaks >/dev/null 2>&1; then
  echo
  echo "提示：本机还没有 gitleaks，钩子目前会直接放行。"
  echo "  macOS: brew install gitleaks"
  echo "  其他:  https://github.com/gitleaks/gitleaks#installing"
fi
