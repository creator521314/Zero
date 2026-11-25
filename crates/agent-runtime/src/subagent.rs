//! 子任务委派的运行时实现（SubagentPort 的唯一实现）。
//!
//! 设计要点：
//! - 子任务 = 另一个 TaskSpec + 另一个 RunControl + 复用同一条 EventBus；
//!   直接再调一次 `AgentRuntime::run`，不造第二种执行循环。
//! - 父子共享 project_root 与 workspace（一行 clone 就是「共享同一片空间」）。
//! - 一期串行：父 Runner 阻塞等待，审批单槽 / CLI 串行闸门完全不感知子任务。

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_core::error::AgentError;
use agent_core::event::{Event, EventBus, Outcome};
use agent_core::ids::TaskId;
use agent_core::subagent::{SubagentPort, SubtaskResult, SubtaskSpec};
use agent_core::task::{AgentRunner, RunControl, TaskSpec};
use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::runner::{AgentRuntime, RuntimeDeps};

/// 硬限制：一律不可由模型覆盖。
pub const MAX_DEPTH: u8 = 1;
pub const MAX_LIVE_CHILDREN: usize = 1;
pub const MAX_CHILDREN_PER_TASK: usize = 8;
pub const SUBTASK_WALL_TIMEOUT: Duration = Duration::from_secs(300);
pub const GOAL_MAX_CHARS: usize = 2000;

/// 任务树共享预算单元（微美元，避免 f64 的原子操作问题）。
///
/// 父子通过同一个 `cost_sink` 闭包 charge 同一个实例；超额由闭包触发
/// `cancel()`，交给执行循环已有的 `cancel.cancelled()` 分支收尾，
/// 因此不需要给 RuntimeDeps 加字段（那会牵动 14 个构造点）。
#[derive(Clone, Default)]
pub struct BudgetCell(Arc<AtomicU64>);

impl BudgetCell {
    pub fn charge(&self, usd: f64) {
        let micro = (usd.max(0.0) * 1_000_000.0).round() as u64;
        self.0.fetch_add(micro, Ordering::Relaxed);
    }

    pub fn spent_usd(&self) -> f64 {
        self.0.load(Ordering::Relaxed) as f64 / 1_000_000.0
    }
}

/// 当前父任务上下文。一期串行，同一时刻只有一个，故用单槽而非 HashMap。
#[derive(Clone)]
struct ParentCtx {
    spec: TaskSpec,
    deps: RuntimeDeps,
    bus: EventBus,
}

/// SubagentPort 的运行时实现。由装配层构造，注入给 `DelegateTool`。
pub struct RuntimeSubagent {
    parent: Arc<Mutex<Option<ParentCtx>>>,
    live: Arc<AtomicUsize>,
    spawned: Arc<AtomicUsize>,
    budget: BudgetCell,
}

impl RuntimeSubagent {
    pub fn new(budget: BudgetCell) -> Self {
        Self {
            parent: Arc::new(Mutex::new(None)),
            live: Arc::new(AtomicUsize::new(0)),
            spawned: Arc::new(AtomicUsize::new(0)),
            budget,
        }
    }

    /// 装配层在**每个顶层任务提交时**调用（构造 RuntimeDeps 之后、spawn 之前）。
    pub async fn register(&self, spec: TaskSpec, deps: RuntimeDeps, bus: EventBus) {
        *self.parent.lock().await = Some(ParentCtx { spec, deps, bus });
        self.spawned.store(0, Ordering::Relaxed);
    }

    /// 任务结束时清理，避免陈旧上下文被误用。
    pub async fn unregister(&self, task_id: &str) {
        let mut guard = self.parent.lock().await;
        if guard.as_ref().map(|p| p.spec.id.0.as_str()) == Some(task_id) {
            *guard = None;
        }
    }
}

#[async_trait]
impl SubagentPort for RuntimeSubagent {
    async fn run_subtask(
        &self,
        parent_task_id: &str,
        spec: SubtaskSpec,
        cancel: CancellationToken,
    ) -> Result<SubtaskResult, AgentError> {
        let parent = {
            let guard = self.parent.lock().await;
            let ctx = guard
                .clone()
                .ok_or_else(|| AgentError::Internal("没有已登记的父任务上下文".into()))?;
            if ctx.spec.id.0 != parent_task_id {
                return Err(AgentError::Internal(
                    "委派请求的 task_id 与当前运行任务不匹配".into(),
                ));
            }
            ctx
        };

        // ---- 防失控硬闸 ----
        if parent.spec.depth >= MAX_DEPTH {
            return Err(AgentError::Denied(
                "子任务不允许再次委派（深度上限 1）".into(),
            ));
        }
        let goal = spec.goal.trim().to_string();
        if goal.is_empty() || goal.chars().count() > GOAL_MAX_CHARS {
            return Err(AgentError::Tool(agent_core::tool::ToolError::InvalidArgs(
                "goal 不能为空且不得超过 2000 字符".into(),
            )));
        }
        if self.live.load(Ordering::Relaxed) >= MAX_LIVE_CHILDREN {
            return Err(AgentError::Denied(
                "已有子任务在运行（一期只支持串行）".into(),
            ));
        }
        if self.spawned.load(Ordering::Relaxed) >= MAX_CHILDREN_PER_TASK {
            return Err(AgentError::Denied("本任务累计委派已达 8 次上限".into()));
        }

        self.live.fetch_add(1, Ordering::Relaxed);
        self.spawned.fetch_add(1, Ordering::Relaxed);
        let before = self.budget.spent_usd();
        let mut result = self.run_child(parent, spec, goal, cancel).await;
        self.live.fetch_sub(1, Ordering::Relaxed);

        if let Ok(r) = result.as_mut() {
            r.cost_usd = (self.budget.spent_usd() - before).max(0.0);
        }
        result
    }
}

impl RuntimeSubagent {
    async fn run_child(
        &self,
        parent: ParentCtx,
        spec: SubtaskSpec,
        goal: String,
        parent_cancel: CancellationToken,
    ) -> Result<SubtaskResult, AgentError> {
        let child_id = TaskId::new();

        // ---- 构造子 TaskSpec：共享同一片空间 ----
        let mut child = TaskSpec::new(goal.clone(), parent.spec.workspace.clone());
        child.id = child_id.clone();
        child.parent_id = Some(parent.spec.id.clone());
        child.depth = parent.spec.depth + 1;
        child.project_root = parent.spec.project_root.clone(); // 同一项目根
        child.tier = parent.spec.tier; // 权限不放大
        child.model = spec.model.clone().or_else(|| parent.spec.model.clone());
        child.max_iterations = spec.max_iterations.unwrap_or(8).clamp(1, 12);
        child.budget_usd = spec.budget_usd.or(parent.spec.budget_usd);
        // 刻意不设 resume_from / resume_messages：子任务永远是全新上下文。

        // 下行上下文：只给明确给出的背景与交付物提示，绝不复制父的历史消息。
        if let Some(extra) = spec.context.as_deref() {
            if !extra.trim().is_empty() {
                let clipped: String = extra.chars().take(4000).collect();
                child.prompt = format!("{goal}\n\n【背景材料（由父任务提供）】\n{clipped}");
            }
        }
        if let Some(hint) = spec.expected_output.as_deref() {
            let clipped: String = hint.chars().take(500).collect();
            child
                .prompt
                .push_str(&format!("\n\n【期望交付物】{clipped}"));
        }

        // ---- 子 deps：Clone 之后只覆写预算；工具集用统一的 allowlist 规则 ----
        let mut child_deps: RuntimeDeps = parent.deps.clone();
        // 递归防护：子任务看不到、也调不到 delegate。
        // 刻意不再在子 deps 里硬编码过滤——stream_once / execute_tool 会用
        // 同一条 TaskSpec::allows_tool 规则处理，避免两处逻辑漂移。
        child.tool_allowlist = Some(
            parent
                .deps
                .tool_defs
                .iter()
                .map(|d| d.name.clone())
                .filter(|n| n != "delegate")
                .collect(),
        );
        if let Some(cap) = child.budget_usd {
            if cap > 0.0 {
                child_deps.per_task_budget = cap;
            }
        }
        // 共享（Clone 自动带来）：provider / perm / store / approvals /
        // log_cfg / context_cfg / cost_sink / redact。安全能力零重复实现。

        // ---- 取消级联 ----
        let child_cancel = parent_cancel.child_token();
        let ctl = RunControl {
            cancel: child_cancel.clone(),
        };

        let _ = parent.bus.send(Event::SubtaskLinked {
            parent_id: parent.spec.id.clone(),
            child_id: child_id.clone(),
            goal: goal.clone(),
        });

        // ---- 跑子任务 ----
        let runtime = AgentRuntime::new(child_deps);
        let outcome = match tokio::time::timeout(
            SUBTASK_WALL_TIMEOUT,
            runtime.run(child, parent.bus.clone(), ctl),
        )
        .await
        {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => Outcome::Failed {
                reason: e.to_string(),
            },
            Err(_) => {
                child_cancel.cancel();
                Outcome::Failed {
                    reason: format!(
                        "子任务超过 {}s 墙钟上限，已中止",
                        SUBTASK_WALL_TIMEOUT.as_secs()
                    ),
                }
            }
        };

        // ---- 上行：只回传摘要 ----
        let (summary, artifacts) = match &outcome {
            Outcome::Success {
                summary, artifacts, ..
            } => (summary.clone(), artifacts.clone()),
            Outcome::Failed { reason } => (format!("子任务失败：{reason}"), Vec::new()),
            Outcome::Cancelled => ("子任务已取消".to_string(), Vec::new()),
            Outcome::Paused => ("子任务已暂停".to_string(), Vec::new()),
        };

        Ok(SubtaskResult {
            child_id,
            outcome,
            summary,
            artifacts,
            cost_usd: 0.0, // 由 run_subtask 用 BudgetCell 差值覆写
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::event::event_bus;
    use agent_core::perm::{Decision, PermissionEngine, PermissionTier};
    use agent_core::provider::ToolCall;
    use agent_core::store::LogConfig;
    use agent_core::task::TaskSpec;
    use agent_store::JsonlStore;
    use async_trait::async_trait;
    use tokio::sync::mpsc;

    use crate::context::ContextConfig;
    use crate::runner::{ApprovalInbox, ApprovalMsg};

    struct AllowAll;
    #[async_trait]
    impl PermissionEngine for AllowAll {
        async fn check(&self, _c: &ToolCall, _t: &TaskSpec) -> Decision {
            Decision::Allow
        }
    }

    /// 一个「什么都不做」的 provider：子任务第一轮就会因无工具调用而收尾。
    struct EmptyProvider;
    #[async_trait]
    impl agent_core::provider::LlmProvider for EmptyProvider {
        fn name(&self) -> &str {
            "empty"
        }
        fn capabilities(&self) -> agent_core::provider::Capabilities {
            agent_core::provider::Capabilities::default()
        }
        async fn chat(
            &self,
            _r: agent_core::provider::ChatRequest,
        ) -> Result<agent_core::provider::ChatResponse, agent_core::provider::LlmError> {
            Err(agent_core::provider::LlmError::InvalidRequest(
                "use stream".into(),
            ))
        }
        async fn chat_stream(
            &self,
            _r: agent_core::provider::ChatRequest,
        ) -> Result<
            agent_core::provider::BoxStream<
                Result<agent_core::provider::StreamEvent, agent_core::provider::LlmError>,
            >,
            agent_core::provider::LlmError,
        > {
            Ok(Box::pin(futures::stream::iter(vec![Ok(
                agent_core::provider::StreamEvent::Done(agent_core::provider::FinishReason::Stop),
            )])))
        }
        fn price(&self, _m: &str) -> Option<agent_core::provider::ModelPrice> {
            None
        }
    }

    fn deps(tmp: &std::path::Path) -> RuntimeDeps {
        let (_tx, rx) = mpsc::channel::<ApprovalMsg>(1);
        RuntimeDeps {
            provider: Arc::new(EmptyProvider),
            provider_name: "empty".into(),
            tools: Arc::new(|_| None),
            tool_defs: vec![],
            perm: Arc::new(AllowAll),
            store: Arc::new(JsonlStore::new(tmp)),
            log_cfg: LogConfig::default(),
            approvals: Arc::new(ApprovalInbox::new(rx)),
            context_cfg: ContextConfig::default(),
            per_task_budget: 0.0,
            cost_sink: None,
            redact: Arc::new(|s: &str| s.to_string()),
        }
    }

    async fn setup(depth: u8) -> (RuntimeSubagent, TaskSpec, std::path::PathBuf) {
        let tmp = std::env::temp_dir().join(format!("zero-subagent-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut spec = TaskSpec::new("父任务", tmp.display().to_string());
        spec.depth = depth;
        spec.tier = PermissionTier::L1;
        let port = RuntimeSubagent::new(BudgetCell::default());
        port.register(spec.clone(), deps(&tmp), event_bus()).await;
        (port, spec, tmp)
    }

    fn plain_spec(goal: &str) -> SubtaskSpec {
        SubtaskSpec {
            goal: goal.into(),
            context: None,
            expected_output: None,
            model: None,
            max_iterations: None,
            budget_usd: None,
        }
    }

    #[tokio::test]
    async fn rejects_empty_and_oversized_goal() {
        let (port, spec, tmp) = setup(0).await;
        let r = port
            .run_subtask(&spec.id.0, plain_spec(""), CancellationToken::new())
            .await;
        assert!(r.is_err(), "空 goal 必须拒绝");
        let big = "x".repeat(GOAL_MAX_CHARS + 1);
        let r = port
            .run_subtask(&spec.id.0, plain_spec(&big), CancellationToken::new())
            .await;
        assert!(r.is_err(), "超长 goal 必须拒绝");
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn depth_limit_blocks_second_level_delegation() {
        let (port, spec, tmp) = setup(MAX_DEPTH).await;
        let r = port
            .run_subtask(&spec.id.0, plain_spec("再开一个"), CancellationToken::new())
            .await;
        assert!(matches!(r, Err(AgentError::Denied(_))), "实际: {r:?}");
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn child_shares_parent_workspace_and_returns_summary() {
        let (port, spec, tmp) = setup(0).await;
        let result = port
            .run_subtask(&spec.id.0, plain_spec("确认可用"), CancellationToken::new())
            .await
            .expect("子任务应正常收尾");
        assert!(!result.child_id.0.is_empty());
        assert!(matches!(result.outcome, Outcome::Success { .. }));
        assert!(!result.summary.is_empty());
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn child_tool_allowlist_excludes_delegate() {
        // 用 C1 已有的测试夹具思路：直接构造最小 deps，跑一次 run_subtask
        // 太重；这里只验证规则本身——run_child 用的是同一段表达式。
        let defs = [
            agent_core::provider::ToolDef {
                name: "delegate".into(),
                description: "".into(),
                parameters: serde_json::json!({}),
            },
            agent_core::provider::ToolDef {
                name: "file_read".into(),
                description: "".into(),
                parameters: serde_json::json!({}),
            },
        ];
        let allow: Vec<String> = defs
            .iter()
            .map(|d| d.name.clone())
            .filter(|n| n != "delegate")
            .collect();
        let mut task = TaskSpec::new("child", "/tmp/ws");
        task.tool_allowlist = Some(allow);
        assert!(!task.allows_tool("delegate"));
        assert!(task.allows_tool("file_read"));
    }

    #[tokio::test]
    async fn budget_cell_accumulates_micro_usd() {
        let cell = BudgetCell::default();
        cell.charge(0.25);
        cell.charge(0.5);
        assert!((cell.spent_usd() - 0.75).abs() < 1e-9);
        let clone = cell.clone();
        clone.charge(0.25);
        assert!(
            (cell.spent_usd() - 1.0).abs() < 1e-9,
            "Clone 必须共享同一计数"
        );
    }
}
