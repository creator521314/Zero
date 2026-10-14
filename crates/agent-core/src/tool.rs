//! 工具系统边界。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

use crate::perm::Risk;
use tokio_util::sync::CancellationToken;

/// 工具执行上下文：提供工作区根、超时与取消令牌。
#[derive(Clone)]
pub struct ToolCtx {
    /// 工作区根目录（工具只能在其中读写；shell 的 cwd 锁定于此）。
    pub workspace: PathBuf,
    pub task_id: String,
    pub timeout: Duration,
    pub cancel: CancellationToken,
}

/// 工具产出的一个文件（用于产物登记与审计）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub path: String,
    pub bytes: u64,
}

/// 工具执行结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    /// 回填给 LLM 的文本。
    pub content: String,
    pub is_error: bool,
    #[serde(default)]
    pub side_effects: Vec<Artifact>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            side_effects: Vec::new(),
        }
    }
    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            side_effects: Vec::new(),
        }
    }
    pub fn with_artifacts(mut self, a: Vec<Artifact>) -> Self {
        self.side_effects = a;
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    #[error("path outside workspace: {0}")]
    PathEscape(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("timed out")]
    Timeout,
    #[error("cancelled")]
    Cancelled,
    #[error("blocked by policy: {0}")]
    Blocked(String),
    #[error("execution failed: {0}")]
    Failed(String),
}

/// 工具 trait。name/description/JSON-Schema 与 MCP 语义对齐，便于二期接入 MCP。
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema，供 LLM 生成 tool_calls。
    fn parameters(&self) -> serde_json::Value;
    /// 静态风险等级，供权限引擎初筛。
    fn risk_level(&self) -> Risk;
    /// 执行。
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError>;
}
