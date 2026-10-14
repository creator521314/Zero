//! 事件总线 —— 全系统唯一解耦机制。
//!
//! Agent 执行侧通过 [`EventBus`] 广播 [`Event`]；TUI 渲染、jsonl 落盘、tracing 桥接
//! 各自订阅，互不阻塞。外界通过 [`Command`] 反向驱动 Agent。

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::error::ErrorInfo;
use crate::ids::{ApprovalId, TaskId, ToolCallId};
use crate::perm::ApprovalRequest;
use crate::task::TaskSpec;

/// Agent → 外界的广播事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    TaskStarted {
        id: TaskId,
        title: String,
    },
    /// 流式正文增量。
    TextDelta {
        id: TaskId,
        text: String,
    },
    /// 思考链增量（TUI 灰显，不混入正文）。
    ReasoningDelta {
        id: TaskId,
        text: String,
    },
    ToolCallStart {
        id: TaskId,
        call_id: ToolCallId,
        name: String,
        args: serde_json::Value,
    },
    ToolCallEnd {
        id: TaskId,
        call_id: ToolCallId,
        ok: bool,
        ms: u64,
    },
    /// 等待用户审批（L3 逐步确认 / 高危拦截）。
    AwaitingApproval {
        id: TaskId,
        approval_id: ApprovalId,
        req: ApprovalRequest,
    },
    TaskFinished {
        id: TaskId,
        outcome: Outcome,
    },
    Error {
        id: TaskId,
        err: ErrorInfo,
    },
}

/// 任务最终结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Success {
        summary: String,
        artifacts: Vec<String>,
    },
    Failed {
        reason: String,
    },
    Cancelled,
    Paused,
}

/// 外界 → Agent 的命令。
#[derive(Debug, Clone)]
pub enum Command {
    SubmitTask(TaskSpec),
    Approve(ApprovalId),
    /// 拒绝，并可附带给模型的原因说明。
    Reject {
        approval_id: ApprovalId,
        reason: String,
    },
    Cancel(TaskId),
    Quit,
}

/// 广播总线。容量 1024；慢订阅者会收到 `Lagged` 错误而不阻塞发送方。
pub type EventBus = broadcast::Sender<Event>;

/// 创建一条事件总线。
pub fn event_bus() -> EventBus {
    broadcast::channel(1024).0
}
