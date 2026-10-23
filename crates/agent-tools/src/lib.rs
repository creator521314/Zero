//! agent-tools —— 工具实现集合。
//!
//! 所有工具都受工作区约束：路径经 realpath 校验后必须落在 [`ToolCtx::workspace`] 内，
//! 防 `../` 逃逸。`shell_exec` 采用参数数组执行（不走 `sh -c` 拼接），降低注入面。

pub mod finish;
pub mod fs_tools;
pub mod paths;
pub mod registry;
pub mod shell;
pub mod web;

pub use registry::{default_tools, ToolRegistry};
pub use web::SearchBackend;
