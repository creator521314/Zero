//! delegate —— 把一段独立工作交给拥有全新上下文的子 Agent。

use std::sync::Arc;

use agent_core::event::Outcome;
use agent_core::perm::Risk;
use agent_core::subagent::{SubagentPort, SubtaskResult, SubtaskSpec};
use agent_core::tool::{Tool, ToolCtx, ToolError, ToolErrorCode, ToolOutput};
use async_trait::async_trait;
use serde_json::json;

/// 委派工具。port 在**构造时**注入，因此不需要改 ToolCtx（全仓 11 个构造点）。
pub struct DelegateTool {
    port: Arc<dyn SubagentPort>,
}

impl DelegateTool {
    pub fn new(port: Arc<dyn SubagentPort>) -> Self {
        Self { port }
    }
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &str {
        "delegate"
    }

    fn description(&self) -> &str {
        "把一段独立、耗时或会产生大量输出的工作交给一个子 Agent 完成。子任务有全新的上下文，只把结果摘要返回给你；它和你共享同一个项目目录与私有工作区，写出的文件你立刻可见。适合：大范围检索、批量读取文件、跑测试并归纳结论、写一份独立草稿。不要把本任务的主线推理委派出去，子任务也不能再委派。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "goal": {
                    "type": "string",
                    "description": "子任务要达成的目标与期望结果，必填，不超过 2000 字符"
                },
                "context": {
                    "type": "string",
                    "description": "可选：必须传给子任务的背景材料；不传则子任务完全隔离，看不到本对话历史"
                },
                "expected_output": {
                    "type": "string",
                    "description": "可选：期望的交付物形态，如「一份对比表」或「修改后的文件路径列表」"
                },
                "model": {
                    "type": "string",
                    "description": "可选：给子任务指定更便宜或更专用的模型名"
                },
                "max_iterations": {
                    "type": "integer",
                    "description": "可选：子任务迭代上限，会被钳制到 1..=12"
                },
                "budget_usd": {
                    "type": "number",
                    "description": "可选：子任务的预算上限（美元）"
                }
            },
            "required": ["goal"]
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
        let spec: SubtaskSpec = serde_json::from_value(args.clone())
            .map_err(|e| ToolError::InvalidArgs(format!("delegate 参数不合法：{e}")))?;
        let caps = self.port.capabilities();
        let unsupported = [
            (
                spec.model.is_some() && !caps.supports_model_override,
                "model",
            ),
            (
                spec.budget_usd.is_some() && !caps.supports_budget,
                "budget_usd",
            ),
            (spec.context.is_some() && !caps.supports_context, "context"),
        ]
        .into_iter()
        .find(|(bad, _)| *bad)
        .map(|(_, key)| key);
        if let Some(key) = unsupported {
            return Ok(ToolOutput::err_code(
                ToolErrorCode::UnsupportedOption,
                format!("当前委派后端不支持选项 {key}，未执行子任务；请去掉该参数后重试"),
            ));
        }
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        match self
            .port
            .run_subtask(&ctx.task_id, spec, ctx.cancel.clone())
            .await
        {
            Ok(result) => Ok(ToolOutput::ok(format_result(&result))),
            // 用 is_error 的工具结果而不是让父任务整体失败：模型可以换策略重试。
            // execute_tool 会统一加 [error] 前缀，这里不要重复加。
            Err(e) => {
                let c = self.port.capacity();
                Ok(ToolOutput::err_code(
                    ToolErrorCode::DelegationFailed,
                    format!(
                        "委派失败，未执行子任务：{e}（当前子任务 {}/{}）",
                        c.running, c.limit
                    ),
                ))
            }
        }
    }
}

/// 只回传摘要 + 产物 + 花费，绝不带子任务的中间消息（上下文隔离的关键）。
fn format_result(r: &SubtaskResult) -> String {
    let status = match &r.outcome {
        Outcome::Success { .. } => "成功",
        Outcome::Failed { .. } => "失败",
        Outcome::Cancelled => "已取消",
        Outcome::Paused => "已暂停",
    };
    let mut s = format!(
        "【子任务结果】状态：{status}｜子任务 id：{}｜本次花费约 ${:.4}\n摘要：{}\n",
        r.child_id, r.cost_usd, r.summary
    );
    if !r.artifacts.is_empty() {
        s.push_str("交付物：\n");
        for a in &r.artifacts {
            s.push_str(&format!("  - {a}\n"));
        }
    }
    s.push_str("（这是子任务的摘要，不是它的完整对话；需要细节请在 goal 里要求它写进文件。）");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::error::AgentError;
    use agent_core::ids::TaskId;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    struct FakePort {
        fail: bool,
    }

    #[async_trait]
    impl SubagentPort for FakePort {
        async fn run_subtask(
            &self,
            parent_task_id: &str,
            spec: SubtaskSpec,
            _cancel: CancellationToken,
        ) -> Result<SubtaskResult, AgentError> {
            if self.fail {
                return Err(AgentError::Denied("测试用拒绝".into()));
            }
            assert_eq!(parent_task_id, "parent-1");
            Ok(SubtaskResult {
                child_id: TaskId::from_raw("child-1"),
                outcome: Outcome::Success {
                    summary: format!("已完成：{}", spec.goal),
                    artifacts: vec!["artifacts/report.md".into()],
                },
                summary: "结论 A 优于 B".into(),
                artifacts: vec![],
                cost_usd: 0.0123,
            })
        }
    }

    fn ctx() -> ToolCtx {
        ToolCtx {
            workspace: PathBuf::from("/tmp/zero-delegate-test"),
            project_root: None,
            task_id: "parent-1".into(),
            timeout: Duration::from_secs(330),
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn delegate_returns_only_the_summary() {
        let tool = DelegateTool::new(Arc::new(FakePort { fail: false }));
        let out = tool
            .execute(json!({ "goal": "对比两个方案" }), &ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("结论 A 优于 B"));
        assert!(out.content.contains("child-1"));
        assert!(
            !out.content.contains("已完成：对比两个方案"),
            "子任务内部的 summary 不应回传，只回传结果摘要"
        );
    }

    #[tokio::test]
    async fn delegate_rejects_missing_goal() {
        let tool = DelegateTool::new(Arc::new(FakePort { fail: false }));
        let err = tool.execute(json!({}), &ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "实际: {err:?}");
    }

    #[tokio::test]
    async fn delegate_failure_is_a_tool_error_not_a_task_failure() {
        let tool = DelegateTool::new(Arc::new(FakePort { fail: true }));
        let out = tool
            .execute(json!({ "goal": "做点什么" }), &ctx())
            .await
            .unwrap();
        assert!(out.is_error, "失败应回填给模型而不是掀翻父任务");
        assert!(out.content.contains("委派失败"));
        assert!(
            !out.content.starts_with("[error]"),
            "前缀由 execute_tool 统一加"
        );
    }

    /// 一个「什么都不支持」的后端：验证 DelegateTool 会明确拒绝而不是静默忽略。
    struct NoOptionsPort;

    #[async_trait]
    impl SubagentPort for NoOptionsPort {
        fn capabilities(&self) -> agent_core::subagent::SubagentCapabilities {
            agent_core::subagent::SubagentCapabilities {
                supports_model_override: false,
                supports_budget: false,
                supports_context: false,
                max_depth: 1,
            }
        }

        async fn run_subtask(
            &self,
            _parent_task_id: &str,
            _spec: SubtaskSpec,
            _cancel: CancellationToken,
        ) -> Result<SubtaskResult, AgentError> {
            panic!("能力校验应先拦下，不应走到这里")
        }
    }

    #[tokio::test]
    async fn unsupported_option_is_rejected_before_dispatch() {
        let tool = DelegateTool::new(Arc::new(NoOptionsPort));
        let out = tool
            .execute(json!({ "goal": "做事", "model": "cheap-model" }), &ctx())
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(
            out.error_code,
            Some(agent_core::tool::ToolErrorCode::UnsupportedOption)
        );
        assert!(out.content.contains("model"));
        assert!(
            !out.content.starts_with("[error]"),
            "前缀由 execute_tool 统一加"
        );
    }

    #[test]
    fn default_capabilities_keep_existing_ports_compatible() {
        // FakePort 没有实现 capabilities()/capacity()，必须走默认值且不报错。
        let port = FakePort { fail: false };
        let caps = SubagentPort::capabilities(&port);
        assert!(caps.supports_model_override);
        assert!(caps.supports_budget);
        assert_eq!(caps.max_depth, 1);
        assert_eq!(SubagentPort::capacity(&port).limit, 0);
    }

    #[test]
    fn schema_requires_goal_and_declares_every_read_key() {
        let tool = DelegateTool::new(Arc::new(FakePort { fail: false }));
        let p = tool.parameters();
        assert_eq!(p["type"], json!("object"));
        assert_eq!(p["required"], json!(["goal"]));
        for key in [
            "goal",
            "context",
            "expected_output",
            "model",
            "max_iterations",
            "budget_usd",
        ] {
            assert!(p["properties"][key].is_object(), "Schema 缺 {key}");
        }
    }
}
