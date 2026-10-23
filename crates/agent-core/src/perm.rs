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

/// 权限引擎边界。
#[async_trait]
pub trait PermissionEngine: Send + Sync {
    /// 对一次工具调用做裁决。实现见 `agent-perm`。
    async fn check(&self, call: &ToolCall, task: &TaskSpec) -> Decision;

    /// 登记「本任务此类操作全部通过」，之后同类调用免询问。
    ///
    /// L3 下用户在审批弹层按「批准本任务同类操作」时，由编排层回调。
    /// 默认空操作：不关心批量批准的引擎（headless / 测试替身）无需关心。
    async fn approve_for_task(&self, _task_id: &str, _tool: &str) {}
}
