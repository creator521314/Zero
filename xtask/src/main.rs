//! xtask —— 构建/校验辅助脚本。
//!
//! 用法：
//!   cargo xtask check    # fmt 检查 + clippy + test
//!   cargo xtask test     # 仅测试
//!   cargo xtask fmt      # 格式化
//!   cargo xtask gen-cfg  # 生成默认配置文件

use std::process::Command;

fn main() -> anyhow::Result<()> {
    let task = std::env::args().nth(1).unwrap_or_else(|| "check".into());
    match task.as_str() {
        "check" => {
            run(&["fmt", "--all", "--", "--check"])?;
            run(&["clippy", "--all-targets", "--", "-D", "warnings"])?;
            run(&["test", "--all"])?;
        }
        "test" => run(&["test", "--all"])?,
        "fmt" => run(&["fmt", "--all"])?,
        "gen-cfg" => {
            let path = agent_providers::ProvidersFile::config_path();
            agent_providers::ProvidersFile::write_template(&path)?;
            println!("wrote {}", path.display());
        }
        other => {
            eprintln!("unknown task: {other}");
            eprintln!("available: check | test | fmt | gen-cfg");
            std::process::exit(2);
        }
    }
    Ok(())
}

fn run(args: &[&str]) -> anyhow::Result<()> {
    println!("$ cargo {}", args.join(" "));
    let status = Command::new("cargo").args(args).status()?;
    if !status.success() {
        anyhow::bail!("`cargo {}` failed", args.join(" "));
    }
    Ok(())
}
