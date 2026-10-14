//! agent-providers —— LLM Provider 抽象的实现层。
//!
//! 一期策略：只写两个 adapter 即可覆盖多家厂商——
//! `openai_compat`（DeepSeek / 通义 / OpenAI / Ollama 通用）+（可选）`anthropic`。

pub mod config;
pub mod openai_compat;
pub mod router;

pub use config::{Budget, ProviderConfig, ProvidersFile, RouterConfig};
pub use openai_compat::OpenAiCompatProvider;
pub use router::ProviderRouter;
