//! 权限模型与引擎边界。

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
    AskUser,
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

/// 权限引擎边界。
#[async_trait]
pub trait PermissionEngine: Send + Sync {
    /// 对一次工具调用做裁决。实现见 `agent-perm`。
    async fn check(&self, call: &ToolCall, task: &TaskSpec) -> Decision;
}
