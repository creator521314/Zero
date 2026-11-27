//! 记忆系统边界：条目、作用域、检索与持久化 trait。
//!
//! 本模块只定义边界与纯函数，**零 IO**：实现见 `agent-store`（jsonl 持久化
//! 与关键词检索），装配层负责注入。依赖铁律不变——实现 crate 只依赖本 crate。
//!
//! 需求与裁决见 `docs/memory-requirements.md`（v1.0），设计见 `docs/memory-design.md`。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// 记忆条目 id。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MemoryId(pub String);

impl MemoryId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl Default for MemoryId {
    fn default() -> Self {
        Self::new()
    }
}

/// 记忆作用域：用户级全局生效，项目级按归一化后的 `project_root` 隔离。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    User,
    Project { root: String },
}

/// 条目状态：候选（待确认）→ 生效 → 归档（逻辑删除，可追溯）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryState {
    Candidate,
    Active,
    Archived,
}

/// 写入来源与时间戳（O1 兜底：来源/时间戳由写入端强制填充，调用方不能伪造）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySource {
    /// 显式 `/remember`。
    Explicit { task_id: Option<String>, at: i64 },
    /// 任务收尾自动提炼（待用户批量确认）。
    Distilled { task_id: Option<String>, at: i64 },
    /// 从历史日志一次性回填。
    Imported { task_id: Option<String>, at: i64 },
}

impl MemorySource {
    pub fn at(&self) -> i64 {
        match self {
            Self::Explicit { at, .. } | Self::Distilled { at, .. } | Self::Imported { at, .. } => {
                *at
            }
        }
    }
}

/// 一条跨任务记忆。
///
/// `key` 可选：同 `scope` + 同 `key` 视为同一条，新覆盖旧并保留历史版本；
/// 无 `key` 时用归一化 content 的哈希做隐式键。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryEntry {
    pub id: MemoryId,
    pub scope: MemoryScope,
    pub key: Option<String>,
    /// 单条限长（`MemoryConfig::max_entry_chars`），准入管道强制。
    pub content: String,
    pub tags: Vec<String>,
    pub state: MemoryState,
    pub source: MemorySource,
    pub version: u32,
    pub created_at: i64,
    pub updated_at: i64,
    /// 注入命中 + 检索命中共同推进（设计 R4）。
    pub last_used_at: i64,
}

impl Default for MemoryEntry {
    fn default() -> Self {
        let now = 0;
        Self {
            id: MemoryId::default(),
            scope: MemoryScope::User,
            key: None,
            content: String::new(),
            tags: Vec::new(),
            state: MemoryState::Active,
            source: MemorySource::Imported {
                task_id: None,
                at: now,
            },
            version: 1,
            created_at: now,
            updated_at: now,
            last_used_at: now,
        }
    }
}

impl MemoryEntry {
    /// 由写入端构造：来源/时间戳在此统一生成，外部不能伪造。
    pub fn new(
        scope: MemoryScope,
        content: impl Into<String>,
        tags: Vec<String>,
        source: MemorySource,
        state: MemoryState,
        now: i64,
    ) -> Self {
        Self {
            id: MemoryId::new(),
            scope,
            key: None,
            content: content.into(),
            tags,
            state,
            source,
            version: 1,
            created_at: now,
            updated_at: now,
            last_used_at: now,
        }
    }

    /// 归一化内容键（去空白差异后的哈希），用于无显式 key 的去重。
    pub fn implicit_key(&self) -> String {
        let norm: String = self
            .content
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_lowercase();
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        norm.hash(&mut h);
        format!("auto:{:x}", h.finish())
    }

    pub fn dedup_key(&self) -> String {
        self.key.clone().unwrap_or_else(|| self.implicit_key())
    }
}

/// 检索查询。
#[derive(Debug, Clone, Default)]
pub struct MemoryQuery {
    /// 关键词：按 content / tags 包含匹配（大小写不敏感）。
    pub keyword: Option<String>,
    /// 作用域过滤；None = 用户级 + 当前项目级。
    pub scope: Option<MemoryScope>,
    /// 状态过滤；None = 只返回 Active。
    pub state: Option<MemoryState>,
    pub limit: usize,
}

/// 任务提交时算好的注入快照：进 `TaskSpec.memory_context`，随事件/日志可复现。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryContext {
    /// 已格式化、已动态截断、带来源标注的注入文本。
    pub text: String,
    pub injected_ids: Vec<MemoryId>,
    /// 被动态截断丢弃的条数，供 TUI 提示。
    pub truncated: usize,
}

/// 记忆系统配置。
#[derive(Debug, Clone)]
pub struct MemoryConfig {
    /// 单条写入限长（字符数，O1 兜底）。
    pub max_entry_chars: usize,
    /// 注入预算占 `max_context` 的比例（O3 动态截断）。
    pub inject_ratio: f64,
    /// 用户级 Active 条数上限。
    pub user_active_limit: usize,
    /// 项目级 Active 条数上限。
    pub project_active_limit: usize,
    /// 候选区条数上限。
    pub candidate_limit: usize,
    /// jsonl 操作记录数超过此值时压缩。
    pub compact_over_records: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            max_entry_chars: 500,
            inject_ratio: 0.03,
            user_active_limit: 200,
            project_active_limit: 500,
            candidate_limit: 100,
            compact_over_records: 2000,
        }
    }
}

/// 记忆操作错误。
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("content too long (max {max} chars)")]
    TooLong { max: usize },
    #[error("content rejected: looks like a credential")]
    SecretDetected,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("store error: {0}")]
    Other(String),
}

/// 记忆类工具名集合（供 `tool_allowlist` / 子任务隔离判定；单一事实源）。
pub fn is_memory_tool(name: &str) -> bool {
    name == "memory_search"
}

/// 凭据扫描：命中常见密钥形态即拒绝写入（需求红线，命中错误文案不回显原文）。
///
/// 零依赖的字符串匹配，覆盖：常见 token 前缀、PEM 私钥、URL 内嵌凭据、
/// AWS AccessKey 形态。这是启发式不是安全边界，与项目静态高危规则互补。
pub fn scan_secret(text: &str) -> bool {
    const PREFIXES: [&str; 8] = [
        "ghp_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "sk-",
        "AKIA",
        "gho_",
        "ghs_",
    ];
    let lower = text.to_lowercase();
    if PREFIXES.iter().any(|p| lower.contains(&p.to_lowercase())) {
        return true;
    }
    if lower.contains("-----begin") && lower.contains("private key") {
        return true;
    }
    // http(s)://user:pass@host 形态
    if lower.contains("://") {
        for seg in lower.split("://").skip(1) {
            if let Some((creds, _)) = seg.split_once('@')
                && creds.contains(':')
                && !creds.contains('/')
            {
                return true;
            }
        }
    }
    // bearer / token: <value> 形态
    if lower.contains("bearer ") || lower.contains("authorization:") {
        return true;
    }
    false
}

/// 持久化 + 管理（实现：`agent-store` 的 jsonl 存储）。
#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn upsert(&self, e: MemoryEntry) -> Result<MemoryEntry, MemoryError>;
    async fn get(&self, id: &MemoryId) -> Result<Option<MemoryEntry>, MemoryError>;
    async fn list(&self, q: MemoryQuery) -> Result<Vec<MemoryEntry>, MemoryError>;
    /// 逻辑删除 → `Archived`（可追溯）。
    async fn delete(&self, id: &MemoryId) -> Result<(), MemoryError>;
    /// 推进 `last_used_at`（注入命中 + 检索命中）。
    async fn touch(&self, ids: &[MemoryId], at: i64) -> Result<(), MemoryError>;
    /// 状态迁移（候选批量确认 / 丢弃归档）。
    async fn set_state(&self, ids: &[MemoryId], state: MemoryState) -> Result<usize, MemoryError>;
}

/// 检索（一期：关键词+标签；二期可换向量实现，接口不变）。
pub trait MemoryIndex: Send + Sync {
    fn search(&self, q: MemoryQuery) -> Vec<MemoryEntry>;
}

/// TUI / 装配层的记忆门面：组合 store + index + 确认动作 + 注入组装。
#[async_trait]
pub trait MemoryPort: Send + Sync {
    /// 显式写入（`/remember`）：过准入管道，直接 `Active`。
    async fn remember(
        &self,
        scope: MemoryScope,
        content: String,
        tags: Vec<String>,
    ) -> Result<MemoryId, MemoryError>;

    /// 候选批量确认 → Active；返回确认条数。
    async fn confirm(&self, ids: &[MemoryId]) -> Result<usize, MemoryError>;

    /// 候选批量丢弃 → Archived；返回处理条数。
    async fn discard(&self, ids: &[MemoryId]) -> Result<usize, MemoryError>;

    /// 面板数据。
    async fn panel(&self, q: MemoryQuery) -> Result<Vec<MemoryEntry>, MemoryError>;

    /// 删除（逻辑删除 → Archived，可追溯）。
    async fn delete(&self, id: &MemoryId) -> Result<(), MemoryError>;

    /// 组装注入快照（动态截断 + 来源标注 + 冲突并存标注，O2/O3）。
    async fn compose(
        &self,
        scope: &MemoryScope,
        max_context: u32,
    ) -> Result<MemoryContext, MemoryError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_secret_rejects_common_credential_shapes() {
        assert!(scan_secret("我的 token 是 ghp_XXXXXXXXXXXXXX 请保存"));
        assert!(scan_secret("key: xoxb-123456-abcdef"));
        assert!(scan_secret("export OPENAI_KEY=sk-abcdefgh"));
        assert!(scan_secret("AKIAIOSFODNN7EXAMPLE"));
        assert!(scan_secret("-----BEGIN RSA PRIVATE KEY-----"));
        assert!(scan_secret("https://user:hunter2@example.com/repo"));
        assert!(scan_secret("Authorization: Bearer abc.def"));
        assert!(scan_secret("用 github_pat_xxx 好了"));
    }

    #[test]
    fn scan_secret_allows_plain_facts() {
        assert!(!scan_secret("推送 GitHub 经常需要重试 3~8 次"));
        assert!(!scan_secret("项目用 rsproxy 镜像加速 cargo"));
        // 假样例占位符不算真实凭据（对齐 interface-hardening §8.3 的惯例）。
        assert!(!scan_secret("示例：EXAMPLE_NOT_A_REAL_KEY_000"));
        assert!(!scan_secret("端口是 7890"));
    }

    #[test]
    fn implicit_key_normalizes_whitespace_and_case() {
        let a = MemoryEntry::new(
            MemoryScope::User,
            "Push GitHub Retry Three Times",
            vec![],
            MemorySource::Imported {
                task_id: None,
                at: 1,
            },
            MemoryState::Active,
            1,
        );
        let b = MemoryEntry::new(
            MemoryScope::User,
            "push  github   retry three  times",
            vec![],
            MemorySource::Imported {
                task_id: None,
                at: 2,
            },
            MemoryState::Active,
            2,
        );
        assert_eq!(a.implicit_key(), b.implicit_key());
    }

    #[test]
    fn memory_entry_serde_defaults_for_legacy_records() {
        // 旧记录没有 last_used_at / version 等字段也能反序列化。
        let json = serde_json::json!({
            "id": "m-1",
            "scope": "user",
            "content": "事实",
            "state": "active",
            "source": {"explicit": {"task_id": null, "at": 1}},
        });
        let e: MemoryEntry = serde_json::from_value(json).expect("旧记录兼容");
        assert_eq!(e.id.0, "m-1");
        assert_eq!(e.state, MemoryState::Active);
    }
}
