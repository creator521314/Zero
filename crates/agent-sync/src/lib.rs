//! agent-sync —— 二期占位。
//!
//! 一期只提供 [`agent_core::sync::SyncEndpoint`] 的空实现，冻结接口形状，
//! 保证二期接入分布式同步（手机权威节点 / 分级同步 / AI 合并）时核心零改动。

use agent_core::store::{Clock, StoredEvent};
use agent_core::sync::{SyncEndpoint, SyncError};
use async_trait::async_trait;

/// 空实现：始终返回 NotImplemented。
pub struct NoopSyncEndpoint;

#[async_trait]
impl SyncEndpoint for NoopSyncEndpoint {
    async fn push(&self, _changed: Vec<StoredEvent>) -> Result<(), SyncError> {
        Err(SyncError::NotImplemented)
    }
    async fn pull(&self, _since: Clock) -> Result<Vec<StoredEvent>, SyncError> {
        Err(SyncError::NotImplemented)
    }
}
