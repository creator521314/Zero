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
use agent_core::event::{Event, EventBus, Outcome};
use agent_core::ids::{ApprovalId, ToolCallId};
use agent_core::perm::{ApprovalRequest, Decision, PermissionEngine};
use agent_core::provider::{
    ChatRequest, ContentPart, LlmProvider, Message, RequestMeta, Role, StreamEvent, ToolCall, Usage,
};
use agent_core::store::Durability;
use agent_core::task::{AgentRunner, RunControl, TaskSpec};
use agent_core::tool::{Tool, ToolCtx};
use agent_store::JsonlStore;
use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};

use crate::context::{ContextConfig, ContextManager};

/// 审批应答。
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
        format!(
            "你是一个个人助理 Agent，通过工具在用户电脑上完成真实任务。\n\
             【用户任务】{prompt}\n\
             【工作区】{ws}（所有文件读写必须在此目录内）\n\
             \n\
             规则：\n\
             1. 先思考再行动；能一次调用多个工具就并行调用。\n\
             2. 产出文件时写入工作区，并在最终回复中列出产物路径。\n\
             3. 任务完成时给出简洁总结，不要啰嗦。\n\
             4. 如果某个操作被拒绝，换一种策略，不要重复同样的调用。",
            prompt = task.prompt,
            ws = task.workspace,
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

#[async_trait]
impl AgentRunner for AgentRuntime {
    async fn run(
        &self,
        task: TaskSpec,
        bus: EventBus,
        ctl: RunControl,
    ) -> Result<Outcome, AgentError> {
        let task_id = task.id.clone();
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

        for iteration in 0..task.max_iterations {
            if cancel.is_cancelled() {
                let _ = bus.send(Event::TaskFinished {
                    id: task_id.clone(),
                    outcome: Outcome::Cancelled,
                });
                return Ok(Outcome::Cancelled);
            }
            cm.maybe_compress();

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

                // ---- 权限裁决 ----
                let mut decision = self.deps.perm.check(call, &task).await;
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
                            ok: false,
                            ms: 0,
                        });
                        continue;
                    }
                    Decision::AskUser => {
                        let approval_id = ApprovalId::new();
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
                                    ok: false,
                                    ms: 0,
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
                    Ok((content, arts)) => {
                        if !arts.is_empty() {
                            progressed = true;
                            for a in arts {
                                if !artifacts.contains(&a) {
                                    artifacts.push(a);
                                }
                            }
                        } else if !content.is_empty() {
                            progressed = true;
                        }
                        tool_messages.push(Message {
                            role: Role::Tool,
                            content: vec![ContentPart::text(content)],
                            tool_calls: vec![],
                            tool_call_id: Some(call_id.clone()),
                            reasoning: None,
                        });
                        let _ = bus.send(Event::ToolCallEnd {
                            id: task_id.clone(),
                            call_id: call_id.clone(),
                            ok: true,
                            ms,
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
                            ok: false,
                            ms,
                        });
                    }
                }
            }

            for m in tool_messages {
                cm.push(m);
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
    async fn wait_approval(
        &self,
        approval_id: &ApprovalId,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<ApprovalReply, AgentError> {
        let mut rx = self.deps.approvals.lock().await;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(AgentError::Cancelled),
                msg = rx.recv() => {
                    match msg {
                        Some((id, reply)) if id == *approval_id => return Ok(reply),
                        Some(_) => continue,     // 非本请求，丢弃
                        None => return Err(AgentError::Internal("approval channel closed".into())),
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
    ) -> Result<(String, Vec<String>), AgentError> {
        let Some(tool) = (self.deps.tools)(&call.name) else {
            return Ok((format!("[error] unknown tool: {}", call.name), vec![]));
        };

        let ctx = ToolCtx {
            workspace: std::path::PathBuf::from(&task.workspace),
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

        // 审计落盘（区分持久化/临时）
        let _ = self
            .deps
            .store
            .record(
                &agent_core::ids::SessionId(task.id.0.clone()),
                &task.id,
                Durability::Transient,
                Event::ToolCallEnd {
                    id: task.id.clone(),
                    call_id: call.id.clone(),
                    ok: !out.is_error,
                    ms: 0,
                },
            )
            .await;

        Ok((content, arts))
    }
}

fn hash_call(call: &ToolCall) -> u64 {
    let mut h = DefaultHasher::new();
    call.name.hash(&mut h);
    // 规范化参数（serde_json::Value 的 Hash 对对象顺序不敏感）
    call.arguments.to_string().hash(&mut h);
    h.finish()
}
