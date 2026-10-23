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
    /// 每任务私有工作区：外置输出等**系统数据**落在里面。
    pub workspace: String,
    /// 项目根目录（可选）：工具的读写沙箱，也是相对路径的基准。
    ///
    /// 为空时沿用 `workspace`——那是一个每任务新建的空目录，Agent 因此
    /// 看不见用户的真实项目（历史上「介绍当前项目」全部工具失败的根因）。
    #[serde(default)]
    pub project_root: Option<String>,
    /// 模型覆盖（None 走路由默认）。
    pub model: Option<String>,
    /// 该任务的最大迭代数。
    pub max_iterations: usize,
    /// 单任务预算上限（USD），None 使用全局配置。
    pub budget_usd: Option<f64>,
    /// 续跑时携带的历史消息（`/resume` 用）。
    ///
    /// 由 store 里该任务最后一次 `ContextSnapshot` 还原而来——那就是当时
    /// **真正送给模型**的上下文。带它启动时，执行循环不再从零拼 system+prompt，
    /// 而是接着这份上下文继续。
    ///
    /// 注意：`prompt` 仍会写进新的 system 固定区（system 提示词每次都要重建），
    /// 所以「当初要做什么」不会因为续跑而丢失。
    #[serde(default)]
    pub resume_messages: Option<Vec<crate::provider::Message>>,
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
            project_root: None,
            model: None,
            max_iterations: 25,
            budget_usd: None,
            resume_messages: None,
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
