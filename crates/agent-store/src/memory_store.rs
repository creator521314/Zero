//! 记忆系统持久化实现：两层 jsonl（append-only）+ 内存索引 + 关键词检索。
//!
//! 设计见 `docs/memory-design.md`：
//! - 用户级 `~/.config/zero/memory.jsonl`，项目级 `<project_root>/.zero/memory.jsonl`；
//! - append-only 操作日志（upsert/touch/delete/state），启动回放，超阈值压缩；
//! - 同 scope + 同 dedup key 冲突：新覆盖旧并保留历史版本（Archived 可追溯）；
//! - Active 条数上限 + LRU 淘汰（`last_used_at` 由注入与检索共同推进）；
//! - `MemoryService` 同时实现 `MemoryStore` / `MemoryIndex` / `MemoryPort` 三个
//!   core trait，共享同一份内存索引，避免多副本漂移。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use agent_core::memory::{
    MemoryConfig, MemoryContext, MemoryEntry, MemoryError, MemoryId, MemoryIndex, MemoryPort,
    MemoryQuery, MemoryScope, MemoryState, MemoryStore,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// jsonl 操作记录。
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum MemoryRecord {
    Upsert {
        entry: MemoryEntry,
    },
    Touch {
        id: MemoryId,
        at: i64,
    },
    Delete {
        id: MemoryId,
        at: i64,
    },
    State {
        id: MemoryId,
        state: MemoryState,
        at: i64,
    },
}

#[derive(Default)]
struct FileCache {
    entries: HashMap<MemoryId, MemoryEntry>,
    records: usize,
}

/// 单实例共享状态：同一文件的缓存只加载/回放一次。
struct Inner {
    cfg: MemoryConfig,
    user_path: PathBuf,
    caches: HashMap<PathBuf, FileCache>,
}

impl Inner {
    fn scope_path(&self, scope: &MemoryScope) -> PathBuf {
        match scope {
            MemoryScope::User => self.user_path.clone(),
            MemoryScope::Project { root } => PathBuf::from(root.trim_end_matches('/'))
                .join(".zero")
                .join("memory.jsonl"),
        }
    }

    /// 加载并回放（幂等）。
    fn ensure_loaded(&mut self, path: &PathBuf) -> Result<(), MemoryError> {
        if self.caches.contains_key(path) {
            return Ok(());
        }
        let mut cache = FileCache::default();
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                // 单行损坏只跳过该行，不阻断整体回放。
                if let Ok(rec) = serde_json::from_str::<MemoryRecord>(line) {
                    apply_record(&mut cache, rec);
                }
            }
        }
        self.caches.insert(path.clone(), cache);
        Ok(())
    }

    /// 追加一行操作记录（含必要的目录创建）。
    fn append(&mut self, path: &PathBuf, rec: &MemoryRecord) -> Result<(), MemoryError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        use std::io::Write;
        let line = serde_json::to_string(rec).map_err(|e| MemoryError::Other(e.to_string()))?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(f, "{line}")?;
        if let Some(c) = self.caches.get_mut(path) {
            c.records += 1;
        }
        Ok(())
    }

    /// 记录数超阈值时，把当前全量状态重写为快照并原子替换。
    fn maybe_compact(&mut self, path: &PathBuf) -> Result<(), MemoryError> {
        let need = self
            .caches
            .get(path)
            .map(|c| c.records > self.cfg.compact_over_records)
            .unwrap_or(false);
        if !need {
            return Ok(());
        }
        let cache = self.caches.get(path).expect("已加载");
        let mut buf = String::new();
        for e in cache.entries.values() {
            buf.push_str(
                &serde_json::to_string(&MemoryRecord::Upsert { entry: e.clone() })
                    .expect("序列化不会失败"),
            );
            buf.push('\n');
        }
        let tmp = path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, buf)?;
        std::fs::rename(&tmp, path)?;
        if let Some(c) = self.caches.get_mut(path) {
            c.records = c.entries.len();
        }
        Ok(())
    }
}

fn apply_record(cache: &mut FileCache, rec: MemoryRecord) {
    match rec {
        MemoryRecord::Upsert { entry } => {
            cache.entries.insert(entry.id.clone(), entry);
            cache.records += 1;
        }
        MemoryRecord::Touch { id, at } => {
            if let Some(e) = cache.entries.get_mut(&id) {
                e.last_used_at = e.last_used_at.max(at);
            }
            cache.records += 1;
        }
        MemoryRecord::Delete { id, at } => {
            if let Some(e) = cache.entries.get_mut(&id) {
                e.state = MemoryState::Archived;
                e.updated_at = at;
            }
            cache.records += 1;
        }
        MemoryRecord::State { id, state, at } => {
            if let Some(e) = cache.entries.get_mut(&id) {
                e.state = state;
                e.updated_at = at;
            }
            cache.records += 1;
        }
    }
}

fn matches_query(e: &MemoryEntry, q: &MemoryQuery) -> bool {
    if let Some(state) = q.state {
        if e.state != state {
            return false;
        }
    } else if e.state != MemoryState::Active {
        return false;
    }
    if let Some(scope) = &q.scope {
        if &e.scope != scope {
            return false;
        }
    }
    if let Some(kw) = &q.keyword {
        let kw = kw.to_lowercase();
        let hit = e.content.to_lowercase().contains(&kw)
            || e.tags.iter().any(|t| t.to_lowercase().contains(&kw));
        if !hit {
            return false;
        }
    }
    true
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// 记忆服务：同时实现 `MemoryStore` / `MemoryIndex` / `MemoryPort`。
pub struct MemoryService {
    inner: Arc<Mutex<Inner>>,
}

impl MemoryService {
    /// `user_path`：用户级 jsonl 路径（生产用 `~/.config/zero/memory.jsonl`，
    /// 测试用临时目录）。构造时立即回放用户级文件；项目级文件按 scope 惰性加载。
    pub fn new(user_path: PathBuf, cfg: MemoryConfig) -> Self {
        let mut inner = Inner {
            cfg,
            user_path: user_path.clone(),
            caches: HashMap::new(),
        };
        if let Err(e) = inner.ensure_loaded(&user_path) {
            tracing::warn!(error = %e, "加载用户级记忆文件失败，按空库继续");
        }
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }
}

#[async_trait]
impl MemoryStore for MemoryService {
    async fn upsert(&self, mut e: MemoryEntry) -> Result<MemoryEntry, MemoryError> {
        let mut inner = self.inner.lock().expect("memory lock poisoned");
        let path = inner.scope_path(&e.scope);
        inner.ensure_loaded(&path)?;
        let now = now_ts();
        e.updated_at = now;
        e.last_used_at = e.last_used_at.max(now);

        // 冲突：同 scope + 同 dedup key 且仍 Active 的旧条目 → 归档留版本。
        let key = e.dedup_key();
        let old = {
            let cache = inner.caches.get(&path).expect("已加载");
            cache
                .entries
                .values()
                .find(|c| {
                    c.state == MemoryState::Active
                        && c.scope == e.scope
                        && c.dedup_key() == key
                        && c.id != e.id
                })
                .cloned()
        };
        if let Some(old) = old {
            let mut newer = e;
            newer.version = old.version + 1;
            newer.created_at = old.created_at;
            inner.append(
                &path,
                &MemoryRecord::State {
                    id: old.id.clone(),
                    state: MemoryState::Archived,
                    at: now,
                },
            )?;
            if let Some(c) = inner.caches.get_mut(&path) {
                if let Some(o) = c.entries.get_mut(&old.id) {
                    o.state = MemoryState::Archived;
                    o.updated_at = now;
                }
            }
            inner.append(
                &path,
                &MemoryRecord::Upsert {
                    entry: newer.clone(),
                },
            )?;
            if let Some(c) = inner.caches.get_mut(&path) {
                c.entries.insert(newer.id.clone(), newer.clone());
            }
            inner.maybe_compact(&path)?;
            return Ok(newer);
        }

        inner.append(&path, &MemoryRecord::Upsert { entry: e.clone() })?;
        if let Some(c) = inner.caches.get_mut(&path) {
            c.entries.insert(e.id.clone(), e.clone());
        }

        // Active 条数上限：超出按 last_used_at 最旧归档（LRU）。
        let limit = match e.scope {
            MemoryScope::User => inner.cfg.user_active_limit,
            MemoryScope::Project { .. } => inner.cfg.project_active_limit,
        };
        let mut actives: Vec<MemoryEntry> = {
            let cache = inner.caches.get(&path).expect("已加载");
            cache
                .entries
                .values()
                .filter(|x| x.state == MemoryState::Active && x.scope == e.scope)
                .cloned()
                .collect()
        };
        if actives.len() > limit {
            actives.sort_by_key(|x| (x.last_used_at, x.created_at));
            let evict = actives.len() - limit;
            for victim in actives.into_iter().take(evict) {
                inner.append(
                    &path,
                    &MemoryRecord::State {
                        id: victim.id.clone(),
                        state: MemoryState::Archived,
                        at: now,
                    },
                )?;
                if let Some(c) = inner.caches.get_mut(&path) {
                    if let Some(v) = c.entries.get_mut(&victim.id) {
                        v.state = MemoryState::Archived;
                        v.updated_at = now;
                    }
                }
            }
        }
        inner.maybe_compact(&path)?;
        Ok(e)
    }

    async fn get(&self, id: &MemoryId) -> Result<Option<MemoryEntry>, MemoryError> {
        let inner = self.inner.lock().expect("memory lock poisoned");
        Ok(inner
            .caches
            .values()
            .find_map(|c| c.entries.get(id))
            .cloned())
    }

    async fn list(&self, q: MemoryQuery) -> Result<Vec<MemoryEntry>, MemoryError> {
        let mut inner = self.inner.lock().expect("memory lock poisoned");
        // 指定了作用域时惰性加载对应文件（项目级文件可能本会话尚未写入过）。
        if let Some(scope) = &q.scope {
            let path = inner.scope_path(scope);
            inner.ensure_loaded(&path)?;
        }
        let mut all: Vec<MemoryEntry> = inner
            .caches
            .values()
            .flat_map(|c| c.entries.values())
            .filter(|e| matches_query(e, &q))
            .cloned()
            .collect();
        // 稳定输出：最近使用优先，其次更新时间，最后 id 保证确定性。
        all.sort_by(|a, b| {
            b.last_used_at
                .cmp(&a.last_used_at)
                .then(b.updated_at.cmp(&a.updated_at))
                .then(a.id.0.cmp(&b.id.0))
        });
        if q.limit > 0 {
            all.truncate(q.limit);
        }
        Ok(all)
    }

    async fn delete(&self, id: &MemoryId) -> Result<(), MemoryError> {
        let mut inner = self.inner.lock().expect("memory lock poisoned");
        let now = now_ts();
        let paths: Vec<PathBuf> = inner.caches.keys().cloned().collect();
        for path in &paths {
            let hit = inner
                .caches
                .get(path)
                .map(|c| c.entries.contains_key(id))
                .unwrap_or(false);
            if hit {
                inner.append(
                    path,
                    &MemoryRecord::Delete {
                        id: id.clone(),
                        at: now,
                    },
                )?;
                if let Some(c) = inner.caches.get_mut(path) {
                    if let Some(e) = c.entries.get_mut(id) {
                        e.state = MemoryState::Archived;
                        e.updated_at = now;
                    }
                }
            }
        }
        Ok(())
    }

    async fn touch(&self, ids: &[MemoryId], at: i64) -> Result<(), MemoryError> {
        let mut inner = self.inner.lock().expect("memory lock poisoned");
        let paths: Vec<PathBuf> = inner.caches.keys().cloned().collect();
        for path in &paths {
            let hit: Vec<MemoryId> = {
                let c = inner.caches.get(path).expect("已加载");
                ids.iter()
                    .filter(|id| c.entries.contains_key(*id))
                    .cloned()
                    .collect()
            };
            for id in hit {
                inner.append(path, &MemoryRecord::Touch { id: id.clone(), at })?;
                if let Some(c) = inner.caches.get_mut(path) {
                    if let Some(e) = c.entries.get_mut(&id) {
                        e.last_used_at = e.last_used_at.max(at);
                    }
                }
            }
        }
        Ok(())
    }

    async fn set_state(&self, ids: &[MemoryId], state: MemoryState) -> Result<usize, MemoryError> {
        let mut inner = self.inner.lock().expect("memory lock poisoned");
        let now = now_ts();
        let paths: Vec<PathBuf> = inner.caches.keys().cloned().collect();
        let mut n = 0usize;
        for path in &paths {
            let hit: Vec<MemoryId> = {
                let c = inner.caches.get(path).expect("已加载");
                ids.iter()
                    .filter(|id| c.entries.contains_key(*id))
                    .cloned()
                    .collect()
            };
            for id in hit {
                inner.append(
                    path,
                    &MemoryRecord::State {
                        id: id.clone(),
                        state,
                        at: now,
                    },
                )?;
                if let Some(c) = inner.caches.get_mut(path) {
                    if let Some(e) = c.entries.get_mut(&id) {
                        e.state = state;
                        e.updated_at = now;
                    }
                }
                n += 1;
            }
        }
        Ok(n)
    }
}

impl MemoryIndex for MemoryService {
    fn search(&self, q: MemoryQuery) -> Vec<MemoryEntry> {
        // 同步快照：锁只在取值时短暂持有；跨 await 不可，故拷出再过滤。
        let mut inner = self.inner.lock().expect("memory lock poisoned");
        if let Some(scope) = &q.scope {
            let path = inner.scope_path(scope);
            if let Err(e) = inner.ensure_loaded(&path) {
                tracing::warn!(error = %e, "加载记忆文件失败，按已加载内容检索");
            }
        }
        let mut all: Vec<MemoryEntry> = inner
            .caches
            .values()
            .flat_map(|c| c.entries.values())
            .filter(|e| matches_query(e, &q))
            .cloned()
            .collect();
        all.sort_by(|a, b| {
            b.last_used_at
                .cmp(&a.last_used_at)
                .then(b.updated_at.cmp(&a.updated_at))
                .then(a.id.0.cmp(&b.id.0))
        });
        if q.limit > 0 {
            all.truncate(q.limit);
        }
        all
    }
}

#[async_trait]
impl MemoryPort for MemoryService {
    async fn remember(
        &self,
        scope: MemoryScope,
        content: String,
        tags: Vec<String>,
    ) -> Result<MemoryId, MemoryError> {
        // ---- 准入管道（顺序不可调换，见设计 §4.1）----
        let cfg_max = self
            .inner
            .lock()
            .expect("memory lock poisoned")
            .cfg
            .max_entry_chars;
        if content.chars().count() > cfg_max {
            return Err(MemoryError::TooLong { max: cfg_max });
        }
        if agent_core::memory::scan_secret(&content) {
            return Err(MemoryError::SecretDetected);
        }
        let now = now_ts();
        let entry = MemoryEntry::new(
            scope,
            content,
            tags,
            agent_core::memory::MemorySource::Explicit {
                task_id: None,
                at: now,
            },
            MemoryState::Active,
            now,
        );
        let stored = MemoryStore::upsert(self, entry).await?;
        Ok(stored.id)
    }

    async fn confirm(&self, ids: &[MemoryId]) -> Result<usize, MemoryError> {
        MemoryStore::set_state(self, ids, MemoryState::Active).await
    }

    async fn discard(&self, ids: &[MemoryId]) -> Result<usize, MemoryError> {
        MemoryStore::set_state(self, ids, MemoryState::Archived).await
    }

    async fn panel(&self, q: MemoryQuery) -> Result<Vec<MemoryEntry>, MemoryError> {
        MemoryStore::list(self, q).await
    }

    async fn compose(
        &self,
        scope: &MemoryScope,
        max_context: u32,
    ) -> Result<MemoryContext, MemoryError> {
        let (ratio, _) = {
            let inner = self.inner.lock().expect("memory lock poisoned");
            (inner.cfg.inject_ratio, inner.cfg.max_entry_chars)
        };
        let now = now_ts();

        // 作用域：User 恒取；scope 为 Project 时再加项目级。冲突条目两条都保留（O2）。
        let mut user_q = MemoryQuery {
            keyword: None,
            scope: Some(MemoryScope::User),
            state: Some(MemoryState::Active),
            limit: 0,
        };
        let mut entries = MemoryStore::list(self, user_q.clone()).await?;
        if let MemoryScope::Project { root } = scope {
            user_q.scope = Some(MemoryScope::Project { root: root.clone() });
            entries.extend(MemoryStore::list(self, user_q).await?);
        }

        // 动态截断（O3）：预算按 max_context * ratio 折算字符（token ≈ chars/3）。
        let budget_tokens = (max_context as f64 * ratio) as usize;
        let budget_chars = budget_tokens.saturating_mul(3);
        // 冲突检测：同 dedup key 跨作用域出现 → 双方标注。
        use std::collections::HashSet;
        let mut seen_keys: HashSet<String> = HashSet::new();
        let mut dup_keys: HashSet<String> = HashSet::new();
        for e in &entries {
            let k = e.dedup_key();
            if !seen_keys.insert(k.clone()) {
                dup_keys.insert(k);
            }
        }

        let mut text = String::new();
        let mut injected_ids = Vec::new();
        let mut truncated = 0usize;
        for e in &entries {
            let scope_label = match &e.scope {
                MemoryScope::User => "用户级",
                MemoryScope::Project { .. } => "项目级",
            };
            let conflict_note = if dup_keys.contains(&e.dedup_key()) {
                "（与另一作用域条目冲突，请自行判断）"
            } else {
                ""
            };
            let tags = if e.tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", e.tags.join(","))
            };
            let line = format!(
                "【记忆 · {scope_label}】{}{tags}{conflict_note}\n",
                e.content
            );
            if text.chars().count() + line.chars().count() > budget_chars {
                truncated += 1;
                continue;
            }
            text.push_str(&line);
            injected_ids.push(e.id.clone());
        }
        MemoryStore::touch(self, &injected_ids, now).await?;
        Ok(MemoryContext {
            text,
            injected_ids,
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::memory::MemorySource;

    fn svc(tmp: &std::path::Path) -> Arc<MemoryService> {
        Arc::new(MemoryService::new(
            tmp.join("user-memory.jsonl"),
            MemoryConfig::default(),
        ))
    }

    fn project(root: &std::path::Path) -> MemoryScope {
        MemoryScope::Project {
            root: root.display().to_string(),
        }
    }

    async fn remember(port: &dyn MemoryPort, scope: MemoryScope, content: &str) -> MemoryId {
        port.remember(scope, content.into(), vec![])
            .await
            .expect("写入应成功")
    }

    #[tokio::test]
    async fn remember_persists_and_reloads_across_instances() {
        let tmp = std::env::temp_dir().join(format!("zero-mem-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let s = svc(&tmp);
        let id = remember(s.as_ref(), MemoryScope::User, "推送 GitHub 要重试").await;

        // 新实例（模拟关掉进程再开）能从 jsonl 回放出来。
        let s2 = MemoryService::new(tmp.join("user-memory.jsonl"), MemoryConfig::default());
        let q = MemoryQuery {
            keyword: Some("github".into()),
            ..Default::default()
        };
        let hits = MemoryIndex::search(&s2, q);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn project_scope_is_isolated() {
        let tmp = std::env::temp_dir().join(format!("zero-mem-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let s = svc(&tmp);
        let px = tmp.join("proj-x");
        let py = tmp.join("proj-y");
        std::fs::create_dir_all(&px).unwrap();
        std::fs::create_dir_all(&py).unwrap();
        remember(s.as_ref(), project(&px), "X 项目用 rsproxy").await;

        let q = MemoryQuery {
            scope: Some(project(&py)),
            ..Default::default()
        };
        assert!(
            MemoryIndex::search(s.as_ref(), q).is_empty(),
            "项目 Y 不应看到项目 X 的记忆"
        );
        let q = MemoryQuery {
            scope: Some(project(&px)),
            ..Default::default()
        };
        assert_eq!(MemoryIndex::search(s.as_ref(), q).len(), 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn conflict_keeps_history_and_bumps_version() {
        let tmp = std::env::temp_dir().join(format!("zero-mem-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let s = svc(&tmp);
        let scope = MemoryScope::User;
        let first = remember(s.as_ref(), scope.clone(), "代理端口是 7890").await;
        let second = remember(s.as_ref(), scope.clone(), "代理端口是 10809").await;

        // 显式 key 冲突走「新覆盖旧」；这里两条内容不同 → 隐式键不同，应并存。
        // 用同 key 验证版本递增：
        let mut e = MemoryEntry::new(
            scope.clone(),
            "同键条目 v1",
            vec![],
            MemorySource::Explicit {
                task_id: None,
                at: 1,
            },
            MemoryState::Active,
            1,
        );
        e.key = Some("proxy.port".into());
        let v1 = MemoryStore::upsert(s.as_ref(), e).await.unwrap();
        let mut e2 = v1.clone();
        e2.id = MemoryId::new();
        e2.content = "同键条目 v2".into();
        e2.source = MemorySource::Explicit {
            task_id: None,
            at: 2,
        };
        let v2 = MemoryStore::upsert(s.as_ref(), e2).await.unwrap();
        assert_eq!(v2.version, v1.version + 1, "同键写入应递增版本");
        let old = MemoryStore::get(s.as_ref(), &v1.id).await.unwrap().unwrap();
        assert_eq!(old.state, MemoryState::Archived, "旧版本应归档保留");
        assert_ne!(first, second);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn secret_and_oversize_are_rejected() {
        let tmp = std::env::temp_dir().join(format!("zero-mem-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let s = svc(&tmp);
        let r = s
            .remember(
                MemoryScope::User,
                "token 是 ghp_AAAAAAAAAAAAAAAAAAAAAA".into(),
                vec![],
            )
            .await;
        assert!(matches!(r, Err(MemoryError::SecretDetected)));
        let long = "x".repeat(501);
        let r = s.remember(MemoryScope::User, long, vec![]).await;
        assert!(matches!(r, Err(MemoryError::TooLong { max: 500 })));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn candidates_are_invisible_until_confirmed() {
        let tmp = std::env::temp_dir().join(format!("zero-mem-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let s = svc(&tmp);
        let mut e = MemoryEntry::new(
            MemoryScope::User,
            "提炼出的候选事实",
            vec![],
            MemorySource::Distilled {
                task_id: None,
                at: 1,
            },
            MemoryState::Candidate,
            1,
        );
        e = MemoryStore::upsert(s.as_ref(), e).await.unwrap();
        // 默认检索只回 Active → 候选不可见。
        assert!(MemoryIndex::search(s.as_ref(), MemoryQuery::default()).is_empty());
        // 确认后可见。
        let n = s.confirm(&[e.id.clone()]).await.unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            MemoryIndex::search(s.as_ref(), MemoryQuery::default()).len(),
            1
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn lru_eviction_enforces_active_limit() {
        let tmp = std::env::temp_dir().join(format!("zero-mem-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let s = Arc::new(MemoryService::new(
            tmp.join("user-memory.jsonl"),
            MemoryConfig {
                user_active_limit: 3,
                ..MemoryConfig::default()
            },
        ));
        for i in 0..4 {
            remember(s.as_ref(), MemoryScope::User, &format!("事实 {i}")).await;
        }
        let q = MemoryQuery {
            scope: Some(MemoryScope::User),
            ..Default::default()
        };
        assert_eq!(
            MemoryIndex::search(s.as_ref(), q).len(),
            3,
            "超出上限应 LRU 淘汰"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn compose_truncates_and_annotates_conflicts() {
        let tmp = std::env::temp_dir().join(format!("zero-mem-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let s = svc(&tmp);
        let px = tmp.join("proj");
        std::fs::create_dir_all(&px).unwrap();
        remember(s.as_ref(), MemoryScope::User, "全局：回复用中文").await;
        remember(s.as_ref(), project(&px), "项目：端口 7890").await;

        let ctx = s
            .compose(&project(&px), 128_000)
            .await
            .expect("compose 应成功");
        assert!(ctx.text.contains("【记忆 · 用户级】"));
        assert!(ctx.text.contains("【记忆 · 项目级】"));
        assert_eq!(ctx.injected_ids.len(), 2);
        assert_eq!(ctx.truncated, 0);

        // 极小预算 → 全部截断。
        let ctx = s.compose(&project(&px), 1).await.unwrap();
        assert_eq!(ctx.injected_ids.len(), 0);
        assert_eq!(ctx.truncated, 2, "极小预算应丢弃全部条目");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn compact_keeps_state_after_rewrite() {
        let tmp = std::env::temp_dir().join(format!("zero-mem-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("user-memory.jsonl");
        let s = Arc::new(MemoryService::new(
            path.clone(),
            MemoryConfig {
                compact_over_records: 5,
                ..MemoryConfig::default()
            },
        ));
        let ids: Vec<MemoryId> = {
            let mut v = Vec::new();
            for i in 0..6 {
                v.push(remember(s.as_ref(), MemoryScope::User, &format!("条目{i}")).await);
            }
            v
        };
        s.delete(&ids[0]).await.unwrap();
        // 新实例回放压缩后的文件：删除仍然生效。
        let s2 = MemoryService::new(path.clone(), MemoryConfig::default());
        let gone = MemoryStore::get(&s2, &ids[0]).await.unwrap().unwrap();
        assert_eq!(gone.state, MemoryState::Archived);
        let alive = MemoryStore::get(&s2, &ids[1]).await.unwrap().unwrap();
        assert_eq!(alive.state, MemoryState::Active);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
