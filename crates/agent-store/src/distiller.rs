//! 记忆提炼 worker：订阅 EventBus，任务收尾后把经验提炼成记忆。
//!
//! 设计（`docs/memory-design.md` §4.2 / 需求 O5，v1.1 按实测缺陷报告修订）：
//! - 仅 `Outcome::Success` / `Failed` 收尾触发；取消/暂停不触发；
//! - 走旁路 worker，**不进执行循环**、不改 `RuntimeDeps`；
//! - provider 调用失败 / 解析失败 → 静默跳过，绝不阻塞任务结束、不发错误事件；
//! - **自动转正（P0-1A）**：通过准入管道的条目直接置 `Active`——旧实现全部停在
//!   `Candidate` 且无批量入口，导致「只写不读」；TUI 面板仍可对 Active 做删除/清理；
//! - **准入过滤（P1-3）**：拒绝「关于记忆系统自身」的元信息（tags/content 黑名单）；
//! - **显式 key（P0-2）**：提炼时先给出已有记忆清单，要求模型复用同主题 key 或
//!   产出新的稳定 key，同 key 新写入覆盖旧条目并归档留版本，杜绝同义重复堆积。

use std::sync::Arc;

use crate::JsonlStore;
use agent_core::event::{Event, EventBus, Outcome};
use agent_core::ids::TaskId;
use agent_core::memory::{
    MemoryConfig, MemoryEntry, MemoryError, MemoryQuery, MemoryScope, MemorySource, MemoryState,
    MemoryStore,
};
use agent_core::provider::{ChatRequest, LlmProvider, Message};

/// 提炼单条候选的中间结构（模型输出的 JSON 数组元素）。
#[derive(Debug, serde::Deserialize)]
struct CandidateRaw {
    content: String,
    #[serde(default)]
    tags: Vec<String>,
    /// 稳定去重键（P0-2）：同主题更新复用同 key → 新覆盖旧并留版本。
    #[serde(default)]
    key: Option<String>,
}

/// 元信息 tags 黑名单（P1-3）：关于记忆系统自身的条目不入库。
const META_TAGS: [&str; 6] = [
    "记忆机制",
    "记忆存储",
    "记忆系统",
    "记忆库",
    "长期记忆",
    "记忆功能",
];
/// 元信息 content 关键词黑名单（P1-3 兜底：模型漏打标签时按内容拦）。
const META_CONTENT_KEYS: [&str; 6] = [
    "记忆系统",
    "记忆库",
    "记忆机制",
    "memory.jsonl",
    "长期记忆",
    "/memory",
];

/// 判定一条候选是否为「关于 Agent 自身记忆机制」的元信息——这类内容
/// 讨论的是系统而非用户世界，写入只会污染记忆库（实测缺陷 P1-3）。
pub fn is_meta_memory(content: &str, tags: &[String]) -> bool {
    if tags.iter().any(|t| {
        let t = t.trim();
        META_TAGS.iter().any(|m| t.contains(m))
    }) {
        return true;
    }
    let lower = content.to_lowercase();
    META_CONTENT_KEYS.iter().any(|k| lower.contains(k))
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
        // 子任务复用父任务的 EventBus，旧实现会把子任务内部的探查过程也
        // 提炼成记忆，污染项目级记忆库。SubtaskLinked 一定先于子任务收尾。
        let mut child_tasks: std::collections::HashSet<String> = Default::default();
        loop {
            let ev = match sub.recv().await {
                Ok(ev) => ev,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            match ev {
                Event::SubtaskLinked { child_id, .. } => {
                    child_tasks.insert(child_id.0);
                }
                Event::TaskFinished { id, outcome } => {
                    if child_tasks.remove(&id.0) {
                        tracing::debug!(task = %id.0, "skip distilling subtask");
                        continue;
                    }
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
                _ => {}
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
    // 输入：由增量快照序列重建的完整上下文（当时真正送给模型的消息），
    // 不读全量会话日志；见 `JsonlStore::resume_context()`。
    let Some(messages): Option<Vec<agent_core::provider::Message>> =
        store.resume_context(task_id).await
    else {
        return Ok(0);
    };
    if messages.is_empty() {
        return Ok(0);
    }

    // P0-2：把已有生效记忆清单喂给模型——要求不重复已记内容，同主题更新复用其 key。
    let existing = memory
        .list(MemoryQuery {
            keyword: None,
            scope: Some(scope.clone()),
            state: Some(MemoryState::Active),
            limit: 20,
        })
        .await
        .unwrap_or_default();
    let existing_block = if existing.is_empty() {
        "（暂无已有记忆）".to_string()
    } else {
        existing
            .iter()
            .map(|e| {
                let k = e.key.clone().unwrap_or_else(|| "-".into());
                let brief: String = e.content.chars().take(80).collect();
                format!("- [key={k}] {brief}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let transcript = clip_transcript(&messages);
    let prompt = format!(
        "你是记忆提炼器。下面是一次 Agent 任务的对话记录。请提炼 0~3 条值得跨任务长期记住的\n\
         **用户偏好与约定**（如「提交信息用中文」「测试跑某个命令」这类用户显式表达的诉求），\n\
         或对后续任务有直接影响的**环境事实/踩坑结论**。要求：\n\
         1. 只输出一个 JSON 数组，元素形如 {{\"key\": \"...\", \"content\": \"...\", \"tags\": [\"...\"]}}，不要输出其它文字；\n\
         2. key 是稳定的去重标识（小写英文短词，如 user:commit_language）；若是「已有记忆清单」中某条的更新，必须复用它的 key；\n\
         3. 每条 content 不超过 {} 个字符，是一句可独立成立的事实，不要引用对话原文；\n\
         4. **忽略关于 Agent 自身机制的元话题**（记忆功能、存储位置、系统设计、工具说明等）——\n\
            用户讨论「你的记忆库在哪」不等于要记住这件事；\n\
         5. 不要重复「已有记忆清单」中已有的内容；\n\
         6. 不要包含任何密钥、凭据、个人信息或大段代码；没有值得记的就输出 []。\n\n\
         【已有记忆清单】\n{existing_block}\n\n【对话记录】\n{transcript}",
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
    for (content, tags, key) in parse_candidates(&text, cfg.max_entry_chars) {
        let mut entry = MemoryEntry::new(
            scope.clone(),
            content,
            tags,
            MemorySource::Distilled {
                task_id: Some(task_id.0.clone()),
                at: now,
            },
            // P0-1A 自动转正：通过准入管道的产出直接生效，避免候选无限堆积不可见。
            MemoryState::Active,
            now,
        );
        let key = key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
        entry.key = key;
        let _ = memory.upsert(entry).await?;
        n += 1;
    }
    Ok(n)
}

/// 把上下文裁成有限的文本记录（总量 ≤6000 字符）。
///
/// 提炼最关心「用户到底要什么」和「最后得出了什么结论」。旧实现从头部
/// 顺序累加、超过 6000 就 break：固定 system 模板 + 早期大段工具输出会把
/// 尾部真正有价值的追问与结论整段挤掉。这里改为跳过 system、按角色分配
/// 不同配额、超预算时**保留尾部**并标注省略条数。
fn clip_transcript(messages: &[Message]) -> String {
    const MAX_CHARS: usize = 6000;
    let mut lines: Vec<String> = Vec::new();
    for m in messages {
        let (role, limit) = match m.role {
            agent_core::provider::Role::System => continue,
            agent_core::provider::Role::User => ("user", 600),
            agent_core::provider::Role::Assistant => ("assistant", 300),
            agent_core::provider::Role::Tool => ("tool", 120),
        };
        let text = m.text();
        let clipped: String = text.chars().take(limit).collect();
        let mut line = format!("{role}: {clipped}");
        if text.chars().count() > limit {
            line.push('…');
        }
        lines.push(line);
    }

    let mut total = 0usize;
    let mut kept: Vec<String> = Vec::new();
    for line in lines.iter().rev() {
        let n = line.chars().count() + 1;
        if total + n > MAX_CHARS {
            break;
        }
        total += n;
        kept.push(line.clone());
    }
    kept.reverse();
    let omitted = lines.len() - kept.len();
    let mut out = String::new();
    if omitted > 0 {
        out.push_str(&format!("[已省略 {omitted} 条较早消息]\n"));
    }
    for line in kept {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// 供测试使用的「提取候选」纯逻辑：解析模型输出并按红线 + 元信息黑名单过滤。
/// 返回 `(content, tags, key)`。
pub fn parse_candidates(
    text: &str,
    max_chars: usize,
) -> Vec<(String, Vec<String>, Option<String>)> {
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
            // P1-3：关于记忆系统自身的元信息直接拒绝。
            !content.is_empty()
                && content.chars().count() <= max_chars
                && !agent_core::memory::scan_secret(content)
                && !is_meta_memory(content, &c.tags)
        })
        .map(|c| {
            let key = c
                .key
                .clone()
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty());
            (c.content.trim().to_string(), c.tags, key)
        })
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
[{"key": "git:push_retry", "content": "推送 GitHub 需要重试", "tags": ["git"]},
 {"key": "", "content": "token 是 ghp_AAAAAAA", "tags": []},
 {"content": "", "tags": []}]
以上。"#,
            500,
        );
        assert_eq!(out.len(), 1, "密钥与空内容应被过滤");
        assert_eq!(out[0].0, "推送 GitHub 需要重试");
        assert_eq!(out[0].1, vec!["git"]);
        assert_eq!(out[0].2.as_deref(), Some("git:push_retry"));
    }

    #[test]
    fn clip_transcript_keeps_tail_and_drops_fixed_system() {
        let mut msgs = vec![Message::system("固定区模板".repeat(200))];
        for i in 0..200 {
            msgs.push(Message::user(format!("早期探查 {i} {}", "x".repeat(300))));
        }
        msgs.push(Message::user("最终追问：代理端口是多少？"));
        msgs.push(Message::assistant("最终结论：代理端口 7890"));

        let out = clip_transcript(&msgs);
        assert!(!out.contains("固定区模板"), "system 模板不该占提炼预算");
        assert!(out.contains("最终追问：代理端口是多少？"));
        assert!(out.contains("最终结论：代理端口 7890"), "{out}");
        assert!(out.contains("[已省略"), "超预算时应标注省略条数: {out}");
        assert!(out.chars().count() <= 6100, "总量要受控: {}", out.len());
    }

    #[test]
    fn parse_candidates_without_json_is_empty() {
        assert!(parse_candidates("模型自由发挥的一段话，没有 JSON", 500).is_empty());
    }

    #[test]
    fn parse_candidates_rejects_meta_memory() {
        // P1-3 回归：关于记忆系统自身的元信息不得入库（tags 或 content 命中均拦）。
        let out = parse_candidates(
            r#"[{"key": "env:store", "content": "长期记忆采用两层 JSONL 存储", "tags": ["环境事实", "记忆存储"]},
 {"key": "env:store2", "content": "记忆库位于 .zero/memory.jsonl", "tags": ["环境事实"]},
 {"key": "pref:lang", "content": "用户要求提交信息用中文", "tags": ["用户偏好"]}]"#,
            500,
        );
        assert_eq!(out.len(), 1, "元信息（tags/content 命中黑名单）应被过滤");
        assert_eq!(out[0].0, "用户要求提交信息用中文");
    }

    #[test]
    fn is_meta_memory_matches_tags_and_content() {
        assert!(is_meta_memory("存储路径说明", &["记忆系统".into()]));
        assert!(is_meta_memory("记忆库位于 ~/.config/zero", &[]));
        assert!(!is_meta_memory(
            "用户偏好：提交信息用中文",
            &["用户偏好".into()]
        ));
    }

    /// P0-1A 回归：提炼产出应直接 Active（可被默认检索看到），
    /// 且同 key 的后续写入应覆盖旧条目（P0-2：不再无限堆积重复）。
    #[tokio::test]
    async fn distilled_entries_are_active_and_dedup_by_key() {
        let tmp = std::env::temp_dir().join(format!("zero-distill-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let svc: Arc<dyn MemoryStore> = Arc::new(MemoryService::new(
            tmp.join("memory.jsonl"),
            MemoryConfig::default(),
        ));
        let mk = |content: &str, key: Option<&str>, at: i64| {
            let mut e = MemoryEntry::new(
                MemoryScope::User,
                content,
                vec![],
                MemorySource::Distilled {
                    task_id: Some("t-1".into()),
                    at,
                },
                MemoryState::Active,
                at,
            );
            e.key = key.map(|k| k.into());
            e
        };
        let v1 = MemoryStore::upsert(svc.as_ref(), mk("端口是 7890", Some("env:proxy"), 1))
            .await
            .unwrap();
        // 默认（Active）检索立即可见——修复前恒为空。
        assert_eq!(
            svc.list(MemoryQuery {
                keyword: None,
                scope: Some(MemoryScope::User),
                state: None,
                limit: 0,
            })
            .await
            .unwrap()
            .len(),
            1
        );
        // 同 key 的更新覆盖旧条目，不产生第二条。
        let v2 = MemoryStore::upsert(svc.as_ref(), mk("代理端口改为 10809", Some("env:proxy"), 2))
            .await
            .unwrap();
        assert_eq!(v2.version, v1.version + 1);
        assert_eq!(
            svc.list(MemoryQuery {
                keyword: None,
                scope: Some(MemoryScope::User),
                state: None,
                limit: 0,
            })
            .await
            .unwrap()
            .len(),
            1,
            "同 key 更新不应产生新条目"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
