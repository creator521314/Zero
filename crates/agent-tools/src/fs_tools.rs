//! 文件系统工具：file_read / file_write / file_edit / list_dir。

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
}
