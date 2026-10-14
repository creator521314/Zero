//! agent-core —— 领域核心枢纽。
//!
//! 本 crate 定义全系统的事件总线与七大 trait 边界，**不含任何 IO 实现**。
//! 依赖铁律：本 crate 永不依赖 ratatui / reqwest / 具体数据库。
//! 二期换传输层、三期换运行环境时，只需替换叶子 crate，核心零改动。

pub mod error;
pub mod event;
pub mod frontend;
pub mod ids;
pub mod perm;
pub mod provider;
pub mod store;
pub mod sync;
pub mod task;
pub mod tool;

pub use error::{AgentError, ErrorInfo};
pub use event::{Command, Event, EventBus, Outcome};
pub use frontend::Frontend;
pub use ids::{SessionId, TaskId, ToolCallId};
pub use perm::{ApprovalRequest, ApprovalResult, Decision, PermissionEngine, Risk};
pub use provider::{
    Capabilities, ChatRequest, ChatResponse, ContentPart, FinishReason, LlmError, LlmProvider,
    Message, ModelPrice, Role, StreamEvent, ToolCall, ToolDef, Usage,
};
pub use store::{Clock, Session, Store, StoreError, StoredEvent, TaskSummary};
pub use sync::{SyncEndpoint, SyncError};
pub use task::{AgentRunner, RunControl, TaskSpec, TaskState};
pub use tool::{Tool, ToolCtx, ToolError, ToolOutput};
