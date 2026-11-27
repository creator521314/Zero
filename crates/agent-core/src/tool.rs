//! 工具系统边界。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

use crate::perm::Risk;
use tokio_util::sync::CancellationToken;

/// 工具执行上下文：提供沙箱根、超时与取消令牌。
#[derive(Clone)]
pub struct ToolCtx {
    /// 每任务私有工作区：外置输出等系统数据落在里面，不受项目根影响。
    pub workspace: PathBuf,
    /// 项目根目录（可选）：工具的读写沙箱，相对路径以它为基准。
    ///
    /// 为空时沙箱就是 `workspace`（每任务一个空目录）。
    pub project_root: Option<PathBuf>,
    pub task_id: String,
    pub timeout: Duration,
    pub cancel: CancellationToken,
}

impl ToolCtx {
    /// 工具沙箱根：项目根优先，否则私有工作区。
    pub fn sandbox_root(&self) -> PathBuf {
        self.project_root
            .clone()
            .unwrap_or_else(|| self.workspace.clone())
    }

    /// 允许访问的根集合。
    ///
    /// 私有工作区始终在内：否则外置到 `ctx/` 的内容工具自己也读不回来。
    pub fn allowed_roots(&self) -> Vec<PathBuf> {
        let sandbox = self.sandbox_root();
        let mut roots = vec![sandbox.clone()];
        if self.workspace != sandbox {
            roots.push(self.workspace.clone());
        }
        roots
    }
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
    /// 稳定错误码（仅 is_error=true 时有意义）。旧日志没有 → None。
    #[serde(default)]
    pub error_code: Option<ToolErrorCode>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            side_effects: Vec::new(),
            error_code: None,
        }
    }
    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            side_effects: Vec::new(),
            error_code: None,
        }
    }
    /// 带稳定错误码的失败。content 里**不要**自己写 `[error]` 前缀：
    /// execute_tool 会统一加，模型可见内容再替换成 `[error:<code>]`。
    pub fn err_code(code: ToolErrorCode, content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            side_effects: Vec::new(),
            error_code: Some(code),
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

/// 稳定的工具错误码。
///
/// 模型可以据此采取不同策略：timeout 换策略重试、invalid_args 修参数、
/// denied / approval_rejected 不要再试、not_permitted 换工具。
/// 不要在这些码里塞人类可读的中文说明——说明放 message。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorCode {
    InvalidArgs,
    PathEscape,
    Io,
    Timeout,
    Cancelled,
    Blocked,
    ToolFailed,
    UnknownTool,
    MalformedArgs,
    NotPermitted,
    Denied,
    ApprovalRejected,
    UnsupportedOption,
    DelegationFailed,
}

impl ToolErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgs => "invalid_args",
            Self::PathEscape => "path_escape",
            Self::Io => "io",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Blocked => "blocked",
            Self::ToolFailed => "tool_failed",
            Self::UnknownTool => "unknown_tool",
            Self::MalformedArgs => "malformed_args",
            Self::NotPermitted => "not_permitted",
            Self::Denied => "denied",
            Self::ApprovalRejected => "approval_rejected",
            Self::UnsupportedOption => "unsupported_option",
            Self::DelegationFailed => "delegation_failed",
        }
    }
}

impl std::fmt::Display for ToolErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ToolError {
    /// 稳定错误码映射：新增 ToolError 变体时必须同步这里（match 是穷尽的，
    /// 编译器会强制你补）。
    pub fn code(&self) -> ToolErrorCode {
        match self {
            Self::InvalidArgs(_) => ToolErrorCode::InvalidArgs,
            Self::PathEscape(_) => ToolErrorCode::PathEscape,
            Self::Io(_) => ToolErrorCode::Io,
            Self::Timeout => ToolErrorCode::Timeout,
            Self::Cancelled => ToolErrorCode::Cancelled,
            Self::Blocked(_) => ToolErrorCode::Blocked,
            Self::Failed(_) => ToolErrorCode::ToolFailed,
        }
    }
}

/// 「模型发来的参数不是合法 JSON 对象」的标记。
///
/// 收口成类型的原因：原先 `__malformed_args_error` 是个散落在 runner 里的
/// 魔法字符串，任何想判断「这次调用参数坏了」的代码都得自己拼这个键。
#[derive(Debug, Clone, Copy)]
pub struct MalformedArgs;

impl MalformedArgs {
    pub const KEY: &'static str = "__malformed_args_error";

    pub fn marker(reason: impl Into<String>) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        m.insert(
            Self::KEY.to_string(),
            serde_json::Value::String(reason.into()),
        );
        serde_json::Value::Object(m)
    }

    pub fn reason(args: &serde_json::Value) -> Option<&str> {
        args.get(Self::KEY).and_then(|v| v.as_str())
    }
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
    /// 是否是「需要用户参与」的交互工具（如 `ask_user`）。
    ///
    /// runtime 对这类工具跳过权限审批：问一句话还要先弹一次批准框是纯粹的骚扰，
    /// 而且审批弹层与问答弹层会互相抢模态。默认 `false`，既有工具行为不变。
    fn is_interactive(&self) -> bool {
        false
    }
    /// 是否是「终止信号」工具：调用后执行循环应立即结束并汇总产物。
    ///
    /// 默认 `false`；`finish` 等显式收尾工具覆盖为 `true`。
    fn is_terminating(&self) -> bool {
        false
    }
    /// 执行。
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_error_codes_are_stable() {
        assert_eq!(ToolError::Timeout.code().as_str(), "timeout");
        assert_eq!(
            ToolError::InvalidArgs("x".into()).code(),
            ToolErrorCode::InvalidArgs
        );
        assert_eq!(
            ToolErrorCode::UnsupportedOption.as_str(),
            "unsupported_option"
        );
        assert_eq!(format!("{}", ToolErrorCode::NotPermitted), "not_permitted");
        let out = ToolOutput::err_code(ToolErrorCode::Blocked, "被挡了");
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ToolErrorCode::Blocked));
    }

    #[test]
    fn malformed_args_roundtrip() {
        let v = MalformedArgs::marker("bad json");
        assert_eq!(MalformedArgs::reason(&v), Some("bad json"));
        assert_eq!(
            MalformedArgs::reason(&serde_json::json!({"path": "a.txt"})),
            None,
            "正常参数不能被误判为 malformed"
        );
        assert_eq!(v[MalformedArgs::KEY], serde_json::json!("bad json"));
    }
}
