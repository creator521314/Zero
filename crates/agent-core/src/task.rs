//! 任务模型与执行容器边界。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::AgentError;
use crate::event::{EventBus, Outcome};
use crate::ids::TaskId;
use crate::perm::PermissionTier;
use tokio_util::sync::CancellationToken;

/// 任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Created,
    Running,
    Paused,
    Done,
    Failed,
    Cancelled,
}

/// 任务规格。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpec {
    pub id: TaskId,
    pub title: String,
    /// 用户原始任务描述（system 中固化，永不丢弃）。
    pub prompt: String,
    pub tier: PermissionTier,
    /// 工作区根目录。
    pub workspace: String,
    /// 模型覆盖（None 走路由默认）。
    pub model: Option<String>,
    /// 该任务的最大迭代数。
    pub max_iterations: usize,
    /// 单任务预算上限（USD），None 使用全局配置。
    pub budget_usd: Option<f64>,
}

impl TaskSpec {
    pub fn new(prompt: impl Into<String>, workspace: impl Into<String>) -> Self {
        let prompt = prompt.into();
        let title = prompt.chars().take(24).collect::<String>();
        Self {
            id: TaskId::new(),
            title,
            prompt,
            tier: PermissionTier::L2,
            workspace: workspace.into(),
            model: None,
            max_iterations: 25,
            budget_usd: None,
        }
    }
}

/// 执行控制：取消令牌 + 交互通道。
pub struct RunControl {
    pub cancel: CancellationToken,
}

impl RunControl {
    pub fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
        }
    }
}

impl Default for RunControl {
    fn default() -> Self {
        Self::new()
    }
}

/// Agent 执行循环边界。实现见 `agent-runtime`。
#[async_trait]
pub trait AgentRunner: Send + Sync {
    async fn run(
        &self,
        task: TaskSpec,
        bus: EventBus,
        ctl: RunControl,
    ) -> Result<Outcome, AgentError>;
}
