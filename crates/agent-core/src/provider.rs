//! LLM Provider 抽象层 —— 统一各厂商差异。
//!
//! 业务层只认本模块的类型；厂商差异（OpenAI 的 tool_calls、Anthropic 的 tool_use
//! 块、system 位置、流式增量格式）全部封在 adapter 内。

use std::pin::Pin;

use futures::Stream;
use serde::{Deserialize, Serialize};

use crate::ids::ToolCallId;

/// 流式事件流类型别名。
pub type BoxStream<T> = Pin<Box<dyn Stream<Item = T> + Send + 'static>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// 多模态内容片段（一期只实现 Text，其余为二期预留）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    Image {
        url: Option<String>,
        b64: Option<String>,
        media_type: String,
    },
    // Audio / File 后续扩展
}

impl ContentPart {
    pub fn text(s: impl Into<String>) -> Self {
        ContentPart::Text { text: s.into() }
    }
}

/// 一次工具调用请求（由 assistant 发起）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: String,
    /// 统一为已解析 JSON：OpenAI 返回字符串、Anthropic 返回对象，adapter 负责抹平。
    pub arguments: serde_json::Value,
}

/// 对外统一的工具定义（供 LLM function-calling）。
/// name / description / JSON-Schema 语义与 MCP 对齐，便于二期零改动接入 MCP。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// 对话消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentPart>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// tool 结果回填时关联的 assistant tool_call id。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<ToolCallId>,
    /// 思考链（DeepSeek-R1 / Claude thinking），独立字段不混入正文。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self::plain(Role::System, text)
    }
    pub fn user(text: impl Into<String>) -> Self {
        Self::plain(Role::User, text)
    }
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::plain(Role::Assistant, text)
    }

    fn plain(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentPart::text(text)],
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning: None,
        }
    }

    /// 取全部文本片段拼接（用于渲染与摘要）。
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

/// 请求元信息：用于路由与计量。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestMeta {
    pub task_id: Option<String>,
    /// 期望的路由提示（如 "code" / "sensitive"）。
    pub route_hint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub stream: bool,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub metadata: RequestMeta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    Other,
}

/// 统一计量（各家字段名不同，adapter 归一）。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
    #[serde(default)]
    pub cached_tokens: Option<u64>,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub message: Message,
    pub finish_reason: FinishReason,
    pub usage: Usage,
    pub provider: String,
    pub model_returned: String,
}

/// 流式事件（统一事件流）。
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Start {
        provider: String,
        model: String,
    },
    TextDelta(String),
    /// 思考链增量，TUI 灰显。
    ReasoningDelta(String),
    /// 工具调用参数增量：累积到 Done 后整体 parse（不边收边 parse）。
    ToolCallDelta {
        index: usize,
        id: Option<ToolCallId>,
        name: Option<String>,
        args_delta: String,
    },
    Usage(Usage),
    Done(FinishReason),
    Error(LlmError),
}

/// Provider 能力声明：上层据此降级，不硬编码厂商差异。
#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    pub streaming: bool,
    pub tools: bool,
    pub parallel_tools: bool,
    pub vision: bool,
    pub reasoning: bool,
    pub json_mode: bool,
    pub max_context: u32,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            streaming: true,
            tools: true,
            parallel_tools: false,
            vision: false,
            reasoning: false,
            json_mode: false,
            max_context: 128_000,
        }
    }
}

/// 定价信息，用于成本估算。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ModelPrice {
    pub input_per_1k: f64,
    pub output_per_1k: f64,
}

/// 统一错误类型。
#[derive(Debug, Clone, thiserror::Error)]
pub enum LlmError {
    #[error("network error: {0}")]
    Network(String),
    #[error("auth error: {0}")]
    Auth(String),
    #[error("rate limited, retry after {retry_after:?}s")]
    RateLimited { retry_after: Option<u64> },
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("server error {status}: {body}")]
    ServerError { status: u16, body: String },
    #[error("timeout")]
    Timeout,
    #[error("stream broken: {0}")]
    StreamBroken(String),
    #[error("budget exceeded: {0}")]
    BudgetExceeded(String),
}

impl LlmError {
    /// 是否值得重试（网络/限流/服务端错误可重试；鉴权/参数错误不可）。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            LlmError::Network(_)
                | LlmError::RateLimited { .. }
                | LlmError::ServerError { .. }
                | LlmError::Timeout
                | LlmError::StreamBroken(_)
        )
    }
}

/// Provider trait（冻结签名）。
#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> Capabilities;

    /// 非流式调用。
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError>;

    /// 流式调用，返回统一事件流。
    async fn chat_stream(
        &self,
        req: ChatRequest,
    ) -> Result<BoxStream<Result<StreamEvent, LlmError>>, LlmError>;

    /// 定价查询，用于成本估算。无定价信息返回 None。
    fn price(&self, model: &str) -> Option<ModelPrice>;
}
