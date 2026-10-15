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
        /// 终结状态。比「成功 / 失败」二值更细：能区分「跑了但失败」与
        /// 「压根没跑」（被权限规则或用户拦下），供 TUI 分色与展示原因。
        #[serde(default)]
        outcome: ToolOutcome,
        /// 执行耗时（毫秒）；未执行时为 0。
        ms: u64,
        /// 失败或拒绝的原因；成功时为 `None`。
        #[serde(default)]
        detail: Option<String>,
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

/// 工具调用的终结状态（[`Event::ToolCallEnd`] 的细分）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    /// 正常返回。
    #[default]
    Ok,
    /// 执行出错（工具自报错误或运行时异常）。
    Failed,
    /// 被权限规则拒绝，未执行。
    Denied,
    /// 被用户拒绝，未执行。
    Rejected,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_end_roundtrip_keeps_outcome_and_detail() {
        let ev = Event::ToolCallEnd {
            id: TaskId::new(),
            call_id: ToolCallId::new(),
            outcome: ToolOutcome::Denied,
            ms: 0,
            detail: Some("命中高危规则".into()),
        };
        let s = serde_json::to_string(&ev).expect("应可序列化");
        match serde_json::from_str::<Event>(&s).expect("应可反序列化") {
            Event::ToolCallEnd {
                outcome, detail, ..
            } => {
                assert_eq!(outcome, ToolOutcome::Denied);
                assert_eq!(detail.as_deref(), Some("命中高危规则"));
            }
            other => panic!("应为 ToolCallEnd: {other:?}"),
        }
    }

    #[test]
    fn tool_call_end_accepts_legacy_payload_without_new_fields() {
        // P1 之前的审计日志没有 outcome / detail，且残留 ok 字段；
        // 必须仍能反序列化，否则历史 jsonl 会整体读取失败。
        let legacy = r#"{
            "type": "tool_call_end",
            "id": "t-1",
            "call_id": "c-1",
            "ok": true,
            "ms": 12
        }"#;
        match serde_json::from_str::<Event>(legacy).expect("旧日志应可反序列化") {
            Event::ToolCallEnd {
                outcome,
                ms,
                detail,
                ..
            } => {
                assert_eq!(outcome, ToolOutcome::Ok, "缺省应兜底为 Ok");
                assert_eq!(ms, 12);
                assert!(detail.is_none(), "缺省应兜底为 None");
            }
            other => panic!("应为 ToolCallEnd: {other:?}"),
        }
    }
}
