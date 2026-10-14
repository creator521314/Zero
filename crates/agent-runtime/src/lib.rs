//! agent-runtime —— Agent 执行核心：主循环 + 上下文管理 + 工具调度 + 权限裁决。

pub mod context;
pub mod runner;

pub use context::ContextManager;
pub use runner::{AgentRuntime, RuntimeDeps};
