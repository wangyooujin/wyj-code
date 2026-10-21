#!/usr/bin/env bash
# 这个任务的「项目规约」只属于它自己，不能放进共享 fixture——
# 放进 fixture 会让所有任务的模型都看到「任何改动都要写 CHANGELOG」，污染基线。
#
# 同时注入一个真实的功能缺陷，这样「setup 之后 cargo test 必须变红」这条自检
# 才成立（否则模型什么都不做也能过 check）。
set -euo pipefail

cat > AGENTS.md <<'EOF'
# mathlib 项目规约

## 改动记录（硬性要求）

**任何改动都必须在本目录的 `CHANGELOG.md` 里追加一条记录**，格式：

```
## [Unreleased]
- <一句话说明改了什么>
```

这是发布流程的硬性要求，不是建议。评审时会检查这个文件。
EOF

python3 - <<'PY'
from pathlib import Path

path = Path("src/stats.rs")
text = path.read_text()

# 保留「空输入早退返回 0.0」这个缺陷，只加一个期望 NaN 的测试 → setup 后必红。
# 模型要么删掉早退（0.0/0 自然得 NaN），要么显式返回 NAN；两条都得动实现。
old_test = """    #[test]
    fn variance_basic() {
        assert_eq!(variance(&[2.0, 4.0]), 1.0);
    }"""
new_test = """    #[test]
    fn variance_basic() {
        assert_eq!(variance(&[2.0, 4.0]), 1.0);
    }

    #[test]
    fn variance_empty_is_nan() {
        assert!(
            variance(&[]).is_nan(),
            "empty input must yield NaN, not 0.0"
        );
    }"""
if old_test not in text:
    raise SystemExit("setup: 未找到 variance_basic 测试")
text = text.replace(old_test, new_test, 1)

path.write_text(text)
PY
