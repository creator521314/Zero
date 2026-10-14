//! agent-providers —— LLM Provider 抽象的实现层。
//!
//! 一期策略：只写两个 adapter 即可覆盖多家厂商——
//! `openai_compat`（DeepSeek / 通义 / OpenAI / Ollama 通用）+（可选）`anthropic`。

pub mod config;
pub mod openai_compat;
pub mod router;
pub mod wizard;

pub use config::{Budget, ConfigError, ProviderConfig, ProvidersFile, RouterConfig};
pub use openai_compat::OpenAiCompatProvider;
pub use router::ProviderRouter;
pub use wizard::{ensure_loaded, run_first_time, run_invalid};
