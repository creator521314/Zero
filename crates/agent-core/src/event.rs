//! 事件总线 —— 全系统唯一解耦机制。
//!
//! Agent 执行侧通过 [`EventBus`] 广播 [`Event`]；TUI 渲染、jsonl 落盘、tracing 桥接
//! 各自订阅，互不阻塞。外界通过 [`Command`] 反向驱动 Agent。

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::error::ErrorInfo;
use crate::ids::{ApprovalId, QuestionId, TaskId, ToolCallId};
use crate::perm::ApprovalRequest;
use crate::provider::{Message, ToolCall, Usage};
use crate::question::{QuestionAnswer, QuestionRequest};
use crate::task::TaskSpec;

/// Agent → 外界的广播事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    TaskStarted {
        id: TaskId,
        title: String,
    },
    /// 用户本次提交的输入原文（已过密钥脱敏）。
    ///
    /// v1.2：用户输入此前**从不落盘**——`TaskStarted` 只带冻结的标题，
    /// `/resume` / 切任务 / 重启后回放出的对话里完全没有用户说过什么。
    /// 独立成事件而不是塞进 `TaskStarted`：任务生命周期与「用户说了什么」
    /// 是两件事实，且旧日志里的 `task_started` 无需改动即可继续解析。
    UserPrompt {
        id: TaskId,
        text: String,
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
    /// 上下文**增量**（v1.2）：自上一轮快照以来新追加到上下文的消息。
    ///
    /// 旧实现每轮把 `cm.messages()` 全量落盘，25 轮长任务的日志按 O(N²)
    /// 膨胀，且续跑、提炼、回放共用这一份全量数据，语义互相打架。
    /// 现在只记增量：首轮 `added` 含「固定 system + 首轮 user」，其后每轮
    /// 只含上一轮的 assistant / tool 结果 / 新指令。
    ///
    /// `reset = true` 表示压缩重写了历史，此时 `added` 是**压缩后的完整基线**，
    /// 重建方应清空已有累积再整份替换。
    ContextSnapshot {
        id: TaskId,
        /// 第几轮（从 0 开始）。
        iteration: u32,
        model: String,
        added: Vec<Message>,
        /// 压缩导致历史被重写（added 为全量基线，而非增量）。
        #[serde(default)]
        reset: bool,
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
    /// 等待用户审批（L3 逐步确认 / 高危拦截 / L2 审视拿不准）。
    AwaitingApproval {
        id: TaskId,
        approval_id: ApprovalId,
        req: ApprovalRequest,
    },
    /// AI 通过 `ask_user` 工具向用户提问：候选项 + 可自由编辑的回答。
    /// 与审批分开：审批是「要不要放行一次操作」，问答是「请用户给一个决策」，
    /// 两者的审计语义不同，不能混成同一个事件。
    QuestionAsked {
        id: TaskId,
        req: QuestionRequest,
    },
    /// 用户对一次问答的最终回答（含取消）。
    QuestionAnswered {
        id: TaskId,
        question_id: QuestionId,
        answer: QuestionAnswer,
    },
    /// 用户对审批请求的最终决定。
    ///
    /// 只记「请求」不记「结果」时，事后无法区分「人工批准」与「自动放行」——
    /// 真实事故里 agent 正是因此把「用户批准过」误判成「权限系统没设防」。
    ApprovalDecided {
        id: TaskId,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    },
    TaskFinished {
        id: TaskId,
        outcome: Outcome,
    },
    Error {
        id: TaskId,
        err: ErrorInfo,
    },
    /// 历史会话列表（TUI 会话面板 `/sessions` / Ctrl+G 的数据源）。
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
    /// 委派关系：谁派生了谁。子任务自己的 TaskStarted/TaskFinished 照常发，
    /// 由 TUI 按 id 过滤；本事件专门把父子关系写进 jsonl，供二期同步还原任务树。
    SubtaskLinked {
        parent_id: TaskId,
        child_id: TaskId,
        goal: String,
    },
    /// 纯界面系统通知（如 /improve 生成补丁后的路径提示）。
    ///
    /// 不落盘（recorder 返回 None）、不进审计真相源；产物文件本身就是凭证。
    System {
        message: String,
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

/// 用户对一次审批请求的决定（[`Event::ApprovalDecided`] 的负载）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// 批准执行；`remember` 表示「本任务同类操作一并批准」。
    Approved { remember: bool },
    /// 拒绝执行，并给出原因（会回填给模型）。
    Rejected { reason: String },
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
///
/// `SubmitTask` 直接按值携带 `TaskSpec`（含若干 Option<Vec<_>> 字段，体积较大）。
/// 这是 mpsc 通道里的低频消息（每次提交任务一条），boxing 会改动装配层的
/// 所有权写法而收益极小；C3 加了 `tool_allowlist` 后 TaskSpec 跨过了
/// clippy 的体积阈值，这里显式豁免而不是为了消警告去改接口。
#[allow(clippy::large_enum_variant)]
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
    /// 回答 AI 的提问。`None` 之外的答案会回填给模型；取消也是一条正式回答，
    /// 让等待中的工具立刻结束而不是无声挂起。
    AnswerQuestion {
        question_id: QuestionId,
        answer: QuestionAnswer,
    },
    Cancel(TaskId),
    /// 列出历史会话（TUI 会话面板）。由编排层读 store 后把结果回灌成
    /// [`Event::TaskList`]。
    ListTasks,
    /// 切到某个历史任务：参数为任务 id（或唯一前缀）。
    ///
    /// 编排层回灌 [`Event::TaskOpened`]（转写事件 + 续跑上下文），界面据此把对话区
    /// 还原成该任务当时的样子；之后用户再发指令，即以同一任务 id 携带这份上下文
    /// 继续跑（`TaskSpec::resume_from`）。
    ResumeTask(String),
    /// 记忆驱动的代码改进建议（TUI `/improve <目标>`）。
    ///
    /// 由编排层在任务循环外单独跑一次模型调用，产物只写 `.patch` 文件
    /// （设计 O7：不建分支、不 commit、不 push），路径经 [`Event::System`] 回给界面。
    Improve {
        goal: String,
    },
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
    fn subtask_linked_roundtrip_and_legacy_events_still_parse() {
        let legacy = r#"{"type":"task_started","id":"t-1","title":"老任务"}"#;
        assert!(matches!(
            serde_json::from_str::<Event>(legacy).expect("旧日志仍可反序列化"),
            Event::TaskStarted { .. }
        ));

        let ev = Event::SubtaskLinked {
            parent_id: TaskId::from_raw("p-1"),
            child_id: TaskId::from_raw("c-1"),
            goal: "跑一遍测试".into(),
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("subtask_linked"));
        assert!(matches!(
            serde_json::from_str::<Event>(&s).unwrap(),
            Event::SubtaskLinked { .. }
        ));
    }

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
    fn context_snapshot_roundtrip_keeps_added_messages() {
        let ev = Event::ContextSnapshot {
            id: TaskId::new(),
            iteration: 2,
            model: "deepseek-flash".into(),
            added: vec![
                Message::system("你是个人助理"),
                Message::user("介绍下本项目"),
                Message::assistant("先看看工作区"),
            ],
            reset: false,
            tools: vec!["list_dir".into(), "file_read".into()],
        };
        let s = serde_json::to_string(&ev).expect("应可序列化");
        match serde_json::from_str::<Event>(&s).expect("应可反序列化") {
            Event::ContextSnapshot {
                iteration,
                model,
                added,
                reset,
                tools,
                ..
            } => {
                assert_eq!(iteration, 2);
                assert_eq!(model, "deepseek-flash");
                assert_eq!(added.len(), 3);
                assert_eq!(added[0].text(), "你是个人助理", "system 必须留下");
                assert!(!reset, "首轮不是压缩重置");
                assert_eq!(tools, vec!["list_dir", "file_read"]);
            }
            other => panic!("应为 ContextSnapshot: {other:?}"),
        }
    }

    #[test]
    fn user_prompt_roundtrip() {
        let ev = Event::UserPrompt {
            id: TaskId::from_raw("t-1"),
            text: "列出你的长期记忆".into(),
        };
        let s = serde_json::to_string(&ev).unwrap();
        match serde_json::from_str::<Event>(&s).unwrap() {
            Event::UserPrompt { text, .. } => assert_eq!(text, "列出你的长期记忆"),
            other => panic!("应为 UserPrompt: {other:?}"),
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
