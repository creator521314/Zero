//! 交互式问答边界：AI 提问，用户从候选中选择或直接编辑回答。
//!
//! 放在 core 是因为它是「工具需要的外部能力」的端口，与 `SubagentPort` 同构：
//! agent-tools 只声明需求，agent-runtime 提供实现，装配层负责注入。
//! 任何实现 crate 都不得反向依赖。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::error::AgentError;
use crate::ids::{QuestionId, TaskId};

/// 一次提问的不可变负载，直接随事件广播与落盘。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionRequest {
    pub id: QuestionId,
    pub task_id: TaskId,
    /// 问题正文（必填）。
    pub question: String,
    /// 可选短标题，弹层顶部展示（如「选哪种部署方式」）。
    #[serde(default)]
    pub header: Option<String>,
    /// 候选答案，1~8 条；由工具的 schema 约束，这里不再二次截断。
    pub options: Vec<String>,
    /// 是否允许用户不选题目里的候选、自己输入答案。默认 true。
    #[serde(default = "default_allow_custom")]
    pub allow_custom: bool,
}

fn default_allow_custom() -> bool {
    true
}

/// 用户的回答。三种形态都要能被审计与回填给模型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QuestionAnswer {
    /// 选中第 `index` 条候选；`label` 冗余存一份，避免日志被候选顺序变更误读。
    Selected { index: usize, label: String },
    /// 用户自己编辑的文本。
    Custom { text: String },
    /// 用户取消（Esc）。不是错误：工具会立刻结束并把取消事实告诉模型。
    Cancelled,
}

impl QuestionAnswer {
    /// 回填给模型的简短文本。
    pub fn as_model_text(&self) -> String {
        match self {
            Self::Selected { index, label } => format!("用户选择：{}. {}", index + 1, label),
            Self::Custom { text } => format!("用户回答：{text}"),
            Self::Cancelled => {
                "用户取消了本次提问，不要重复同样的提问；换一种方式继续任务。".into()
            }
        }
    }
}

/// 问答后端端口。
#[async_trait]
pub trait UserQuestionPort: Send + Sync {
    /// 展示问题并等待回答。`cancel` 被触发时必须立刻返回，不能把任务挂死。
    async fn ask(
        &self,
        req: QuestionRequest,
        cancel: CancellationToken,
    ) -> Result<QuestionAnswer, AgentError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_serde_roundtrip_and_default_custom() {
        let q = QuestionRequest {
            id: QuestionId::from_raw("q-1"),
            task_id: TaskId::from_raw("t-1"),
            question: "选哪个".into(),
            header: None,
            options: vec!["A".into(), "B".into()],
            allow_custom: true,
        };
        let s = serde_json::to_string(&q).unwrap();
        assert!(s.contains("\"allow_custom\":true"));
        let back: QuestionRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.options, q.options);

        let a = QuestionAnswer::Selected {
            index: 1,
            label: "B".into(),
        };
        let s = serde_json::to_string(&a).unwrap();
        assert!(s.contains("\"kind\":\"selected\""));
        assert_eq!(serde_json::from_str::<QuestionAnswer>(&s).unwrap(), a);
    }

    #[test]
    fn model_text_distinguishes_selection_custom_and_cancel() {
        assert!(QuestionAnswer::Selected {
            index: 0,
            label: "A".into()
        }
        .as_model_text()
        .contains("A"));
        assert!(QuestionAnswer::Custom {
            text: "都不要".into()
        }
        .as_model_text()
        .contains("都不要"));
        assert!(QuestionAnswer::Cancelled.as_model_text().contains("取消"));
    }
}
