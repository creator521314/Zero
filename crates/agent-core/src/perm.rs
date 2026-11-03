//! 权限模型与引擎边界。

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::ids::{ApprovalId, TaskId};
use crate::provider::ToolCall;
use crate::task::TaskSpec;

/// 静态风险等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    Low,
    Medium,
    High,
}

/// 权限档位（任务级配置）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum PermissionTier {
    /// L1 全自动：全部放行，仅记审计。
    L1,
    /// L2 AI 审视：由 AI 判断是否放行；任何故障降级为询问用户。
    L2,
    /// L3 逐步确认：每个操作都需用户确认。
    L3,
}

/// 权限裁决。
#[derive(Debug, Clone)]
pub enum Decision {
    Allow,
    Deny(String),
    /// 需要用户确认。
    ///
    /// 必须带上「为什么」：弹窗要给人看，日志要给复盘看。历史实现是无负载的
    /// 单元变体，导致日志里所有审批长得一模一样（`reason` 固定文案、
    /// `matched_rule` 恒为 None），复盘时无法区分静态规则与 AI 审视。
    AskUser {
        /// 为什么需要用户确认（进弹窗、进日志）。
        reason: String,
        /// 裁决来源：Static=静态规则，Ai=AI 审视/审视通道异常，User=策略（如 L3）。
        source: ApprovalSource,
        /// 静态规则命中时的规则描述；其余为 None。
        matched_rule: Option<String>,
    },
}

/// 待审批请求（发给 TUI 展示）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: ApprovalId,
    pub task_id: TaskId,
    pub tool: String,
    pub args: serde_json::Value,
    /// 风险等级。
    pub risk: Risk,
    /// 为何被拦截（静态规则命中 / AI 判定 / L3 策略）。
    pub reason: String,
    /// 若来自静态规则，附命中的规则描述。
    pub matched_rule: Option<String>,
    /// 裁决来源。旧日志没有这个字段 → None。
    #[serde(default)]
    pub source: Option<ApprovalSource>,
}

/// AI 审视的判定结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResult {
    pub allow: bool,
    pub risk: Risk,
    pub reason: String,
    pub confidence: f64,
    pub source: ApprovalSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalSource {
    Static,
    Ai,
    User,
}

/// AI 审视器边界：把「这次操作该不该放行」交给模型判。
///
/// 定义在 core 而不是 `agent-perm`：它的入参出参全是 core 类型，放在这里
/// 让实现方（`agent-providers::LlmReviewer`）**不必依赖 `agent-perm`**——
/// 「实现 crate 只依赖 agent-core」的依赖铁律才守得住。
///
/// 契约：任何失败都以 `Err` 返回，由引擎降级为询问用户。
/// 实现方**不得**在解析失败时默认放行（误放行的不可逆代价 > 多问一次）。
#[async_trait]
pub trait AiReviewer: Send + Sync {
    async fn review(
        &self,
        task: &TaskSpec,
        call: &ToolCall,
        static_risk: Risk,
        timeout: Duration,
    ) -> Result<ApprovalResult, String>;
}

/// 一次调用的「操作类」键，供「批准本任务同类操作」使用。
///
/// 为什么不能直接用工具名：`shell_exec` 是万能入口，批准一次 `ls` 就放行本任务
/// 后续所有 shell 命令（`cat ~/.ssh/id_rsa`、`curl ...` 都在其中）。这里把
/// shell 命令按**首个词**归类（`shell:ls`、`shell:git`），复合命令
/// （`&&`/`;`/`|`/`>`）则退化为「只批准这一条」，避免一次授权被放大成任意命令。
///
/// 这是启发式，不是安全边界：静态高危规则在批量检查**之前**执行，仍然优先拦截。
pub fn class_key(call: &ToolCall) -> String {
    if call.name != "shell_exec" {
        return call.name.clone();
    }
    let Some(cmd) = call.arguments.get("cmd").and_then(|v| v.as_str()) else {
        return "shell:<unparsed>".into();
    };
    if cmd.contains("&&")
        || cmd.contains("||")
        || cmd.contains(';')
        || cmd.contains('|')
        || cmd.contains('>')
    {
        // 复合命令不批量放行：键里带内容哈希，等价于「只批准这一条」。
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        cmd.hash(&mut h);
        return format!("shell:once:{:x}", h.finish());
    }

    let mut words = cmd.split_whitespace();
    let mut first = words.next().unwrap_or("");
    if first == "sudo" || first == "doas" {
        first = words.next().unwrap_or("");
    }
    let first = first.trim_matches(|c| c == '\'' || c == '"');
    if first.is_empty() {
        return "shell:<unparsed>".into();
    }
    format!("shell:{first}")
}

/// 权限引擎边界。
#[async_trait]
pub trait PermissionEngine: Send + Sync {
    /// 对一次工具调用做裁决。实现见 `agent-perm`。
    async fn check(&self, call: &ToolCall, task: &TaskSpec) -> Decision;

    /// 登记「本任务此类操作全部通过」，之后同类调用免询问。
    ///
    /// `class` 是 [`class_key`] 算出的操作类键（非 shell 工具就是工具名，
    /// shell 是 `shell:<首词>`）。L3 下用户按「批准本任务同类操作」时，
    /// 由执行循环回调。
    /// 默认空操作：不关心批量批准的引擎（headless / 测试替身）无需关心。
    async fn approve_for_task(&self, _task_id: &str, _class: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ToolCallId;

    fn shell(cmd: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new(),
            name: "shell_exec".into(),
            arguments: serde_json::json!({ "cmd": cmd }),
        }
    }

    #[test]
    fn non_shell_tools_use_the_tool_name() {
        let call = ToolCall {
            id: ToolCallId::new(),
            name: "file_write".into(),
            arguments: serde_json::json!({}),
        };
        assert_eq!(class_key(&call), "file_write");
    }

    #[test]
    fn shell_class_is_the_first_word() {
        assert_eq!(class_key(&shell("ls -la")), "shell:ls");
        assert_eq!(class_key(&shell("git status")), "shell:git");
        // sudo 前缀不算「类」，剥掉后再看真正执行的命令。
        assert_eq!(class_key(&shell("sudo ls /root")), "shell:ls");
    }

    #[test]
    fn compound_shell_commands_are_not_batchable() {
        let a = class_key(&shell("ls && rm -f x"));
        let b = class_key(&shell("ls && rm -f x"));
        let c = class_key(&shell("ls && rm -f y"));
        assert!(a.starts_with("shell:once:"), "复合命令应退化为一次性: {a}");
        assert_eq!(a, b, "同一条命令的键应稳定");
        assert_ne!(a, c, "不同命令不能共用批量授权");
        // 重定向同样不批量放行（`env > secrets.txt` 不该继承 `env` 的授权）。
        assert!(class_key(&shell("env > /tmp/out")).starts_with("shell:once:"));
    }
}
