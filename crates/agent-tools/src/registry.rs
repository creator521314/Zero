//! 工具注册表与默认工具集。

use std::collections::HashMap;
use std::sync::Arc;

use agent_core::provider::ToolDef;
use agent_core::tool::Tool;

use crate::finish::Finish;
use crate::fs_tools::{FileEdit, FileRead, FileWrite, ListDir};
use crate::shell::ShellExec;
use crate::web::{SearchBackend, WebFetch, WebSearch};

/// 工具注册表：按名索引，可注入自定义工具。
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.tools.keys().cloned().collect();
        v.sort();
        v
    }

    /// 导出为 LLM function-calling 用的工具定义。
    pub fn defs(&self) -> Vec<ToolDef> {
        let mut defs: Vec<ToolDef> = self
            .tools
            .values()
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect();
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs
    }
}

/// 组装一期默认工具集。
///
/// `search_backends` 由 [`SearchBackend::resolve`] 产出：为空表示配置了关闭搜索，
/// 此时 `web_search` 仍在工具列表里但只会返回一条明确的「已关闭」提示。
pub fn default_tools(client: reqwest::Client, search_backends: Vec<SearchBackend>) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(FileRead));
    reg.register(Arc::new(FileWrite));
    reg.register(Arc::new(FileEdit));
    reg.register(Arc::new(ListDir));
    reg.register(Arc::new(ShellExec));
    reg.register(Arc::new(WebFetch::new(client.clone())));
    reg.register(Arc::new(WebSearch::new(client, search_backends)));
    reg.register(Arc::new(Finish));
    reg
}
