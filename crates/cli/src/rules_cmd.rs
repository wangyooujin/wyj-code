//! `wyj-code rules`：结构裁剪项目里的规约文件。
//!
//! 不调用模型。默认只打印预览，`--yes` 才写盘。

use anyhow::Result;
use std::path::Path;
use wyj_core::RulesOptimizeAction;

pub fn run(root: &Path, apply: bool, json: bool) -> Result<()> {
    let report = wyj_core::optimize_project_rules(root, apply)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{}", wyj_commands::render_rules_optimize(&report, 100));
    }
    if report
        .files
        .iter()
        .any(|file| matches!(file.action, RulesOptimizeAction::Failed { .. }))
    {
        anyhow::bail!("{}", wyj_i18n::tr("rules.failed"));
    }
    Ok(())
}
