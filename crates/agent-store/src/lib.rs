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

    /// 更新任务摘要（覆盖写 tasks/<id>.json）。
    pub async fn upsert_task(&self, summary: &TaskSummary) -> Result<(), StoreError> {
        let path = self.tasks_dir().join(format!("{}.json", summary.id));
        let data = serde_json::to_vec_pretty(summary)?;
        tokio::fs::write(path, data).await?;
        Ok(())
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
        self.cache
            .lock()
            .await
            .insert(sid.clone(), events.clone());
        Ok(Session {
            id: sid.clone(),
            events,
        })
    }

    async fn list_tasks(&self, limit: usize) -> Result<Vec<TaskSummary>, StoreError> {
        self.init().await?;
        let mut dir = tokio::fs::read_dir(self.tasks_dir()).await?;
        let mut out = Vec::new();
        while let Some(entry) = dir.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(bytes) = tokio::fs::read(&path).await {
                if let Ok(s) = serde_json::from_slice::<TaskSummary>(&bytes) {
                    out.push(s);
                }
            }
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        out.truncate(limit);
        Ok(out)
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
}
