//! CLI 定义。

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "wyj-code",
    version,
    about = "Claude Code CLI 辅助程序:管理国内 coding plan profile 并以 launcher 模式启动 claude"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// 以指定 profile 启动 claude(无参数用默认 profile)
    Run {
        /// 透传给 claude 的参数;首个非 `-` 开头 token 视为 profile 名,其余透传。
        /// 例:`run huoshan -- --version`、`run -- --version`(用默认 profile)。
        #[arg(num_args = 0.., trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// 输出 profile 的 export 语句,供 `eval "$(wyj-code env)"` 使用
    Env {
        /// profile 名(缺省用默认 profile)
        profile: Option<String>,
    },

    /// 列出所有 profile
    #[command(alias = "ls")]
    List,

    /// 新增 profile
    Add(AddArgs),

    /// 编辑 profile(交互逐字段,或 --raw 用 $EDITOR 开整文件)
    Edit {
        name: String,
        /// 用 $EDITOR 打开整个 profiles.toml
        #[arg(long)]
        raw: bool,
    },

    /// 删除 profile
    #[command(alias = "rm")]
    Remove {
        name: String,
        /// 跳过确认
        #[arg(short, long)]
        force: bool,
    },

    /// 查看或设置默认 profile
    Default {
        /// profile 名;缺省则打印当前默认
        name: Option<String>,
    },

    /// `default <profile>` 的别名
    Use { profile: String },

    /// 从 zshrc 导入 alias model_* 为 profile
    Import(ImportArgs),

    /// 设置 profile 的某个 env / 具名字段
    Set {
        profile: String,
        key: String,
        value: String,
    },

    /// 删除 profile 的某个 env / 具名字段
    Unset {
        profile: String,
        key: String,
    },

    /// 翻转 profile 的某个开关(在 "1"/"0" 之间)
    Toggle {
        profile: String,
        switch: String,
    },

    /// 交互式配置菜单
    Config,

    /// 生成 shell 补全脚本(输出到 stdout,自行 source 或写入补全目录)
    Completions {
        /// 目标 shell:bash / zsh / fish / elvish / powershell
        shell: String,
    },

    /// 管理存储在 macOS Keychain 的 AUTH_TOKEN(仅 darwin)
    Token {
        /// profile 名
        profile: String,
        #[command(subcommand)]
        action: TokenAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum TokenAction {
    /// 把 token 存入 Keychain(交互输入,不回显)
    Set,
    /// 从 Keychain 读取并打印
    Get,
    /// 从 Keychain 删除
    Delete,
}

#[derive(clap::Args, Debug)]
pub struct AddArgs {
    /// profile 名
    pub name: Option<String>,
    #[arg(long)]
    pub base_url: Option<String>,
    #[arg(long)]
    pub auth_token: Option<String>,
    #[arg(long)]
    pub api_key: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub small_fast_model: Option<String>,
    /// 直接设置 env 字段(可多次,格式 KEY=VALUE)
    #[arg(long = "set", value_name = "KEY=VALUE")]
    pub set: Vec<String>,
    /// 不提示设为默认
    #[arg(long)]
    pub no_default: bool,
    /// 覆盖同名 profile
    #[arg(short, long)]
    pub force: bool,
}

#[derive(clap::Args, Debug)]
pub struct ImportArgs {
    /// zshrc 路径(缺省 ~/.zshrc)
    #[arg(long)]
    pub zshrc: Option<String>,
    /// 仅导入指定 alias 名(默认导入所有 model_* )
    #[arg(long)]
    pub name: Option<String>,
    /// 仅预览,不写入
    #[arg(long)]
    pub dry_run: bool,
    /// 覆盖同名 profile
    #[arg(short, long)]
    pub force: bool,
}
