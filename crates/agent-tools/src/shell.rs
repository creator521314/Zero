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
//!
//! 子进程输出先按 UTF-8 严格解码，失败再按系统 OEM 代码页解（见
//! [`decode_output`]）。

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

/// 把子进程输出解成文本。
///
/// 两种来源都要照顾：
/// - `dir` / `echo` 这类系统命令按**本地 OEM 代码页**输出（中文 Windows 是
///   GBK），硬按 UTF-8 解会得到一串 `U+FFFD`，模型看到的目录名全是乱码
///   ——真实会话里的 `dir /a` 就是这样变废的；
/// - `git` / `rg` / `python` 这类工具本身就吐 UTF-8，按 GBK 解会把它们弄坏。
///
/// 所以先严格试 UTF-8，失败再退回 OEM 代码页。注意 `chcp 65001` 解决不了
/// 这个问题：它只改控制台**显示**用的代码页，对管道里的字节毫无影响
/// （实测 `chcp 65001 && echo 中文` 的字节与不加时完全一致）。
fn decode_output(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    decode_oem(bytes)
}

/// 按系统 OEM 代码页转码（交给系统做，省得自带一张 GBK 表）。
#[cfg(windows)]
fn decode_oem(bytes: &[u8]) -> String {
    use windows_sys::Win32::Globalization::MultiByteToWideChar;
    /// 预定义代码页：当前系统 OEM 代码页（中文 Windows = 936/GBK）。
    /// 不用 `GetOEMCP()`——windows-sys 0.61 没导出它，而这个常量等价。
    const CP_OEMCP: u32 = 1;
    // dwFlags = 0：非法字节由系统替换成默认字符，而不是整段失败。
    unsafe {
        let cp = CP_OEMCP;
        let len = MultiByteToWideChar(
            cp,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            std::ptr::null_mut(),
            0,
        );
        if len <= 0 {
            return String::from_utf8_lossy(bytes).to_string();
        }
        let mut buf = vec![0u16; len as usize];
        let n = MultiByteToWideChar(
            cp,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            buf.as_mut_ptr(),
            len,
        );
        if n <= 0 {
            return String::from_utf8_lossy(bytes).to_string();
        }
        buf.truncate(n as usize);
        String::from_utf16_lossy(&buf)
    }
}

/// 非 Windows 只有 UTF-8 一种约定，非法字节只能 lossy。
#[cfg(not(windows))]
fn decode_oem(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_string()
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
        let mut std_cmd = std::process::Command::new(prog);
        std_cmd
            .arg(flag)
            .arg(cmd)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Windows：给子进程一个自己的（不可见）控制台。
        //
        // 否则它共享 TUI 的控制台，`chcp` 这类改控制台状态的调用会连带把整个
        // 屏幕擦掉（`cls`、改标题同理）。而 TUI 只发送差分，被擦掉的静态区域
        // （边框、输入框、页脚）再也不会被重画，用户看到的就是「大部分区域
        // 变黑，只剩下还在刷新的那几行」。
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            std_cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut command = tokio::process::Command::from(std_cmd);
        command.kill_on_drop(true);

        let child = command
            .spawn()
            .map_err(|e| ToolError::Failed(format!("spawn failed: {e}")))?;

        let fut = async {
            let out = child.wait_with_output().await?;
            let stdout = decode_output(&out.stdout);
            let stderr = decode_output(&out.stderr);
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

    /// 中文必须活着回来：`echo` 按本地 OEM 代码页（中文 Windows = GBK）
    /// 输出，以前硬按 UTF-8 解就是一堆 `U+FFFD`。
    #[tokio::test]
    async fn chinese_output_from_builtin_is_readable() {
        let dir = std::env::temp_dir().join("agent-tools-shell-gbk-test");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let out = ShellExec
            .execute(json!({ "cmd": "echo 中文测试" }), &ctx(&dir))
            .await
            .unwrap();
        assert!(out.content.contains("中文测试"), "实际: {}", out.content);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// `type`/`cat` 原样吐文件字节：UTF-8 文件必须原样活着回来，
    /// 不能被「一律按 GBK 解」这种更省事的做法弄坏。
    #[tokio::test]
    async fn utf8_output_from_tools_survives_roundtrip() {
        let dir = std::env::temp_dir().join("agent-tools-shell-utf8-test");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("u8.txt"), "中文测试")
            .await
            .unwrap();
        let cmd = if cfg!(windows) {
            "type u8.txt"
        } else {
            "cat u8.txt"
        };
        let out = ShellExec
            .execute(json!({ "cmd": cmd }), &ctx(&dir))
            .await
            .unwrap();
        assert!(out.content.contains("中文测试"), "实际: {}", out.content);
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
