//! 事件总线 —— 全系统唯一解耦机制。
//!
//! Agent 执行侧通过 [`EventBus`] 广播 [`Event`]；TUI 渲染、jsonl 落盘、tracing 桥接
//! 各自订阅，互不阻塞。外界通过 [`Command`] 反向驱动 Agent。

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::error::ErrorInfo;
use crate::ids::{ApprovalId, TaskId, ToolCallId};
use crate::perm::ApprovalRequest;
use crate::provider::{Message, ToolCall, Usage};
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
    /// 每轮送给模型的**完整上下文快照**（system 固定区 + 全部历史消息）。
    ///
    /// 复盘时最关键的一条：模型是依据这些材料做出的判断。
    /// 只服务审计，不进对话流（TUI 忽略）。
    ContextSnapshot {
        id: TaskId,
        /// 第几轮（从 0 开始）。
        iteration: u32,
        model: String,
        messages: Vec<Message>,
        /// 本轮可用的工具名。
        tools: Vec<String>,
    },
    /// 模型本轮的完整回复（正文 + 工具调用 + 用量）。
    ///
    /// 与 `TextDelta` 内容重合，但一行可还原整轮，且流被中断时也有部分留存。
    AssistantTurn {
        id: TaskId,
        iteration: u32,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        calls: Vec<ToolCall>,
        usage: Usage,
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
        /// 工具返回给模型的原文（超大输出已外置，这里留的是摘要 + 路径）。
        /// 未执行（被拒绝 / 被拦截）时为 `None`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
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
    /// 历史任务列表（TUI `/tasks` 与任务面板的数据源）。
    ///
    /// 由编排层读 store 后回灌：TUI 不依赖 `agent-store`，只负责显示。
    TaskList {
        tasks: Vec<crate::store::TaskSummary>,
    },
    /// 切到某个历史任务：带上它的转写事件，供界面把对话区还原成当时的样子。
    ///
    /// `events` 只含与「对话长什么样」有关的事件（任务开始/工具调用/整轮回复/
    /// 收尾），已按时间顺序排好；GUI 侧按与实时事件相同的规则回放。
    TaskOpened {
        id: TaskId,
        title: String,
        events: Vec<Event>,
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
    /// 批准一次工具调用。`remember` 为 true 表示「本任务同类操作一并批准」
    /// —— 对应审批弹层的「批准本任务同类操作」（`a` 键）。
    Approve {
        approval_id: ApprovalId,
        remember: bool,
    },
    /// 拒绝，并可附带给模型的原因说明。
    Reject {
        approval_id: ApprovalId,
        reason: String,
    },
    Cancel(TaskId),
    /// 列出历史任务（TUI `/tasks` 与任务面板）。由编排层读 store 后把结果回灌成
    /// [`Event::TaskList`]。
    ListTasks,
    /// 切到某个历史任务：参数为任务 id（或唯一前缀）。
    ///
    /// 编排层回灌 [`Event::TaskOpened`]（转写事件 + 续跑上下文），界面据此把对话区
    /// 还原成该任务当时的样子；之后用户再发指令，即以同一任务 id 携带这份上下文
    /// 继续跑（`TaskSpec::resume_from`）。
    ResumeTask(String),
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
            output: None,
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

    #[test]
    fn tool_call_end_keeps_tool_output() {
        let ev = Event::ToolCallEnd {
            id: TaskId::new(),
            call_id: ToolCallId::new(),
            outcome: ToolOutcome::Failed,
            ms: 3,
            detail: Some("系统找不到指定的路径".into()),
            output: Some("[error] 系统找不到指定的路径".into()),
        };
        let s = serde_json::to_string(&ev).expect("应可序列化");
        match serde_json::from_str::<Event>(&s).expect("应可反序列化") {
            Event::ToolCallEnd { output, .. } => {
                assert_eq!(output.as_deref(), Some("[error] 系统找不到指定的路径"));
            }
            other => panic!("应为 ToolCallEnd: {other:?}"),
        }
    }

    #[test]
    fn tool_call_end_accepts_legacy_payload_without_output() {
        let legacy = r#"{
            "type": "tool_call_end",
            "id": "t-1",
            "call_id": "c-1",
            "outcome": "failed",
            "ms": 9,
            "detail": "boom"
        }"#;
        match serde_json::from_str::<Event>(legacy).expect("旧日志应可反序列化") {
            Event::ToolCallEnd { output, .. } => assert!(output.is_none(), "缺省应兜底为 None"),
            other => panic!("应为 ToolCallEnd: {other:?}"),
        }
    }

    #[test]
    fn context_snapshot_roundtrip_keeps_full_messages() {
        let ev = Event::ContextSnapshot {
            id: TaskId::new(),
            iteration: 2,
            model: "deepseek-flash".into(),
            messages: vec![
                Message::system("你是个人助理"),
                Message::user("介绍下本项目"),
                Message::assistant("先看看工作区"),
            ],
            tools: vec!["list_dir".into(), "file_read".into()],
        };
        let s = serde_json::to_string(&ev).expect("应可序列化");
        match serde_json::from_str::<Event>(&s).expect("应可反序列化") {
            Event::ContextSnapshot {
                iteration,
                model,
                messages,
                tools,
                ..
            } => {
                assert_eq!(iteration, 2);
                assert_eq!(model, "deepseek-flash");
                assert_eq!(messages.len(), 3);
                assert_eq!(messages[0].text(), "你是个人助理", "system 必须留下");
                assert_eq!(tools, vec!["list_dir", "file_read"]);
            }
            other => panic!("应为 ContextSnapshot: {other:?}"),
        }
    }

    #[test]
    fn assistant_turn_roundtrip_keeps_text_and_calls() {
        let ev = Event::AssistantTurn {
            id: TaskId::new(),
            iteration: 1,
            text: "我来列一下目录".into(),
            calls: vec![ToolCall {
                id: ToolCallId::new(),
                name: "list_dir".into(),
                arguments: serde_json::json!({ "path": "." }),
            }],
            usage: Usage {
                input_tokens: 100,
                output_tokens: 20,
                reasoning_tokens: None,
                cached_tokens: None,
            },
        };
        let s = serde_json::to_string(&ev).expect("应可序列化");
        match serde_json::from_str::<Event>(&s).expect("应可反序列化") {
            Event::AssistantTurn {
                text, calls, usage, ..
            } => {
                assert_eq!(text, "我来列一下目录");
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "list_dir");
                assert_eq!(usage.total(), 120);
            }
            other => panic!("应为 AssistantTurn: {other:?}"),
        }
    }
}
