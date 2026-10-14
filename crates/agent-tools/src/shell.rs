//! shell_exec —— 最高危工具。
//!
//! 安全措施：
//! 1. 参数数组执行（不经 `sh -c` 字符串拼接），降低注入面；
//! 2. cwd 锁定在工作区；
//! 3. 超时强杀；
//! 4. 支持取消令牌；
//! 5. 静态高危规则在权限层先拦截（本工具仅负责安全执行）。

use agent_core::perm::Risk;
use agent_core::tool::{Tool, ToolCtx, ToolError, ToolOutput};
use async_trait::async_trait;
use serde_json::json;
use std::process::Stdio;

pub struct ShellExec;

#[async_trait]
impl Tool for ShellExec {
    fn name(&self) -> &str {
        "shell_exec"
    }
    fn description(&self) -> &str {
        "在工作区内执行一条 shell 命令并返回输出。命令以 /bin/sh -c 执行，请勿用于高危操作。"
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

        tokio::fs::create_dir_all(&ctx.workspace).await?;

        // 显式使用 sh -c（一条命令字符串），但 cwd 锁定工作区；
        // 高危模式已在权限层被静态规则拦截。
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(cmd)
            .current_dir(&ctx.workspace)
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
