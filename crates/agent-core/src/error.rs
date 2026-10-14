//! 统一错误类型与错误信息（可跨线程传递、可序列化落盘）。

use serde::{Deserialize, Serialize};

/// Agent 顶层错误。
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("provider error: {0}")]
    Provider(#[from] crate::provider::LlmError),
    #[error("tool error: {0}")]
    Tool(#[from] crate::tool::ToolError),
    #[error("store error: {0}")]
    Store(#[from] crate::store::StoreError),
    #[error("permission denied: {0}")]
    Denied(String),
    #[error("task cancelled")]
    Cancelled,
    #[error("iteration limit exceeded ({0})")]
    IterationLimit(usize),
    #[error("budget exceeded: {0}")]
    BudgetExceeded(String),
    /// 配置错误（启动期：反序列化失败、字段缺失、provider 不存在等）。
    #[error("config error: {0}")]
    Config(String),
    #[error("internal error: {0}")]
    Internal(String),
}

/// 可序列化的错误快照，用于写入事件总线与落盘。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorInfo {
    pub kind: String,
    pub message: String,
}

impl ErrorInfo {
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
        }
    }
}

impl From<&AgentError> for ErrorInfo {
    fn from(e: &AgentError) -> Self {
        let kind = match e {
            AgentError::Provider(_) => "provider",
            AgentError::Tool(_) => "tool",
            AgentError::Store(_) => "store",
            AgentError::Denied(_) => "denied",
            AgentError::Cancelled => "cancelled",
            AgentError::IterationLimit(_) => "iteration_limit",
            AgentError::BudgetExceeded(_) => "budget_exceeded",
            AgentError::Config(_) => "config",
            AgentError::Internal(_) => "internal",
        };
        ErrorInfo::new(kind, e.to_string())
    }
}
