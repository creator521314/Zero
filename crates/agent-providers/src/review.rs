//! L2 AI 审视器：把「这个工具调用该不该放行」交给模型判。
//!
//! 原先这份实现长在 `agent-cli/src/main.rs` 里，`agent-server` 干脆没有审视器
//! ——headless 下 L2 档等于「一律询问」，任务会卡死在没人点确认的地方。抽到
//! provider 层后两个入口共用一份，也能被真实模型实测。
//!
//! 第一性原则（`agent-perm` 也这么说）：**拿不准就问人**。所以这里任何失败
//! （超时/解析不出 JSON/字段缺失）都以 `Err` 收场，由上层降级为询问，
//! 绝不在解析失败时默认放行。

use std::sync::Arc;
use std::time::Duration;

use agent_core::perm::{AiReviewer, ApprovalResult, ApprovalSource, Risk};
use agent_core::provider::ToolCall;
use agent_core::provider::{ChatRequest, ChatResponse, LlmProvider, Message};
use agent_core::task::TaskSpec;
use async_trait::async_trait;

/// 审视提示词。
///
/// 要求**只输出 JSON**：这是让解析稳定的关键，模型多说一句废话都会让
/// `find('{')` 之后的 slice 失效（解析失败 = 降级询问 = 安全但烦人）。
///
/// 两次实测调出来的三条，都不是想当然：
/// 1. 「默认放行，只有**危害**才 deny」：第一版把「是否服务于任务」也当成否决
///    条件，结果模型连「抓一个和任务无关的网页」都拦（2026-10-24 实测）。
///    误拦只是多问一次，误放行才是不可逆代价——所以相关性只用来判断危害，
///    不单独作为否决项。
/// 2. 「reason 不超过 40 字」：模型写长中文理由会把 `max_tokens` 用满，
///    JSON 断在 `"confidence":` 处，整条判定作废、白白降级一次询问。
/// 3. `risk` 由「能否造成不可逆后果」决定，与 decision 独立——两者都低才算低。
const SYSTEM_PROMPT: &str = "你是操作安全审查员，只判断危害，不判断创意。\
                             默认放行；仅当操作可能造成不可逆危害、访问敏感凭据、\
                             把数据外传到不可信地址，或明显是滥用时才 deny。\
                             与任务相关性只作为判断危害的参考，不单独作为否决理由。\
                             只输出 JSON：{\"decision\":\"allow|deny\",\"risk\":\"low|medium|high\",\"reason\":\"...\",\"confidence\":0.0}，\
                             reason 用中文且不超过 40 字。";

pub struct LlmReviewer {
    provider: Arc<dyn LlmProvider>,
}

impl LlmReviewer {
    pub fn new(provider: Arc<dyn LlmProvider>) -> Self {
        Self { provider }
    }

    /// 从模型文本里解析判定结果。
    ///
    /// 单独成函数是为了能对「模型说了什么」做单测——实测时最容易暴露的就是
    /// 模型不按格式回（多说一段话、JSON 外带反引号等）。
    ///
    /// `reasoning` 是思考链兜底：部分模型（含 DeepSeek 推理档）会把结论写进
    /// `reasoning` 而正文留空，只看正文会误判「no json」而白降级一次询问。
    fn parse(text: &str, reasoning: Option<&str>) -> Result<ApprovalResult, String> {
        match Self::parse_one(text) {
            ok @ Ok(_) => ok,
            Err(from_content) => {
                let fallback = reasoning
                    .filter(|r| !r.trim().is_empty())
                    .and_then(|r| Self::parse_one(r).ok());
                fallback.ok_or(from_content)
            }
        }
    }

    fn parse_one(text: &str) -> Result<ApprovalResult, String> {
        let start = text.find('{').ok_or("no json in review")?;
        // 收尾 '}' 可能压根不存在（被 max_tokens 截断），此时把剩下的全当候选 slice，
        // 让下面的 salvage 兜底决定能不能用。end 落在 char 边界上：`}` 与 len() 都是。
        let end = text
            .rfind('}')
            .map(|i| i + 1)
            .filter(|e| *e > start)
            .unwrap_or(text.len());
        let slice = &text[start..end];
        let json: serde_json::Value = match serde_json::from_str(slice) {
            Ok(v) => v,
            Err(e) => {
                // JSON 被 max_tokens 截断时，至少把 decision 捞回来：
                // 明确 deny 的场景（删库、读私钥）不能因为尾巴缺了个 `}` 就
                // 变成「什么都不知道」。捞不到 decision 才报错。
                return Self::salvage_decision(slice).ok_or_else(|| e.to_string());
            }
        };

        let allow = json
            .get("decision")
            .and_then(|d| d.as_str())
            .map(|d| d.eq_ignore_ascii_case("allow"))
            // 没有 decision 字段 → Err（上层降级询问），绝不当成 allow
            .ok_or_else(|| "review missing 'decision'".to_string())?;
        let risk = match json.get("risk").and_then(|r| r.as_str()) {
            Some("high") => Risk::High,
            Some("medium") => Risk::Medium,
            _ => Risk::Low,
        };
        Ok(ApprovalResult {
            allow,
            risk,
            reason: json
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string(),
            confidence: json
                .get("confidence")
                .and_then(|c| c.as_f64())
                .unwrap_or(0.5),
            source: ApprovalSource::Ai,
        })
    }

    /// 截断 JSON 的兜底：只认 `"decision":"allow|deny"`，其余一概不信。
    ///
    /// 截断时 risk 读不到 → 按 `High` 处理，引擎仍会去问人（安全侧不变）。
    /// 只捞 decision 的原因是它是唯一影响结论的字段，且字面量匹配不可能误判。
    fn salvage_decision(slice: &str) -> Option<ApprovalResult> {
        for (lit, allow) in [
            ("\"decision\":\"allow\"", true),
            ("\"decision\":\"deny\"", false),
        ] {
            if slice.contains(lit) {
                return Some(ApprovalResult {
                    allow,
                    risk: Risk::High,
                    reason: "（判定文本被截断，仅取回 decision）".to_string(),
                    confidence: 0.5,
                    source: ApprovalSource::Ai,
                });
            }
        }
        None
    }
}

#[async_trait]
impl AiReviewer for LlmReviewer {
    async fn review(
        &self,
        task: &TaskSpec,
        call: &ToolCall,
        static_risk: Risk,
        timeout: Duration,
    ) -> Result<ApprovalResult, String> {
        let ctx = format!(
            "任务目标: {}\n工作区: {}\n静态风险: {:?}\n当前操作: 工具={} 参数={}",
            task.prompt, task.workspace, static_risk, call.name, call.arguments
        );
        let req = ChatRequest {
            model: String::new(),
            messages: vec![Message::system(SYSTEM_PROMPT), Message::user(ctx)],
            tools: vec![],
            temperature: Some(0.0),
            // 200 实测会被长中文理由顶满导致 JSON 截断（见 SYSTEM_PROMPT 注释）。
            max_tokens: Some(400),
            stream: false,
            reasoning_effort: None,
            metadata: Default::default(),
        };

        // 超时不能拖垮整个任务：模型慢半拍就当它没回话，上层会降级为询问。
        let resp: ChatResponse = tokio::time::timeout(timeout, self.provider.chat(req))
            .await
            .map_err(|_| "ai review timeout".to_string())?
            .map_err(|e| e.to_string())?;

        // 调提示词时定位「模型到底回了什么」用：AGENT_REVIEW_DEBUG=1。
        // 用 eprintln 而不是 tracing：测试进程里没有 subscriber，debug 日志会被吞掉。
        if std::env::var("AGENT_REVIEW_DEBUG").is_ok_and(|v| v != "0") {
            eprintln!(
                "[review-debug] tool={} content={:?} reasoning={:?}",
                call.name,
                resp.message.text(),
                resp.message.reasoning.as_deref().unwrap_or("")
            );
        }

        Self::parse(&resp.message.text(), resp.message.reasoning.as_deref())
    }
}

/// 便于测试/替身注入的别名。
pub type ReviewerFn = Arc<dyn AiReviewer>;

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::ids::ToolCallId;
    use agent_core::perm::{Decision, PermissionTier};
    use agent_core::provider::{FinishReason, LlmError, ModelPrice, StreamEvent, Usage};
    use agent_perm::rules::StaticRules;
    use agent_perm::{PermConfig, PermissionEngine, PermissionEngineImpl};
    use serde_json::json;

    fn call(tool: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: ToolCallId::new(),
            name: tool.into(),
            arguments: args,
        }
    }

    fn task() -> TaskSpec {
        TaskSpec::new("整理这个目录", "/tmp/ws")
    }

    #[test]
    fn parses_clean_json() {
        let r = LlmReviewer::parse(
            r#"{"decision":"allow","risk":"low","reason":"只读","confidence":0.9}"#,
            None,
        )
        .expect("应能解析");
        assert!(r.allow);
        assert_eq!(r.risk, Risk::Low);
        assert_eq!(r.reason, "只读");
        assert_eq!(r.confidence, 0.9);
        assert_eq!(r.source, ApprovalSource::Ai, "应标明判定来自 AI");
    }

    #[test]
    fn parses_json_wrapped_in_prose_or_fences() {
        // 实测中模型常带 ```json 围栏或前后废话：不能因此判失败。
        let r = LlmReviewer::parse(
            "好的，我的判断是：\n```json\n{\"decision\":\"deny\",\"risk\":\"high\",\"reason\":\"会删库\"}\n```\n以上。",
            None,
        )
        .expect("围栏/废话不应导致解析失败");
        assert!(!r.allow);
        assert_eq!(r.risk, Risk::High);
        assert_eq!(r.reason, "会删库");
    }

    #[test]
    fn falls_back_to_reasoning_when_content_has_no_json() {
        // 实测回归：deepseek-reasoner 把 JSON 放思考链、正文留空。
        // 只看正文会得到 "no json in review" → 白降级一次询问（烦人但安全）。
        let r = LlmReviewer::parse(
            "",
            Some(r#"我先判断一下：{"decision":"allow","risk":"low","reason":"只读"}"#),
        )
        .expect("应能从思考链里解析出来");
        assert!(r.allow);
        assert_eq!(r.reason, "只读");
    }

    #[test]
    fn content_wins_over_reasoning_when_both_have_json() {
        // 正文优先：思考链只是兜底，避免用过程推导覆盖最终结论。
        let r = LlmReviewer::parse(
            r#"{"decision":"deny","risk":"high","reason":"正文结论"}"#,
            Some(r#"{"decision":"allow","risk":"low","reason":"思考链结论"}"#),
        )
        .expect("应能解析");
        assert!(!r.allow, "应以正文为准");
        assert_eq!(r.reason, "正文结论");
    }

    #[test]
    fn salvages_decision_from_truncated_json() {
        // 实测回归：长中文理由把 max_tokens 用满，JSON 断在 "confidence": 处。
        let truncated = r#"{"decision":"deny","risk":"high","reason":"该命令递归删除用户主目录下的 Documents，与任务无关，且会造成不可逆的数据丢失。","confidence":"#;
        let r = LlmReviewer::parse(truncated, None).expect("截断时至少要把 decision 捞回来");
        assert!(!r.allow, "截断也不该误判为放行");
        assert_eq!(r.risk, Risk::High, "截断时读不到 risk，必须按最坏处理");
    }

    #[test]
    fn truncated_json_without_decision_is_still_an_error() {
        // 没有 decision 就不猜：宁可降级询问，也不要臆测。
        let truncated = r#"{"risk":"low","reason":"读取 notes.md 是整理目录所需","confi"#;
        assert!(LlmReviewer::parse(truncated, None).is_err());
    }

    #[test]
    fn missing_decision_is_treated_as_deny_not_error_silently() {
        // 缺 decision → Err（上层降级为询问）。绝不能当成 allow。
        let err = LlmReviewer::parse(r#"{"risk":"low"}"#, None).unwrap_err();
        assert!(err.contains("decision"), "应明确指出缺字段：{err}");
    }

    #[test]
    fn no_json_at_all_is_an_error() {
        assert!(LlmReviewer::parse("我觉得可以", None).is_err());
        assert!(LlmReviewer::parse("", None).is_err());
    }

    #[test]
    fn decision_is_case_insensitive() {
        assert!(
            LlmReviewer::parse(r#"{"decision":"ALLOW","risk":"low"}"#, None)
                .unwrap()
                .allow
        );
    }

    /// 固定回一段文本的假 provider。
    struct FixedProvider {
        text: &'static str,
    }

    #[async_trait]
    impl LlmProvider for FixedProvider {
        fn name(&self) -> &str {
            "fixed"
        }
        fn capabilities(&self) -> agent_core::provider::Capabilities {
            Default::default()
        }
        async fn chat(&self, _req: ChatRequest) -> Result<ChatResponse, LlmError> {
            Ok(ChatResponse {
                message: Message::assistant(self.text),
                usage: Usage::default(),
                finish_reason: FinishReason::Stop,
                provider: "fixed".into(),
                model_returned: "m".into(),
            })
        }
        async fn chat_stream(
            &self,
            _req: ChatRequest,
        ) -> Result<futures::stream::BoxStream<'static, Result<StreamEvent, LlmError>>, LlmError>
        {
            Err(LlmError::InvalidRequest("not used".into()))
        }
        fn price(&self, _model: &str) -> Option<ModelPrice> {
            None
        }
    }

    #[tokio::test]
    async fn high_risk_review_is_downgraded_to_ask() {
        // 模型说 allow 但 risk=high → 仍要问人。这是「误放行比多问一次贵」的落地。
        let reviewer = LlmReviewer::new(Arc::new(FixedProvider {
            text: r#"{"decision":"allow","risk":"high","reason":"看着不太妙"}"#,
        }));
        let eng = PermissionEngineImpl::new(StaticRules::default(), PermConfig::default())
            .with_reviewer(Box::new(reviewer));
        let d = eng
            .check(&call("shell_exec", json!({"cmd":"ls"})), &task())
            .await;
        assert!(
            matches!(d, Decision::AskUser),
            "风险 high 时即便 allow 也要询问，实际 {d:?}"
        );
    }

    #[tokio::test]
    async fn low_risk_review_allows() {
        let reviewer = LlmReviewer::new(Arc::new(FixedProvider {
            text: r#"{"decision":"allow","risk":"low","reason":"只读"}"#,
        }));
        let eng = PermissionEngineImpl::new(StaticRules::default(), PermConfig::default())
            .with_reviewer(Box::new(reviewer));
        let d = eng
            .check(&call("list_dir", json!({"path":"."})), &task())
            .await;
        assert!(matches!(d, Decision::Allow), "低风险放行，实际 {d:?}");
    }

    #[tokio::test]
    async fn timeout_is_downgraded_not_allowed() {
        // provider 永不返回 → 超时 → 必须降级询问，绝不能默认放行。
        struct Hanging;
        #[async_trait]
        impl LlmProvider for Hanging {
            fn name(&self) -> &str {
                "hanging"
            }
            fn capabilities(&self) -> agent_core::provider::Capabilities {
                Default::default()
            }
            async fn chat(&self, _req: ChatRequest) -> Result<ChatResponse, LlmError> {
                std::future::pending::<()>().await;
                unreachable!()
            }
            async fn chat_stream(
                &self,
                _req: ChatRequest,
            ) -> Result<futures::stream::BoxStream<'static, Result<StreamEvent, LlmError>>, LlmError>
            {
                Err(LlmError::InvalidRequest("not used".into()))
            }
            fn price(&self, _model: &str) -> Option<ModelPrice> {
                None
            }
        }

        let reviewer = LlmReviewer::new(Arc::new(Hanging));
        let eng = PermissionEngineImpl::new(StaticRules::default(), PermConfig::default())
            .with_reviewer(Box::new(reviewer));
        let d = eng
            .check(&call("list_dir", json!({"path":"."})), &task())
            .await;
        assert!(
            matches!(d, Decision::AskUser),
            "超时应降级为询问，实际 {d:?}"
        );
    }

    #[tokio::test]
    async fn l2_tier_is_respected_through_engine() {
        // 同一个审视器在 L1/L3 下不该被调用（L1 全放行、L3 逐个问）。
        let reviewer = LlmReviewer::new(Arc::new(FixedProvider {
            text: r#"{"decision":"allow","risk":"low"}"#,
        }));
        let eng = PermissionEngineImpl::new(StaticRules::default(), PermConfig::default())
            .with_reviewer(Box::new(reviewer));
        let mut l3 = task();
        l3.tier = PermissionTier::L3;
        let d = eng.check(&call("list_dir", json!({})), &l3).await;
        assert!(matches!(d, Decision::AskUser), "L3 不因有审视器而放行");
    }
}
