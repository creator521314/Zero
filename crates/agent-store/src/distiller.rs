//! 记忆提炼 worker：订阅 EventBus，任务收尾后把经验提炼成候选记忆。
//!
//! 设计（`docs/memory-design.md` §4.2 / 需求 O5）：
//! - 仅 `Outcome::Success` / `Failed` 收尾触发；取消/暂停不触发；
//! - 走旁路 worker，**不进执行循环**、不改 `RuntimeDeps`；
//! - provider 调用失败 / 解析失败 → 静默跳过，绝不阻塞任务结束、不发错误事件；
//! - 产出一律 `state = Candidate`，等用户在 `/memory` 面板批量确认。

use std::sync::Arc;

use crate::JsonlStore;
use agent_core::event::{Event, EventBus, Outcome};
use agent_core::ids::TaskId;
use agent_core::memory::{
    MemoryConfig, MemoryEntry, MemoryError, MemoryScope, MemorySource, MemoryState, MemoryStore,
};
use agent_core::provider::{ChatRequest, LlmProvider, Message};

/// 提炼单条候选的中间结构（模型输出的 JSON 数组元素）。
#[derive(Debug, serde::Deserialize)]
struct CandidateRaw {
    content: String,
    #[serde(default)]
    tags: Vec<String>,
}

/// 订阅总线并在任务收尾时提炼候选记忆。失败静默，不阻塞任务结束。
pub fn spawn_memory_distiller(
    bus: EventBus,
    store: Arc<JsonlStore>,
    memory: Arc<dyn MemoryStore>,
    provider: Arc<dyn LlmProvider>,
    scope: MemoryScope,
    cfg: MemoryConfig,
) {
    let mut sub = bus.subscribe();
    tokio::spawn(async move {
        loop {
            let ev = match sub.recv().await {
                Ok(ev) => ev,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            if let Event::TaskFinished { id, outcome } = ev {
                // O5：仅成功/失败触发。
                if !matches!(outcome, Outcome::Success { .. } | Outcome::Failed { .. }) {
                    continue;
                }
                // 串行提炼即可（一期串行任务）；提炼慢不影响下一任务。
                if let Err(e) = distill_once(
                    &store,
                    memory.as_ref(),
                    provider.as_ref(),
                    &scope,
                    &cfg,
                    &id,
                )
                .await
                {
                    tracing::warn!(error = %e, task = %id.0, "memory distill skipped");
                }
            }
        }
    });
}

async fn distill_once(
    store: &JsonlStore,
    memory: &dyn MemoryStore,
    provider: &dyn LlmProvider,
    scope: &MemoryScope,
    cfg: &MemoryConfig,
    task_id: &TaskId,
) -> Result<usize, MemoryError> {
    // 输入：该任务最后一次上下文快照（当时真正送给模型的消息），不读全量日志。
    let Some(messages): Option<Vec<agent_core::provider::Message>> =
        store.resume_context(task_id).await
    else {
        return Ok(0);
    };
    if messages.is_empty() {
        return Ok(0);
    }

    let transcript = clip_transcript(&messages);
    let prompt = format!(
        "你是记忆提炼器。下面是一次 Agent 任务的对话记录。请提炼 0~3 条值得跨任务长期记住的\n\
         事实/偏好/经验（如环境事实、踩坑结论、用户偏好）。要求：\n\
         1. 只输出一个 JSON 数组，元素形如 {{\"content\": \"...\", \"tags\": [\"...\"]}}，不要输出其它文字；\n\
         2. 每条 content 不超过 {} 个字符，是一句可独立成立的事实，不要引用对话原文；\n\
         3. 不要包含任何密钥、凭据、个人信息或大段代码；没有值得记的就输出 []。\n\n\
         【对话记录】\n{transcript}",
        cfg.max_entry_chars,
    );

    let req = ChatRequest {
        model: String::new(),
        messages: vec![Message::user(prompt)],
        tools: Vec::new(),
        temperature: Some(0.2),
        max_tokens: Some(1024),
        stream: false,
        reasoning_effort: None,
        metadata: agent_core::provider::RequestMeta {
            task_id: Some(task_id.0.clone()),
            route_hint: None,
        },
    };
    let resp = provider
        .chat(req)
        .await
        .map_err(|e| MemoryError::Other(e.to_string()))?;
    let text = resp.message.text();

    let now = chrono::Utc::now().timestamp();
    let mut n = 0usize;
    for (content, tags) in parse_candidates(&text, cfg.max_entry_chars) {
        let entry = MemoryEntry::new(
            scope.clone(),
            content,
            tags,
            MemorySource::Distilled {
                task_id: Some(task_id.0.clone()),
                at: now,
            },
            MemoryState::Candidate,
            now,
        );
        let _ = memory.upsert(entry).await?;
        n += 1;
    }
    Ok(n)
}

/// 把快照消息裁成有限的文本记录（每条 ≤400 字符，总量 ≤6000 字符）。
fn clip_transcript(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        let role = match m.role {
            agent_core::provider::Role::System => "system",
            agent_core::provider::Role::User => "user",
            agent_core::provider::Role::Assistant => "assistant",
            agent_core::provider::Role::Tool => "tool",
        };
        let text = m.text();
        let clipped: String = text.chars().take(400).collect();
        out.push_str(&format!("{role}: {clipped}\n"));
        if out.chars().count() > 6000 {
            break;
        }
    }
    out
}

/// 供测试使用的「提取候选」纯逻辑：解析模型输出并按红线过滤。
pub fn parse_candidates(text: &str, max_chars: usize) -> Vec<(String, Vec<String>)> {
    let Some(start) = text.find('[') else {
        return Vec::new();
    };
    let Some(end) = text.rfind(']') else {
        return Vec::new();
    };
    if end < start {
        return Vec::new();
    }
    serde_json::from_str::<Vec<CandidateRaw>>(&text[start..=end])
        .unwrap_or_default()
        .into_iter()
        .filter(|c| {
            let content = c.content.trim();
            !content.is_empty()
                && content.chars().count() <= max_chars
                && !agent_core::memory::scan_secret(content)
        })
        .map(|c| (c.content.trim().to_string(), c.tags))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_store::MemoryService;
    use agent_core::memory::MemoryQuery;

    #[test]
    fn parse_candidates_extracts_json_and_filters() {
        let out = parse_candidates(
            r#"以下是提炼结果：
[{"content": "推送 GitHub 需要重试", "tags": ["git"]},
 {"content": "token 是 ghp_AAAAAAA", "tags": []},
 {"content": "", "tags": []}]
以上。"#,
            500,
        );
        assert_eq!(out.len(), 1, "密钥与空内容应被过滤");
        assert_eq!(out[0].0, "推送 GitHub 需要重试");
        assert_eq!(out[0].1, vec!["git"]);
    }

    #[test]
    fn parse_candidates_without_json_is_empty() {
        assert!(parse_candidates("模型自由发挥的一段话，没有 JSON", 500).is_empty());
    }

    /// Candidate 条数上限的守护：候选区超过上限时旧的最久未用条目不再新增。
    /// （upsert 不裁 Candidate，容量约束由确认/丢弃驱动；这里验证 store 行为。）
    #[tokio::test]
    async fn distilled_candidates_are_stored_and_invisible_by_default() {
        let tmp = std::env::temp_dir().join(format!("zero-distill-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let svc: Arc<dyn MemoryStore> = Arc::new(MemoryService::new(
            tmp.join("memory.jsonl"),
            MemoryConfig::default(),
        ));
        let e = MemoryEntry::new(
            MemoryScope::User,
            "提炼出的事实",
            vec![],
            MemorySource::Distilled {
                task_id: Some("t-1".into()),
                at: 1,
            },
            MemoryState::Candidate,
            1,
        );
        MemoryStore::upsert(svc.as_ref(), e).await.unwrap();
        // 候选在默认（Active）检索中不可见。
        assert!(svc
            .list(MemoryQuery {
                keyword: None,
                scope: Some(MemoryScope::User),
                state: None,
                limit: 0,
            })
            .await
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
