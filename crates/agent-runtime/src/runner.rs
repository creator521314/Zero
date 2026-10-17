//! Agent 执行循环实现。
//!
//! 状态机（对应方案 4.1）：
//! ```text
//! Idle → Planning → (tool_calls) → 权限裁决 → Executing → 观察写回 → Iterating → Planning
//!                  → (final text) → Done
//! ```
//! 防护：MAX_ITER 硬上限、重复动作检测、无进展检测、取消令牌。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_core::error::AgentError;
use agent_core::event::{Event, EventBus, Outcome, ToolOutcome};
use agent_core::ids::{ApprovalId, ToolCallId};
use agent_core::perm::{ApprovalRequest, Decision, PermissionEngine};
use agent_core::provider::{
    ChatRequest, ContentPart, LlmProvider, Message, RequestMeta, Role, StreamEvent, ToolCall, Usage,
};
use agent_core::store::LogConfig;
use agent_core::task::{AgentRunner, RunControl, TaskSpec};
use agent_core::tool::{Tool, ToolCtx};
use agent_store::JsonlStore;
use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};

use crate::context::{ContextConfig, ContextManager};

/// 审批应答。
#[derive(Debug, Clone)]
pub enum ApprovalReply {
    Approve { remember: bool },
    Reject { reason: String },
}

/// 工具查找函数：按名取工具。
pub type ToolLookup = Arc<dyn Fn(&str) -> Option<Arc<dyn Tool>> + Send + Sync>;

/// 批量批准回调：(task_id, tool_name)。
pub type BatchApproveFn = Arc<dyn Fn(String, String) + Send + Sync>;

/// 审批消息：(审批 ID, 应答)。
pub type ApprovalMsg = (ApprovalId, ApprovalReply);

/// 流式解析中累积的工具调用参数。
type ToolAccum = std::collections::BTreeMap<usize, (Option<ToolCallId>, Option<String>, String)>;

/// 运行时依赖。
pub struct RuntimeDeps {
    pub provider: Arc<dyn LlmProvider>,
    pub provider_name: String,
    /// 工具查找函数（按名取工具）。
    pub tools: ToolLookup,
    pub tool_defs: Vec<agent_core::provider::ToolDef>,
    pub perm: Arc<dyn PermissionEngine>,
    pub store: Arc<JsonlStore>,
    /// 审计日志落盘策略：决定上下文快照与逐字增量的去留。
    pub log_cfg: LogConfig,
    /// 审批应答接收端（一次一个，由前端回填）。
    pub approvals: Arc<Mutex<mpsc::Receiver<ApprovalMsg>>>,
    pub context_cfg: ContextConfig,
    /// 供权限引擎登记「本任务此类全批准」的回调。
    pub batch_approve: Option<BatchApproveFn>,
    /// 单任务预算（USD）。
    pub per_task_budget: f64,
}

/// Agent 运行时。
pub struct AgentRuntime {
    deps: RuntimeDeps,
}

impl AgentRuntime {
    pub fn new(deps: RuntimeDeps) -> Self {
        Self { deps }
    }

    fn system_prompt(task: &TaskSpec) -> String {
        // 工具的相对路径基于项目根（没有项目根时就是私有工作区）；
        // 提示词里给出的必须是同一个目录，否则模型会照着它去猜不存在的路径。
        let ws = task
            .project_root
            .clone()
            .unwrap_or_else(|| task.workspace.clone());
        format!(
            "你是一个个人助理 Agent，通过工具在用户电脑上完成真实任务。\n\
             【用户任务】{prompt}\n\
             【工作目录】{ws}（相对路径基于它；所有文件读写必须在此目录内）\n\
             \n\
             规则：\n\
             1. 先思考再行动；能一次调用多个工具就并行调用。\n\
             2. 产出文件时写入工作目录（推荐放到 artifacts/ 下），并在 finish 的 artifacts 里登记交付物路径。\n\
             3. 任务完成时调用 finish 工具结束：在 summary 里给用户一段简洁总结。\n\
             4. 如果某个操作被拒绝，换一种策略，不要重复同样的调用。\n\
             5. 目标文件/目录不存在或目录为空时，**不要继续猜路径**；\n\
                直接说明缺什么，向用户索要材料或改用其他思路。",
            prompt = task.prompt,
            ws = ws,
        )
    }

    /// 单次模型调用 + 流式聚合。返回 (文本, 工具调用集合, finish_reason, usage, provider)。
    async fn stream_once(
        &self,
        bus: &EventBus,
        task_id: &str,
        cm: &mut ContextManager,
        cancel: &tokio_util::sync::CancellationToken,
        task: &TaskSpec,
    ) -> Result<StreamOutcome, AgentError> {
        let req = ChatRequest {
            model: task.model.clone().unwrap_or_default(),
            messages: cm.messages().to_vec(),
            tools: self.deps.tool_defs.clone(),
            temperature: Some(0.3),
            max_tokens: None,
            stream: true,
            reasoning_effort: None,
            metadata: RequestMeta {
                task_id: Some(task_id.to_string()),
                route_hint: None,
            },
        };

        let mut stream = self.deps.provider.chat_stream(req).await?;
        let mut text = String::new();
        let mut tool_acc: ToolAccum = std::collections::BTreeMap::new();
        let mut finish = agent_core::provider::FinishReason::Stop;
        let mut usage = Usage::default();

        use futures::StreamExt;
        loop {
            let ev = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(AgentError::Cancelled),
                ev = stream.next() => ev,
            };
            let Some(ev) = ev else { break };
            match ev? {
                StreamEvent::Start { .. } => {}
                StreamEvent::TextDelta(t) => {
                    text.push_str(&t);
                    let _ = bus.send(Event::TextDelta {
                        id: agent_core::ids::TaskId(task_id.to_string()),
                        text: t,
                    });
                }
                StreamEvent::ReasoningDelta(r) => {
                    let _ = bus.send(Event::ReasoningDelta {
                        id: agent_core::ids::TaskId(task_id.to_string()),
                        text: r,
                    });
                }
                StreamEvent::ToolCallDelta {
                    index,
                    id,
                    name,
                    args_delta,
                } => {
                    let e = tool_acc.entry(index).or_insert((None, None, String::new()));
                    if id.is_some() {
                        e.0 = id;
                    }
                    if name.is_some() {
                        e.1 = name;
                    }
                    e.2.push_str(&args_delta);
                }
                StreamEvent::Usage(u) => usage = u,
                StreamEvent::Done(f) => {
                    finish = f;
                    break;
                }
                StreamEvent::Error(e) => return Err(AgentError::Provider(e)),
            }
        }

        // 参数只在 Done 后整体 parse（不边收边 parse）
        let mut calls = Vec::new();
        for (_idx, (id, name, args)) in tool_acc {
            let Some(name) = name else { continue };
            let arguments = serde_json::from_str(&args).unwrap_or(serde_json::Value::Null);
            calls.push(ToolCall {
                id: id.unwrap_or_else(ToolCallId::new),
                name,
                arguments,
            });
        }

        Ok(StreamOutcome {
            text,
            calls,
            finish,
            usage,
        })
    }
}

struct StreamOutcome {
    text: String,
    calls: Vec<ToolCall>,
    #[allow(dead_code)]
    finish: agent_core::provider::FinishReason,
    usage: Usage,
}

/// 工具执行结果。
///
/// 除了回填给模型的内容，还要把**工具原文**带回执行循环：它是审计里唯一
/// 能回答「工具到底返回了什么」的字段（工具的错误文本只存在于这一路）。
struct ToolExecuted {
    /// 回填给模型的内容（大输出已外置，出错时带 `[error]` 前缀）。
    content: String,
    /// 工具自报失败（如 `web_search` 未配置后端）。
    is_error: bool,
    artifacts: Vec<String>,
}

#[async_trait]
impl AgentRunner for AgentRuntime {
    async fn run(
        &self,
        task: TaskSpec,
        bus: EventBus,
        ctl: RunControl,
    ) -> Result<Outcome, AgentError> {
        let task_id = task.id.clone();
        tracing::info!(
            task = %task_id.0,
            provider = %self.deps.provider_name,
            max_iterations = task.max_iterations,
            workspace = %task.workspace,
            project_root = %task.project_root.clone().unwrap_or_else(|| "-".into()),
            "run start"
        );
        let _ = bus.send(Event::TaskStarted {
            id: task_id.clone(),
            title: task.title.clone(),
        });

        let mut cm = ContextManager::new(self.deps.context_cfg.clone(), Self::system_prompt(&task));
        cm.push(Message::user(&task.prompt));

        let cancel = ctl.cancel.clone();
        let mut artifacts: Vec<String> = Vec::new();
        let mut repeats: std::collections::HashMap<u64, usize> = Default::default();
        let mut no_progress = 0usize;
        let mut spent = 0.0f64;
        // 审计用：记录真实生效的模型（未显式指定时取 provider 名）。
        let model = task
            .model
            .clone()
            .unwrap_or_else(|| self.deps.provider_name.clone());

        for iteration in 0..task.max_iterations {
            if cancel.is_cancelled() {
                let _ = bus.send(Event::TaskFinished {
                    id: task_id.clone(),
                    outcome: Outcome::Cancelled,
                });
                return Ok(Outcome::Cancelled);
            }
            cm.maybe_compress();

            // 落盘本轮送往模型的完整上下文（含 system 固定区）。
            // 放在请求之前：即使本轮流被中断或报错，也知道模型当时看到了什么。
            if self.deps.log_cfg.save_context {
                let _ = bus.send(Event::ContextSnapshot {
                    id: task_id.clone(),
                    iteration: iteration as u32,
                    model: model.clone(),
                    messages: cm.messages().to_vec(),
                    tools: self.deps.tool_defs.iter().map(|t| t.name.clone()).collect(),
                });
            }

            let out = match self
                .stream_once(&bus, &task_id.0, &mut cm, &cancel, &task)
                .await
            {
                Ok(o) => o,
                Err(AgentError::Cancelled) => {
                    let _ = bus.send(Event::TaskFinished {
                        id: task_id.clone(),
                        outcome: Outcome::Cancelled,
                    });
                    return Ok(Outcome::Cancelled);
                }
                Err(e) => {
                    let _ = bus.send(Event::Error {
                        id: task_id.clone(),
                        err: agent_core::ErrorInfo::from(&e),
                    });
                    return Err(e);
                }
            };

            // 本轮模型的完整回复：正文 + 工具调用 + 用量。
            // 与逐字增量内容重合，但一行即可还原整轮，也是默认的落盘形态。
            let _ = bus.send(Event::AssistantTurn {
                id: task_id.clone(),
                iteration: iteration as u32,
                text: out.text.clone(),
                calls: out.calls.clone(),
                usage: out.usage,
            });

            // 成本累计（简化：按 provider 单价）
            if let Some(price) = self.deps.provider.price("") {
                spent += (out.usage.input_tokens as f64 / 1000.0) * price.input_per_1k
                    + (out.usage.output_tokens as f64 / 1000.0) * price.output_per_1k;
                if self.deps.per_task_budget > 0.0 && spent > self.deps.per_task_budget {
                    let _ = bus.send(Event::Error {
                        id: task_id.clone(),
                        err: agent_core::ErrorInfo::new(
                            "budget_exceeded",
                            format!("task budget ${:.2} exceeded", self.deps.per_task_budget),
                        ),
                    });
                    return Err(AgentError::BudgetExceeded(format!("${spent:.2}")));
                }
            }

            // 无工具调用 → 任务完成
            if out.calls.is_empty() {
                let summary = if out.text.is_empty() {
                    "（无文本输出）".to_string()
                } else {
                    out.text.clone()
                };
                let _ = bus.send(Event::TaskFinished {
                    id: task_id.clone(),
                    outcome: Outcome::Success {
                        summary: summary.clone(),
                        artifacts: artifacts.clone(),
                    },
                });
                return Ok(Outcome::Success { summary, artifacts });
            }

            // 记录 assistant 的工具调用消息
            let mut assistant_msg = Message {
                role: Role::Assistant,
                content: if out.text.is_empty() {
                    vec![]
                } else {
                    vec![ContentPart::text(out.text.clone())]
                },
                tool_calls: out.calls.clone(),
                tool_call_id: None,
                reasoning: None,
            };
            assistant_msg.reasoning = None;
            cm.push(assistant_msg);

            // 逐个执行工具
            let mut tool_messages = Vec::new();
            let mut progressed = false;
            // 终止信号（finish）的结果：一旦出现即提前收尾。
            let mut finish_summary: Option<String> = None;
            for call in &out.calls {
                let call_id = call.id.clone();
                let _ = bus.send(Event::ToolCallStart {
                    id: task_id.clone(),
                    call_id: call_id.clone(),
                    name: call.name.clone(),
                    args: call.arguments.clone(),
                });

                // 重复动作检测
                let h = hash_call(call);
                let cnt = repeats.entry(h).or_insert(0);
                *cnt += 1;
                let repeat_count = *cnt;

                // 终止信号工具（如 finish）无副作用，跳过权限裁决直接执行。
                let terminating = (self.deps.tools)(&call.name)
                    .map(|t| t.is_terminating())
                    .unwrap_or(false);

                // ---- 权限裁决 ----
                let mut decision = if terminating {
                    Decision::Allow
                } else {
                    self.deps.perm.check(call, &task).await
                };
                if repeat_count >= 3 && matches!(decision, Decision::Allow) {
                    decision =
                        Decision::Deny("相同操作已重复 3 次，请更换策略或结束任务".to_string());
                }

                match decision {
                    Decision::Deny(reason) => {
                        tool_messages.push(Message {
                            role: Role::Tool,
                            content: vec![ContentPart::text(format!("[denied] {reason}"))],
                            tool_calls: vec![],
                            tool_call_id: Some(call_id.clone()),
                            reasoning: None,
                        });
                        let _ = bus.send(Event::ToolCallEnd {
                            id: task_id.clone(),
                            call_id: call_id.clone(),
                            outcome: ToolOutcome::Denied,
                            ms: 0,
                            detail: Some(reason),
                            output: None,
                        });
                        continue;
                    }
                    Decision::AskUser => {
                        let approval_id = ApprovalId::new();
                        tracing::info!(
                            task = %task_id.0,
                            tool = %call.name,
                            approval = %approval_id.0,
                            "awaiting user approval"
                        );
                        let _ = bus.send(Event::AwaitingApproval {
                            id: task_id.clone(),
                            approval_id: approval_id.clone(),
                            req: ApprovalRequest {
                                id: approval_id.clone(),
                                task_id: task_id.clone(),
                                tool: call.name.clone(),
                                args: call.arguments.clone(),
                                risk: self.tool_risk(&call.name),
                                reason: "需要用户确认".into(),
                                matched_rule: None,
                            },
                        });
                        match self.wait_approval(&approval_id, &cancel).await {
                            Ok(ApprovalReply::Approve { remember }) => {
                                if remember {
                                    if let Some(cb) = &self.deps.batch_approve {
                                        cb(task.id.0.clone(), call.name.clone());
                                    }
                                }
                            }
                            Ok(ApprovalReply::Reject { reason }) => {
                                tool_messages.push(Message {
                                    role: Role::Tool,
                                    content: vec![ContentPart::text(format!(
                                        "[rejected by user] {reason}"
                                    ))],
                                    tool_calls: vec![],
                                    tool_call_id: Some(call_id.clone()),
                                    reasoning: None,
                                });
                                let _ = bus.send(Event::ToolCallEnd {
                                    id: task_id.clone(),
                                    call_id: call_id.clone(),
                                    outcome: ToolOutcome::Rejected,
                                    ms: 0,
                                    detail: Some(reason),
                                    output: None,
                                });
                                continue;
                            }
                            Err(_) => {
                                return Ok(Outcome::Cancelled);
                            }
                        }
                    }
                    Decision::Allow => {}
                }

                // ---- 执行 ----
                let started = Instant::now();
                let result = self.execute_tool(call, &task, &cancel, &mut cm).await;
                let ms = started.elapsed().as_millis() as u64;

                match result {
                    Ok(r) => {
                        if terminating {
                            finish_summary = Some(if r.content.is_empty() {
                                "（无文本输出）".to_string()
                            } else {
                                r.content.clone()
                            });
                        }
                        if !r.artifacts.is_empty() {
                            progressed = true;
                            for a in &r.artifacts {
                                if !artifacts.contains(a) {
                                    artifacts.push(a.clone());
                                }
                            }
                        } else if !r.content.is_empty() {
                            progressed = true;
                        }
                        // 工具自报失败（is_error）也算失败：以前统一按 Ok 记，
                        // 「工具返回了错误文本」在日志里反而显示成功。
                        let outcome = if r.is_error {
                            ToolOutcome::Failed
                        } else {
                            ToolOutcome::Ok
                        };
                        tool_messages.push(Message {
                            role: Role::Tool,
                            content: vec![ContentPart::text(r.content.clone())],
                            tool_calls: vec![],
                            tool_call_id: Some(call_id.clone()),
                            reasoning: None,
                        });
                        let _ = bus.send(Event::ToolCallEnd {
                            id: task_id.clone(),
                            call_id: call_id.clone(),
                            outcome,
                            ms,
                            detail: if r.is_error {
                                Some(r.content.clone())
                            } else {
                                None
                            },
                            // 工具返回的原文一并落盘：这是事后回答「工具到底
                            // 返回了什么」的唯一依据。
                            output: Some(r.content),
                        });
                    }
                    Err(e) => {
                        tool_messages.push(Message {
                            role: Role::Tool,
                            content: vec![ContentPart::text(format!("[error] {e}"))],
                            tool_calls: vec![],
                            tool_call_id: Some(call_id.clone()),
                            reasoning: None,
                        });
                        let _ = bus.send(Event::ToolCallEnd {
                            id: task_id.clone(),
                            call_id: call_id.clone(),
                            outcome: ToolOutcome::Failed,
                            ms,
                            detail: Some(e.to_string()),
                            output: None,
                        });
                    }
                }

                // 遇到终止信号立即结束工具循环。
                if finish_summary.is_some() {
                    break;
                }
            }

            for m in tool_messages {
                cm.push(m);
            }

            // 终止信号：finish 已把总结与产物登记好，直接收尾。
            if let Some(summary) = finish_summary {
                let _ = bus.send(Event::TaskFinished {
                    id: task_id.clone(),
                    outcome: Outcome::Success {
                        summary: summary.clone(),
                        artifacts: artifacts.clone(),
                    },
                });
                return Ok(Outcome::Success { summary, artifacts });
            }

            // 无进展检测
            if progressed {
                no_progress = 0;
            } else {
                no_progress += 1;
                if no_progress >= 4 {
                    let summary = format!(
                        "连续 {} 轮没有产生新的文件或有效输出，任务可能已停滞。\
                         最后的模型输出：\n{}",
                        no_progress, out.text
                    );
                    let _ = bus.send(Event::TaskFinished {
                        id: task_id.clone(),
                        outcome: Outcome::Success {
                            summary: summary.clone(),
                            artifacts: artifacts.clone(),
                        },
                    });
                    return Ok(Outcome::Success { summary, artifacts });
                }
            }

            tracing::debug!(task = %task_id, iteration, "iteration complete");
        }

        // 迭代上限
        let err = AgentError::IterationLimit(task.max_iterations);
        let _ = bus.send(Event::Error {
            id: task_id.clone(),
            err: agent_core::ErrorInfo::from(&err),
        });
        Err(err)
    }
}

impl AgentRuntime {
    fn tool_risk(&self, name: &str) -> agent_core::perm::Risk {
        (self.deps.tools)(name)
            .map(|t| t.risk_level())
            .unwrap_or(agent_core::perm::Risk::Medium)
    }

    /// 等待审批应答。
    ///
    /// 这是「任务无声卡住」最常见的悬挂点：只要应答送不进来（前端没转发、
    /// 编排层阻塞、通道关闭），任务就会一直停在这里且界面上毫无动静。
    /// 因此进出与异常分支都留日志，便于事后从 logs/agent.log 复盘。
    async fn wait_approval(
        &self,
        approval_id: &ApprovalId,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<ApprovalReply, AgentError> {
        tracing::info!(approval = %approval_id.0, "waiting for approval reply");
        let mut rx = self.deps.approvals.lock().await;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    tracing::warn!(approval = %approval_id.0, "approval wait cancelled");
                    return Err(AgentError::Cancelled);
                }
                msg = rx.recv() => {
                    match msg {
                        Some((id, reply)) if id == *approval_id => {
                            tracing::info!(
                                approval = %approval_id.0,
                                reply = ?reply,
                                "approval reply received"
                            );
                            return Ok(reply);
                        }
                        Some((id, _)) => {
                            // 通常意味着前端把应答发给了另一个审批（ID 错配）。
                            tracing::warn!(
                                got = %id.0,
                                want = %approval_id.0,
                                "stale approval reply dropped"
                            );
                            continue;
                        }
                        None => {
                            tracing::error!(approval = %approval_id.0, "approval channel closed");
                            return Err(AgentError::Internal("approval channel closed".into()));
                        }
                    }
                }
            }
        }
    }

    /// 执行工具（含外置与超时）。
    async fn execute_tool(
        &self,
        call: &ToolCall,
        task: &TaskSpec,
        cancel: &tokio_util::sync::CancellationToken,
        cm: &mut ContextManager,
    ) -> Result<ToolExecuted, AgentError> {
        let Some(tool) = (self.deps.tools)(&call.name) else {
            return Ok(ToolExecuted {
                content: format!("[error] unknown tool: {}", call.name),
                is_error: true,
                artifacts: vec![],
            });
        };

        let ctx = ToolCtx {
            workspace: std::path::PathBuf::from(&task.workspace),
            project_root: task.project_root.as_ref().map(std::path::PathBuf::from),
            task_id: task.id.0.clone(),
            timeout: Duration::from_secs(120),
            cancel: cancel.clone(),
        };

        let out = tool
            .execute(call.arguments.clone(), &ctx)
            .await
            .map_err(AgentError::Tool)?;

        let arts: Vec<String> = out.side_effects.iter().map(|a| a.path.clone()).collect();

        // 大输出外置
        let content = if cm.should_externalize(out.content.len()) {
            let ctxdir = std::path::Path::new(&task.workspace).join("ctx");
            let _ = tokio::fs::create_dir_all(&ctxdir).await;
            let fname = format!("{}.txt", uuid::Uuid::new_v4());
            let fpath = ctxdir.join(&fname);
            let _ = tokio::fs::write(&fpath, out.content.as_bytes()).await;
            let snippet: String = out.content.chars().take(500).collect();
            format!(
                "[输出已外置到 ctx/{fname}，共 {} 字节] 摘要：{snippet}",
                out.content.len()
            )
        } else {
            out.content.clone()
        };

        let content = if out.is_error {
            format!("[error] {content}")
        } else {
            content
        };

        // 注意：这里**不落盘**。事件一律由执行循环统一发到总线，
        // 交给 `agent_store::recorder` 单点裁决，避免同一条事件被写两遍。
        Ok(ToolExecuted {
            content,
            is_error: out.is_error,
            artifacts: arts,
        })
    }
}

fn hash_call(call: &ToolCall) -> u64 {
    let mut h = DefaultHasher::new();
    call.name.hash(&mut h);
    // 规范化参数（serde_json::Value 的 Hash 对对象顺序不敏感）
    call.arguments.to_string().hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::event::event_bus;
    use agent_core::ids::SessionId;
    use agent_core::perm::{Decision, PermissionEngine, Risk};
    use agent_core::provider::{
        BoxStream, Capabilities, ChatResponse, FinishReason, LlmError, ModelPrice, StreamEvent,
        ToolDef,
    };
    use agent_core::store::Store;
    use agent_core::tool::{ToolError, ToolOutput};
    use futures::stream;
    use std::path::PathBuf;

    /// 按脚本吐流式事件的假 provider：每轮取走一条剧本。
    struct ScriptedProvider {
        turns: Mutex<Vec<Vec<StreamEvent>>>,
    }

    #[async_trait]
    impl LlmProvider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        async fn chat(&self, _req: ChatRequest) -> Result<ChatResponse, LlmError> {
            Err(LlmError::InvalidRequest("tests use chat_stream".into()))
        }
        async fn chat_stream(
            &self,
            _req: ChatRequest,
        ) -> Result<BoxStream<Result<StreamEvent, LlmError>>, LlmError> {
            let mut turns = self.turns.lock().await;
            let turn = if turns.is_empty() {
                Vec::new()
            } else {
                turns.remove(0)
            };
            Ok(Box::pin(stream::iter(turn.into_iter().map(Ok))))
        }
        fn price(&self, _model: &str) -> Option<ModelPrice> {
            None
        }
    }

    /// 固定返回的工具：`fail` 为真时自报失败。
    struct FixedTool {
        name: &'static str,
        fail: bool,
    }

    #[async_trait]
    impl Tool for FixedTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "测试用固定输出工具"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn risk_level(&self) -> Risk {
            Risk::Low
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCtx,
        ) -> Result<ToolOutput, ToolError> {
            if self.fail {
                Ok(ToolOutput::err("工具自报失败"))
            } else {
                Ok(ToolOutput::ok("工具原文输出"))
            }
        }
    }

    /// 回报自己看到的沙箱根：用于验证 runner → ToolCtx 的接线。
    struct SandboxProbe;

    #[async_trait]
    impl Tool for SandboxProbe {
        fn name(&self) -> &str {
            "sandbox_probe"
        }
        fn description(&self) -> &str {
            "回报工具沙箱根"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        fn risk_level(&self) -> Risk {
            Risk::Low
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            ctx: &ToolCtx,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok(ctx.sandbox_root().display().to_string()))
        }
    }

    struct AllowAll;

    #[async_trait]
    impl PermissionEngine for AllowAll {
        async fn check(&self, _call: &ToolCall, _task: &TaskSpec) -> Decision {
            Decision::Allow
        }
    }

    /// 一律询问的权限引擎：验证终止信号工具会跳过它。
    struct AskAll;

    #[async_trait]
    impl PermissionEngine for AskAll {
        async fn check(&self, _call: &ToolCall, _task: &TaskSpec) -> Decision {
            Decision::AskUser
        }
    }

    /// 终止信号工具：验证 `is_terminating` 触发提前收尾。
    struct TerminatingTool;

    #[async_trait]
    impl Tool for TerminatingTool {
        fn name(&self) -> &str {
            "finish_mock"
        }
        fn description(&self) -> &str {
            "测试用终止信号工具"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
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
            _ctx: &ToolCtx,
        ) -> Result<ToolOutput, ToolError> {
            let summary = args
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("done")
                .to_string();
            Ok(
                ToolOutput::ok(summary).with_artifacts(vec![agent_core::tool::Artifact {
                    path: "/tmp/out.txt".into(),
                    bytes: 3,
                }]),
            )
        }
    }

    /// 装配一个「脚本化 provider + 固定输出工具」的运行时。
    ///
    /// 返回 (运行时, store, 临时根目录)；工作区固定为 `<临时根>/workspace`。
    fn make_runtime(
        turns: Vec<Vec<StreamEvent>>,
        log_cfg: LogConfig,
    ) -> (AgentRuntime, Arc<JsonlStore>, PathBuf) {
        let tmp = std::env::temp_dir().join(format!("agent-rt-log-{}", uuid::Uuid::new_v4()));
        let ws = tmp.join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let store = Arc::new(JsonlStore::new(&tmp));

        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(FixedTool {
                name: "ok_tool",
                fail: false,
            }),
            Arc::new(FixedTool {
                name: "fail_tool",
                fail: true,
            }),
        ];
        let defs: Vec<ToolDef> = tools
            .iter()
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect();
        let lookup: ToolLookup = Arc::new(move |name: &str| {
            let ok: Arc<dyn Tool> = Arc::new(FixedTool {
                name: "ok_tool",
                fail: false,
            });
            let bad: Arc<dyn Tool> = Arc::new(FixedTool {
                name: "fail_tool",
                fail: true,
            });
            match name {
                "ok_tool" => Some(ok),
                "fail_tool" => Some(bad),
                "sandbox_probe" => Some(Arc::new(SandboxProbe)),
                _ => None,
            }
        });
        let (_ap_tx, ap_rx) = mpsc::channel::<ApprovalMsg>(1);

        let runtime = AgentRuntime::new(RuntimeDeps {
            provider: Arc::new(ScriptedProvider {
                turns: Mutex::new(turns),
            }),
            provider_name: "scripted".into(),
            tools: lookup,
            tool_defs: defs,
            perm: Arc::new(AllowAll),
            store: store.clone(),
            log_cfg,
            approvals: Arc::new(Mutex::new(ap_rx)),
            context_cfg: ContextConfig::default(),
            batch_approve: None,
            per_task_budget: 0.0,
        });
        (runtime, store, tmp)
    }

    /// 装配真实工具（file_write / file_read / shell_exec / finish）的运行时，
    /// 用于 M2「写文件 → 执行脚本 → 报告」端到端验收。
    fn make_runtime_real_tools(
        turns: Vec<Vec<StreamEvent>>,
    ) -> (AgentRuntime, Arc<JsonlStore>, PathBuf) {
        use agent_tools::finish::Finish;
        use agent_tools::fs_tools::{FileRead, FileWrite};
        use agent_tools::shell::ShellExec;

        let tmp = std::env::temp_dir().join(format!("agent-rt-m2-{}", uuid::Uuid::new_v4()));
        let ws = tmp.join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let store = Arc::new(JsonlStore::new(&tmp));

        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(FileWrite),
            Arc::new(FileRead),
            Arc::new(ShellExec),
            Arc::new(Finish),
        ];
        let defs: Vec<ToolDef> = tools
            .iter()
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect();
        let lookup: ToolLookup = Arc::new(move |name: &str| match name {
            "file_write" => Some(Arc::new(FileWrite) as Arc<dyn Tool>),
            "file_read" => Some(Arc::new(FileRead) as Arc<dyn Tool>),
            "shell_exec" => Some(Arc::new(ShellExec) as Arc<dyn Tool>),
            "finish" => Some(Arc::new(Finish) as Arc<dyn Tool>),
            _ => None,
        });
        let (_ap_tx, ap_rx) = mpsc::channel::<ApprovalMsg>(1);

        let runtime = AgentRuntime::new(RuntimeDeps {
            provider: Arc::new(ScriptedProvider {
                turns: Mutex::new(turns),
            }),
            provider_name: "scripted".into(),
            tools: lookup,
            tool_defs: defs,
            perm: Arc::new(AllowAll),
            store: store.clone(),
            log_cfg: LogConfig::default(),
            approvals: Arc::new(Mutex::new(ap_rx)),
            context_cfg: ContextConfig::default(),
            batch_approve: None,
            per_task_budget: 0.0,
        });
        (runtime, store, tmp)
    }

    /// 跨平台的「读取文件内容」命令：Windows 用 type，Unix 用 cat。
    fn read_cmd(path: &str) -> String {
        if cfg!(windows) {
            format!("type {path}")
        } else {
            format!("cat {path}")
        }
    }

    /// 跑一次任务，返回总线上收到的全部事件。
    async fn run_scripted(turns: Vec<Vec<StreamEvent>>) -> (Vec<Event>, Outcome) {
        let (runtime, _store, tmp) = make_runtime(turns, LogConfig::default());
        let bus = event_bus();
        let mut sub = bus.subscribe();
        let mut spec = TaskSpec::new("介绍下本项目", tmp.join("workspace").display().to_string());
        spec.max_iterations = 4;
        let outcome = runtime.run(spec, bus, RunControl::new()).await.unwrap();

        let mut events = Vec::new();
        while let Ok(ev) = sub.try_recv() {
            events.push(ev);
        }
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        (events, outcome)
    }

    fn tool_call(index: usize, id: &str, name: &str) -> StreamEvent {
        tool_call_args(index, id, name, "{}")
    }

    /// 带完整参数的工具调用（args 为一段合法 JSON）。
    fn tool_call_args(index: usize, id: &str, name: &str, args: &str) -> StreamEvent {
        StreamEvent::ToolCallDelta {
            index,
            id: Some(agent_core::ids::ToolCallId(id.to_string())),
            name: Some(name.to_string()),
            args_delta: args.into(),
        }
    }

    #[tokio::test]
    async fn logs_full_context_and_tool_output() {
        // 第一轮：说话 + 调两个工具（一个成功、一个自报失败）；第二轮：纯文本收尾。
        let (events, outcome) = run_scripted(vec![
            vec![
                StreamEvent::TextDelta("我先看看工作区".into()),
                tool_call(0, "c1", "ok_tool"),
                tool_call(1, "c2", "fail_tool"),
                StreamEvent::Usage(Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                    reasoning_tokens: None,
                    cached_tokens: None,
                }),
                StreamEvent::Done(agent_core::provider::FinishReason::ToolCalls),
            ],
            vec![
                StreamEvent::TextDelta("工作区是空的".into()),
                StreamEvent::Usage(Usage {
                    input_tokens: 12,
                    output_tokens: 3,
                    reasoning_tokens: None,
                    cached_tokens: None,
                }),
                StreamEvent::Done(agent_core::provider::FinishReason::Stop),
            ],
        ])
        .await;
        assert!(matches!(outcome, Outcome::Success { .. }));

        let snapshots: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::ContextSnapshot {
                    iteration,
                    model,
                    messages,
                    tools,
                    ..
                } => Some((*iteration, model.clone(), messages.clone(), tools.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(snapshots.len(), 2, "每轮都要留一份上下文快照");
        // 第 0 轮：system + user
        assert_eq!(snapshots[0].2.len(), 2);
        assert!(
            snapshots[0].2[0].text().contains("个人助理 Agent"),
            "system 固定区必须留存：{}",
            snapshots[0].2[0].text()
        );
        assert_eq!(snapshots[0].2[1].text(), "介绍下本项目");
        // 第 1 轮：system + user + assistant + 2 条工具结果
        assert_eq!(snapshots[1].2.len(), 5, "工具结果应回填进下一轮上下文");
        assert_eq!(snapshots[1].0, 1);
        assert_eq!(snapshots[1].1, "scripted");
        assert_eq!(snapshots[1].3.len(), 2, "可用工具名要记录");

        let turns: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::AssistantTurn {
                    iteration,
                    text,
                    calls,
                    ..
                } => Some((*iteration, text.clone(), calls.len())),
                _ => None,
            })
            .collect();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0], (0, "我先看看工作区".to_string(), 2));
        assert_eq!(turns[1], (1, "工作区是空的".to_string(), 0));

        let ends: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::ToolCallEnd {
                    outcome,
                    detail,
                    output,
                    ..
                } => Some((*outcome, detail.clone(), output.clone())),
                _ => None,
            })
            .collect();
        // 回归：曾经 execute_tool 直写 store、总线订阅者再写一遍，导致每条落两遍。
        assert_eq!(ends.len(), 2, "两次工具调用 → 两条终结事件，不得重复");
        let ok = ends.iter().find(|(o, _, _)| *o == ToolOutcome::Ok).unwrap();
        assert_eq!(ok.2.as_deref(), Some("工具原文输出"), "工具原文要落盘");
        let bad = ends
            .iter()
            .find(|(o, _, _)| *o == ToolOutcome::Failed)
            .unwrap();
        assert_eq!(
            bad.2.as_deref(),
            Some("[error] 工具自报失败"),
            "工具自报失败也要留下原文"
        );
        assert_eq!(bad.1.as_deref(), Some("[error] 工具自报失败"));
    }

    #[tokio::test]
    async fn context_snapshot_is_skipped_when_disabled() {
        let (runtime, _store, tmp) = make_runtime(
            vec![vec![StreamEvent::TextDelta("hi".into())]],
            LogConfig {
                save_context: false,
                save_stream_deltas: false,
            },
        );
        let bus = event_bus();
        let mut sub = bus.subscribe();
        let mut spec = TaskSpec::new("t", tmp.join("workspace").display().to_string());
        spec.max_iterations = 2;
        let _ = runtime.run(spec, bus, RunControl::new()).await.unwrap();

        let mut snapshots = 0;
        let mut turns = 0;
        while let Ok(ev) = sub.try_recv() {
            match ev {
                Event::ContextSnapshot { .. } => snapshots += 1,
                Event::AssistantTurn { .. } => turns += 1,
                _ => {}
            }
        }
        assert_eq!(snapshots, 0, "关闭后不再记录上下文");
        assert_eq!(turns, 1, "整轮回复不受影响");
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    /// 端到端：runner → 总线 → recorder → jsonl。
    ///
    /// 前两个用例只验证「事件发出去了」，这里验证「确实写进文件、且没写多余内容」。
    #[tokio::test]
    async fn recorder_writes_context_and_tool_output_to_jsonl() {
        let (runtime, store, tmp) = make_runtime(
            vec![
                vec![
                    StreamEvent::TextDelta("我先看看工作区".into()),
                    tool_call(0, "c1", "ok_tool"),
                    StreamEvent::Usage(Usage {
                        input_tokens: 5,
                        output_tokens: 1,
                        reasoning_tokens: None,
                        cached_tokens: None,
                    }),
                    StreamEvent::Done(FinishReason::ToolCalls),
                ],
                vec![
                    StreamEvent::TextDelta("看完了".into()),
                    StreamEvent::Usage(Usage::default()),
                    StreamEvent::Done(FinishReason::Stop),
                ],
            ],
            LogConfig::default(),
        );
        let bus = event_bus();
        let mut spec = TaskSpec::new("介绍下本项目", tmp.join("workspace").display().to_string());
        spec.max_iterations = 4;
        let tid = spec.id.clone();
        let sid = SessionId(tid.0.clone());

        agent_store::spawn_event_sink(bus.clone(), store.clone(), LogConfig::default(), None);
        runtime.run(spec, bus, RunControl::new()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let session = store.load_session(&sid).await.unwrap();
        let kinds: Vec<&str> = session
            .events
            .iter()
            .map(|e| match &e.payload {
                Event::TaskStarted { .. } => "task_started",
                Event::ContextSnapshot { .. } => "context_snapshot",
                Event::AssistantTurn { .. } => "assistant_turn",
                Event::ToolCallStart { .. } => "tool_call_start",
                Event::ToolCallEnd { .. } => "tool_call_end",
                Event::TaskFinished { .. } => "task_finished",
                Event::TextDelta { .. } => "text_delta",
                other => panic!("不该落盘的事件: {other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "task_started",
                "context_snapshot",
                "assistant_turn",
                "tool_call_start",
                "tool_call_end",
                "context_snapshot",
                "assistant_turn",
                "task_finished",
            ]
        );

        // 上下文快照带着 system 提示词一起落盘
        let snap = session.events[1].payload.clone();
        match snap {
            Event::ContextSnapshot {
                messages, tools, ..
            } => {
                assert!(messages[0].text().contains("个人助理 Agent"));
                assert_eq!(messages[1].text(), "介绍下本项目");
                assert_eq!(tools.len(), 2);
            }
            other => panic!("应为 ContextSnapshot: {other:?}"),
        }
        // 工具返回原文落盘
        match &session.events[4].payload {
            Event::ToolCallEnd { output, .. } => {
                assert_eq!(output.as_deref(), Some("工具原文输出"));
            }
            other => panic!("应为 ToolCallEnd: {other:?}"),
        }
        // 逻辑时钟单调（seq 由 store 统一分配）
        let seqs: Vec<u64> = session.events.iter().map(|e| e.clock.seq).collect();
        assert!(
            seqs.windows(2).all(|w| w[0] < w[1]),
            "seq 应单调递增：{seqs:?}"
        );

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    /// 修 P0 的关键接线：项目根必须一路传到工具，否则工具还是在空工作区里干活。
    #[tokio::test]
    async fn tools_see_the_project_root_not_the_private_workspace() {
        let (runtime, _store, tmp) = make_runtime(
            vec![
                vec![
                    tool_call(0, "c1", "sandbox_probe"),
                    StreamEvent::Usage(Usage::default()),
                    StreamEvent::Done(FinishReason::ToolCalls),
                ],
                vec![
                    StreamEvent::TextDelta("好了".into()),
                    StreamEvent::Usage(Usage::default()),
                    StreamEvent::Done(FinishReason::Stop),
                ],
            ],
            LogConfig::default(),
        );
        let project = tmp.join("project");
        std::fs::create_dir_all(&project).unwrap();

        let bus = event_bus();
        let mut sub = bus.subscribe();
        let mut spec = TaskSpec::new("介绍下本项目", tmp.join("workspace").display().to_string());
        spec.project_root = Some(project.display().to_string());
        spec.max_iterations = 4;
        let _ = runtime.run(spec, bus, RunControl::new()).await.unwrap();

        let mut probe = None;
        while let Ok(ev) = sub.try_recv() {
            if let Event::ToolCallEnd { output, .. } = ev {
                probe = output;
            }
        }
        assert_eq!(
            probe.as_deref(),
            Some(project.display().to_string().as_str()),
            "工具的沙箱根必须是项目根，而不是私有工作区"
        );
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    /// finish 语义：终止信号工具跳过权限、提前结束、汇总总结与产物。
    #[tokio::test]
    async fn finish_terminates_and_skips_permission() {
        let tmp = std::env::temp_dir().join(format!("agent-rt-finish-{}", uuid::Uuid::new_v4()));
        let ws = tmp.join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let store = Arc::new(JsonlStore::new(&tmp));

        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(TerminatingTool)];
        let defs: Vec<ToolDef> = tools
            .iter()
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect();
        let lookup: ToolLookup = Arc::new(move |name: &str| {
            if name == "finish_mock" {
                Some(Arc::new(TerminatingTool) as Arc<dyn Tool>)
            } else {
                None
            }
        });
        let (_ap_tx, ap_rx) = mpsc::channel::<ApprovalMsg>(1);

        let runtime = AgentRuntime::new(RuntimeDeps {
            provider: Arc::new(ScriptedProvider {
                turns: Mutex::new(vec![vec![
                    tool_call_args(0, "c1", "finish_mock", r#"{"summary":"完成"}"#),
                    StreamEvent::Usage(Usage::default()),
                    StreamEvent::Done(FinishReason::ToolCalls),
                ]]),
            }),
            provider_name: "scripted".into(),
            tools: lookup,
            tool_defs: defs,
            // 即使权限引擎「一律询问」，终止信号也应跳过裁决直接执行。
            perm: Arc::new(AskAll),
            store: store.clone(),
            log_cfg: LogConfig::default(),
            approvals: Arc::new(Mutex::new(ap_rx)),
            context_cfg: ContextConfig::default(),
            batch_approve: None,
            per_task_budget: 0.0,
        });

        let bus = event_bus();
        let mut sub = bus.subscribe();
        let mut spec = TaskSpec::new("收尾", ws.display().to_string());
        spec.max_iterations = 4;
        let outcome = runtime.run(spec, bus, RunControl::new()).await.unwrap();

        match outcome {
            Outcome::Success { summary, artifacts } => {
                assert_eq!(summary, "完成");
                assert_eq!(artifacts.len(), 1);
                assert!(artifacts[0].ends_with("out.txt"), "实际: {}", artifacts[0]);
            }
            other => panic!("应为 Success: {other:?}"),
        }

        // 终止信号不应触发审批。
        let mut asked = false;
        while let Ok(ev) = sub.try_recv() {
            if matches!(ev, Event::AwaitingApproval { .. }) {
                asked = true;
            }
        }
        assert!(!asked, "终止信号工具不应进入审批流程");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    /// M2 验收：一次对话内「写文件 → 执行脚本 → 报告结果」。
    #[tokio::test]
    async fn m2_write_script_execute_report() {
        let (runtime, _store, tmp) = make_runtime_real_tools(vec![
            // 第 1 轮：写脚本
            vec![
                tool_call_args(
                    0,
                    "c1",
                    "file_write",
                    r#"{"path":"run_me.txt","content":"from-script\n"}"#,
                ),
                StreamEvent::Usage(Usage::default()),
                StreamEvent::Done(FinishReason::ToolCalls),
            ],
            // 第 2 轮：执行脚本（跨平台读文件命令）
            vec![
                tool_call_args(
                    0,
                    "c2",
                    "shell_exec",
                    &format!(r#"{{"cmd":"{}"}}"#, read_cmd("run_me.txt")),
                ),
                StreamEvent::Usage(Usage::default()),
                StreamEvent::Done(FinishReason::ToolCalls),
            ],
            // 第 3 轮：finish 报告并登记产物
            vec![
                tool_call_args(
                    0,
                    "c3",
                    "finish",
                    r#"{"summary":"脚本输出正常","artifacts":["run_me.txt"]}"#,
                ),
                StreamEvent::Usage(Usage::default()),
                StreamEvent::Done(FinishReason::ToolCalls),
            ],
        ]);

        let project = tmp.join("project");
        std::fs::create_dir_all(&project).unwrap();

        let bus = event_bus();
        let mut sub = bus.subscribe();
        let mut spec = TaskSpec::new(
            "写个脚本并跑一下",
            tmp.join("workspace").display().to_string(),
        );
        spec.project_root = Some(project.display().to_string());
        spec.max_iterations = 6;
        let outcome = runtime.run(spec, bus, RunControl::new()).await.unwrap();

        match outcome {
            Outcome::Success { summary, artifacts } => {
                assert_eq!(summary, "脚本输出正常");
                assert_eq!(
                    artifacts.len(),
                    1,
                    "file_write 与 finish 登记同一个文件应去重: {artifacts:?}"
                );
                assert!(
                    artifacts[0].ends_with("run_me.txt"),
                    "实际: {}",
                    artifacts[0]
                );
            }
            other => panic!("应为 Success: {other:?}"),
        }

        // 脚本文件确实落到项目根。
        let content = tokio::fs::read_to_string(project.join("run_me.txt"))
            .await
            .unwrap();
        assert_eq!(content, "from-script\n");

        // shell_exec 的输出应读到脚本内容（只有它会包含这段文本）。
        let mut saw_output = false;
        while let Ok(ev) = sub.try_recv() {
            if let Event::ToolCallEnd {
                output: Some(o), ..
            } = ev
            {
                if o.contains("from-script") {
                    saw_output = true;
                }
            }
        }
        assert!(saw_output, "shell_exec 应读到脚本输出");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[test]
    fn system_prompt_points_at_project_root() {
        let mut spec = TaskSpec::new("介绍下本项目", "/private/workspace/t1");
        spec.project_root = Some("/home/u/proj/zero".into());
        let p = AgentRuntime::system_prompt(&spec);
        assert!(p.contains("/home/u/proj/zero"), "提示词应给项目根: {p}");
        assert!(
            !p.contains("/private/workspace/t1"),
            "不应漏私有工作区: {p}"
        );
        // 提示词与工具行为一致：目标不存在时别再猜路径。
        assert!(p.contains("不要继续猜路径"), "提示词应约束盲目重试: {p}");
    }
}
