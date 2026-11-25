//! Subagent 边界类型与依赖倒置端口。
//!
//! 只依赖 agent-core 内部类型：agent-tools 通过它使用委派能力，
//! agent-runtime 提供实现，装配层负责注入。任何实现 crate 不得反向依赖。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::error::AgentError;
use crate::event::Outcome;
use crate::ids::TaskId;

/// 委派请求：模型通过 `delegate` 工具的参数构造。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtaskSpec {
    /// 子任务目标（必填）。模型必须写清「要什么结果」。
    pub goal: String,
    /// 可选背景材料。不传 = 完全隔离。
    #[serde(default)]
    pub context: Option<String>,
    /// 期望交付物形态提示，例如 "一份对比表"。
    #[serde(default)]
    pub expected_output: Option<String>,
    /// 模型覆盖：给子任务指定更便宜 / 更专用的模型。
    #[serde(default)]
    pub model: Option<String>,
    /// 迭代上限（实现里钳制到 1..=12）。
    #[serde(default)]
    pub max_iterations: Option<usize>,
    /// 子任务预算上限（USD）。
    #[serde(default)]
    pub budget_usd: Option<f64>,
}

/// 委派结果：只回传摘要，绝不回传子任务全量对话。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtaskResult {
    pub child_id: TaskId,
    pub outcome: Outcome,
    /// 给父模型看的最终文本（已由工具的 redact 管道处理）。
    pub summary: String,
    /// 子任务登记的交付物路径。
    #[serde(default)]
    pub artifacts: Vec<String>,
    /// 本次委派的花费（USD），由共享预算单元填。
    pub cost_usd: f64,
}

/// 委派后端声明自己支持什么。DelegateTool 在调用前据此拒绝不支持的选项，
/// 而不是静默忽略（模型会以为「我指定了便宜模型」但根本没生效）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentCapabilities {
    pub supports_model_override: bool,
    pub supports_budget: bool,
    pub supports_context: bool,
    pub max_depth: u8,
}

impl Default for SubagentCapabilities {
    fn default() -> Self {
        Self {
            supports_model_override: true,
            supports_budget: true,
            supports_context: true,
            max_depth: 1,
        }
    }
}

/// 当前委派树的并发占用。`limit == 0` 表示「后端不提供容量信息」。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentCapacity {
    pub running: usize,
    pub limit: usize,
}

/// 依赖倒置端口：实现在 `agent-runtime`，装配层注入给 `agent-tools`。
///
/// 为什么入参是 `parent_task_id: &str` 而不是 `&TaskSpec`：
/// `Tool::execute` 只能拿到 `ToolCtx`，其中只有 `task_id` 字符串。
/// 实现方由装配层登记「当前父任务」的上下文，这里按 id 取。
#[async_trait]
pub trait SubagentPort: Send + Sync {
    /// 后端声明自己支持什么。默认「全支持」，保证既有实现行为不变。
    fn capabilities(&self) -> SubagentCapabilities {
        SubagentCapabilities::default()
    }

    /// 当前并发占用。`limit == 0` 表示不提供容量信息。
    fn capacity(&self) -> SubagentCapacity {
        SubagentCapacity::default()
    }

    async fn run_subtask(
        &self,
        parent_task_id: &str,
        spec: SubtaskSpec,
        cancel: CancellationToken,
    ) -> Result<SubtaskResult, AgentError>;
}
