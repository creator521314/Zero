//! finish —— 显式终止信号。
//!
//! 任务完成时由模型调用：用 `summary` 给用户一段总结，并在 `artifacts`
//! 里登记交付物路径。执行循环识别到 `is_terminating` 后立即收尾。

use agent_core::perm::Risk;
use agent_core::tool::{Artifact, Tool, ToolCtx, ToolError, ToolOutput};
use async_trait::async_trait;
use serde_json::json;

use crate::paths::resolve_in_ctx;

/// 显式终止工具。
pub struct Finish;

#[async_trait]
impl Tool for Finish {
    fn name(&self) -> &str {
        "finish"
    }
    fn description(&self) -> &str {
        "任务完成时调用：用 summary 给用户一段最终总结，并在 artifacts 里登记交付物路径（相对工作区）。调用后任务立即结束。"
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "summary": { "type": "string", "description": "给用户的最终总结，可选" },
                "artifacts": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "交付物路径列表（相对工作区），可选"
                }
            },
            "required": []
        })
    }
    fn risk_level(&self) -> Risk {
        Risk::Low
    }
    fn is_terminating(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let summary = args.get("summary").and_then(|v| v.as_str()).unwrap_or("");

        // 登记交付物：相对路径解析到沙箱内；单个路径非法/不存在时跳过，
        // 不应因为一个登记项就掀翻已经完成的任务。
        let mut arts = Vec::new();
        if let Some(arr) = args.get("artifacts").and_then(|v| v.as_array()) {
            for a in arr {
                let Some(raw) = a.as_str() else { continue };
                if raw.trim().is_empty() {
                    continue;
                }
                if let Ok(full) = resolve_in_ctx(ctx, raw) {
                    // 实测（M4.2）：模型会把「写又被自己删掉」的临时文件登记成交付物，
                    // 落到审计里就是一条 bytes=0 的幽灵产物。文件不在就跳过，
                    // 别让不存在的路径污染产物清单。
                    let Ok(meta) = tokio::fs::metadata(&full).await else {
                        continue;
                    };
                    arts.push(Artifact {
                        path: full.display().to_string(),
                        bytes: meta.len(),
                    });
                }
            }
        }

        let body = if summary.is_empty() {
            "任务完成".to_string()
        } else {
            summary.to_string()
        };
        Ok(ToolOutput::ok(body).with_artifacts(arts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn ctx(project_root: Option<&std::path::Path>, workspace: &std::path::Path) -> ToolCtx {
        ToolCtx {
            workspace: workspace.to_path_buf(),
            project_root: project_root.map(|p| p.to_path_buf()),
            task_id: "t".into(),
            timeout: Duration::from_secs(5),
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn finish_registers_summary_and_artifacts() {
        let base = std::env::temp_dir().join("agent-tools-finish-test");
        let proj = base.join("proj");
        let ws = base.join("workspace").join("t1");
        tokio::fs::create_dir_all(&proj).await.unwrap();
        tokio::fs::create_dir_all(&ws).await.unwrap();
        tokio::fs::write(proj.join("out.md"), "# 结果")
            .await
            .unwrap();

        let out = Finish
            .execute(
                json!({ "summary": "完成了", "artifacts": ["out.md"] }),
                &ctx(Some(&proj), &ws),
            )
            .await
            .unwrap();

        assert!(!out.is_error, "实际: {}", out.content);
        assert_eq!(out.content, "完成了");
        assert_eq!(out.side_effects.len(), 1);
        assert!(
            out.side_effects[0].path.ends_with("out.md"),
            "实际: {}",
            out.side_effects[0].path
        );
        assert_eq!(out.side_effects[0].bytes, "# 结果".len() as u64);

        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn finish_defaults_when_no_args() {
        let base = std::env::temp_dir().join("agent-tools-finish-empty-test");
        let ws = base.join("workspace").join("t1");
        tokio::fs::create_dir_all(&ws).await.unwrap();

        let out = Finish.execute(json!({}), &ctx(None, &ws)).await.unwrap();
        assert_eq!(out.content, "任务完成");
        assert!(out.side_effects.is_empty());

        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn finish_skips_invalid_artifact_paths() {
        let base = std::env::temp_dir().join("agent-tools-finish-skip-test");
        let proj = base.join("proj");
        let ws = base.join("workspace").join("t1");
        tokio::fs::create_dir_all(&proj).await.unwrap();
        tokio::fs::create_dir_all(&ws).await.unwrap();

        // 逃逸路径（../../etc/passwd）应被跳过，不掀翻整个 finish。
        let out = Finish
            .execute(
                json!({ "summary": "ok", "artifacts": ["../../etc/passwd"] }),
                &ctx(Some(&proj), &ws),
            )
            .await
            .unwrap();
        assert_eq!(out.content, "ok");
        assert!(
            out.side_effects.is_empty(),
            "逃逸路径不应登记: {:?}",
            out.side_effects
        );

        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn finish_skips_artifacts_that_no_longer_exist() {
        // 实测回归（M4.2）：模型把「写完又删掉的临时脚本」登记成交付物，
        // 旧实现会记成 bytes=0 的幽灵产物。
        let base = std::env::temp_dir().join("agent-tools-finish-ghost-test");
        let proj = base.join("proj");
        let ws = base.join("workspace").join("t1");
        tokio::fs::create_dir_all(&proj).await.unwrap();
        tokio::fs::create_dir_all(&ws).await.unwrap();
        tokio::fs::write(proj.join("real.md"), "# 真产物")
            .await
            .unwrap();

        let out = Finish
            .execute(
                json!({ "summary": "ok", "artifacts": ["real.md", "deleted_tmp.ps1"] }),
                &ctx(Some(&proj), &ws),
            )
            .await
            .unwrap();

        assert_eq!(
            out.side_effects.len(),
            1,
            "只登记真实存在的文件，实际: {:?}",
            out.side_effects
        );
        assert!(out.side_effects[0].path.ends_with("real.md"));

        let _ = tokio::fs::remove_dir_all(&base).await;
    }
}
