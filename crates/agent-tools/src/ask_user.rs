//! ask_user —— 让 AI 就某个决策点主动询问用户。
//!
//! 候选由模型给，用户既可以选一条，也可以自己编辑。回答经端口回填给模型，
//! 与工具调用一起进入上下文快照，因此后续推理能看到用户真实的回答。

use std::sync::Arc;

use agent_core::ids::{QuestionId, TaskId};
use agent_core::perm::Risk;
use agent_core::question::{QuestionRequest, UserQuestionPort};
use agent_core::tool::{Tool, ToolCtx, ToolError, ToolOutput};
use async_trait::async_trait;
use serde_json::json;

/// 一次最多给几条候选：再多弹层放不下，用户也懒得看。
const MAX_OPTIONS: usize = 8;

pub struct AskUserTool {
    port: Arc<dyn UserQuestionPort>,
}

impl AskUserTool {
    pub fn new(port: Arc<dyn UserQuestionPort>) -> Self {
        Self { port }
    }
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "当你需要用户在几个明确选项之间做决定、或需要用户提供只有他知道的偏好/事实时调用。给出清晰的问题与 1~8 个候选，用户可以选一条，也可以自己编辑回答。只在真正影响后续做法的决策点使用——不要用寒暄式提问，也不要把整段任务拆成一串提问。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "要问用户的问题，必填，尽量一句话说清决策点"
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "maxItems": 8,
                    "description": "候选答案，必填，1~8 条；每条尽量短且互斥"
                },
                "header": {
                    "type": "string",
                    "description": "可选短标题，显示在问答弹层顶部"
                },
                "allow_custom": {
                    "type": "boolean",
                    "description": "是否允许用户不选题目里的候选、自己输入回答，默认 true"
                }
            },
            "required": ["question", "options"]
        })
    }

    fn risk_level(&self) -> Risk {
        Risk::Low
    }

    fn is_interactive(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let question = args
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if question.is_empty() {
            return Err(ToolError::InvalidArgs("question 不能为空".into()));
        }

        let options: Vec<String> = args
            .get("options")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if options.is_empty() || options.len() > MAX_OPTIONS {
            return Err(ToolError::InvalidArgs(format!(
                "options 需要 1~{MAX_OPTIONS} 条非空候选"
            )));
        }

        let allow_custom = args
            .get("allow_custom")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let req = QuestionRequest {
            id: QuestionId::new(),
            task_id: TaskId::from_raw(ctx.task_id.clone()),
            question,
            header: args
                .get("header")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            options,
            allow_custom,
        };

        match self.port.ask(req, ctx.cancel.clone()).await {
            Ok(answer) => Ok(ToolOutput::ok(answer.as_model_text())),
            Err(e) => Err(ToolError::Failed(format!("问答通道不可用：{e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::error::AgentError;
    use agent_core::question::QuestionAnswer;
    use std::sync::Mutex;

    struct StubPort {
        answer: QuestionAnswer,
        seen: Mutex<Vec<QuestionRequest>>,
    }

    #[async_trait]
    impl UserQuestionPort for StubPort {
        async fn ask(
            &self,
            req: QuestionRequest,
            _cancel: CancellationToken,
        ) -> Result<QuestionAnswer, AgentError> {
            self.seen.lock().unwrap().push(req);
            Ok(self.answer.clone())
        }
    }

    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolCtx {
        ToolCtx {
            workspace: std::path::PathBuf::from("/tmp/zero-ask"),
            project_root: None,
            task_id: "t-1".into(),
            timeout: std::time::Duration::from_secs(5),
            cancel: CancellationToken::new(),
        }
    }

    fn args() -> serde_json::Value {
        json!({"question": "用哪种数据库？", "options": ["SQLite", "Postgres"]})
    }

    #[tokio::test]
    async fn ask_user_formats_selected_answer_for_model() {
        let port = Arc::new(StubPort {
            answer: QuestionAnswer::Selected {
                index: 1,
                label: "Postgres".into(),
            },
            seen: Mutex::new(Vec::new()),
        });
        let tool = AskUserTool::new(port.clone());
        let out = tool.execute(args(), &ctx()).await.unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("Postgres"), "{}", out.content);
        let seen = port.seen.lock().unwrap();
        assert_eq!(seen[0].options.len(), 2);
        assert!(seen[0].allow_custom, "默认允许自由编辑");
        assert_eq!(seen[0].task_id.0, "t-1");
    }

    #[tokio::test]
    async fn ask_user_rejects_empty_question_and_bad_options() {
        let tool = AskUserTool::new(Arc::new(StubPort {
            answer: QuestionAnswer::Cancelled,
            seen: Mutex::new(Vec::new()),
        }));
        let err = tool
            .execute(json!({"question": " ", "options": ["A"]}), &ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));

        let err = tool
            .execute(json!({"question": "选", "options": []}), &ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[tokio::test]
    async fn ask_user_is_interactive_and_low_risk() {
        let tool = AskUserTool::new(Arc::new(StubPort {
            answer: QuestionAnswer::Cancelled,
            seen: Mutex::new(Vec::new()),
        }));
        assert!(tool.is_interactive());
        assert_eq!(tool.risk_level(), Risk::Low);
        // 取消也算一次正式回答，模型能据此换策略。
        let out = tool.execute(args(), &ctx()).await.unwrap();
        assert!(out.content.contains("取消"));
    }
}
