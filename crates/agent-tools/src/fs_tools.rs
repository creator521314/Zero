//! 文件系统工具：file_read / file_write / file_edit / file_restore / list_dir。

use agent_core::perm::Risk;
use agent_core::tool::{Artifact, Tool, ToolCtx, ToolError, ToolOutput};
use async_trait::async_trait;
use serde_json::json;

use crate::paths::resolve_in_ctx;

/// 读取文件（支持分段）。
pub struct FileRead;

#[async_trait]
impl Tool for FileRead {
    fn name(&self) -> &str {
        "file_read"
    }
    fn description(&self) -> &str {
        "读取工作区内某个文件的文本内容。可选 offset/limit 按行分段读取。"
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "相对工作区的路径" },
                "offset": { "type": "integer", "description": "起始行（0 基），可选" },
                "limit": { "type": "integer", "description": "最多读取行数，可选" }
            },
            "required": ["path"]
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
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'path'".into()))?;
        let full = resolve_in_ctx(ctx, path)?;

        // 读目录只会拿到「拒绝访问」这类系统级报错，模型看不懂；换成明确指引。
        if tokio::fs::metadata(&full)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            return Ok(ToolOutput::err(format!(
                "{path} 是目录，请用 list_dir 列出它"
            )));
        }

        let content = tokio::fs::read_to_string(&full)
            .await
            .map_err(|e| ToolError::Failed(format!("read {}: {e}", full.display())))?;

        let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        let slice: Vec<&str> = match limit {
            Some(l) => lines.iter().skip(offset).take(l).copied().collect(),
            None => lines.iter().skip(offset).copied().collect(),
        };
        let body = slice.join("\n");
        Ok(ToolOutput::ok(format!(
            "[{path}] ({total} lines total, showing {}-{})\n{body}",
            offset,
            offset + slice.len()
        )))
    }
}

/// 写入文件（覆写前由上层做快照）。
pub struct FileWrite;

#[async_trait]
impl Tool for FileWrite {
    fn name(&self) -> &str {
        "file_write"
    }
    fn description(&self) -> &str {
        "在工作区内创建或覆盖一个文件。目录会自动创建。"
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        })
    }
    fn risk_level(&self) -> Risk {
        Risk::Medium
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'path'".into()))?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'content'".into()))?;
        let full = resolve_in_ctx(ctx, path)?;

        // 覆写前快照：目标已存在则备份旧内容，供误写后回滚。
        if let Ok(old) = tokio::fs::read(&full).await {
            snapshot_before_write(ctx, &full, &old).await?;
        }

        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&full, content).await?;

        let bytes = content.len() as u64;
        Ok(
            ToolOutput::ok(format!("wrote {bytes} bytes to {path}")).with_artifacts(vec![
                Artifact {
                    path: full.display().to_string(),
                    bytes,
                },
            ]),
        )
    }
}

/// 精确字符串替换编辑。
pub struct FileEdit;

#[async_trait]
impl Tool for FileEdit {
    fn name(&self) -> &str {
        "file_edit"
    }
    fn description(&self) -> &str {
        "在文件内做精确字符串替换（old_str 必须唯一出现一次）。用于小范围修改，避免整文件覆写。"
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_str": { "type": "string" },
                "new_str": { "type": "string" }
            },
            "required": ["path", "old_str", "new_str"]
        })
    }
    fn risk_level(&self) -> Risk {
        Risk::Medium
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'path'".into()))?;
        let old = args
            .get("old_str")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'old_str'".into()))?;
        let new = args
            .get("new_str")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'new_str'".into()))?;
        let full = resolve_in_ctx(ctx, path)?;

        let content = tokio::fs::read_to_string(&full).await?;
        let count = content.matches(old).count();
        if count == 0 {
            return Ok(ToolOutput::err(format!("old_str not found in {path}")));
        }
        if count > 1 {
            return Ok(ToolOutput::err(format!(
                "old_str appears {count} times in {path}; make it unique"
            )));
        }
        let updated = content.replacen(old, new, 1);
        // 覆写前快照（file_edit 读到的就是旧内容）。
        snapshot_before_write(ctx, &full, content.as_bytes()).await?;
        tokio::fs::write(&full, &updated).await?;
        Ok(
            ToolOutput::ok(format!("edited {path}")).with_artifacts(vec![Artifact {
                path: full.display().to_string(),
                bytes: updated.len() as u64,
            }]),
        )
    }
}

/// 列目录。
pub struct ListDir;

#[async_trait]
impl Tool for ListDir {
    fn name(&self) -> &str {
        "list_dir"
    }
    fn description(&self) -> &str {
        "列出工作区内某个目录的直接子项。"
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string", "description": "相对工作区，默认 ." } },
            "required": []
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
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let full = resolve_in_ctx(ctx, path)?;
        let mut rd = tokio::fs::read_dir(&full).await?;
        let mut items = Vec::new();
        while let Some(e) = rd.next_entry().await? {
            let ty = e.file_type().await?;
            let kind = if ty.is_dir() { "dir" } else { "file" };
            items.push(format!("{kind}\t{}", e.file_name().to_string_lossy()));
        }
        items.sort();
        // 空目录要明说：否则模型只看到一个空列表，会继续猜子路径（历史教训：
        // 在一个空工作区里连撞 10 次 file_read）。
        if items.is_empty() {
            return Ok(ToolOutput::ok(format!(
                "[{path}]\n(空目录，没有任何文件或子目录)"
            )));
        }
        Ok(ToolOutput::ok(format!("[{path}]\n{}", items.join("\n"))))
    }
}

/// 把 `path` 相对沙箱根的路径编码成快照名的 label 部分。
///
/// 目录分隔符替换成 `_`（快照名必须是单级文件名），截断到 80 字符防超长。
/// `file_restore` 按同样的规则重建 label 来匹配某文件的快照。
fn snapshot_label(ctx: &ToolCtx, target: &std::path::Path) -> String {
    let base = target.strip_prefix(
        ctx.project_root
            .as_deref()
            .unwrap_or(ctx.workspace.as_path()),
    );
    let base = base.unwrap_or(target);
    let label: String = base
        .to_string_lossy()
        .chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect::<String>()
        .chars()
        .take(80)
        .collect();
    if label.is_empty() {
        "file".to_string()
    } else {
        label
    }
}

/// 快照回滚：把某文件在 `snapshots/` 里的备份写回原位置。
///
/// 用法：`{"path": "README.md"}` 恢复到最近一次覆写前的版本；
/// `{"path": "README.md", "snapshot": "README.md.171...3"}` 恢复到指定版本。
/// 恢复动作本身也会先把**当前内容**快照，所以回滚是可逆的。
pub struct FileRestore;

/// 快照名（`label.ts.seq`）按时间排序用的时间戳：取倒数第二段数字。
fn snapshot_ts(name: &str) -> u128 {
    name.rsplit('.')
        .nth(1)
        .and_then(|s| s.parse::<u128>().ok())
        .unwrap_or(0)
}

#[async_trait]
impl Tool for FileRestore {
    fn name(&self) -> &str {
        "file_restore"
    }
    fn description(&self) -> &str {
        "把文件恢复到某次覆写前的快照版本（file_write/file_edit 覆写前的旧内容会自动备份到 snapshots/）。不带 snapshot 参数时恢复最近一次备份。"
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "要恢复的文件路径（相对工作区/项目根）" },
                "snapshot": { "type": "string", "description": "具体快照文件名（可选；缺省=最近一次）。可先 list_dir \"snapshots/\" 查看" }
            },
            "required": ["path"]
        })
    }
    fn risk_level(&self) -> Risk {
        Risk::Medium
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'path'".into()))?;
        let target = resolve_in_ctx(ctx, path)?;

        let dir = ctx.workspace.join("snapshots");
        let label = snapshot_label(ctx, &target);

        // 收集该文件的所有快照，按时间从旧到新。
        let snaps: Vec<String> = match tokio::fs::read_dir(&dir).await {
            Ok(mut rd) => {
                let mut v = Vec::new();
                while let Some(e) = rd.next_entry().await? {
                    let name = e.file_name().to_string_lossy().to_string();
                    if name.starts_with(&format!("{label}.")) {
                        v.push(name);
                    }
                }
                v.sort_by_key(|n| snapshot_ts(n));
                v
            }
            Err(_) => Vec::new(),
        };
        if snaps.is_empty() {
            return Ok(ToolOutput::err(format!(
                "{path} 没有任何快照（只有 file_write/file_edit 覆写已存在的文件时才会产生备份）"
            )));
        }

        // 选快照：指定名必须存在；否则取最新。
        let chosen = match args.get("snapshot").and_then(|v| v.as_str()) {
            Some(name) => {
                if !snaps.iter().any(|s| s == name) {
                    return Ok(ToolOutput::err(format!(
                        "快照 {name} 不存在。{path} 的可用快照：\n{}",
                        snaps.join("\n")
                    )));
                }
                name.to_string()
            }
            None => snaps.last().unwrap().clone(),
        };

        let snap_path = dir.join(&chosen);
        let old = tokio::fs::read(&snap_path)
            .await
            .map_err(|e| ToolError::Failed(format!("read snapshot: {e}")))?;

        // 恢复前把当前内容也快照：误恢复可再滚回去。
        if let Ok(cur) = tokio::fs::read(&target).await {
            snapshot_before_write(ctx, &target, &cur).await?;
        }
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&target, &old).await?;

        let mut msg = format!(
            "restored {path} from snapshot {chosen} ({} bytes); 剩余 {} 个更早的快照可继续回滚",
            old.len(),
            snaps.iter().position(|s| *s == chosen).unwrap_or(0)
        );
        // 提示可用 list_dir 自己看快照目录。
        msg.push_str("\n可用 list_dir 查看 snapshots/ 目录了解全部备份。");
        Ok(ToolOutput::ok(msg))
    }
}

/// 覆写前把旧内容备份到私有工作区的 `snapshots/`（临时，支持回滚）。
///
/// 快照落在 `ctx.workspace/snapshots/` 而非项目根，避免污染用户仓库。
async fn snapshot_before_write(
    ctx: &ToolCtx,
    target: &std::path::Path,
    old: &[u8],
) -> Result<(), ToolError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let dir = ctx.workspace.join("snapshots");
    tokio::fs::create_dir_all(&dir).await?;

    // 快照名 = 目标相对沙箱根的路径 + 纳秒时间戳 + 进程内序号。
    // - 只取文件名：不同目录下的同名文件（a/config.toml、b/config.toml）
    //   会互相覆盖；
    // - 毫秒时间戳：同一毫秒内的两次覆写会撞名，旧快照被静默覆盖；
    // - 序号兜底保证任何情况下快照不重名。
    let label = snapshot_label(ctx, target);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    let snap = dir.join(format!("{label}.{ts}.{seq}"));
    tokio::fs::write(&snap, old).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    /// 沙箱在 `project_root`、私有数据在 `workspace` 的上下文。
    fn ctx(project_root: Option<&std::path::Path>, workspace: &std::path::Path) -> ToolCtx {
        ToolCtx {
            workspace: workspace.to_path_buf(),
            project_root: project_root.map(|p| p.to_path_buf()),
            task_id: "t".into(),
            timeout: Duration::from_secs(5),
            cancel: CancellationToken::new(),
        }
    }

    /// 根因级回归：曾有项目根时，相对路径会落到每任务新建的空工作区，
    /// 于是 `README.md` 一律「文件不存在」。
    #[tokio::test]
    async fn file_read_resolves_against_project_root() {
        let base = std::env::temp_dir().join("agent-tools-fs-projroot-test");
        let proj = base.join("proj");
        let ws = base.join("workspace").join("t1");
        tokio::fs::create_dir_all(&proj).await.unwrap();
        tokio::fs::create_dir_all(&ws).await.unwrap();
        tokio::fs::write(proj.join("README.md"), "# zero")
            .await
            .unwrap();

        let out = FileRead
            .execute(json!({ "path": "README.md" }), &ctx(Some(&proj), &ws))
            .await
            .expect("项目根下的文件必须能读到");
        assert!(!out.is_error, "实际: {}", out.content);
        assert!(out.content.contains("# zero"), "实际: {}", out.content);

        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    /// 没有项目根时退回私有工作区（旧行为），不能就此失效。
    #[tokio::test]
    async fn file_read_falls_back_to_private_workspace() {
        let base = std::env::temp_dir().join("agent-tools-fs-ws-test");
        let ws = base.join("workspace").join("t1");
        tokio::fs::create_dir_all(&ws).await.unwrap();
        tokio::fs::write(ws.join("a.txt"), "in-ws").await.unwrap();

        let out = FileRead
            .execute(json!({ "path": "a.txt" }), &ctx(None, &ws))
            .await
            .unwrap();
        assert!(out.content.contains("in-ws"), "实际: {}", out.content);

        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn list_dir_says_empty_instead_of_returning_blank() {
        let base = std::env::temp_dir().join("agent-tools-fs-empty-test");
        tokio::fs::create_dir_all(&base).await.unwrap();
        let out = ListDir
            .execute(json!({ "path": "." }), &ctx(None, &base))
            .await
            .unwrap();
        assert!(out.content.contains("空目录"), "实际: {}", out.content);

        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn reading_a_directory_gives_actionable_error() {
        let base = std::env::temp_dir().join("agent-tools-fs-isdir-test");
        tokio::fs::create_dir_all(&base).await.unwrap();
        let out = FileRead
            .execute(json!({ "path": "." }), &ctx(None, &base))
            .await
            .unwrap();
        assert!(out.is_error, "读目录应标为错误");
        assert!(
            out.content.contains("list_dir"),
            "应给出替代工具: {}",
            out.content
        );

        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    /// M2：file_write 覆写前把旧内容快照到私有工作区的 snapshots/。
    #[tokio::test]
    async fn file_write_snapshots_before_overwrite() {
        let base = std::env::temp_dir().join("agent-tools-fs-snap-write-test");
        let proj = base.join("proj");
        let ws = base.join("workspace").join("t1");
        tokio::fs::create_dir_all(&proj).await.unwrap();
        tokio::fs::create_dir_all(&ws).await.unwrap();
        tokio::fs::write(proj.join("note.txt"), "v1").await.unwrap();

        let out = FileWrite
            .execute(
                json!({ "path": "note.txt", "content": "v2" }),
                &ctx(Some(&proj), &ws),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "实际: {}", out.content);
        assert_eq!(
            tokio::fs::read_to_string(proj.join("note.txt"))
                .await
                .unwrap(),
            "v2"
        );

        // 快照目录里应有旧版本 v1（命名 note.txt.<毫秒>）。
        let snap_dir = ws.join("snapshots");
        let mut found = false;
        let mut entries = tokio::fs::read_dir(&snap_dir).await.unwrap();
        while let Some(e) = entries.next_entry().await.unwrap() {
            let p = e.path();
            let name = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            if name.starts_with("note.txt.") {
                assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "v1");
                found = true;
            }
        }
        assert!(found, "快照目录应包含 note.txt 的旧版本");

        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    /// M2：file_edit 覆写前同样快照。
    #[tokio::test]
    async fn file_edit_snapshots_before_overwrite() {
        let base = std::env::temp_dir().join("agent-tools-fs-snap-edit-test");
        let proj = base.join("proj");
        let ws = base.join("workspace").join("t1");
        tokio::fs::create_dir_all(&proj).await.unwrap();
        tokio::fs::create_dir_all(&ws).await.unwrap();
        tokio::fs::write(proj.join("src.txt"), "a-b-c")
            .await
            .unwrap();

        let out = FileEdit
            .execute(
                json!({ "path": "src.txt", "old_str": "b", "new_str": "B" }),
                &ctx(Some(&proj), &ws),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "实际: {}", out.content);
        assert_eq!(
            tokio::fs::read_to_string(proj.join("src.txt"))
                .await
                .unwrap(),
            "a-B-c"
        );

        let snap_dir = ws.join("snapshots");
        let mut found = false;
        let mut entries = tokio::fs::read_dir(&snap_dir).await.unwrap();
        while let Some(e) = entries.next_entry().await.unwrap() {
            let p = e.path();
            let name = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            if name.starts_with("src.txt.") {
                assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "a-b-c");
                found = true;
            }
        }
        assert!(found, "快照目录应包含 src.txt 的旧版本");

        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    /// 快照不能互相覆盖：不同目录下的同名文件、同一文件的连续两次覆写，
    /// 每次都必须留下独立的快照。
    ///
    /// 回归：快照名曾只用 `<文件名>.<毫秒>`——`a/config.toml` 与
    /// `b/config.toml` 同名，同一毫秒内的两次写也同名，旧快照被静默覆盖。
    #[tokio::test]
    async fn snapshots_never_overwrite_each_other() {
        let base = std::env::temp_dir().join("agent-tools-fs-snap-collide-test");
        let proj = base.join("proj");
        let ws = base.join("workspace").join("t1");
        tokio::fs::create_dir_all(proj.join("a")).await.unwrap();
        tokio::fs::create_dir_all(proj.join("b")).await.unwrap();
        tokio::fs::create_dir_all(&ws).await.unwrap();
        tokio::fs::write(proj.join("a/config.toml"), "A1")
            .await
            .unwrap();
        tokio::fs::write(proj.join("b/config.toml"), "B1")
            .await
            .unwrap();

        let c = ctx(Some(&proj), &ws);
        for (path, content) in [
            ("a/config.toml", "A2"),
            ("b/config.toml", "B2"),
            // 同一文件连续覆写两次：两份不同的旧内容都要留档
            ("a/config.toml", "A3"),
        ] {
            FileWrite
                .execute(json!({ "path": path, "content": content }), &c)
                .await
                .unwrap();
        }

        let mut names = Vec::new();
        let mut bodies = Vec::new();
        let mut entries = tokio::fs::read_dir(ws.join("snapshots")).await.unwrap();
        while let Some(e) = entries.next_entry().await.unwrap() {
            names.push(e.file_name().to_string_lossy().to_string());
            bodies.push(tokio::fs::read_to_string(e.path()).await.unwrap());
        }
        assert_eq!(names.len(), 3, "三次覆写应留下三份快照: {names:?}");
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 3, "快照名不能重复: {names:?}");
        for want in ["A1", "B1", "A2"] {
            assert!(
                bodies.iter().any(|b| b == want),
                "缺少旧内容 {want} 的快照: {bodies:?}"
            );
        }
        // 路径进入快照名：不同目录的同名文件可区分。
        assert!(
            names.iter().any(|n| n.contains("a_config.toml")),
            "{names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("b_config.toml")),
            "{names:?}"
        );

        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    /// M2：新建文件（不存在）不应产生空快照。
    #[tokio::test]
    async fn file_write_creates_no_snapshot_for_new_file() {
        let base = std::env::temp_dir().join("agent-tools-fs-snap-new-test");
        let proj = base.join("proj");
        let ws = base.join("workspace").join("t1");
        tokio::fs::create_dir_all(&proj).await.unwrap();
        tokio::fs::create_dir_all(&ws).await.unwrap();

        FileWrite
            .execute(
                json!({ "path": "fresh.txt", "content": "new" }),
                &ctx(Some(&proj), &ws),
            )
            .await
            .unwrap();

        let snap_dir = ws.join("snapshots");
        assert!(
            !snap_dir.exists(),
            "新建文件不应产生快照目录: {:?}",
            snap_dir
        );

        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    /// file_restore：默认恢复最近一次覆写前的内容；恢复动作本身也先快照
    /// （回滚可逆）；再滚一步能拿回被恢复前的那版。
    #[tokio::test]
    async fn file_restore_reverts_and_is_itself_reversible() {
        let base = std::env::temp_dir().join("agent-tools-fs-restore-test");
        let proj = base.join("proj");
        let ws = base.join("workspace").join("t1");
        tokio::fs::create_dir_all(&proj).await.unwrap();
        tokio::fs::create_dir_all(&ws).await.unwrap();
        tokio::fs::write(proj.join("doc.txt"), "v1").await.unwrap();

        let c = ctx(Some(&proj), &ws);
        FileWrite
            .execute(json!({ "path": "doc.txt", "content": "v2" }), &c)
            .await
            .unwrap();
        FileWrite
            .execute(json!({ "path": "doc.txt", "content": "v3" }), &c)
            .await
            .unwrap();

        // 无 snapshot 参数 → 恢复到最近一次覆写前（v2）。
        let out = FileRestore
            .execute(json!({ "path": "doc.txt" }), &c)
            .await
            .unwrap();
        assert!(out.content.contains("restored"), "{}", out.content);
        let body = tokio::fs::read_to_string(proj.join("doc.txt"))
            .await
            .unwrap();
        assert_eq!(body, "v2", "应恢复到最近一次覆写前的 v2");

        // 再滚一步（恢复动作先快照了 v3，所以有 3 份快照：v1/v2/v3），
        // 指定最早的快照 → 回到 v1。
        let mut snaps: Vec<String> = Vec::new();
        let mut rd = tokio::fs::read_dir(ws.join("snapshots")).await.unwrap();
        while let Some(e) = rd.next_entry().await.unwrap() {
            snaps.push(e.file_name().to_string_lossy().to_string());
        }
        assert_eq!(snaps.len(), 3, "v1/v2 快照 + 恢复时对 v3 的快照: {snaps:?}");
        snaps.sort_by_key(|n| snapshot_ts(n));
        let oldest = snaps.first().unwrap().clone();
        FileRestore
            .execute(json!({ "path": "doc.txt", "snapshot": oldest }), &c)
            .await
            .unwrap();
        let body = tokio::fs::read_to_string(proj.join("doc.txt"))
            .await
            .unwrap();
        assert_eq!(body, "v1", "指定快照应恢复到最早的 v1");

        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    /// 没有快照时给出可操作的错误，而不是泛泛的 not found。
    #[tokio::test]
    async fn file_restore_without_snapshots_explains_why() {
        let base = std::env::temp_dir().join("agent-tools-fs-restore-none-test");
        let proj = base.join("proj");
        let ws = base.join("workspace").join("t1");
        tokio::fs::create_dir_all(&proj).await.unwrap();
        tokio::fs::create_dir_all(&ws).await.unwrap();
        tokio::fs::write(proj.join("a.txt"), "x").await.unwrap();

        let c = ctx(Some(&proj), &ws);
        let out = FileRestore
            .execute(json!({ "path": "a.txt" }), &c)
            .await
            .unwrap();
        assert!(
            out.content.contains("没有任何快照") && out.content.contains("a.txt"),
            "错误要说明原因: {}",
            out.content
        );

        // 指定不存在的快照名 → 列出可用快照。
        tokio::fs::write(proj.join("a.txt"), "y").await.unwrap();
        FileWrite
            .execute(json!({ "path": "a.txt", "content": "z" }), &c)
            .await
            .unwrap();
        let out = FileRestore
            .execute(json!({ "path": "a.txt", "snapshot": "nope.0.0" }), &c)
            .await
            .unwrap();
        assert!(
            out.content.contains("可用快照"),
            "应列出可用快照: {}",
            out.content
        );

        let _ = tokio::fs::remove_dir_all(&base).await;
    }
}
