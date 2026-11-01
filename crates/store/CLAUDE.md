# crates/store

插件、lockfile、项目级信任。权限判定链在根 `CLAUDE.md`，不要在这里重写一份。

## 插件

`extensions` / `plugin_runtime`：lockfile v1/v2。贡献包括 commands/skills、agents、MCP、hooks、styles、themes、channels、LSP、monitors、settings、userConfig。先在 staged catalog 校验，任一失败则整插件回滚。名称冲突先到先得。LSP 用 `Content-Length` framing；失败不影响本地 CodeIndex。

## 项目根与开关

`config::project_root` 只向上找 `.git`，不跑 git 命令。项目配置一律在 `<git-root>/.wyj-code/`。`/init` 只在 `mcp.toml` / `settings.toml` 缺失时写模板，已存在则不动，不预建空的 `skills/` `agents/`。

`.wyj-code/settings.toml` 的 `disabled_skills` / `disabled_mcp_servers` 按名字禁用，覆盖手动丢进来、没有 lockfile 的条目。与 lockfile 的 `enabled: false` 取并集。

## 信任门控

批准记录一律在 `~/.wyj-code/projects/<project_key>/`，不能放进仓库：仓库能改掉自己的「已批准」标记。无 UI（`-p` / headless）静默排除并提示一次，**不要**阻塞读 stdin。`wyj-code trust` 是总入口；`trust-mcp` / `trust-skills` / `trust-hooks` 必须分开，不能合成一个「批准了就全放行」。

- **MCP**（`project_trust`）：只指纹项目级 `mcp.toml`。未信任的 server 不进 `effective_mcp_servers_for_runtime`，没有第二条连接路径。
- **Skill**（`skill_trust`）：指纹覆盖 `<git-root>/.wyj-code/skills/` **整棵树**，含 `references/`，且哈希一切普通文件。只哈希 `SKILL.md` 会留下二阶注入（SKILL.md 让模型去读 references）。不 follow symlink；不把 mtime/inode/权限放进哈希。忽略名单不能交给 skill 作者。产物目录是 `layout::project_skill_data_root`，经 `WYJ_CODE_SKILL_DATA` 给脚本。`ProjectSkillsGate::resolve` 是唯一构造入口。批准后要重建 `cmd_registry` 和 slash 补全。状态四态，弹窗只看 `needs_prompt()`，不要用「目录非空」当判据。`approve_skills` 走 `write_atomic`。
- **Hooks / verify / acceptance**（`core::hooks_trust`）：未批准则整块跳过项目级，只留全局。指纹只含文件里**实际存在**的 `hooks` / `verify` / `acceptance` 键。`.wyj-code/agents/*.md` 仍没有门控。

## SkillHub

`store::skillhub` 是第三方 HTTP 镜像，不是稳定 API（`page` 会被静默忽略）。安装必须逐条确认；非 TTY 要 `--yes`。解压按不可信归档：拒绝 `..`、绝对路径、符号链接，限制体积，根上必须有 `SKILL.md`，先 staging 再 rename。`upgrade_skill` 按 `source.is_registry()` 分流，不要对 https URL 做 `git clone`。同名目录只有 lockfile 纳管时才允许覆盖。安装名只留 `[A-Za-z0-9_-]`。
