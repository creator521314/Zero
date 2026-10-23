//! xtask —— 构建/校验/排障辅助脚本。
//!
//! 用法：
//!   cargo xtask check    # fmt 检查 + clippy + test
//!   cargo xtask test     # 仅测试
//!   cargo xtask smoke    # 工具端到端冒烟（真实跑一遍 8 个工具）
//!   cargo xtask fmt      # 格式化
//!   cargo xtask gen-cfg  # 生成默认配置文件
//!   cargo xtask log <id|路径> [--full] [--errors]   # 读会话日志

mod log_view;

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
        // 它也会被 `check` / `test` 带上（集成测试在 --all 里），这里只是单独
        // 跑一次的入口：想确认「工具现在真的能用」时用这个。
        "smoke" => run(&[
            "test",
            "-p",
            "agent-tools",
            "--test",
            "smoke",
            "--",
            "--nocapture",
        ])?,
        "fmt" => run(&["fmt", "--all"])?,
        "gen-cfg" => {
            let path = agent_providers::ProvidersFile::config_path();
            agent_providers::ProvidersFile::write_template(&path)?;
            println!("wrote {}", path.display());
        }
        "log" => {
            let rest: Vec<String> = std::env::args().skip(2).collect();
            let full = rest.iter().any(|a| a == "--full");
            let errors_only = rest.iter().any(|a| a == "--errors");
            let Some(target) = rest.iter().find(|a| !a.starts_with("--")) else {
                eprintln!("用法: cargo xtask log <session.jsonl | task-id> [--full] [--errors]");
                eprintln!("  --full    展开完整上下文与工具返回原文");
                eprintln!("  --errors  只看失败项（工具失败 / 被拦 / 报错）");
                std::process::exit(2);
            };
            let path = log_view::resolve(target);
            if !path.exists() {
                eprintln!("找不到会话文件：{}", path.display());
                std::process::exit(2);
            }
            let opts = log_view::ViewOpts { full, errors_only };
            print!("{}", log_view::render(&path, &opts)?);
        }
        other => {
            eprintln!("unknown task: {other}");
            eprintln!("available: check | test | smoke | fmt | gen-cfg | log");
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
