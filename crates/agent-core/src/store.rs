//! 持久化边界。
//!
//! 设计要点：**jsonl 事件流是唯一真相源**（追加无锁、崩溃不丢、天然适配二期同步，
//! 且每行带 `schema_version` + 逻辑时钟 `Clock`，为二期 AI 合并/回滚预留）。
//! sqlite 仅作可重建的索引缓存。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::event::Event;
use crate::ids::{SessionId, TaskId};
use crate::task::TaskState;

/// 逻辑时钟（Lamport 风格），二期多端同步的定序依据。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Clock {
    pub wall_ms: i64,
    pub seq: u64,
}

impl Clock {
    pub fn now(seq: u64) -> Self {
        Self {
            wall_ms: chrono::Utc::now().timestamp_millis(),
            seq,
        }
    }
}

/// 落盘事件。`schema_version` 用于格式迁移；`durability` 区分持久化与临时数据，
/// 直接作为二期「重要性 × 紧急性」分级同步的输入。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEvent {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub clock: Clock,
    pub durability: Durability,
    pub payload: Event,
}

/// 数据分层：决定是否参与二期跨设备同步。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Durability {
    /// 持久化数据：参与同步、长期保留。
    Persistent,
    /// 任务临时信息：单机保留、超期清理、不一定合入持久库。
    Transient,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: TaskId,
    pub title: String,
    pub state: TaskState,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub events: Vec<StoredEvent>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("database error: {0}")]
    Db(String),
    #[error("not found: {0}")]
    NotFound(String),
}

#[async_trait]
pub trait Store: Send + Sync {
    /// 追加一条事件（jsonl append-only）。
    async fn append_event(&self, ev: &StoredEvent) -> Result<(), StoreError>;
    /// 载入整个会话。
    async fn load_session(&self, sid: &SessionId) -> Result<Session, StoreError>;
    /// 列出任务摘要（按更新时间倒序）。
    async fn list_tasks(&self, limit: usize) -> Result<Vec<TaskSummary>, StoreError>;
}
