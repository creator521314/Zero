//! 文件系统工具：file_read / file_write / file_edit / list_dir。

use agent_core::perm::Risk;
use agent_core::tool::{Artifact, Tool, ToolCtx, ToolError, ToolOutput};
use async_trait::async_trait;
use serde_json::json;

use crate::paths::resolve_in_workspace;

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

    async fn execute(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'path'".into()))?;
        let full = resolve_in_workspace(&ctx.workspace, path)?;

        let content = tokio::fs::read_to_string(&full).await.map_err(|e| {
            ToolError::Failed(format!("read {}: {e}", full.display()))
        })?;

        let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let limit = args.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize);

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

    async fn execute(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = args.get("path").and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'path'".into()))?;
        let content = args.get("content").and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'content'".into()))?;
        let full = resolve_in_workspace(&ctx.workspace, path)?;

        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&full, content).await?;

        let bytes = content.len() as u64;
        Ok(ToolOutput::ok(format!("wrote {bytes} bytes to {path}")).with_artifacts(vec![Artifact {
            path: full.display().to_string(),
            bytes,
        }]))
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

    async fn execute(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = args.get("path").and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'path'".into()))?;
        let old = args.get("old_str").and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'old_str'".into()))?;
        let new = args.get("new_str").and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'new_str'".into()))?;
        let full = resolve_in_workspace(&ctx.workspace, path)?;

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
        Ok(ToolOutput::ok(format!("edited {path}")).with_artifacts(vec![Artifact {
            path: full.display().to_string(),
            bytes: updated.len() as u64,
        }]))
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

    async fn execute(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let full = resolve_in_workspace(&ctx.workspace, path)?;
        let mut rd = tokio::fs::read_dir(&full).await?;
        let mut items = Vec::new();
        while let Some(e) = rd.next_entry().await? {
            let ty = e.file_type().await?;
            let kind = if ty.is_dir() { "dir" } else { "file" };
            items.push(format!("{kind}\t{}", e.file_name().to_string_lossy()));
        }
        items.sort();
        Ok(ToolOutput::ok(format!("[{path}]\n{}", items.join("\n"))))
    }
}
