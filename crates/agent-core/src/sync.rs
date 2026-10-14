//! 同步边界 —— 一期冻结签名、空实现；二期接入。
//!
//! 冻结的意义：一期即锁定接口形状，二期实现分布式同步时核心与工具链零改动。

use async_trait::async_trait;

use crate::store::{Clock, StoredEvent};

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("not implemented (planned for phase 2)")]
    NotImplemented,
    #[error("conflict: {0}")]
    Conflict(String),
}

/// 同步端点。二期由 `agent-sync` 提供真实实现（手机权威节点 / 分级同步 / AI 合并）。
#[async_trait]
pub trait SyncEndpoint: Send + Sync {
    /// 推送本地变更。
    async fn push(&self, changed: Vec<StoredEvent>) -> Result<(), SyncError>;
    /// 拉取自 `since` 之后的远端变更。
    async fn pull(&self, since: Clock) -> Result<Vec<StoredEvent>, SyncError>;
}
