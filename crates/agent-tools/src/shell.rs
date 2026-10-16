//! shell_exec —— 最高危工具。
//!
//! 安全措施：
//! 1. cwd 锁定在工具沙箱（项目根，或每任务的私有工作区）；
//! 2. 超时强杀；
//! 3. 支持取消令牌；
//! 4. 静态高危规则在权限层先拦截（本工具仅负责安全执行）。
//!
//! 命令以**平台原生解释器**执行：Windows 上没有 `/bin/sh`，硬编码它会让
//! 每一次调用都在 spawn 阶段失败（os error 3），历史上 100% 不可用。

use agent_core::perm::Risk;
use agent_core::tool::{Tool, ToolCtx, ToolError, ToolOutput};
use async_trait::async_trait;
use serde_json::json;
use std::process::Stdio;

/// 命令解释器：`(程序, 传整条命令的参数)`。
#[cfg(unix)]
const SHELL: (&str, &str) = ("/bin/sh", "-c");
#[cfg(windows)]
const SHELL: (&str, &str) = ("cmd", "/C");

pub struct ShellExec;

/// 控制台代码页在中文 Windows 上是 GBK，直接取字节会得到乱码；
/// 先切到 65001，输出才是 UTF-8。
#[cfg(windows)]
fn decorate(cmd: &str) -> String {
    format!("chcp 65001 >nul && {cmd}")
}
#[cfg(unix)]
fn decorate(cmd: &str) -> String {
    cmd.to_string()
}

#[async_trait]
impl Tool for ShellExec {
    fn name(&self) -> &str {
        "shell_exec"
    }
    fn description(&self) -> &str {
        // 工具描述就是模型看到的用法说明：Windows 上必须说清是 cmd 语法，
        // 否则模型会一直发 `ls -la` 这类 Unix 命令，然后撞上「不是内部或外部命令」。
        if cfg!(windows) {
            "在工作目录内执行一条 Windows 命令（cmd /C）并返回输出。可用 dir / type / echo / findstr 等；不支持 ls / cat / grep 等 Unix 命令。请勿用于高危操作。"
        } else {
            "在工作目录内执行一条 shell 命令（/bin/sh -c）并返回输出。请勿用于高危操作。"
        }
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "cmd": { "type": "string", "description": "要执行的命令" },
                "timeout_secs": { "type": "integer", "description": "超时秒数，默认 60" }
            },
            "required": ["cmd"]
        })
    }
    fn risk_level(&self) -> Risk {
        Risk::High
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let cmd = args
            .get("cmd")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'cmd'".into()))?;
        let timeout_secs = args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(60);

        let cwd = ctx.sandbox_root();
        tokio::fs::create_dir_all(&cwd).await?;

        // 一条命令字符串交给平台解释器；cwd 锁定沙箱，高危模式已在权限层拦截。
        let (prog, flag) = SHELL;
        let mut command = tokio::process::Command::new(prog);
        command
            .arg(flag)
            .arg(decorate(cmd))
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child = command
            .spawn()
            .map_err(|e| ToolError::Failed(format!("spawn failed: {e}")))?;

        let fut = async {
            let out = child.wait_with_output().await?;
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            Ok::<_, std::io::Error>((out.status, stdout, stderr))
        };

        let result: Result<Result<(std::process::ExitStatus, String, String), std::io::Error>, _> = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            r = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), fut) => r,
        };

        match result {
            Err(_) => Err(ToolError::Timeout),
            Ok(Err(e)) => Err(ToolError::Io(e)),
            Ok(Ok((status, stdout, stderr))) => {
                let code = status.code().unwrap_or(-1);
                let mut body = String::new();
                if !stdout.is_empty() {
                    body.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !body.is_empty() {
                        body.push('\n');
                    }
                    body.push_str("[stderr]\n");
                    body.push_str(&stderr);
                }
                let text = format!("exit code: {code}\n{body}");
                if status.success() {
                    Ok(ToolOutput::ok(text))
                } else {
                    Ok(ToolOutput::err(text))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &std::path::Path) -> ToolCtx {
        ToolCtx {
            workspace: dir.to_path_buf(),
            project_root: None,
            task_id: "t".into(),
            timeout: Duration::from_secs(10),
            cancel: CancellationToken::new(),
        }
    }

    /// 回归保护：曾经硬编码 `/bin/sh`，Windows 上每一次调用都在 spawn 阶段失败。
    #[tokio::test]
    async fn spawns_a_command_on_this_platform() {
        let dir = std::env::temp_dir().join("agent-tools-shell-echo-test");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let out = ShellExec
            .execute(json!({ "cmd": "echo hello" }), &ctx(&dir))
            .await
            .expect("本平台的 shell 必须能起来");
        assert!(!out.is_error, "实际: {}", out.content);
        assert!(out.content.contains("hello"), "实际: {}", out.content);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 非 0 退出码要标成错误，模型才知道命令没跑成。
    #[tokio::test]
    async fn non_zero_exit_is_reported_as_error() {
        let dir = std::env::temp_dir().join("agent-tools-shell-fail-test");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let out = ShellExec
            .execute(json!({ "cmd": "exit 3" }), &ctx(&dir))
            .await
            .unwrap();
        assert!(out.is_error, "实际: {}", out.content);
        assert!(
            out.content.contains("exit code: 3"),
            "实际: {}",
            out.content
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn description_matches_platform() {
        let d = ShellExec.description();
        if cfg!(windows) {
            assert!(d.contains("cmd /C"), "Windows 上要说清是 cmd 语法: {d}");
        } else {
            assert!(d.contains("/bin/sh"), "Unix 上要说清是 sh 语法: {d}");
        }
    }
}
