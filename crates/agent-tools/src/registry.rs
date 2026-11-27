//! 工具注册表与默认工具集。

use std::collections::HashMap;
use std::sync::Arc;

use agent_core::provider::ToolDef;
use agent_core::question::UserQuestionPort;
use agent_core::subagent::SubagentPort;
use agent_core::tool::Tool;

use crate::delegate::DelegateTool;
use crate::finish::Finish;
use crate::fs_tools::{FileEdit, FileRead, FileRestore, FileWrite, ListDir};
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

    /// 显式注入委派工具。
    ///
    /// 刻意不放进 `default_tools()`：冒烟测试走生产装配但没有 port，
    /// 默认注册会让「每个工具都能被真实调用」的契约用例失去意义。
    /// 生产装配（agent-cli / agent-server）在构造注册表后调用本方法。
    pub fn register_delegate(&mut self, port: Arc<dyn SubagentPort>) {
        self.register(Arc::new(DelegateTool::new(port)));
    }

    /// 显式注入交互式问答工具。
    ///
    /// 不放进 `default_tools()`：headless 服务没有前端回答，注册了只会让模型
    /// 用一个永远等不到应答的工具。只有装配了 TUI 的入口才调用本方法。
    pub fn register_question(&mut self, port: Arc<dyn UserQuestionPort>) {
        self.register(Arc::new(crate::ask_user::AskUserTool::new(port)));
    }

    /// 显式注入记忆检索工具（同 `register_delegate` 的理由）。
    pub fn register_memory(
        &mut self,
        index: Arc<dyn agent_core::memory::MemoryIndex>,
        store: Arc<dyn agent_core::memory::MemoryStore>,
        injected: crate::memory_search::InjectedRegistry,
    ) {
        self.register(Arc::new(crate::memory_search::MemorySearchTool::new(
            index, store, injected,
        )));
    }
}

/// 组装一期默认工具集。
///
/// `search_backends` 由 [`SearchBackend::resolve`] 产出：为空表示配置了关闭搜索，
/// 此时 `web_search` 仍在工具列表里但只会返回一条明确的「已关闭」提示。
///
/// 网络类工具统一使用**守卫客户端**（禁自动重定向，配合
/// `agent_net::guarded_get` 做逐跳 SSRF 校验）；LLM provider 等可信端点
/// 仍走普通 [`agent_net::build_client`]，两者互不影响。
pub fn default_tools(search_backends: Vec<SearchBackend>) -> ToolRegistry {
    let client = agent_net::build_guarded_client(&agent_net::HttpClientConfig::default())
        .expect("守卫 HTTP 客户端构建失败");
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(FileRead));
    reg.register(Arc::new(FileWrite));
    reg.register(Arc::new(FileEdit));
    reg.register(Arc::new(FileRestore));
    reg.register(Arc::new(ListDir));
    reg.register(Arc::new(ShellExec));
    reg.register(Arc::new(WebFetch::new(client.clone())));
    reg.register(Arc::new(WebSearch::new(client, search_backends)));
    reg.register(Arc::new(Finish));
    reg
}
