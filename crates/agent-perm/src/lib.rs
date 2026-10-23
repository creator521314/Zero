//! agent-perm —— 权限引擎实现（双层危险识别）。
//!
//! 第一层：静态规则（快、不可绕过）。命中高危规则时，**即使 L1 全自动也强制人工确认**。
//! 第二层：AI 审视（仅 L2 且静态未命中时）。AI 任何故障（超时/解析失败）**一律降级为询问用户**。
//!
//! 第一性原则：无沙箱下「误放行」的不可逆代价 > 「多问一次」的骚扰成本。

pub mod rules;

use std::time::Duration;

use agent_core::perm::{Decision, PermissionTier, Risk};
use agent_core::provider::ToolCall;
use agent_core::task::TaskSpec;
use async_trait::async_trait;

pub use rules::{RuleMatch, StaticRules};

// AiReviewer 定义在 core：它的入参出参都是 core 类型，放那里实现方
// （agent-providers）就不必反向依赖 agent-perm。这里转出来，沿用旧路径。
pub use agent_core::perm::AiReviewer;
/// 权限引擎 trait 也一并转出：实现细节在 agent-perm，边界在 core。
pub use agent_core::perm::PermissionEngine;

/// 权限引擎配置。
#[derive(Debug, Clone)]
pub struct PermConfig {
    /// AI 审视超时。
    pub ai_timeout: Duration,
    /// 是否允许 `a`（本任务此类全批准）降级。默认允许。
    pub allow_batch_approve: bool,
}

impl Default for PermConfig {
    fn default() -> Self {
        Self {
            ai_timeout: Duration::from_secs(8),
            allow_batch_approve: true,
        }
    }
}

/// 权限引擎。
pub struct PermissionEngineImpl {
    rules: StaticRules,
    reviewer: Option<Box<dyn AiReviewer>>,
    cfg: PermConfig,
    /// 记录「本任务此类操作已全批准」的 (task, tool) 集合（L3 下可选降级）。
    batch_approved: tokio::sync::Mutex<std::collections::HashSet<(String, String)>>,
}

impl PermissionEngineImpl {
    pub fn new(rules: StaticRules, cfg: PermConfig) -> Self {
        Self {
            rules,
            reviewer: None,
            cfg,
            batch_approved: Default::default(),
        }
    }

    pub fn with_reviewer(mut self, r: Box<dyn AiReviewer>) -> Self {
        self.reviewer = Some(r);
        self
    }

    /// 用户批准后登记「本任务此类全批准」。
    pub async fn approve_for_task(&self, task_id: &str, tool: &str) {
        self.batch_approved
            .lock()
            .await
            .insert((task_id.to_string(), tool.to_string()));
    }

    async fn is_batch_approved(&self, task_id: &str, tool: &str) -> bool {
        self.cfg.allow_batch_approve
            && self
                .batch_approved
                .lock()
                .await
                .contains(&(task_id.to_string(), tool.to_string()))
    }
}

#[async_trait]
impl PermissionEngine for PermissionEngineImpl {
    async fn check(&self, call: &ToolCall, task: &TaskSpec) -> Decision {
        let task_id = task.id.0.as_str();

        // ---- 第一层：静态规则（先跑、不可绕过）----
        if let RuleMatch::Hit { rule, .. } = self.rules.match_call(call) {
            return match task.tier {
                // L1 已授权的高危任务仍需人工确认（记审计由上层负责）
                PermissionTier::L1 => {
                    Decision::Deny(format!("高危操作被静态规则拦截，需人工确认：{rule}"))
                }
                PermissionTier::L2 | PermissionTier::L3 => Decision::AskUser,
            };
        }

        // ---- L1 全自动：直接放行 ----
        if task.tier == PermissionTier::L1 {
            return Decision::Allow;
        }

        // ---- L3 逐步确认：除已批量批准外一律询问 ----
        if task.tier == PermissionTier::L3 {
            if self.is_batch_approved(task_id, &call.name).await {
                return Decision::Allow;
            }
            return Decision::AskUser;
        }

        // ---- L2：AI 审视 ----
        let Some(reviewer) = self.reviewer.as_ref() else {
            // 无审视器可用 → 降级询问（绝不默认放行）
            return Decision::AskUser;
        };

        match reviewer
            .review(
                task,
                call,
                self.rules.static_risk(call),
                self.cfg.ai_timeout,
            )
            .await
        {
            Ok(r) if r.allow && r.risk != Risk::High => Decision::Allow,
            Ok(_) => Decision::AskUser,
            Err(e) => {
                tracing::warn!(error = %e, "AI review failed, downgrade to AskUser");
                Decision::AskUser
            }
        }
    }

    async fn approve_for_task(&self, task_id: &str, tool: &str) {
        // 转发到同名固有方法：登记逻辑保持单点，单测也可直接打到它上面。
        PermissionEngineImpl::approve_for_task(self, task_id, tool).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::ids::ToolCallId;

    fn call(tool: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: ToolCallId::new(),
            name: tool.into(),
            arguments: args,
        }
    }

    fn task(tier: PermissionTier) -> TaskSpec {
        let mut t = TaskSpec::new("test", "/tmp/ws");
        t.tier = tier;
        t
    }

    #[tokio::test]
    async fn l1_allows_safe_op() {
        let eng = PermissionEngineImpl::new(StaticRules::default(), PermConfig::default());
        let d = eng
            .check(
                &call("file_read", serde_json::json!({"path":"a.txt"})),
                &task(PermissionTier::L1),
            )
            .await;
        assert!(matches!(d, Decision::Allow));
    }

    #[tokio::test]
    async fn l1_blocked_by_static_rule() {
        let eng = PermissionEngineImpl::new(StaticRules::default(), PermConfig::default());
        // 即使 L1 全自动，rm -rf / 也必须拦截
        let d = eng
            .check(
                &call("shell_exec", serde_json::json!({"cmd":"rm -rf /"})),
                &task(PermissionTier::L1),
            )
            .await;
        assert!(
            matches!(d, Decision::Deny(_) | Decision::AskUser),
            "高危命令不应被 L1 放行"
        );
    }

    #[tokio::test]
    async fn l2_without_reviewer_asks() {
        let eng = PermissionEngineImpl::new(StaticRules::default(), PermConfig::default());
        let d = eng
            .check(
                &call("shell_exec", serde_json::json!({"cmd":"ls"})),
                &task(PermissionTier::L2),
            )
            .await;
        assert!(matches!(d, Decision::AskUser), "无审视器时必须降级为询问");
    }

    #[tokio::test]
    async fn l3_asks_everything() {
        let eng = PermissionEngineImpl::new(StaticRules::default(), PermConfig::default());
        let t = task(PermissionTier::L3);
        let d = eng
            .check(&call("list_dir", serde_json::json!({"path":"."})), &t)
            .await;
        assert!(matches!(d, Decision::AskUser));
        // 批量批准后放行
        eng.approve_for_task(t.id.0.as_str(), "list_dir").await;
        let d = eng
            .check(&call("list_dir", serde_json::json!({"path":"."})), &t)
            .await;
        assert!(matches!(d, Decision::Allow));
    }
}
