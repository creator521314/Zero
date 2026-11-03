//! agent-store —— 持久化实现。
//!
//! jsonl 为唯一真相源（append-only，崩溃不丢、易 diff、天然适配二期同步）；
//! 一期先用 jsonl + 内存索引，sqlite 索引留待任务量上来后接入。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use agent_core::event::Event;
use agent_core::ids::{SessionId, TaskId};
use agent_core::store::{Clock, Durability, Session, Store, StoreError, StoredEvent, TaskSummary};
use agent_core::task::TaskState;
use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

pub mod recorder;

pub use recorder::{classify, spawn_event_sink};

pub const SCHEMA_VERSION: u32 = 1;

/// 基于 jsonl 的存储。
///
/// 目录布局（与方案 4.5 一致）：
/// ```text
/// <root>/logs/<session>.jsonl     # 审计事件流
/// <root>/tasks/<task>.json        # 任务摘要
/// ```
pub struct JsonlStore {
    root: PathBuf,
    /// session -> 已加载的事件（内存缓存）
    cache: Mutex<HashMap<SessionId, Vec<StoredEvent>>>,
    /// session -> 单调递增序号，供逻辑时钟使用
    seq: Mutex<HashMap<SessionId, u64>>,
}

impl JsonlStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            cache: Mutex::new(HashMap::new()),
            seq: Mutex::new(HashMap::new()),
        }
    }

    fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }
    fn tasks_dir(&self) -> PathBuf {
        self.root.join("tasks")
    }
    fn session_path(&self, sid: &SessionId) -> PathBuf {
        self.logs_dir().join(format!("{sid}.jsonl"))
    }

    /// 确保目录存在。
    pub async fn init(&self) -> Result<(), StoreError> {
        tokio::fs::create_dir_all(self.logs_dir()).await?;
        tokio::fs::create_dir_all(self.tasks_dir()).await?;
        Ok(())
    }

    /// 追加落盘的便捷方法：自动补全 schema_version、时钟与分层。
    pub async fn record(
        &self,
        sid: &SessionId,
        task_id: &TaskId,
        durability: Durability,
        payload: Event,
    ) -> Result<StoredEvent, StoreError> {
        let clock = {
            let mut seq = self.seq.lock().await;
            let e = seq.entry(sid.clone()).or_insert(0);
            *e += 1;
            Clock::now(*e)
        };
        let ev = StoredEvent {
            schema_version: SCHEMA_VERSION,
            session_id: sid.clone(),
            task_id: task_id.clone(),
            clock,
            durability,
            payload,
        };
        self.append_event(&ev).await?;
        Ok(ev)
    }

    /// 更新任务摘要（先写临时文件再改名：崩溃时不会留下半截 JSON）。
    pub async fn upsert_task(&self, summary: &TaskSummary) -> Result<(), StoreError> {
        tokio::fs::create_dir_all(self.tasks_dir()).await?;
        let path = self.tasks_dir().join(format!("{}.json", summary.id));
        let data = serde_json::to_vec_pretty(summary)?;
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, data).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }

    /// 登记 / 更新一个任务的状态，保留首次创建时间。
    ///
    /// 执行循环在开始与收尾各调用一次——**这是 `/tasks` 面板唯一的数据来源**。
    /// 历史教训：`upsert_task` 早在 M1 就写好了，但没有任何生产路径调用它，
    /// 于是 `tasks/` 目录永远是空的，`/tasks` 列出来一片空白。
    pub async fn touch_task(
        &self,
        id: &TaskId,
        title: &str,
        state: TaskState,
    ) -> Result<(), StoreError> {
        let now = chrono::Utc::now().timestamp_millis();
        let created_at =
            match tokio::fs::read(self.tasks_dir().join(format!("{}.json", id.0))).await {
                Ok(bytes) => serde_json::from_slice::<TaskSummary>(&bytes)
                    .map(|s| s.created_at)
                    .unwrap_or(now),
                Err(_) => now,
            };
        let summary = TaskSummary {
            id: id.clone(),
            title: title.to_string(),
            state,
            created_at,
            updated_at: now,
        };
        self.upsert_task(&summary).await
    }

    /// 列出所有已知任务（按更新时间倒序）。
    ///
    /// 供 TUI 的 `/tasks` 面板用。读不到/读不懂的单个文件跳过，不让一条坏数据
    /// 把整个列表搞没。
    pub async fn recent_tasks(&self, limit: usize) -> Vec<TaskSummary> {
        let mut out = Vec::new();
        let mut rd = match tokio::fs::read_dir(self.tasks_dir()).await {
            Ok(rd) => rd,
            Err(_) => return out,
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(text) = tokio::fs::read_to_string(&path).await {
                if let Ok(summary) = serde_json::from_str::<TaskSummary>(&text) {
                    out.push(summary);
                }
            }
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        out.truncate(limit);
        out
    }

    /// 取某个任务最后一份上下文快照里的消息（用于「恢复继续」）。
    ///
    /// 之所以用快照而不是重放事件：快照就是**当时真正送给模型的那份上下文**
    /// （含 system 固定区与历史工具结果），重放还得自己重新拼。
    /// 会话 id 与任务 id 同名（见 `recorder`），找不到就返回 None。
    pub async fn last_messages(
        &self,
        task_id: &TaskId,
    ) -> Option<Vec<agent_core::provider::Message>> {
        let session = self
            .load_session(&SessionId::from_raw(task_id.0.clone()))
            .await
            .ok()?;
        session
            .events
            .iter()
            .rev()
            .find_map(|ev| match &ev.payload {
                Event::ContextSnapshot { messages, .. } => Some(messages.clone()),
                _ => None,
            })
    }

    /// 续跑上下文：最后一次快照 + 快照之后的模型答复。
    ///
    /// 快照是在**每次请求之前**拍的，所以任务最后那轮答复不在快照里；不带上的话，
    /// 恢复后模型会「忘记自己刚刚说过什么」。快照之前的答复都已在快照内，不重复补。
    pub async fn resume_context(
        &self,
        task_id: &TaskId,
    ) -> Option<Vec<agent_core::provider::Message>> {
        let session = self
            .load_session(&SessionId::from_raw(task_id.0.clone()))
            .await
            .ok()?;
        let last = session
            .events
            .iter()
            .rposition(|e| matches!(e.payload, Event::ContextSnapshot { .. }))?;
        let mut messages = match &session.events[last].payload {
            Event::ContextSnapshot { messages, .. } => messages.clone(),
            _ => return None,
        };
        for ev in &session.events[last + 1..] {
            if let Event::AssistantTurn { text, .. } = &ev.payload {
                if !text.trim().is_empty() {
                    messages.push(agent_core::provider::Message::assistant(text));
                }
            }
        }
        Some(messages)
    }

    /// 读某个任务的摘要（不存在 / 坏文件都返回 None）。
    pub async fn task_summary(&self, id: &TaskId) -> Option<TaskSummary> {
        let bytes = tokio::fs::read(self.tasks_dir().join(format!("{}.json", id.0)))
            .await
            .ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// 任务转写：只取「对话长什么样」相关的事件，按时间顺序。
    ///
    /// 上下文快照刻意排除在外——它每轮都重复整份历史，几百 KB 且对界面无用
    /// （一次 24 工具的会话里，快照占了绝大部分体积）。
    pub async fn task_events(&self, task_id: &TaskId) -> Vec<Event> {
        let Ok(session) = self
            .load_session(&SessionId::from_raw(task_id.0.clone()))
            .await
        else {
            return Vec::new();
        };
        session
            .events
            .into_iter()
            .map(|e| e.payload)
            .filter(|e| {
                matches!(
                    e,
                    Event::TaskStarted { .. }
                        | Event::TextDelta { .. }
                        | Event::ReasoningDelta { .. }
                        | Event::AssistantTurn { .. }
                        | Event::ToolCallStart { .. }
                        | Event::ToolCallEnd { .. }
                        | Event::TaskFinished { .. }
                        | Event::Error { .. }
                )
            })
            .collect()
    }

    /// 清理超过保留期的 Transient 事件；Persistent 事件一律保留。
    ///
    /// jsonl 是 append-only 的，删除中间行只能重写整个文件：先写临时文件、
    /// 再原子 rename。损坏行原样保留（宁可留着看不懂的，也不静默丢数据）。
    /// 返回删除的事件数；日志目录不存在视为无事可做。
    pub async fn prune_transient(&self, retain_days: u64) -> Result<usize, StoreError> {
        let days = retain_days.max(1) as i64;
        let cutoff = chrono::Utc::now().timestamp_millis() - days * 24 * 60 * 60 * 1000;
        let mut removed = 0usize;

        let mut rd = match tokio::fs::read_dir(self.logs_dir()).await {
            Ok(rd) => rd,
            Err(_) => return Ok(0),
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(bytes) = tokio::fs::read(&path).await else {
                continue;
            };

            let text = String::from_utf8_lossy(&bytes);
            let mut kept: Vec<&str> = Vec::new();
            let mut removed_here = 0usize;
            let mut max_seq = 0u64;
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<StoredEvent>(line) {
                    Ok(e) => {
                        let expired =
                            e.durability == Durability::Transient && e.clock.wall_ms < cutoff;
                        if expired {
                            removed_here += 1;
                        } else {
                            max_seq = max_seq.max(e.clock.seq);
                            kept.push(line);
                        }
                    }
                    // 损坏行保留：清理不该顺手毁掉排查线索。
                    Err(_) => kept.push(line),
                }
            }
            if removed_here == 0 {
                continue;
            }

            let mut body = kept.join("\n");
            if !body.is_empty() {
                body.push('\n');
            }
            let tmp = path.with_extension("jsonl.tmp");
            tokio::fs::write(&tmp, body).await?;
            tokio::fs::rename(&tmp, &path).await?;

            // 缓存按磁盘上的新内容重建；逻辑时钟只进不退——否则清理一次，
            // 事件序号会从 1 重新开始，二期按 Clock 定序时会出现「新事件排在旧事件前」。
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                let sid = SessionId::from_raw(stem);
                if let Some(events) = self.cache.lock().await.get_mut(&sid) {
                    events.retain(|e| {
                        e.durability == Durability::Persistent || e.clock.wall_ms >= cutoff
                    });
                }
                let mut seq = self.seq.lock().await;
                let entry = seq.entry(sid).or_insert(0);
                *entry = (*entry).max(max_seq);
            }
            removed += removed_here;
        }
        Ok(removed)
    }
}

#[async_trait]
impl Store for JsonlStore {
    async fn append_event(&self, ev: &StoredEvent) -> Result<(), StoreError> {
        self.init().await?;
        let line = serde_json::to_string(ev)?;
        let path = self.session_path(&ev.session_id);

        // append-only：OpenOptions append + 单次写入整行，保证不丢
        let mut f = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        f.write_all(line.as_bytes()).await?;
        f.write_all(b"\n").await?;
        f.flush().await?;

        self.cache
            .lock()
            .await
            .entry(ev.session_id.clone())
            .or_default()
            .push(ev.clone());
        Ok(())
    }

    async fn load_session(&self, sid: &SessionId) -> Result<Session, StoreError> {
        if let Some(cached) = self.cache.lock().await.get(sid) {
            return Ok(Session {
                id: sid.clone(),
                events: cached.clone(),
            });
        }
        let path = self.session_path(sid);
        if !path.exists() {
            return Err(StoreError::NotFound(sid.to_string()));
        }
        let content = tokio::fs::read_to_string(&path).await?;
        let mut events = Vec::new();
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            // 容忍尾部半行（崩溃场景）
            match serde_json::from_str::<StoredEvent>(line) {
                Ok(e) => events.push(e),
                Err(err) => {
                    tracing::warn!(%err, "skip malformed jsonl line");
                }
            }
        }
        self.cache.lock().await.insert(sid.clone(), events.clone());
        Ok(Session {
            id: sid.clone(),
            events,
        })
    }

    async fn list_tasks(&self, limit: usize) -> Result<Vec<TaskSummary>, StoreError> {
        self.init().await?;
        Ok(JsonlStore::recent_tasks(self, limit).await)
    }
}

/// 新建任务摘要的便捷构造。
pub fn new_task_summary(id: &TaskId, title: impl Into<String>) -> TaskSummary {
    let now = chrono::Utc::now().timestamp_millis();
    TaskSummary {
        id: id.clone(),
        title: title.into(),
        state: TaskState::Created,
        created_at: now,
        updated_at: now,
    }
}

/// 便捷：拼接工作区路径。
pub fn workspace_of(root: &Path, task_id: &TaskId) -> PathBuf {
    root.join("workspace").join(task_id.0.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::event::Outcome;

    #[tokio::test]
    async fn append_and_reload() {
        let tmp = std::env::temp_dir().join(format!("agent-store-test-{}", uuid::Uuid::new_v4()));
        let store = JsonlStore::new(&tmp);
        store.init().await.unwrap();

        let sid = SessionId::new();
        let tid = TaskId::new();
        store
            .record(
                &sid,
                &tid,
                Durability::Persistent,
                Event::TaskStarted {
                    id: tid.clone(),
                    title: "t".into(),
                },
            )
            .await
            .unwrap();
        store
            .record(
                &sid,
                &tid,
                Durability::Transient,
                Event::TaskFinished {
                    id: tid.clone(),
                    outcome: Outcome::Success {
                        summary: "ok".into(),
                        artifacts: vec![],
                    },
                },
            )
            .await
            .unwrap();

        let s = store.load_session(&sid).await.unwrap();
        assert_eq!(s.events.len(), 2);
        assert_eq!(s.events[1].durability, Durability::Transient);
        // 逻辑时钟单调
        assert!(s.events[1].clock.seq > s.events[0].clock.seq);

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    fn temp_store() -> (JsonlStore, std::path::PathBuf) {
        let tmp = std::env::temp_dir().join(format!("agent-store-test-{}", uuid::Uuid::new_v4()));
        (JsonlStore::new(&tmp), tmp)
    }

    /// 回归：`upsert_task` 早就写好了却没有任何生产路径调用，`tasks/` 永远为空，
    /// `/tasks` 列出来一片空白。`touch_task` 是执行循环实际调用的那一层。
    #[tokio::test]
    async fn touch_task_writes_summary_and_preserves_created_at() {
        let (store, tmp) = temp_store();
        let tid = TaskId::new();
        store
            .touch_task(&tid, "写个统计脚本", TaskState::Running)
            .await
            .unwrap();

        let first = store.task_summary(&tid).await.expect("摘要应已落盘");
        assert_eq!(first.title, "写个统计脚本");
        assert_eq!(first.state, TaskState::Running);

        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
        store
            .touch_task(&tid, "写个统计脚本", TaskState::Done)
            .await
            .unwrap();
        let second = store.task_summary(&tid).await.unwrap();
        assert_eq!(second.state, TaskState::Done);
        assert_eq!(
            second.created_at, first.created_at,
            "创建时间不能被后续更新改掉"
        );
        assert!(second.updated_at >= first.updated_at);

        // 列表按更新时间倒序，且能同时看到标题与状态
        let listed = store.recent_tasks(10).await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].state, TaskState::Done);

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn resume_context_appends_answer_after_last_snapshot() {
        let (store, tmp) = temp_store();
        let tid = TaskId::new();
        let sid = SessionId(tid.0.clone());
        // 快照在「最后一次请求之前」拍下：只到 user 消息为止。
        store
            .record(
                &sid,
                &tid,
                Durability::Transient,
                Event::ContextSnapshot {
                    id: tid.clone(),
                    iteration: 0,
                    model: "m".into(),
                    messages: vec![
                        agent_core::provider::Message::system("你是助理"),
                        agent_core::provider::Message::user("统计文件数"),
                    ],
                    tools: vec![],
                },
            )
            .await
            .unwrap();
        // 之后模型才给出最终答复：不在快照里，续跑必须补上。
        store
            .record(
                &sid,
                &tid,
                Durability::Transient,
                Event::AssistantTurn {
                    id: tid.clone(),
                    iteration: 0,
                    text: "共 3 个文件。".into(),
                    calls: vec![],
                    usage: agent_core::provider::Usage::default(),
                },
            )
            .await
            .unwrap();

        let ctx = store.resume_context(&tid).await.expect("应有上下文");
        assert_eq!(ctx.len(), 3, "快照 2 条 + 快照后的答复 1 条");
        assert_eq!(ctx[1].text(), "统计文件数");
        assert_eq!(
            ctx[2].text(),
            "共 3 个文件。",
            "恢复后模型要记得自己刚说过什么"
        );

        // 转写给界面用：快照排除在外（每轮重复整份历史，体积大且界面用不上）。
        let events = store.task_events(&tid).await;
        assert_eq!(events.len(), 1, "只留 AssistantTurn：{events:?}");
        assert!(matches!(events[0], Event::AssistantTurn { .. }));

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn broken_task_file_does_not_break_listing() {
        let (store, tmp) = temp_store();
        store.init().await.unwrap();
        let good = TaskId::new();
        store
            .touch_task(&good, "好任务", TaskState::Done)
            .await
            .unwrap();
        tokio::fs::write(tmp.join("tasks/bad.json"), b"{ not json")
            .await
            .unwrap();

        let listed = store.recent_tasks(10).await;
        assert_eq!(listed.len(), 1, "坏文件应被跳过，而不是让整个列表消失");
        assert_eq!(listed[0].id.0, good.0);

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    /// `transient_retention_days` 曾经只是配置里的死字段：Transient 事件永不清理，
    /// 密钥之类的临时数据因此可以永久留在盘上。
    #[tokio::test]
    async fn prune_transient_removes_only_expired_transient() {
        let (store, tmp) = temp_store();
        store.init().await.unwrap();
        let sid = SessionId::new();
        let tid = TaskId::new();
        let old = chrono::Utc::now().timestamp_millis() - 10 * 24 * 60 * 60 * 1000;
        let now = chrono::Utc::now().timestamp_millis();

        let ev = |durability: Durability, wall_ms: i64, seq: u64, text: &str| StoredEvent {
            schema_version: SCHEMA_VERSION,
            session_id: sid.clone(),
            task_id: tid.clone(),
            clock: Clock { wall_ms, seq },
            durability,
            payload: Event::TextDelta {
                id: tid.clone(),
                text: text.into(),
            },
        };
        store
            .append_event(&ev(Durability::Transient, old, 1, "旧临时"))
            .await
            .unwrap();
        store
            .append_event(&ev(Durability::Persistent, old, 2, "旧持久"))
            .await
            .unwrap();
        store
            .append_event(&ev(Durability::Transient, now, 3, "新临时"))
            .await
            .unwrap();

        let removed = store.prune_transient(7).await.unwrap();
        assert_eq!(removed, 1, "只该删掉过期的那条 Transient");

        let session = store.load_session(&sid).await.unwrap();
        assert_eq!(session.events.len(), 2, "旧持久 + 新临时 应保留");
        assert!(
            session
                .events
                .iter()
                .any(|e| e.durability == Durability::Persistent),
            "Persistent 永不过期"
        );
        assert!(
            session
                .events
                .iter()
                .any(|e| matches!(&e.payload, Event::TextDelta { text, .. } if text == "新临时")),
            "未过期的 Transient 必须保留"
        );

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }
}
