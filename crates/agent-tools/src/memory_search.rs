//! memory_search —— 按需检索长期记忆（注入双通道的通道 B）。
//!
//! 构造时注入 `MemoryIndex` / `MemoryStore` / 注入 id 注册表，不改 `ToolCtx`
//! （与 `DelegateTool` 同一模式）。受 `TaskSpec::allows_tool` 管可见性与可执行性；
//! 子任务由 `TaskSpec::allows_tool` 白名单直接排除（设计 §6）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use agent_core::memory::{
    MemoryId, MemoryIndex, MemoryQuery, MemoryScope, MemoryState, MemoryStore,
};
use agent_core::perm::Risk;
use agent_core::tool::{Tool, ToolCtx, ToolError, ToolOutput};
use async_trait::async_trait;
use serde_json::json;

/// 每任务已注入 system 固定区的记忆 id。
///
/// 设计约定：已注入的条目不再出现在检索结果里，避免同一事实重复占用上下文。
/// 装配层在任务提交时 `register`，任务结束 `unregister`（与 `RuntimeSubagent`
/// 的登记模式一致）。
#[derive(Clone, Default)]
pub struct InjectedRegistry(Arc<Mutex<HashMap<String, HashSet<MemoryId>>>>);

impl InjectedRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, task_id: &str, ids: Vec<MemoryId>) {
        self.0
            .lock()
            .expect("injected registry poisoned")
            .insert(task_id.to_string(), ids.into_iter().collect());
    }

    pub fn unregister(&self, task_id: &str) {
        self.0
            .lock()
            .expect("injected registry poisoned")
            .remove(task_id);
    }

    fn injected(&self, task_id: &str) -> HashSet<MemoryId> {
        self.0
            .lock()
            .expect("injected registry poisoned")
            .get(task_id)
            .cloned()
            .unwrap_or_default()
    }
}

/// 按需检索工具。
pub struct MemorySearchTool {
    index: Arc<dyn MemoryIndex>,
    store: Arc<dyn MemoryStore>,
    injected: InjectedRegistry,
}

impl MemorySearchTool {
    pub fn new(
        index: Arc<dyn MemoryIndex>,
        store: Arc<dyn MemoryStore>,
        injected: InjectedRegistry,
    ) -> Self {
        Self {
            index,
            store,
            injected,
        }
    }
}

fn scope_label(s: &MemoryScope) -> &'static str {
    match s {
        MemoryScope::User => "用户级",
        MemoryScope::Project { .. } => "项目级",
    }
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }

    fn description(&self) -> &str {
        "检索你的长期记忆（跨任务沉淀的用户偏好、项目约定、历史结论与环境事实）。当你需要回忆用户之前交代过的偏好、项目规则或以前任务的结论时使用。已经自动注入的背景不会重复出现在结果里。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "关键词，按记忆内容与标签匹配（大小写不敏感）"
                },
                "scope": {
                    "type": "string",
                    "enum": ["user", "project"],
                    "description": "可选：限定作用域；不传则同时查用户级与项目级"
                },
                "limit": {
                    "type": "integer",
                    "description": "可选：最多返回条数（默认 5，钳制 1..=20）"
                }
            },
            "required": ["query"]
        })
    }

    fn risk_level(&self) -> Risk {
        Risk::Low
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 query 参数".into()))?;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, 20))
            .unwrap_or(5);
        let scope_filter = args.get("scope").and_then(|v| v.as_str());

        // 作用域：默认用户级 + 项目级（ctx.project_root）；显式限定则只查一个。
        let mut hits: Vec<agent_core::memory::MemoryEntry> = Vec::new();
        let mut run = |scope: MemoryScope| {
            if scope_filter.is_some() && !scope_filter_matches(scope_filter, &scope) {
                return;
            }
            let q = MemoryQuery {
                keyword: Some(query.clone()),
                scope: Some(scope),
                state: Some(MemoryState::Active),
                limit,
            };
            hits.extend(self.index.search(q));
        };
        run(MemoryScope::User);
        if let Some(root) = &ctx.project_root {
            run(MemoryScope::Project {
                root: root.display().to_string(),
            });
        }

        // 去重 + 排除已注入条目。
        let injected = self.injected.injected(&ctx.task_id);
        hits.retain(|e| !injected.contains(&e.id));
        hits.sort_by(|a, b| b.last_used_at.cmp(&a.last_used_at));
        hits.dedup_by(|a, b| a.id == b.id);
        hits.truncate(limit);

        if hits.is_empty() {
            return Ok(ToolOutput::ok(format!(
                "没有找到与「{query}」相关的记忆。若这是需要长期记住的事实，请用户用 /remember 显式记录。"
            )));
        }

        // 检索命中推进 last_used_at（LRU 依据，设计 R4）。
        let ids: Vec<MemoryId> = hits.iter().map(|e| e.id.clone()).collect();
        let now = chrono::Utc::now().timestamp();
        let _ = self.store.touch(&ids, now).await;

        let mut out = String::from("相关记忆：\n");
        for e in &hits {
            let tags = if e.tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", e.tags.join(","))
            };
            out.push_str(&format!(
                "- 【{}】{}{}\n",
                scope_label(&e.scope),
                e.content,
                tags
            ));
        }
        Ok(ToolOutput::ok(out))
    }
}

fn scope_filter_matches(filter: Option<&str>, scope: &MemoryScope) -> bool {
    matches!(
        (filter, scope),
        (Some("user"), MemoryScope::User) | (Some("project"), MemoryScope::Project { .. })
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::memory::{MemoryEntry, MemoryError, MemorySource};
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    /// 测试内假实现：不依赖 agent-store（依赖铁律），只覆盖工具行为。
    struct FakeIndex(Vec<MemoryEntry>);

    impl MemoryIndex for FakeIndex {
        fn search(&self, q: MemoryQuery) -> Vec<MemoryEntry> {
            self.0
                .iter()
                .filter(|e| {
                    if e.state != MemoryState::Active {
                        return false;
                    }
                    if let Some(scope) = &q.scope {
                        if &e.scope != scope {
                            return false;
                        }
                    }
                    match &q.keyword {
                        Some(kw) => e.content.to_lowercase().contains(&kw.to_lowercase()),
                        None => true,
                    }
                })
                .cloned()
                .collect()
        }
    }

    struct FakeStore(std::sync::Mutex<Vec<MemoryEntry>>);

    #[async_trait]
    impl MemoryStore for FakeStore {
        async fn upsert(&self, e: MemoryEntry) -> Result<MemoryEntry, MemoryError> {
            self.0.lock().unwrap().push(e.clone());
            Ok(e)
        }
        async fn get(&self, _id: &MemoryId) -> Result<Option<MemoryEntry>, MemoryError> {
            Ok(None)
        }
        async fn list(&self, _q: MemoryQuery) -> Result<Vec<MemoryEntry>, MemoryError> {
            Ok(Vec::new())
        }
        async fn delete(&self, _id: &MemoryId) -> Result<(), MemoryError> {
            Ok(())
        }
        async fn touch(&self, _ids: &[MemoryId], _at: i64) -> Result<(), MemoryError> {
            Ok(())
        }
        async fn set_state(
            &self,
            _ids: &[MemoryId],
            _state: MemoryState,
        ) -> Result<usize, MemoryError> {
            Ok(0)
        }
    }

    fn entry(scope: MemoryScope, content: &str) -> MemoryEntry {
        MemoryEntry::new(
            scope,
            content,
            vec![],
            MemorySource::Imported {
                task_id: None,
                at: 1,
            },
            MemoryState::Active,
            1,
        )
    }

    fn ctx(project_root: Option<&str>) -> ToolCtx {
        ToolCtx {
            workspace: PathBuf::from("/tmp/ws"),
            project_root: project_root.map(std::path::PathBuf::from),
            task_id: "t-1".into(),
            timeout: Duration::from_secs(30),
            cancel: CancellationToken::new(),
        }
    }

    fn tool(entries: Vec<MemoryEntry>) -> (MemorySearchTool, InjectedRegistry) {
        let injected = InjectedRegistry::new();
        (
            MemorySearchTool::new(
                Arc::new(FakeIndex(entries)),
                Arc::new(FakeStore(std::sync::Mutex::new(Vec::new()))),
                injected.clone(),
            ),
            injected,
        )
    }

    #[tokio::test]
    async fn search_requires_query() {
        let (tool, _) = tool(vec![]);
        let err = tool.execute(json!({}), &ctx(None)).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[tokio::test]
    async fn search_hits_both_scopes_and_excludes_injected() {
        let (tool, injected) = tool(vec![
            entry(MemoryScope::User, "回复用中文"),
            entry(
                MemoryScope::Project {
                    root: "/tmp/proj".into(),
                },
                "项目用 rsproxy",
            ),
        ]);
        injected.register("t-1", vec![]);

        let out = tool
            .execute(json!({"query": "中文"}), &ctx(Some("/tmp/proj")))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("用户级"), "{}", out.content);

        let out = tool
            .execute(json!({"query": "rsproxy"}), &ctx(Some("/tmp/proj")))
            .await
            .unwrap();
        assert!(out.content.contains("项目级"), "{}", out.content);

        // 已注入的条目不再出现在检索结果里。
        let proj = MemoryScope::Project {
            root: "/tmp/proj".into(),
        };
        let hits = tool.index.search(MemoryQuery {
            keyword: Some("rsproxy".into()),
            scope: Some(proj),
            ..Default::default()
        });
        assert_eq!(hits.len(), 1);
        injected.register("t-1", vec![hits[0].id.clone()]);
        let out = tool
            .execute(json!({"query": "rsproxy"}), &ctx(Some("/tmp/proj")))
            .await
            .unwrap();
        assert!(
            out.content.contains("没有找到"),
            "已注入条目应被排除: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn empty_result_is_a_clear_hint_not_an_error() {
        let (tool, _) = tool(vec![]);
        let out = tool
            .execute(json!({"query": "不存在的东西"}), &ctx(None))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.error_code, None);
    }
}
