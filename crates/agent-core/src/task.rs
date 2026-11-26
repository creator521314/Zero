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
    /// 续跑：从哪个历史任务接上（`/resume` / 任务面板切换后提交时由界面填入）。
    ///
    /// 编排层据此从 store 取该任务最后一次 `ContextSnapshot` 还原 `resume_messages`。
    /// 界面只传 id、不碰消息，因为 TUI 不依赖 `agent-store`。
    #[serde(default)]
    pub resume_from: Option<TaskId>,
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
    /// 父任务 id；None = 顶层任务。用于委派关系与审计回放。
    #[serde(default)]
    pub parent_id: Option<TaskId>,
    /// 委派深度：顶层 0，子任务 1。硬上限 1（一期）。
    #[serde(default)]
    pub depth: u8,
    /// 本任务可见/可执行的工具白名单。None = 不限制（默认，兼容旧日志）。
    /// 子任务由 RuntimeSubagent 设为「父工具集去掉 delegate」。
    #[serde(default)]
    pub tool_allowlist: Option<Vec<String>>,
    /// 记忆注入快照（装配层在任务提交时算好；子任务恒为 None，完全隔离）。
    /// `Some` 且文本非空时拼进 system 固定区；随事件/日志可复现当时注入了什么。
    #[serde(default)]
    pub memory_context: Option<crate::memory::MemoryContext>,
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
            resume_from: None,
            resume_messages: None,
            parent_id: None,
            depth: 0,
            tool_allowlist: None,
            memory_context: None,
        }
    }

    /// 工具是否对本任务可见/可执行。None = 全放行。
    pub fn allows_tool(&self, name: &str) -> bool {
        self.tool_allowlist
            .as_ref()
            .is_none_or(|list| list.iter().any(|n| n == name))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_allowlist_defaults_to_unrestricted_and_filters_when_set() {
        let mut t = TaskSpec::new("t", "/tmp/ws");
        assert!(t.allows_tool("anything"));
        t.tool_allowlist = Some(vec!["file_read".into(), "delegate".into()]);
        assert!(t.allows_tool("file_read"));
        assert!(!t.allows_tool("shell_exec"));
    }

    #[test]
    fn legacy_task_spec_without_allowlist_deserializes() {
        // 用真实 JSON 形态：旧日志里没有这个字段
        let json = serde_json::json!({
            "id": "t-1",
            "title": "老任务",
            "prompt": "hi",
            "tier": "L2",
            "workspace": "/tmp/ws",
            "model": null,
            "max_iterations": 25,
            "budget_usd": null
        });
        let t: TaskSpec = serde_json::from_value(json).expect("旧日志仍可反序列化");
        assert!(t.tool_allowlist.is_none());
        assert!(t.memory_context.is_none());
        assert!(t.allows_tool("shell_exec"));
    }

    #[test]
    fn memory_context_survives_serde_roundtrip() {
        let mut t = TaskSpec::new("t", "/tmp/ws");
        t.memory_context = Some(crate::memory::MemoryContext {
            text: "【记忆 · 用户级】端口 7890\n".into(),
            injected_ids: vec![crate::memory::MemoryId("m-1".into())],
            truncated: 1,
        });
        let s = serde_json::to_string(&t).unwrap();
        let back: TaskSpec = serde_json::from_str(&s).unwrap();
        let mc = back.memory_context.expect("快照应保留");
        assert_eq!(mc.truncated, 1);
        assert!(mc.text.contains("端口 7890"));
    }
}
