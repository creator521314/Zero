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
use agent_core::event::{ApprovalDecision, Event, EventBus, Outcome, ToolOutcome};
use agent_core::ids::{ApprovalId, ToolCallId};
use agent_core::perm::{class_key, ApprovalRequest, Decision, PermissionEngine};
use agent_core::provider::{
    ChatRequest, ContentPart, LlmProvider, Message, RequestMeta, Role, StreamEvent, ToolCall, Usage,
};
use agent_core::store::LogConfig;
use agent_core::task::{AgentRunner, RunControl, TaskSpec, TaskState};
use agent_core::tool::{MalformedArgs, Tool, ToolCtx, ToolErrorCode};
use agent_store::JsonlStore;
use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};

use crate::context::{ContextAssembler, ContextConfig, ContextManager};

/// 单个工具调用的执行超时。超过即按失败回填，模型可换策略。
const TOOL_TIMEOUT: Duration = Duration::from_secs(120);

/// delegate 内部自带 300s 墙钟超时，父层工具超时必须更长，
/// 否则父层会在 120s 先把它掐断（TOOL_TIMEOUT 对所有工具一视同仁）。
const DELEGATE_TOOL_TIMEOUT: Duration = Duration::from_secs(330);

/// 交互问答在等**人**，120s 的机器超时会在用户还没想好时把工具掐断。
/// 30 分钟仍未回答时按超时失败回填，模型换策略；取消令牌仍可立刻打断。
const QUESTION_TOOL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// 审批应答。
#[derive(Debug, Clone)]
pub enum ApprovalReply {
    Approve { remember: bool },
    Reject { reason: String },
}

/// 工具查找函数：按名取工具。
pub type ToolLookup = Arc<dyn Fn(&str) -> Option<Arc<dyn Tool>> + Send + Sync>;

/// 成本上报回调：本次调用花了多少 USD。
///
/// 接 `ProviderRouter::record_spend`。之所以要这条：执行循环是流式直连 provider
/// 的（`provider.chat_stream`），不经过路由，月度用量一度因此**永远累计为 0**——
/// 熔断配了却等于没配。
pub type CostSinkFn = Arc<dyn Fn(f64) + Send + Sync>;

/// 工具输出脱敏函数：把已知密钥值（API key 等）替换成 `<redacted>`。
///
/// 由装配层从配置 + 进程环境构造；执行循环在**回填给模型之前**与**落盘之前**
/// 各用一次，保证上下文快照与 jsonl 里都不出现明文密钥。
pub type RedactFn = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// 审批消息：(审批 ID, 应答)。
pub type ApprovalMsg = (ApprovalId, ApprovalReply);

/// 流式解析中累积的工具调用参数。
type ToolAccum = std::collections::BTreeMap<usize, (Option<ToolCallId>, Option<String>, String)>;

/// 审批应答收件箱。
///
/// 多任务共用一条应答通道，应答可能以**任意顺序**到达（用户在任务 A 的
/// 弹窗上点了「批准」而任务 B 正在等自己的应答）。直接把 ID 不匹配的
/// 应答丢掉会让它对应的任务永远等待——这里改为暂存到 pending，
/// 由后续 `recv_for(同一 ID)` 取走。
pub struct ApprovalInbox {
    rx: Mutex<mpsc::Receiver<ApprovalMsg>>,
    /// 已到达但对应任务还没来取的应答（ID → 应答）。
    pending: Mutex<std::collections::HashMap<ApprovalId, ApprovalReply>>,
}

impl ApprovalInbox {
    pub fn new(rx: mpsc::Receiver<ApprovalMsg>) -> Self {
        Self {
            rx: Mutex::new(rx),
            pending: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// 等待 `id` 对应的应答；先前被「错配」暂存的应答会被优先取走。
    pub async fn recv_for(
        &self,
        id: &ApprovalId,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<ApprovalReply, AgentError> {
        // 先看暂存区：错配到达过的应答不能再错过。
        if let Some(reply) = self.pending.lock().await.remove(id) {
            tracing::info!(approval = %id.0, "approval reply taken from pending buffer");
            return Ok(reply);
        }

        let mut rx = self.rx.lock().await;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    tracing::warn!(approval = %id.0, "approval wait cancelled");
                    return Err(AgentError::Cancelled);
                }
                msg = rx.recv() => {
                    match msg {
                        Some((got, reply)) if got == *id => {
                            tracing::info!(
                                approval = %id.0,
                                reply = ?reply,
                                "approval reply received"
                            );
                            return Ok(reply);
                        }
                        Some((got, reply)) => {
                            // 应答属于另一个审批：暂存，等那个任务来取。
                            tracing::warn!(
                                got = %got.0,
                                want = %id.0,
                                "approval reply buffered for another approval"
                            );
                            let mut pending = self.pending.lock().await;
                            // 上限保护：应答都有时效，超量时整体清空即可。
                            if pending.len() >= 64 {
                                pending.clear();
                            }
                            pending.insert(got, reply);
                            continue;
                        }
                        None => {
                            tracing::error!(approval = %id.0, "approval channel closed");
                            return Err(AgentError::Internal("approval channel closed".into()));
                        }
                    }
                }
            }
        }
    }
}

/// 运行时依赖。
///
/// Clone 用于子任务委派：子 deps 由父 deps 克隆后覆写工具集与预算，
/// 其余依赖（provider / perm / store / approvals / cost_sink / redact）天然共享。
#[derive(Clone)]
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
    /// 审批应答收件箱（多任务共用，错配应答会被暂存而不是丢弃）。
    pub approvals: Arc<ApprovalInbox>,
    pub context_cfg: ContextConfig,
    /// 单任务预算（USD）；`<= 0` 表示不限。
    pub per_task_budget: f64,
    /// 把成本报给账本，用于月度熔断。
    pub cost_sink: Option<CostSinkFn>,
    /// 工具输出脱敏。测试 / 无密钥场景注入恒等函数。
    pub redact: RedactFn,
}

/// Agent 运行时。
pub struct AgentRuntime {
    deps: RuntimeDeps,
}

impl AgentRuntime {
    pub fn new(deps: RuntimeDeps) -> Self {
        Self { deps }
    }

    /// 固定 system 区（v1.2）：**不含本次任务、不含记忆**。
    ///
    /// 旧实现把 `{prompt}` 与【长期记忆】都拼在这里，追问时第 0 条消息
    /// 就变，前缀缓存全灭；且续跑回灌的旧快照又带一份旧 system，于是
    /// system 条数随追问次数线性增长。现在这里只放逐字节恒定的内容，
    /// 任务与记忆由 `ContextAssembler::first_user` 以 user 消息承载。
    fn system_prompt(task: &TaskSpec) -> String {
        let ws = task
            .project_root
            .clone()
            .unwrap_or_else(|| task.workspace.clone());
        ContextAssembler::fixed_system(&ws)
    }

    /// 纯寒暄识别：只做**整句精确匹配**，避免把「你好，帮我改个文件」误判成问好。
    ///
    /// 新会话里「你好」曾被当成任务：固定 system 强调「通过工具完成真实任务」
    /// 与「探查操作批量并行」，模型Sampling时偶尔就会先扫一遍项目再答。这里在
    /// 首轮请求尾部追加一条不落盘的提醒（与预算提醒同机制，不污染固定前缀），
    /// 让寒暄稳定走直接回应路径。
    fn is_plain_greeting(s: &str) -> bool {
        let t = s.trim().to_ascii_lowercase();
        let t = t.trim_matches(|c: char| {
            c.is_whitespace()
                || matches!(c, '。' | '！' | '？' | '～' | '…' | '.' | '!' | '?' | '~')
        });
        matches!(
            t,
            "你好"
                | "你好呀"
                | "你好啊"
                | "您好"
                | "嗨"
                | "哈喽"
                | "在吗"
                | "在么"
                | "早上好"
                | "中午好"
                | "下午好"
                | "晚上好"
                | "谢谢"
                | "谢谢你"
                | "hi"
                | "hello"
                | "hey"
                | "yo"
                | "thanks"
                | "thank you"
        )
    }

    /// 单次模型调用 + 流式聚合。
    ///
    /// `extra` 是**只 appended 到本次请求、不写进历史**的临时消息
    /// （预算提醒、强制收尾指令）。追加在尾部既不影响前缀缓存，也不会
    /// 污染续跑上下文或记忆提炼输入（v1.2，旧实现把提醒以 Role::User
    /// 永久 push 进历史）。
    /// `allow_tools = false` 用于强制收尾轮：禁止模型再发起工具调用。
    async fn stream_once(
        &self,
        bus: &EventBus,
        cm: &mut ContextManager,
        cancel: &tokio_util::sync::CancellationToken,
        task: &TaskSpec,
        extra: &[Message],
        allow_tools: bool,
    ) -> Result<StreamOutcome, AgentError> {
        let task_id = task.id.0.as_str();
        let mut messages = cm.messages().to_vec();
        messages.extend_from_slice(extra);
        let req = ChatRequest {
            model: task.model.clone().unwrap_or_default(),
            messages,
            tools: if allow_tools {
                self.deps
                    .tool_defs
                    .iter()
                    .filter(|d| task.allows_tool(&d.name))
                    .cloned()
                    .collect()
            } else {
                Vec::new()
            },
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
        let mut reasoning = String::new();
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
                    // 子任务只保留整轮 AssistantTurn，不广播逐字增量：
                    // 既不挤占 1024 容量的总线，也不会串进父任务对话区。
                    if task.depth == 0 {
                        let _ = bus.send(Event::TextDelta {
                            id: agent_core::ids::TaskId(task_id.to_string()),
                            text: t,
                        });
                    }
                }
                StreamEvent::ReasoningDelta(r) => {
                    reasoning.push_str(&r);
                    if task.depth == 0 {
                        let _ = bus.send(Event::ReasoningDelta {
                            id: agent_core::ids::TaskId(task_id.to_string()),
                            text: r,
                        });
                    }
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
            // 非法/截断的 JSON 不能静默变 Null：Null 会被工具当成「没传参数」，
            // 模型永远不知道自己发错了格式，只会反复撞同一条 InvalidArgs。
            // 保留原文并打上标记，执行阶段据此回填一条明确的错误结果。
            let arguments = match serde_json::from_str::<serde_json::Value>(&args) {
                Ok(v) if v.is_object() => v,
                Ok(v) => MalformedArgs::marker(format!("参数不是 JSON 对象：{v}")),
                Err(e) => {
                    let raw: String = args.chars().take(300).collect();
                    MalformedArgs::marker(format!("JSON 解析失败：{e}；原文：{raw}"))
                }
            };
            calls.push(ToolCall {
                id: id.unwrap_or_else(ToolCallId::new),
                name,
                arguments,
            });
        }

        Ok(StreamOutcome {
            text,
            reasoning,
            calls,
            finish,
            usage,
        })
    }
}

struct StreamOutcome {
    text: String,
    /// 思考链在统一层保留（续跑/回放不丢），但不回传厂商：
    /// OpenAI 兼容端点不接受请求里的 reasoning 字段。
    reasoning: String,
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
    /// 工具自报失败（如 `web_search` 所有后端都没搜到结果）。
    is_error: bool,
    artifacts: Vec<String>,
    /// 稳定错误码；run_inner 据此把 `[error] ` 替换成 `[error:<code>] `。
    error_code: Option<ToolErrorCode>,
}

#[async_trait]
impl AgentRunner for AgentRuntime {
    async fn run(
        &self,
        task: TaskSpec,
        bus: EventBus,
        ctl: RunControl,
    ) -> Result<Outcome, AgentError> {
        // 任务摘要先登记为 Running：`/tasks`、任务面板都只认这一份数据。
        // 收尾再落终态——中途 panic / 断电也留下「跑过但没跑完」的痕迹。
        self.mark_task(&task, TaskState::Running).await;
        let outcome = self.run_inner(task.clone(), bus, ctl).await;
        let state = match &outcome {
            Ok(Outcome::Success { .. }) => TaskState::Done,
            Ok(Outcome::Failed { .. }) => TaskState::Failed,
            Ok(Outcome::Cancelled) => TaskState::Cancelled,
            Ok(Outcome::Paused) => TaskState::Paused,
            Err(_) => TaskState::Failed,
        };
        self.mark_task(&task, state).await;
        outcome
    }
}

impl AgentRuntime {
    /// 把自上次以来的新增消息落盘（v1.2 增量协议）。
    ///
    /// 收尾路径也必须调用：否则最后一轮的答复与工具结果只存在于事件里，
    /// 快照序列缺一段，续跑与复盘都要靠另一套逻辑去补。
    fn flush_context(
        &self,
        bus: &EventBus,
        task_id: &agent_core::ids::TaskId,
        iteration: u32,
        model: &str,
        cm: &mut ContextManager,
        tools: Vec<String>,
    ) {
        let (added, reset) = cm.flush_delta();
        if added.is_empty() && !reset {
            return;
        }
        if self.deps.log_cfg.save_context {
            let _ = bus.send(Event::ContextSnapshot {
                id: task_id.clone(),
                iteration,
                model: model.to_string(),
                added,
                reset,
                tools,
            });
        }
    }

    /// 记录任务状态（失败只记日志：摘要写不进去不该让任务本身失败）。
    async fn mark_task(&self, task: &TaskSpec, state: TaskState) {
        if let Err(e) = self
            .deps
            .store
            .touch_task(&task.id, &task.title, state)
            .await
        {
            tracing::warn!(task = %task.id.0, error = %e, "task summary write failed");
        }
    }

    /// 执行循环本体；状态登记与终态收尾在 [`AgentRunner::run`] 里做。
    async fn run_inner(
        &self,
        mut task: TaskSpec,
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
        let prompt_text = (self.deps.redact)(&task.prompt);
        let _ = bus.send(Event::TaskStarted {
            id: task_id.clone(),
            title: task.title.clone(),
        });
        // v1.2：用户输入独立落盘。旧实现只有冻结的标题，`/resume` / 切任务 /
        // 重启后回放出的对话里完全没有用户说过什么。
        let _ = bus.send(Event::UserPrompt {
            id: task_id.clone(),
            text: prompt_text.clone(),
        });

        let memory_text = task
            .memory_context
            .as_ref()
            .map(|mc| (self.deps.redact)(&mc.text))
            .filter(|t| !t.trim().is_empty());

        // 上下文唯一构造点：新任务 = 固定 system + 首轮 user（任务 + 记忆）；
        // 续跑 = 沿用快照里的固定 system（不再重建）+ 规范化历史 + 尾部新增。
        // 这样「同一对话追问 N 次」时 Role::System 恒为 1 条，前缀逐字节一致。
        let mut cm = match task.resume_messages.take() {
            Some(history) if !history.is_empty() => {
                let turns = history.len();
                let normalized = ContextAssembler::normalize_history(history);
                let mut cm =
                    ContextManager::from_messages(self.deps.context_cfg.clone(), normalized);
                // 记忆变化才追加尾部增量；不变则历史里那份继续生效，前缀完全不动。
                if let Some(upd) = task
                    .memory_update
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                {
                    let safe = (self.deps.redact)(upd);
                    cm.push(ContextAssembler::background_update(&safe));
                }
                if !prompt_text.trim().is_empty() {
                    cm.push(Message::user(&prompt_text));
                }
                tracing::info!(task = %task_id.0, turns, "resumed from increment context");
                cm
            }
            _ => {
                let mut cm =
                    ContextManager::new(self.deps.context_cfg.clone(), Self::system_prompt(&task));
                cm.push(ContextAssembler::first_user(
                    &prompt_text,
                    memory_text.as_deref(),
                ));
                cm
            }
        };

        let cancel = ctl.cancel.clone();
        let mut artifacts: Vec<String> = Vec::new();
        let mut repeats: std::collections::HashMap<u64, usize> = Default::default();
        let mut no_progress = 0usize;
        // 预算提醒按「剩余轮数」在请求尾部即时构造，不写进历史（见循环内）。
        let mut spent = 0.0f64;
        // 审计用：记录真实生效的模型（未显式指定时取 provider 名）。
        let model = task
            .model
            .clone()
            .unwrap_or_else(|| self.deps.provider_name.clone());
        let tool_names: Vec<String> = self
            .deps
            .tool_defs
            .iter()
            .filter(|d| task.allows_tool(&d.name))
            .map(|d| d.name.clone())
            .collect();

        for iteration in 0..task.max_iterations {
            if cancel.is_cancelled() {
                let _ = bus.send(Event::TaskFinished {
                    id: task_id.clone(),
                    outcome: Outcome::Cancelled,
                });
                self.flush_context(
                    &bus,
                    &task_id,
                    iteration as u32 + 1,
                    &model,
                    &mut cm,
                    tool_names.clone(),
                );
                return Ok(Outcome::Cancelled);
            }
            cm.maybe_compress();

            // 只落「自上次快照以来新增的消息」；`reset=true` 表示压缩重写了
            // 历史，此时 added 是全量基线。旧实现每轮全量落盘，日志按 O(N²) 膨胀。
            self.flush_context(
                &bus,
                &task_id,
                iteration as u32,
                &model,
                &mut cm,
                tool_names.clone(),
            );

            // 预算临近提醒：只附加在本次请求尾部，**不写进历史**。
            // 旧实现以 Role::User 永久 push 进 cm，于是提醒进了快照、
            // 进了提炼输入、跨续跑被当成真实用户输入，过期数字还一直误导。
            let remaining = task.max_iterations.saturating_sub(iteration + 1);
            let budget_extra: Vec<Message> = if (1..=3).contains(&remaining) {
                vec![Message::user(format!(
                    "[系统提示] 迭代预算只剩 {remaining} 轮。请立即收敛：汇总已有发现直接作答，\
                     或调用 finish 结束任务；不要再发起新的大范围探查。"
                ))]
            } else {
                Vec::new()
            };
            // 寒暄只在「新任务的首轮」加提醒：续跑里的「你好」是真实追问，
            // 不能强行改写成寒暄模式。追加在尾部，不进历史、不破坏前缀缓存。
            let greeting_extra: Vec<Message> = if iteration == 0
                && task.resume_messages.is_none()
                && Self::is_plain_greeting(&task.prompt)
            {
                vec![Message::user(
                    "[系统提示] 用户只是问好或寒暄：直接用一句话回应，不要调用任何工具、\
                     不要探查项目、也不要调用 finish。",
                )]
            } else {
                Vec::new()
            };
            let extra = if greeting_extra.is_empty() {
                budget_extra
            } else {
                greeting_extra
            };

            let out = match self
                .stream_once(&bus, &mut cm, &cancel, &task, &extra, true)
                .await
            {
                Ok(o) => o,
                Err(AgentError::Cancelled) => {
                    let _ = bus.send(Event::TaskFinished {
                        id: task_id.clone(),
                        outcome: Outcome::Cancelled,
                    });
                    self.flush_context(
                        &bus,
                        &task_id,
                        iteration as u32 + 1,
                        &model,
                        &mut cm,
                        tool_names.clone(),
                    );
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

            // 成本累计（简化：按 provider 单价）。
            // 传本轮**实际请求的模型**：task.model 指定了具体模型时必须按它
            // 计价；未指定时空串是约定值，表示 provider 的默认模型
            // （见 LlmProvider::price 文档）。原先硬编码 ""，按模型定价的
            // provider 会把指定模型的成本静默算成默认模型的价。
            let price_model = task.model.clone().unwrap_or_default();
            if let Some(price) = self.deps.provider.price(&price_model) {
                let cost = (out.usage.input_tokens as f64 / 1000.0) * price.input_per_1k
                    + (out.usage.output_tokens as f64 / 1000.0) * price.output_per_1k;
                spent += cost;
                // 报到账本：月度熔断要用。不在这里记的话，配好的月度预算永远
                // 花了也看不出来（这条路径不经过 ProviderRouter）。
                if let Some(sink) = self.deps.cost_sink.as_ref() {
                    sink(cost);
                }
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
                // 最终答复也要进上下文：增量快照序列必须覆盖完整对话，
                // 否则续跑时「模型刚刚说过什么」只能靠事件反推。
                if !out.text.is_empty() {
                    cm.push(Message::assistant(out.text.clone()));
                }
                let _ = bus.send(Event::TaskFinished {
                    id: task_id.clone(),
                    outcome: Outcome::Success {
                        summary: summary.clone(),
                        artifacts: artifacts.clone(),
                    },
                });
                self.flush_context(
                    &bus,
                    &task_id,
                    iteration as u32 + 1,
                    &model,
                    &mut cm,
                    tool_names.clone(),
                );
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
            assistant_msg.reasoning = if out.reasoning.is_empty() {
                None
            } else {
                Some(out.reasoning.clone())
            };
            cm.push(assistant_msg);

            // 逐个执行工具
            let mut tool_messages = Vec::new();
            let mut progressed = false;
            // 终止信号（finish）的结果：一旦出现即提前收尾。
            let mut finish_summary: Option<String> = None;
            for call in &out.calls {
                let call_id = call.id.clone();

                // 参数解析失败的调用：不执行工具，回填明确错误让模型重发。
                if let Some(reason) = MalformedArgs::reason(&call.arguments) {
                    let content = format!(
                        "[error:malformed_args] 工具参数不是合法 JSON，未执行：{reason}\n\
                         请重新完整输出合法的 JSON 参数。"
                    );
                    let _ = bus.send(Event::ToolCallStart {
                        id: task_id.clone(),
                        call_id: call_id.clone(),
                        name: call.name.clone(),
                        args: call.arguments.clone(),
                    });
                    let _ = bus.send(Event::ToolCallEnd {
                        id: task_id.clone(),
                        call_id: call_id.clone(),
                        outcome: ToolOutcome::Failed,
                        ms: 0,
                        detail: Some("malformed JSON arguments".to_string()),
                        output: Some(content.clone()),
                    });
                    tool_messages.push(Message {
                        role: Role::Tool,
                        content: vec![ContentPart::text(content)],
                        tool_calls: vec![],
                        tool_call_id: Some(call_id),
                        reasoning: None,
                    });
                    progressed = true;
                    continue;
                }

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
                // 交互工具（ask_user）同理：问一句话还要先弹批准框是骚扰，
                // 而且两个弹层会互相抢模态；重复调用仍由下面的重复检测拦住。
                let (terminating, interactive) = (self.deps.tools)(&call.name)
                    .map(|t| (t.is_terminating(), t.is_interactive()))
                    .unwrap_or((false, false));

                // ---- 权限裁决 ----
                let mut decision = if terminating || interactive {
                    Decision::Allow
                } else {
                    self.deps.perm.check(call, &task).await
                };
                if repeat_count >= 3 && matches!(decision, Decision::Allow) {
                    decision =
                        Decision::Deny("相同操作已重复 3 次，请更换策略或结束任务".to_string());
                }

                // 本次调用是否由用户亲手批准（只影响回填给模型的文本）。
                let mut user_approved = false;

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
                    Decision::AskUser {
                        reason,
                        source,
                        matched_rule,
                    } => {
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
                                reason,
                                matched_rule,
                                source: Some(source),
                            },
                        });
                        match self.wait_approval(&approval_id, &cancel).await {
                            Ok(ApprovalReply::Approve { remember }) => {
                                user_approved = true;
                                // 「被批准」本身也是审计事实：只记请求不记结果，
                                // 事后无法区分「人工放行」与「自动放行」。
                                let _ = bus.send(Event::ApprovalDecided {
                                    id: task_id.clone(),
                                    approval_id: approval_id.clone(),
                                    decision: ApprovalDecision::Approved { remember },
                                });
                                if remember {
                                    // 直接 await 引擎的登记：曾经这里走一个同步回调、
                                    // 回调内部再 `tokio::spawn` 去登记，于是**下一次同类
                                    // 调用可能在登记落定前就查过了**——用户按了 `a`
                                    // 却还被再问一次（实测复现，见
                                    // batch_approval_silences_repeat_calls_end_to_end）。
                                    // 执行循环本身就是 async，没有理由绕这一圈。
                                    self.deps
                                        .perm
                                        .approve_for_task(task.id.0.as_str(), &class_key(call))
                                        .await;
                                }
                            }
                            Ok(ApprovalReply::Reject { reason }) => {
                                let _ = bus.send(Event::ApprovalDecided {
                                    id: task_id.clone(),
                                    approval_id: approval_id.clone(),
                                    decision: ApprovalDecision::Rejected {
                                        reason: reason.clone(),
                                    },
                                });
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
                                self.flush_context(
                                    &bus,
                                    &task_id,
                                    iteration as u32 + 1,
                                    &model,
                                    &mut cm,
                                    tool_names.clone(),
                                );
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
                        // 脱敏必须发生在回填模型与落盘之前：工具输出里可能带出
                        // 明文 API key（`env`、`cat config.toml`），一旦进了
                        // 上下文或 jsonl 就收不回来了。
                        let redacted = (self.deps.redact)(&r.content);
                        // 稳定错误码只进「模型可见内容」，不进 output 原文：
                        // output 是事后审计「工具到底返回了什么」的唯一依据，
                        // 已有测试断言它的字面值，不能被前缀污染。
                        let model_content = match (r.error_code, user_approved) {
                            (Some(code), _) => {
                                let s = redacted.strip_prefix("[error] ").unwrap_or(&redacted);
                                format!("[error:{code}] {s}")
                            }
                            (None, true) => format!("[user-approved] {redacted}"),
                            (None, false) => redacted.clone(),
                        };
                        if terminating {
                            finish_summary = Some(if redacted.is_empty() {
                                "（无文本输出）".to_string()
                            } else {
                                redacted.clone()
                            });
                        }
                        if !r.artifacts.is_empty() {
                            progressed = true;
                            for a in &r.artifacts {
                                if !artifacts.contains(a) {
                                    artifacts.push(a.clone());
                                }
                            }
                        } else if !redacted.is_empty() {
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
                            content: vec![ContentPart::text(model_content)],
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
                                Some(redacted.clone())
                            } else {
                                None
                            },
                            // 工具返回的原文一并落盘：这是事后回答「工具到底
                            // 返回了什么」的唯一依据。
                            output: Some(redacted),
                        });
                    }
                    Err(e) => {
                        let code = match &e {
                            AgentError::Tool(t) => Some(t.code()),
                            _ => None,
                        };
                        let text = match code {
                            Some(c) => format!("[error:{c}] {e}"),
                            None => format!("[error] {e}"),
                        };
                        tool_messages.push(Message {
                            role: Role::Tool,
                            content: vec![ContentPart::text(text)],
                            tool_calls: vec![],
                            tool_call_id: Some(call_id.clone()),
                            reasoning: None,
                        });
                        let _ = bus.send(Event::ToolCallEnd {
                            id: task_id.clone(),
                            call_id: call_id.clone(),
                            outcome: ToolOutcome::Failed,
                            ms,
                            detail: Some(match code {
                                Some(c) => format!("[{c}] {e}"),
                                None => e.to_string(),
                            }),
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
                self.flush_context(
                    &bus,
                    &task_id,
                    iteration as u32 + 1,
                    &model,
                    &mut cm,
                    tool_names.clone(),
                );
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
                    self.flush_context(
                        &bus,
                        &task_id,
                        iteration as u32 + 1,
                        &model,
                        &mut cm,
                        tool_names.clone(),
                    );
                    return Ok(Outcome::Success { summary, artifacts });
                }
            }

            tracing::debug!(task = %task_id, iteration, "iteration complete");
        }

        // R2：迭代预算耗尽不直接失败——追加一轮强制收尾，让模型基于
        // 已收集的上下文给出阶段结论。25 轮的探查成果不该随错误一起作废。
        if !cancel.is_cancelled() {
            let final_extra = vec![Message::user(
                "[系统提示] 迭代预算已耗尽。请立即基于以上已收集的信息输出最终结论：\
                 完成了什么、发现了什么、还剩什么没做。不要再调用任何工具。",
            )];
            // 收尾轮也要留审计痕迹：旧实现既不发快照也不发 AssistantTurn，
            // 最终结论只存在于 TaskFinished.summary，提炼与回放都看不到。
            self.flush_context(
                &bus,
                &task_id,
                task.max_iterations as u32,
                &model,
                &mut cm,
                Vec::new(),
            );
            // allow_tools=false：收尾轮不再给模型任何工具，杜绝「又调一个」。
            if let Ok(out) = self
                .stream_once(&bus, &mut cm, &cancel, &task, &final_extra, false)
                .await
            {
                let _ = bus.send(Event::AssistantTurn {
                    id: task_id.clone(),
                    iteration: task.max_iterations as u32,
                    text: out.text.clone(),
                    calls: out.calls.clone(),
                    usage: out.usage,
                });
                let text = out.text.trim().to_string();
                if !text.is_empty() {
                    // 收尾答复也要进增量序列，续跑才能看到模型最后的结论。
                    cm.push(Message::assistant(text.clone()));
                    let summary = format!(
                        "【部分完成 · 迭代预算 {} 轮已耗尽】\n{}",
                        task.max_iterations, text
                    );
                    let _ = bus.send(Event::TaskFinished {
                        id: task_id.clone(),
                        outcome: Outcome::Success {
                            summary: summary.clone(),
                            artifacts: artifacts.clone(),
                        },
                    });
                    self.flush_context(
                        &bus,
                        &task_id,
                        task.max_iterations as u32,
                        &model,
                        &mut cm,
                        tool_names.clone(),
                    );
                    return Ok(Outcome::Success { summary, artifacts });
                }
            }
        }

        // 收尾轮也失败（模型无文本输出 / provider 报错 / 已取消）→ 才报迭代上限。
        let err = AgentError::IterationLimit(task.max_iterations);
        let _ = bus.send(Event::Error {
            id: task_id.clone(),
            err: agent_core::ErrorInfo::from(&err),
        });
        Err(err)
    }

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
        self.deps.approvals.recv_for(approval_id, cancel).await
    }

    /// 执行工具（含外置与超时）。
    async fn execute_tool(
        &self,
        call: &ToolCall,
        task: &TaskSpec,
        cancel: &tokio_util::sync::CancellationToken,
        cm: &mut ContextManager,
    ) -> Result<ToolExecuted, AgentError> {
        // 模型可见性（stream_once 过滤 tool_defs）与可执行性（这里）共用同一规则。
        // 即使模型从别处猜到工具名并硬调用，也过不了这一关。
        if !task.allows_tool(&call.name) {
            return Ok(ToolExecuted {
                content: format!(
                    "[error] 工具 {} 不在本任务允许的工具集内，未执行",
                    call.name
                ),
                is_error: true,
                artifacts: vec![],
                error_code: Some(ToolErrorCode::NotPermitted),
            });
        }

        let Some(tool) = (self.deps.tools)(&call.name) else {
            return Ok(ToolExecuted {
                content: format!("[error] unknown tool: {}", call.name),
                is_error: true,
                artifacts: vec![],
                error_code: Some(ToolErrorCode::UnknownTool),
            });
        };

        let ctx = ToolCtx {
            workspace: std::path::PathBuf::from(&task.workspace),
            project_root: task.project_root.as_ref().map(std::path::PathBuf::from),
            task_id: task.id.0.clone(),
            timeout: if tool.is_interactive() {
                QUESTION_TOOL_TIMEOUT
            } else if call.name == "delegate" {
                DELEGATE_TOOL_TIMEOUT
            } else {
                TOOL_TIMEOUT
            },
            cancel: cancel.clone(),
        };

        // ToolCtx.timeout 必须真正施加：此前只是填了字段，没有任何一处
        // 用它包住 execute，一个卡住的工具（网络挂起 / 子进程不退）能把
        // 整个任务无限挂住。超时按「工具失败」回填，模型能换策略重试。
        let executed =
            tokio::time::timeout(ctx.timeout, tool.execute(call.arguments.clone(), &ctx)).await;
        let out = match executed {
            Ok(r) => r.map_err(AgentError::Tool)?,
            Err(_) => {
                return Ok(ToolExecuted {
                    content: format!("[error] 工具执行超时（{}s），已中止", ctx.timeout.as_secs()),
                    is_error: true,
                    artifacts: vec![],
                    error_code: Some(ToolErrorCode::Timeout),
                })
            }
        };

        let mut arts: Vec<String> = out.side_effects.iter().map(|a| a.path.clone()).collect();

        // 脱敏必须在外置落盘**之前**：外置文件写的是原文的话，明文密钥
        // 会永久留在 ctx/*.txt，而下游只对回填模型的 500 字符摘要做脱敏，
        // 外置文件本身永远不会过脱敏管道。先脱敏，再外置。
        let redacted = (self.deps.redact)(&out.content);

        // 大输出外置。外置文件也要登记为产物：否则模型只有回填文本里
        // 一个 UUID 路径，事后无法追溯属于哪次调用、也拿不到交付物清单。
        let content = if cm.should_externalize(redacted.len()) {
            let ctxdir = std::path::Path::new(&task.workspace).join("ctx");
            let _ = tokio::fs::create_dir_all(&ctxdir).await;
            let fname = format!("{}.txt", uuid::Uuid::new_v4());
            let fpath = ctxdir.join(&fname);
            let _ = tokio::fs::write(&fpath, redacted.as_bytes()).await;
            arts.push(format!("ctx/{fname}"));
            let snippet: String = redacted.chars().take(500).collect();
            format!(
                "[输出已外置到 ctx/{fname}，共 {} 字节] 摘要：{snippet}",
                redacted.len()
            )
        } else {
            redacted.clone()
        };

        let error_code = out.error_code.or(if out.is_error {
            Some(ToolErrorCode::ToolFailed)
        } else {
            None
        });

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
            error_code,
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

    #[test]
    fn plain_greeting_is_exact_not_substring() {
        for s in ["你好", "你好呀！", "HI", "hello~", "在吗", "谢谢"] {
            assert!(AgentRuntime::is_plain_greeting(s), "应识别为寒暄：{s}");
        }
        for s in ["你好，帮我改下 README", "hello world", "在吗？把文件发我"] {
            assert!(!AgentRuntime::is_plain_greeting(s), "不能误判为寒暄：{s}");
        }
    }
    use agent_core::provider::{
        BoxStream, Capabilities, ChatResponse, FinishReason, LlmError, ModelPrice, StreamEvent,
        ToolDef,
    };
    use agent_core::store::Store;
    use agent_core::tool::{ToolError, ToolOutput};
    use futures::stream;
    use std::path::PathBuf;

    /// 把增量快照序列还原成「每份快照落盘时的完整上下文」。
    /// 与 `JsonlStore::resume_context()` 同语义：`reset` 清空重建，否则追加。
    fn snapshot_states(events: &[Event]) -> Vec<Vec<Message>> {
        let mut cur: Vec<Message> = Vec::new();
        let mut out = Vec::new();
        for e in events {
            if let Event::ContextSnapshot { added, reset, .. } = e {
                if *reset {
                    cur.clear();
                }
                cur.extend(added.iter().cloned());
                out.push(cur.clone());
            }
        }
        out
    }

    /// 所有真正进入过上下文的消息（脱敏 / 隔离检查用）。
    /// 只看 `added`，避免快照重建后的重复项干扰断言。
    fn all_snapshot_messages(events: &[Event]) -> Vec<Message> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::ContextSnapshot { added, .. } => Some(added),
                _ => None,
            })
            .flatten()
            .cloned()
            .collect()
    }

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

    /// 给 `ScriptedProvider` 加个单价：算成本要用。
    ///
    /// 包一层而不是改 `ScriptedProvider`：后者被一堆既有用例共用，改默认行为
    /// 会连带影响它们。
    struct PricedProvider {
        inner: ScriptedProvider,
    }

    #[async_trait]
    impl LlmProvider for PricedProvider {
        fn name(&self) -> &str {
            "priced"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
            self.inner.chat(req).await
        }
        async fn chat_stream(
            &self,
            req: ChatRequest,
        ) -> Result<BoxStream<Result<StreamEvent, LlmError>>, LlmError> {
            self.inner.chat_stream(req).await
        }
        fn price(&self, _model: &str) -> Option<ModelPrice> {
            Some(ModelPrice {
                input_per_1k: 0.002,
                output_per_1k: 0.004,
            })
        }
    }

    /// 按模型定价的 provider：只认请求里指定的模型名，别的返回 None。
    /// 记录 price() 收到的模型名，验证执行循环传的是实际请求的模型。
    struct ModelPricedProvider {
        inner: ScriptedProvider,
        seen: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl LlmProvider for ModelPricedProvider {
        fn name(&self) -> &str {
            "model-priced"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
            self.inner.chat(req).await
        }
        async fn chat_stream(
            &self,
            req: ChatRequest,
        ) -> Result<BoxStream<Result<StreamEvent, LlmError>>, LlmError> {
            self.inner.chat_stream(req).await
        }
        fn price(&self, model: &str) -> Option<ModelPrice> {
            self.seen.lock().unwrap().push(model.to_string());
            (model == "premium").then_some(ModelPrice {
                input_per_1k: 0.01,
                output_per_1k: 0.02,
            })
        }
    }

    /// 会记录执行次数的固定输出工具（验证「不该执行时确实没执行」）。
    struct CountingTool {
        hits: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl Tool for CountingTool {
        fn name(&self) -> &str {
            "shell_exec"
        }
        fn description(&self) -> &str {
            "计数工具"
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
            self.hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ToolOutput::ok("ran"))
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

    /// 返回带密钥文本的测试工具：验证 runner 在回填模型与落盘前脱敏。
    struct LeakyTool;

    #[async_trait]
    impl Tool for LeakyTool {
        fn name(&self) -> &str {
            "leaky"
        }
        fn description(&self) -> &str {
            "返回一段带密钥的文本"
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
            _ctx: &ToolCtx,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok("token=sk-leak-value-123456 done"))
        }
    }

    /// 输出超过外置阈值（8KB）的泄漏工具：验证外置文件也经过脱敏。
    struct HugeLeakyTool;

    #[async_trait]
    impl Tool for HugeLeakyTool {
        fn name(&self) -> &str {
            "huge_leaky"
        }
        fn description(&self) -> &str {
            "返回一段超长且带密钥的文本"
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
            _ctx: &ToolCtx,
        ) -> Result<ToolOutput, ToolError> {
            // 9KB 填充 + 末尾的密钥行：外置后密钥只存在于文件里，不在摘要里。
            let filler = "x".repeat(9 * 1024);
            Ok(ToolOutput::ok(format!(
                "{filler}\ntoken=sk-leak-value-123456"
            )))
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
            Decision::AskUser {
                reason: "测试：一律询问".into(),
                source: agent_core::perm::ApprovalSource::User,
                matched_rule: None,
            }
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
            approvals: Arc::new(ApprovalInbox::new(ap_rx)),
            context_cfg: ContextConfig::default(),
            per_task_budget: 0.0,
            cost_sink: None,
            redact: Arc::new(|s: &str| s.to_string()),
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
            approvals: Arc::new(ApprovalInbox::new(ap_rx)),
            context_cfg: ContextConfig::default(),
            per_task_budget: 0.0,
            cost_sink: None,
            redact: Arc::new(|s: &str| s.to_string()),
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

    #[test]
    fn malformed_args_are_built_by_the_typed_helper_not_a_magic_string() {
        let v = MalformedArgs::marker("oops");
        assert!(MalformedArgs::reason(&v) == Some("oops"));
        assert!(v.is_object());
        // 正常参数绝不能被误判
        assert!(MalformedArgs::reason(&serde_json::json!({"cmd": "ls"})).is_none());
    }

    #[tokio::test]
    async fn tool_allowlist_blocks_execution_with_stable_code() {
        let (runtime, _store, _tmp) = make_runtime(vec![], LogConfig::default());
        let mut task = TaskSpec::new("t", "/tmp/ws");
        task.tool_allowlist = Some(vec!["ok_tool".into()]);
        let call = ToolCall {
            id: agent_core::ids::ToolCallId::new(),
            name: "fail_tool".into(),
            arguments: serde_json::json!({}),
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut cm = ContextManager::new(ContextConfig::default(), "sys");
        let r = runtime
            .execute_tool(&call, &task, &cancel, &mut cm)
            .await
            .unwrap();
        assert!(r.is_error);
        assert_eq!(r.error_code, Some(ToolErrorCode::NotPermitted));
    }

    #[tokio::test]
    async fn unknown_tool_has_stable_error_code() {
        let (runtime, _store, _tmp) = make_runtime(vec![], LogConfig::default());
        let task = TaskSpec::new("t", "/tmp/ws");
        let call = ToolCall {
            id: agent_core::ids::ToolCallId::new(),
            name: "no_such_tool".into(),
            arguments: serde_json::json!({}),
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut cm = ContextManager::new(ContextConfig::default(), "sys");
        let r = runtime
            .execute_tool(&call, &task, &cancel, &mut cm)
            .await
            .unwrap();
        assert!(r.is_error);
        assert_eq!(r.error_code, Some(ToolErrorCode::UnknownTool));
        assert!(r.content.starts_with("[error]"));
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

        let states = snapshot_states(&events);
        let snapshots: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::ContextSnapshot {
                    iteration,
                    model,
                    tools,
                    ..
                } => Some((*iteration, model.clone(), tools.clone())),
                _ => None,
            })
            .zip(states)
            .map(|((iteration, model, tools), messages)| (iteration, model, messages, tools))
            .collect();
        // 每轮开头一份 + 收尾把最终答复补齐的一份。
        assert_eq!(snapshots.len(), 3, "每轮增量快照 + 收尾快照");
        // 第 0 轮：system + user
        assert_eq!(snapshots[0].2.len(), 2);
        assert!(
            snapshots[0].2[0].text().contains("个人助理 Agent"),
            "system 固定区必须留存：{}",
            snapshots[0].2[0].text()
        );
        assert!(
            snapshots[0].2[1].text().contains("介绍下本项目"),
            "任务只出现在首轮 user 里：{}",
            snapshots[0].2[1].text()
        );
        // 第 1 轮：system + user + assistant + 2 条工具结果。
        // 预算提醒是 ephemeral，只随本次请求发送、**不写进历史快照**。
        assert_eq!(snapshots[1].2.len(), 5, "工具结果应回填，提醒不进历史");
        assert!(
            !snapshots[1]
                .2
                .iter()
                .any(|m| m.text().contains("[系统提示] 迭代预算只剩")),
            "预算提醒不得污染上下文历史"
        );
        // 第 2 轮（收尾）：把最终答复也补进快照序列。
        assert_eq!(snapshots[2].2.len(), 6);
        assert!(snapshots[2].2[5].text().contains("工作区是空的"));
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
                Event::UserPrompt { .. } => "user_prompt",
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
                "user_prompt",
                "context_snapshot",
                "assistant_turn",
                "tool_call_start",
                "tool_call_end",
                "context_snapshot",
                "assistant_turn",
                "task_finished",
                "context_snapshot",
            ]
        );

        // 首份增量快照就是全量基线：固定 system + 首轮 user。
        let snap = session.events[2].payload.clone();
        match snap {
            Event::ContextSnapshot { added, tools, .. } => {
                assert!(added[0].text().contains("个人助理 Agent"));
                assert!(added[1].text().contains("介绍下本项目"));
                assert_eq!(tools.len(), 2);
            }
            other => panic!("应为 ContextSnapshot: {other:?}"),
        }
        // 工具返回原文落盘
        match &session.events[5].payload {
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
            approvals: Arc::new(ApprovalInbox::new(ap_rx)),
            context_cfg: ContextConfig::default(),
            per_task_budget: 0.0,
            cost_sink: None,
            redact: Arc::new(|s: &str| s.to_string()),
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

    /// 交互工具（ask_user）不能进审批流程：用户已经要回答问题了，
    /// 再弹一次批准框会把两个模态叠在一起，而且审批没人回时任务会永远挂着。
    struct InteractiveTool;

    #[async_trait]
    impl Tool for InteractiveTool {
        fn name(&self) -> &str {
            "ask_mock"
        }
        fn description(&self) -> &str {
            "测试用交互工具"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn risk_level(&self) -> Risk {
            Risk::Low
        }
        fn is_interactive(&self) -> bool {
            true
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCtx,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok("用户选择：A"))
        }
    }

    #[tokio::test]
    async fn interactive_tool_skips_permission_approval() {
        let tmp = std::env::temp_dir().join(format!("agent-rt-ask-{}", uuid::Uuid::new_v4()));
        let ws = tmp.join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let store = Arc::new(JsonlStore::new(&tmp));

        let lookup: ToolLookup = Arc::new(|name: &str| {
            (name == "ask_mock").then(|| Arc::new(InteractiveTool) as Arc<dyn Tool>)
        });
        let (_ap_tx, ap_rx) = mpsc::channel::<ApprovalMsg>(1);
        let runtime = AgentRuntime::new(RuntimeDeps {
            provider: Arc::new(ScriptedProvider {
                turns: Mutex::new(vec![
                    vec![
                        tool_call_args(0, "c1", "ask_mock", "{}"),
                        StreamEvent::Done(agent_core::provider::FinishReason::ToolCalls),
                    ],
                    vec![StreamEvent::Done(agent_core::provider::FinishReason::Stop)],
                ]),
            }),
            provider_name: "scripted".into(),
            tools: lookup,
            tool_defs: vec![],
            // 一律询问：若交互工具没有豁免，这里会卡在等待审批上。
            perm: Arc::new(AskAll),
            store,
            log_cfg: LogConfig::default(),
            approvals: Arc::new(ApprovalInbox::new(ap_rx)),
            context_cfg: ContextConfig::default(),
            per_task_budget: 0.0,
            cost_sink: None,
            redact: Arc::new(|s: &str| s.to_string()),
        });

        let bus = event_bus();
        let mut sub = bus.subscribe();
        let mut spec = TaskSpec::new("问答", ws.display().to_string());
        spec.max_iterations = 3;
        let outcome = tokio::time::timeout(
            Duration::from_secs(3),
            runtime.run(spec, bus, RunControl::new()),
        )
        .await
        .expect("交互工具不应卡在审批等待")
        .unwrap();
        assert!(matches!(outcome, Outcome::Success { .. }));

        while let Ok(ev) = sub.try_recv() {
            assert!(
                !matches!(ev, Event::AwaitingApproval { .. }),
                "交互工具不应触发审批"
            );
        }
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
        // 模型必须知道「被批准的操作长什么样」，以及自己看不到审批。
        assert!(p.contains("[user-approved]"), "提示词应说明批准标记: {p}");
        assert!(
            p.contains("无法得知"),
            "提示词应声明模型看不到审批结果: {p}"
        );
    }

    #[test]
    fn memory_is_injected_into_first_user_not_system() {
        let mut spec = TaskSpec::new("t", "/tmp/ws");
        let fixed = AgentRuntime::system_prompt(&spec);
        assert!(!fixed.contains("【长期记忆】"), "固定 system 不含记忆");
        assert!(!fixed.contains("【用户任务】"), "固定 system 不含本次任务");

        // 记忆怎么变，system 都必须逐字节恒定——这是前缀缓存的前提。
        spec.memory_context = Some(agent_core::memory::MemoryContext::default());
        assert_eq!(AgentRuntime::system_prompt(&spec), fixed);
        spec.memory_context = Some(agent_core::memory::MemoryContext {
            text: "【记忆 · 用户级】端口 7890\n".into(),
            injected_ids: vec![],
            truncated: 0,
        });
        assert_eq!(AgentRuntime::system_prompt(&spec), fixed);

        // 记忆随首轮 user 注入，且可以被回读用于判断是否需要尾部增量。
        let user = ContextAssembler::first_user("t", Some("【记忆 · 用户级】端口 7890\n"));
        let t = user.text();
        assert!(t.contains("【当前任务】t"), "任务只出现一次: {t}");
        assert!(t.contains("【长期记忆】"), "记忆段应在首轮 user: {t}");
        assert!(t.contains("端口 7890"));
        assert_eq!(
            ContextAssembler::extract_memory(std::slice::from_ref(&user)).as_deref(),
            Some("【记忆 · 用户级】端口 7890")
        );
        // 尾部是普通追问时，仍要能回溯到历史里的记忆（不能每次追问重发增量）。
        let history = vec![user, Message::user("再问一句")];
        assert!(
            ContextAssembler::extract_memory(&history)
                .unwrap()
                .contains("端口 7890"),
            "extract_memory 必须倒序遍历所有 user 消息"
        );
    }

    /// 系统级验证「批准本任务同类操作」：按过 `a` 之后，同类调用不再弹窗。
    ///
    /// 跑完整链路——真实引擎 + 两轮同样的工具调用，断言**只发出一次审批请求**。
    /// 只有这条通过，L3 下才真的不会被同一个操作反复打扰。
    ///
    /// 它同时锁住一个实测抓到的竞态：早期实现在审批通过后走同步回调、
    /// 回调内部再 `tokio::spawn` 去登记，于是下一次同类调用可能在登记落定前
    /// 就查过了——用户按了 `a` 却还被再问一次。
    #[tokio::test]
    async fn batch_approval_silences_repeat_calls_end_to_end() {
        let tmp = std::env::temp_dir().join(format!("agent-runner-batch-{}", std::process::id()));
        let ws = tmp.join("ws");
        tokio::fs::create_dir_all(&ws).await.unwrap();
        let store = Arc::new(JsonlStore::new(&tmp));

        // 真实权限引擎（L3 档），不是「一律询问」的替身——否则批量批准无从生效。
        let engine: Arc<agent_perm::PermissionEngineImpl> =
            Arc::new(agent_perm::PermissionEngineImpl::new(
                agent_perm::rules::StaticRules::new(),
                agent_perm::PermConfig::default(),
            ));
        let (ap_tx, ap_rx) = mpsc::channel::<ApprovalMsg>(8);

        let lookup: ToolLookup = Arc::new(|_| None);
        let runtime = AgentRuntime::new(RuntimeDeps {
            provider: Arc::new(ScriptedProvider {
                // 两轮都调用同一个工具：第二轮若还弹窗就是回归。
                turns: Mutex::new(vec![
                    vec![
                        tool_call_args(0, "c1", "ok_tool", "{}"),
                        StreamEvent::Done(agent_core::provider::FinishReason::ToolCalls),
                    ],
                    vec![
                        tool_call_args(0, "c2", "ok_tool", "{}"),
                        StreamEvent::Done(agent_core::provider::FinishReason::ToolCalls),
                    ],
                ]),
            }),
            provider_name: "scripted".into(),
            tools: lookup,
            tool_defs: vec![],
            perm: engine,
            store,
            log_cfg: LogConfig::default(),
            approvals: Arc::new(ApprovalInbox::new(ap_rx)),
            context_cfg: ContextConfig::default(),
            per_task_budget: 0.0,
            cost_sink: None,
            redact: Arc::new(|s: &str| s.to_string()),
        });

        let bus = event_bus();
        let mut sub = bus.subscribe();
        // 数一数到底弹了几次窗。
        let approvals_seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen_for_watcher = approvals_seen.clone();
        // 模拟用户每次都按「批准并记住同类」。
        tokio::spawn(async move {
            while let Ok(ev) = sub.recv().await {
                if let Event::AwaitingApproval { req, .. } = ev {
                    seen_for_watcher.lock().unwrap().push(req.tool.clone());
                    let _ = ap_tx
                        .send((req.id, ApprovalReply::Approve { remember: true }))
                        .await;
                }
            }
        });

        let mut spec = TaskSpec::new("批量批准链路", ws.display().to_string());
        spec.tier = agent_core::perm::PermissionTier::L3;
        spec.max_iterations = 6;
        let _ = runtime.run(spec, bus, RunControl::new()).await;

        let asked = approvals_seen.lock().unwrap().len();
        assert_eq!(
            asked, 1,
            "同类工具应只询问一次（登记后放行），实际询问 {asked} 次"
        );
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    /// 成本必须真的上报给账本。
    ///
    /// 回归：执行循环是流式直连 provider 的，不经过 `ProviderRouter`，月度用量
    /// 一度**永远累计为 0**——配置里的月度预算形同虚设。此处断言每轮调用都会
    /// 通过 `cost_sink` 把金额报出去（金额由 provider 单价 × usage 算出）。
    #[tokio::test]
    async fn every_turn_reports_cost_to_the_sink() {
        let tmp = std::env::temp_dir().join(format!("agent-runner-cost-{}", std::process::id()));
        let ws = tmp.join("ws");
        tokio::fs::create_dir_all(&ws).await.unwrap();
        let store = Arc::new(JsonlStore::new(&tmp));

        let reported = Arc::new(std::sync::Mutex::new(Vec::<f64>::new()));
        let sink = reported.clone();
        let (ap_tx, ap_rx) = mpsc::channel::<ApprovalMsg>(1);

        let deps = RuntimeDeps {
            provider: Arc::new(PricedProvider {
                inner: ScriptedProvider {
                    turns: Mutex::new(vec![vec![
                        StreamEvent::Usage(Usage {
                            input_tokens: 1_000,
                            output_tokens: 100,
                            reasoning_tokens: None,
                            cached_tokens: None,
                        }),
                        StreamEvent::Done(agent_core::provider::FinishReason::Stop),
                    ]]),
                },
            }),
            provider_name: "priced".into(),
            tools: Arc::new(|_| None),
            tool_defs: vec![],
            perm: Arc::new(AskAll),
            store,
            log_cfg: LogConfig::default(),
            approvals: Arc::new(ApprovalInbox::new(ap_rx)),
            context_cfg: ContextConfig::default(),
            per_task_budget: 0.0,
            cost_sink: Some(Arc::new(move |cost: f64| {
                sink.lock().unwrap().push(cost);
            })),
            redact: Arc::new(|s: &str| s.to_string()),
        };
        // 让 provider 有单价，否则成本算不出来。
        let runtime = AgentRuntime::new(deps);
        let bus = event_bus();
        let _ = runtime
            .run(
                TaskSpec::new("记账", ws.display().to_string()),
                bus,
                RunControl::new(),
            )
            .await;

        let costs = reported.lock().unwrap().clone();
        assert_eq!(costs.len(), 1, "每轮应上报一次成本，实际 {costs:?}");
        assert!(costs[0] > 0.0, "usage 非零时成本应 > 0，实际 {costs:?}");
        drop(ap_tx);
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    /// 定价必须按**本轮实际请求的模型**查：指定了 premium 模型却传空串，
    /// 按模型定价的 provider 会返回 None，成本静默不计、月度熔断失效。
    #[tokio::test]
    async fn cost_uses_the_requested_model_name() {
        let tmp =
            std::env::temp_dir().join(format!("agent-runner-model-price-{}", std::process::id()));
        let ws = tmp.join("ws");
        tokio::fs::create_dir_all(&ws).await.unwrap();
        let store = Arc::new(JsonlStore::new(&tmp));

        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let reported = Arc::new(std::sync::Mutex::new(Vec::<f64>::new()));
        let sink = reported.clone();
        let (_ap_tx, ap_rx) = mpsc::channel::<ApprovalMsg>(1);

        let mut spec = TaskSpec::new("模型计价", ws.display().to_string());
        spec.model = Some("premium".into());
        spec.max_iterations = 2;

        let runtime = AgentRuntime::new(RuntimeDeps {
            provider: Arc::new(ModelPricedProvider {
                inner: ScriptedProvider {
                    turns: Mutex::new(vec![vec![
                        StreamEvent::Usage(Usage {
                            input_tokens: 1_000,
                            output_tokens: 100,
                            reasoning_tokens: None,
                            cached_tokens: None,
                        }),
                        StreamEvent::Done(agent_core::provider::FinishReason::Stop),
                    ]]),
                },
                seen: seen.clone(),
            }),
            provider_name: "model-priced".into(),
            tools: Arc::new(|_| None),
            tool_defs: vec![],
            perm: Arc::new(AskAll),
            store,
            log_cfg: LogConfig::default(),
            approvals: Arc::new(ApprovalInbox::new(ap_rx)),
            context_cfg: ContextConfig::default(),
            per_task_budget: 0.0,
            cost_sink: Some(Arc::new(move |c: f64| {
                sink.lock().unwrap().push(c);
            })),
            redact: Arc::new(|s: &str| s.to_string()),
        });

        let _ = runtime.run(spec, event_bus(), RunControl::new()).await;

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["premium"],
            "price() 应收到请求里的模型名"
        );
        let costs = reported.lock().unwrap().clone();
        assert_eq!(costs.len(), 1, "成本必须计入账本: {costs:?}");
        assert!(costs[0] > 0.0, "{costs:?}");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    /// 模型发来的工具参数是截断/非法 JSON 时：工具不得执行，模型必须收到
    /// 明确的错误结果（而不是把 Null 当「没传参数」闷头跑一遍）。
    #[tokio::test]
    async fn malformed_tool_args_are_reported_not_silently_nulled() {
        let tmp = std::env::temp_dir().join(format!("agent-runner-badargs-{}", std::process::id()));
        let ws = tmp.join("ws");
        tokio::fs::create_dir_all(&ws).await.unwrap();
        let store = Arc::new(JsonlStore::new(&tmp));
        let (_ap_tx, ap_rx) = mpsc::channel::<ApprovalMsg>(1);

        let executed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let executed_for_tool = executed.clone();

        let runtime = AgentRuntime::new(RuntimeDeps {
            provider: Arc::new(ScriptedProvider {
                turns: Mutex::new(vec![
                    vec![
                        // 截断的 JSON：{"cmd": "ls
                        tool_call_args(0, "c1", "shell_exec", "{\"cmd\": \"ls"),
                        StreamEvent::Done(agent_core::provider::FinishReason::ToolCalls),
                    ],
                    vec![
                        StreamEvent::TextDelta("好".into()),
                        StreamEvent::Done(agent_core::provider::FinishReason::Stop),
                    ],
                ]),
            }),
            provider_name: "scripted".into(),
            tools: Arc::new(move |name: &str| {
                if name != "shell_exec" {
                    return None;
                }
                Some(Arc::new(CountingTool {
                    hits: executed_for_tool.clone(),
                }) as Arc<dyn Tool>)
            }),
            tool_defs: vec![ToolDef {
                name: "shell_exec".into(),
                description: "shell".into(),
                parameters: serde_json::json!({ "type": "object" }),
            }],
            perm: Arc::new(AllowAll),
            store,
            log_cfg: LogConfig::default(),
            approvals: Arc::new(ApprovalInbox::new(ap_rx)),
            context_cfg: ContextConfig::default(),
            per_task_budget: 0.0,
            cost_sink: None,
            redact: Arc::new(|s: &str| s.to_string()),
        });

        let bus = event_bus();
        let mut sub = bus.subscribe();
        let mut spec = TaskSpec::new("坏参数", ws.display().to_string());
        spec.max_iterations = 3;
        let _ = runtime.run(spec, bus, RunControl::new()).await;

        let mut events = Vec::new();
        while let Ok(ev) = sub.try_recv() {
            events.push(ev);
        }
        // 工具返回里必须是明确的「参数不是合法 JSON」，不能是 Null 正常执行。
        let tool_out = events
            .iter()
            .find_map(|e| match e {
                Event::ToolCallEnd { output, .. } => output.clone(),
                _ => None,
            })
            .expect("应有工具结束事件");
        assert!(
            tool_out.contains("不是合法 JSON"),
            "应明确报参数错误: {tool_out}"
        );
        assert_eq!(
            executed.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "坏参数不应执行工具"
        );

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    /// 应答顺序与等待顺序错配时：错配应答必须暂存，等对应任务来取——
    /// 直接丢弃会让那个任务永远等下去（多任务共用一个通道时的真实竞态）。
    #[tokio::test]
    async fn mismatched_approval_reply_is_buffered_not_lost() {
        let (tx, rx) = mpsc::channel::<ApprovalMsg>(4);
        let inbox = ApprovalInbox::new(rx);
        let a = ApprovalId("approval-a".into());
        let b = ApprovalId("approval-b".into());

        // B 的应答先到，而 A 在等：A 不能把 B 的应答当成自己的，也不能丢掉。
        tx.send((b.clone(), ApprovalReply::Approve { remember: false }))
            .await
            .unwrap();
        tx.send((
            a.clone(),
            ApprovalReply::Reject {
                reason: "no".into(),
            },
        ))
        .await
        .unwrap();

        let cancel = tokio_util::sync::CancellationToken::new();
        let ra = inbox.recv_for(&a, &cancel).await.expect("A 应拿到拒绝");
        assert!(matches!(ra, ApprovalReply::Reject { .. }), "{ra:?}");
        let rb = inbox.recv_for(&b, &cancel).await.expect("B 的应答应已暂存");
        assert!(matches!(rb, ApprovalReply::Approve { .. }), "{rb:?}");
    }

    /// 工具输出里的密钥必须在**回填模型之前**与**落盘之前**被替换。
    ///
    /// 回归：`env | grep key` 的完整 key 曾同时进入模型上下文与 jsonl（18 处）。
    #[tokio::test]
    async fn tool_output_is_redacted_before_model_and_log() {
        const SECRET: &str = "sk-leak-value-123456";
        let tmp =
            std::env::temp_dir().join(format!("agent-runner-redact-{}", uuid::Uuid::new_v4()));
        let ws = tmp.join("ws");
        tokio::fs::create_dir_all(&ws).await.unwrap();
        let store = Arc::new(JsonlStore::new(&tmp));
        let (_ap_tx, ap_rx) = mpsc::channel::<ApprovalMsg>(1);

        let defs = vec![ToolDef {
            name: "leaky".into(),
            description: "leaky".into(),
            parameters: serde_json::json!({ "type": "object" }),
        }];
        let runtime = AgentRuntime::new(RuntimeDeps {
            provider: Arc::new(ScriptedProvider {
                turns: Mutex::new(vec![
                    vec![
                        tool_call(0, "c1", "leaky"),
                        StreamEvent::Done(agent_core::provider::FinishReason::ToolCalls),
                    ],
                    vec![
                        StreamEvent::TextDelta("完成".into()),
                        StreamEvent::Done(agent_core::provider::FinishReason::Stop),
                    ],
                ]),
            }),
            provider_name: "scripted".into(),
            tools: Arc::new(|name: &str| {
                (name == "leaky").then(|| Arc::new(LeakyTool) as Arc<dyn Tool>)
            }),
            tool_defs: defs,
            perm: Arc::new(AllowAll),
            store,
            log_cfg: LogConfig::default(),
            approvals: Arc::new(ApprovalInbox::new(ap_rx)),
            context_cfg: ContextConfig::default(),
            per_task_budget: 0.0,
            cost_sink: None,
            redact: Arc::new(|s: &str| s.replace(SECRET, "<redacted>")),
        });

        let bus = event_bus();
        let mut sub = bus.subscribe();
        let mut spec = TaskSpec::new("脱敏", ws.display().to_string());
        spec.max_iterations = 3;
        let _ = runtime.run(spec, bus, RunControl::new()).await;

        let mut events = Vec::new();
        while let Ok(ev) = sub.try_recv() {
            events.push(ev);
        }

        let output = events
            .iter()
            .find_map(|e| match e {
                Event::ToolCallEnd { output, .. } => output.clone(),
                _ => None,
            })
            .expect("应有工具返回");
        assert!(output.contains("<redacted>"), "返回应被替换: {output}");
        assert!(!output.contains(SECRET), "落盘原文里不能有密钥: {output}");

        let model_saw: String = all_snapshot_messages(&events)
            .iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| m.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            model_saw.contains("<redacted>"),
            "模型上下文应已脱敏: {model_saw}"
        );
        assert!(
            !model_saw.contains(SECRET),
            "模型上下文里不能有密钥: {model_saw}"
        );

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    /// 外置到 ctx/*.txt 的大输出也必须先脱敏再落盘。
    ///
    /// 回归：外置发生在脱敏之前，超阈值输出把明文密钥写进外置文件，
    /// 而下游只对回填模型的摘要脱敏——文件里的密钥永不清理。
    #[tokio::test]
    async fn externalized_output_is_redacted_before_disk_write() {
        const SECRET: &str = "sk-leak-value-123456";
        let tmp =
            std::env::temp_dir().join(format!("agent-runner-redact-ext-{}", uuid::Uuid::new_v4()));
        let ws = tmp.join("ws");
        tokio::fs::create_dir_all(&ws).await.unwrap();
        let store = Arc::new(JsonlStore::new(&tmp));
        let (_ap_tx, ap_rx) = mpsc::channel::<ApprovalMsg>(1);

        let defs = vec![ToolDef {
            name: "huge_leaky".into(),
            description: "huge_leaky".into(),
            parameters: serde_json::json!({ "type": "object" }),
        }];
        let runtime = AgentRuntime::new(RuntimeDeps {
            provider: Arc::new(ScriptedProvider {
                turns: Mutex::new(vec![
                    vec![
                        tool_call(0, "c1", "huge_leaky"),
                        StreamEvent::Done(agent_core::provider::FinishReason::ToolCalls),
                    ],
                    vec![
                        StreamEvent::TextDelta("完成".into()),
                        StreamEvent::Done(agent_core::provider::FinishReason::Stop),
                    ],
                ]),
            }),
            provider_name: "scripted".into(),
            tools: Arc::new(|name: &str| {
                (name == "huge_leaky").then(|| Arc::new(HugeLeakyTool) as Arc<dyn Tool>)
            }),
            tool_defs: defs,
            perm: Arc::new(AllowAll),
            store,
            log_cfg: LogConfig::default(),
            approvals: Arc::new(ApprovalInbox::new(ap_rx)),
            context_cfg: ContextConfig::default(),
            per_task_budget: 0.0,
            cost_sink: None,
            redact: Arc::new(|s: &str| s.replace(SECRET, "<redacted>")),
        });

        let bus = event_bus();
        let mut spec = TaskSpec::new("外置脱敏", ws.display().to_string());
        spec.max_iterations = 3;
        let _ = runtime.run(spec, bus, RunControl::new()).await;

        // 外置目录里必须有文件，且任何文件都不含明文密钥。
        let ctxdir = ws.join("ctx");
        let mut found_any = false;
        let mut rd = tokio::fs::read_dir(&ctxdir).await.expect("ctx 目录应存在");
        while let Some(entry) = rd.next_entry().await.unwrap() {
            if entry.path().extension().and_then(|e| e.to_str()) != Some("txt") {
                continue;
            }
            found_any = true;
            let body = tokio::fs::read_to_string(entry.path()).await.unwrap();
            assert!(
                !body.contains(SECRET),
                "外置文件 {} 里有明文密钥",
                entry.path().display()
            );
        }
        assert!(found_any, "应有外置文件产生");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    /// 审批必须留下「请求 → 决定 → 结果」三件事，且模型能看到「被批准过」。
    ///
    /// 回归：只记 `AwaitingApproval` 时，事后无法区分人工放行与自动放行；
    /// 模型也看不到批准标记，于是把「用户批准过」误判成「权限系统没设防」。
    #[tokio::test]
    async fn approval_is_observable_in_log_and_to_model() {
        let tmp =
            std::env::temp_dir().join(format!("agent-runner-approval-{}", uuid::Uuid::new_v4()));
        let ws = tmp.join("ws");
        tokio::fs::create_dir_all(&ws).await.unwrap();
        let store = Arc::new(JsonlStore::new(&tmp));
        let (ap_tx, ap_rx) = mpsc::channel::<ApprovalMsg>(4);

        let defs = vec![ToolDef {
            name: "ok_tool".into(),
            description: "ok".into(),
            parameters: serde_json::json!({ "type": "object" }),
        }];
        let runtime = AgentRuntime::new(RuntimeDeps {
            provider: Arc::new(ScriptedProvider {
                turns: Mutex::new(vec![
                    vec![
                        tool_call(0, "c1", "ok_tool"),
                        StreamEvent::Done(agent_core::provider::FinishReason::ToolCalls),
                    ],
                    vec![
                        StreamEvent::TextDelta("完成".into()),
                        StreamEvent::Done(agent_core::provider::FinishReason::Stop),
                    ],
                ]),
            }),
            provider_name: "scripted".into(),
            tools: Arc::new(|name: &str| {
                (name == "ok_tool").then(|| {
                    Arc::new(FixedTool {
                        name: "ok_tool",
                        fail: false,
                    }) as Arc<dyn Tool>
                })
            }),
            tool_defs: defs,
            perm: Arc::new(AskAll),
            store,
            log_cfg: LogConfig::default(),
            approvals: Arc::new(ApprovalInbox::new(ap_rx)),
            context_cfg: ContextConfig::default(),
            per_task_budget: 0.0,
            cost_sink: None,
            redact: Arc::new(|s: &str| s.to_string()),
        });

        let bus = event_bus();
        let mut watcher = bus.subscribe();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<
            agent_core::perm::ApprovalRequest,
        >::new()));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            while let Ok(ev) = watcher.recv().await {
                if let Event::AwaitingApproval { req, .. } = ev {
                    seen2.lock().unwrap().push(req.clone());
                    let _ = ap_tx
                        .send((req.id, ApprovalReply::Approve { remember: false }))
                        .await;
                }
            }
        });
        tokio::task::yield_now().await;

        let mut sub = bus.subscribe();
        let mut spec = TaskSpec::new("审批可观测", ws.display().to_string());
        spec.max_iterations = 3;
        let _ = runtime.run(spec, bus, RunControl::new()).await;

        let mut events = Vec::new();
        while let Ok(ev) = sub.try_recv() {
            events.push(ev);
        }

        // 审批请求要带真实原因与来源（不再是固定文案 + null）。
        let req = seen.lock().unwrap().first().cloned().expect("应有一次审批");
        assert!(!req.reason.is_empty(), "审批原因不能为空");
        assert_eq!(
            req.source,
            Some(agent_core::perm::ApprovalSource::User),
            "L3/策略询问的来源应标为 User"
        );

        // 事件顺序：请求 → 决定 → 结果。
        let ask_at = events
            .iter()
            .position(|e| matches!(e, Event::AwaitingApproval { .. }))
            .expect("缺少审批请求事件");
        let decided_at = events
            .iter()
            .position(|e| matches!(e, Event::ApprovalDecided { .. }))
            .expect("缺少审批决定事件");
        let end_at = events
            .iter()
            .position(|e| matches!(e, Event::ToolCallEnd { .. }))
            .expect("缺少工具结束事件");
        assert!(
            ask_at < decided_at && decided_at < end_at,
            "顺序应为 请求 → 决定 → 结果"
        );
        match &events[decided_at] {
            Event::ApprovalDecided { decision, .. } => {
                assert_eq!(*decision, ApprovalDecision::Approved { remember: false })
            }
            other => panic!("应为 ApprovalDecided，实际 {other:?}"),
        }

        // 模型侧：工具消息带 [user-approved]。
        let model_tool: Vec<String> = all_snapshot_messages(&events)
            .iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| m.text())
            .collect();
        assert!(
            model_tool.iter().any(|t| t.starts_with("[user-approved]")),
            "模型应看到批准标记: {model_tool:?}"
        );

        // 日志侧：ToolCallEnd 保留工具原文（不加标记）。
        let logged = events
            .iter()
            .find_map(|e| match e {
                Event::ToolCallEnd { output, .. } => output.clone(),
                _ => None,
            })
            .expect("工具应有返回");
        assert!(
            !logged.starts_with("[user-approved]"),
            "日志不该被模型标记污染: {logged}"
        );

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // ---- M4：任务摘要（`/tasks` 的数据源）与续跑 ----

    #[tokio::test]
    async fn task_summary_is_recorded_on_start_and_finish() {
        // 回归：`tasks/<id>.json` 曾因为没有任何生产路径调用 `upsert_task`
        // 而永远是空的——`/tasks` 打开就是一片空白。
        let (runtime, store, tmp) = make_runtime(
            vec![vec![StreamEvent::Done(
                agent_core::provider::FinishReason::Stop,
            )]],
            LogConfig::default(),
        );
        let mut spec = TaskSpec::new("写个统计脚本", tmp.join("workspace").display().to_string());
        let id = spec.id.clone();
        spec.max_iterations = 2;

        runtime
            .run(spec, event_bus(), RunControl::new())
            .await
            .unwrap();

        let summary = store.task_summary(&id).await.expect("任务摘要必须落盘");
        assert_eq!(summary.title, "写个统计脚本");
        assert_eq!(
            summary.state,
            TaskState::Done,
            "跑完应落终态，而不是一直挂在 Running"
        );
        let listed = store.recent_tasks(10).await;
        assert_eq!(listed.len(), 1, "列表要能查到这条任务");
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    /// 记录每次请求发给模型的消息，用于断言「续跑到底带了什么」。
    struct RecordingProvider {
        seen: Mutex<Vec<Vec<Message>>>,
    }

    #[async_trait]
    impl LlmProvider for RecordingProvider {
        fn name(&self) -> &str {
            "recording"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        async fn chat(&self, _req: ChatRequest) -> Result<ChatResponse, LlmError> {
            Err(LlmError::InvalidRequest("tests use chat_stream".into()))
        }
        async fn chat_stream(
            &self,
            req: ChatRequest,
        ) -> Result<BoxStream<Result<StreamEvent, LlmError>>, LlmError> {
            self.seen.lock().await.push(req.messages.clone());
            Ok(Box::pin(stream::iter(vec![Ok(StreamEvent::Done(
                agent_core::provider::FinishReason::Stop,
            ))])))
        }
        fn price(&self, _model: &str) -> Option<ModelPrice> {
            None
        }
    }

    #[tokio::test]
    async fn resume_sends_history_and_then_the_new_instruction() {
        let tmp = std::env::temp_dir().join(format!("agent-rt-resume-{}", uuid::Uuid::new_v4()));
        let ws = tmp.join("workspace");
        tokio::fs::create_dir_all(&ws).await.unwrap();
        let store = Arc::new(JsonlStore::new(&tmp));
        let provider = Arc::new(RecordingProvider {
            seen: Mutex::new(Vec::new()),
        });
        let (_ap_tx, ap_rx) = mpsc::channel::<ApprovalMsg>(1);

        let runtime = AgentRuntime::new(RuntimeDeps {
            provider: provider.clone(),
            provider_name: "recording".into(),
            tools: Arc::new(|_| None),
            tool_defs: vec![],
            perm: Arc::new(AllowAll),
            store: store.clone(),
            log_cfg: LogConfig::default(),
            approvals: Arc::new(ApprovalInbox::new(ap_rx)),
            context_cfg: ContextConfig::default(),
            per_task_budget: 0.0,
            cost_sink: None,
            redact: Arc::new(|s: &str| s.to_string()),
        });

        let mut spec = TaskSpec::new("再把结果写进 README", ws.display().to_string());
        spec.resume_messages = Some(vec![
            Message::system("旧的 system"),
            Message::user("统计文件数"),
            Message::assistant("共 3 个文件。"),
        ]);
        spec.max_iterations = 2;
        runtime
            .run(spec, event_bus(), RunControl::new())
            .await
            .unwrap();

        let seen = provider.seen.lock().await;
        let text: Vec<String> = seen[0].iter().map(|m| m.text()).collect();
        assert!(
            text.iter().any(|t| t == "统计文件数"),
            "历史消息必须带上，否则「恢复」等于新开一个任务：{text:?}"
        );
        assert!(
            text.iter().any(|t| t == "共 3 个文件。"),
            "快照之后的答复也要带上（模型得记得自己刚说过什么）：{text:?}"
        );
        // 续跑沿用历史里的固定 system，不再重建第二条。
        assert_eq!(
            seen[0].iter().filter(|m| m.role == Role::System).count(),
            1,
            "v1.2：Role::System 恒 1 条"
        );
        assert_eq!(text[0], "旧的 system", "要沿用历史固定区而不是重建");
        // 预算提醒是 ephemeral、只挂在请求尾部；排除它之后新指令仍在历史末尾。
        let last_historical = text
            .iter()
            .rfind(|t| !t.starts_with("[系统提示]"))
            .map(String::as_str);
        assert_eq!(
            last_historical,
            Some("再把结果写进 README"),
            "新的指令必须补在历史最后——只接历史而不接新指令，模型会把上一轮再答一遍：{text:?}"
        );
        assert!(
            text.iter().filter(|t| t.starts_with("[系统提示]")).count() <= 1,
            "预算提醒最多 1 条且不累积"
        );
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    /// 委派闭环：父上下文里只有一条工具结果，子任务写的文件父任务能读。
    #[tokio::test]
    async fn delegate_end_to_end_shares_workspace_and_isolates_context() {
        use agent_core::subagent::SubagentPort;
        use agent_core::tool::Tool;
        use agent_tools::DelegateTool;

        /// 一个「真的往共享项目根写文件」的假 port：
        /// 绕开子 Runner，直接验证 DelegateTool → 回填 → 父 file_read 的链路。
        struct WritingPort {
            project_root: PathBuf,
        }
        #[async_trait]
        impl SubagentPort for WritingPort {
            async fn run_subtask(
                &self,
                _parent: &str,
                spec: agent_core::subagent::SubtaskSpec,
                _cancel: tokio_util::sync::CancellationToken,
            ) -> Result<agent_core::subagent::SubtaskResult, agent_core::error::AgentError>
            {
                tokio::fs::create_dir_all(self.project_root.join("artifacts"))
                    .await
                    .ok();
                tokio::fs::write(self.project_root.join("artifacts/child.md"), &spec.goal)
                    .await
                    .ok();
                Ok(agent_core::subagent::SubtaskResult {
                    child_id: agent_core::ids::TaskId::from_raw("child-1"),
                    outcome: Outcome::Success {
                        summary: "子任务内部过程，不应出现在父上下文".into(),
                        artifacts: vec!["artifacts/child.md".into()],
                    },
                    summary: "结论：方案 A 更省 token".into(),
                    artifacts: vec!["artifacts/child.md".into()],
                    cost_usd: 0.0,
                })
            }
        }

        let tmp = std::env::temp_dir().join(format!("zero-delegate-e2e-{}", uuid::Uuid::new_v4()));
        let proj = tmp.join("proj");
        let ws = tmp.join("workspace");
        tokio::fs::create_dir_all(&proj).await.unwrap();
        tokio::fs::create_dir_all(&ws).await.unwrap();
        let store = Arc::new(JsonlStore::new(&tmp));

        let port = Arc::new(WritingPort {
            project_root: proj.clone(),
        });
        let delegate = Arc::new(DelegateTool::new(port.clone())) as Arc<dyn Tool>;
        let file_read = Arc::new(agent_tools::fs_tools::FileRead) as Arc<dyn Tool>;
        let finish = Arc::new(agent_tools::finish::Finish) as Arc<dyn Tool>;

        let defs = [delegate.clone(), file_read.clone(), finish.clone()]
            .iter()
            .map(|t| agent_core::provider::ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect::<Vec<_>>();
        let d2 = delegate.clone();
        let lookup: ToolLookup = Arc::new(move |name: &str| match name {
            "delegate" => Some(d2.clone()),
            "file_read" => Some(Arc::new(agent_tools::fs_tools::FileRead) as Arc<dyn Tool>),
            "finish" => Some(Arc::new(agent_tools::finish::Finish) as Arc<dyn Tool>),
            _ => None,
        });
        let (_tx, rx) = mpsc::channel::<ApprovalMsg>(1);

        // 剧本：第 1 轮委派；第 2 轮读回子任务写的文件；第 3 轮收尾。
        let turns = vec![
            vec![tool_call_args(
                0,
                "c1",
                "delegate",
                r#"{"goal":"写一份草稿"}"#,
            )],
            vec![tool_call_args(
                0,
                "c2",
                "file_read",
                r#"{"path":"artifacts/child.md"}"#,
            )],
            vec![
                StreamEvent::TextDelta("全部完成".into()),
                StreamEvent::Done(FinishReason::Stop),
            ],
        ];

        let runtime = AgentRuntime::new(RuntimeDeps {
            provider: Arc::new(ScriptedProvider {
                turns: Mutex::new(turns),
            }),
            provider_name: "scripted".into(),
            tools: lookup,
            tool_defs: defs,
            perm: Arc::new(AllowAll),
            store,
            log_cfg: LogConfig::default(),
            approvals: Arc::new(ApprovalInbox::new(rx)),
            context_cfg: ContextConfig::default(),
            per_task_budget: 0.0,
            cost_sink: None,
            redact: Arc::new(|s: &str| s.to_string()),
        });

        let mut spec = TaskSpec::new("主任务", ws.display().to_string());
        spec.project_root = Some(proj.display().to_string());
        spec.max_iterations = 5;
        let bus = event_bus();
        let mut sub = bus.subscribe();
        let outcome = runtime.run(spec, bus, RunControl::new()).await.unwrap();
        assert!(
            matches!(outcome, Outcome::Success { .. }),
            "实际: {outcome:?}"
        );

        // 共享空间：父任务能读到子任务写出的文件。
        let written = tokio::fs::read_to_string(proj.join("artifacts/child.md"))
            .await
            .unwrap();
        assert_eq!(written, "写一份草稿");

        // 上下文隔离：快照里不得出现「子任务内部过程」这段文本。
        let mut leaked = false;
        while let Ok(ev) = sub.try_recv() {
            if let Event::ContextSnapshot { added, .. } = ev {
                for m in added {
                    if m.text().contains("子任务内部过程，不应出现在父上下文") {
                        leaked = true;
                    }
                }
            }
        }
        assert!(!leaked, "子任务内部 summary 泄漏进了父上下文");
        assert!(tokio::fs::read_to_string(proj.join("artifacts/child.md"))
            .await
            .is_ok());
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    /// R2 回归：迭代预算耗尽不再直接报 iteration_limit——追加的强制收尾轮
    /// 应把已收集信息汇总成「部分完成」结论返回，而不是让 25 轮探查全部作废。
    #[tokio::test]
    async fn iteration_limit_degrades_to_partial_summary_instead_of_error() {
        let (runtime, _store, tmp) = make_runtime(
            vec![
                // 预算内两轮都在调工具（模拟截图里「只探查不出结论」的场景）。
                vec![
                    StreamEvent::TextDelta("先看看环境".into()),
                    tool_call(0, "c1", "ok_tool"),
                    StreamEvent::Done(agent_core::provider::FinishReason::ToolCalls),
                ],
                vec![
                    StreamEvent::TextDelta("再看点别的".into()),
                    tool_call(0, "c2", "ok_tool"),
                    StreamEvent::Done(agent_core::provider::FinishReason::ToolCalls),
                ],
                // 强制收尾轮：模型基于已收集信息给出阶段结论。
                vec![
                    StreamEvent::TextDelta("已确认项目结构，结论如下……".into()),
                    StreamEvent::Done(agent_core::provider::FinishReason::Stop),
                ],
            ],
            LogConfig::default(),
        );
        let bus = event_bus();
        let mut spec = TaskSpec::new("调查任务", tmp.join("workspace").display().to_string());
        spec.max_iterations = 2;
        let outcome = runtime.run(spec, bus, RunControl::new()).await;

        match outcome.unwrap() {
            Outcome::Success { summary, .. } => {
                assert!(
                    summary.contains("【部分完成 · 迭代预算 2 轮已耗尽】"),
                    "应标注部分完成：{summary}"
                );
                assert!(
                    summary.contains("已确认项目结构"),
                    "收尾轮的结论应进入 summary：{summary}"
                );
            }
            other => panic!("预算耗尽应降级为部分完成，而非失败：{other:?}"),
        }
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    /// R1 回归：剩余轮数 ≤3 时在**请求尾部**附加收敛提醒，
    /// 且它是 ephemeral 的——绝不写进上下文历史 / 快照 / 续跑基线。
    #[tokio::test]
    async fn budget_warning_is_injected_once_near_iteration_limit() {
        let (runtime, _store, tmp) = make_runtime(
            vec![
                vec![
                    tool_call(0, "c1", "ok_tool"),
                    StreamEvent::Done(agent_core::provider::FinishReason::ToolCalls),
                ],
                vec![
                    tool_call(0, "c2", "ok_tool"),
                    StreamEvent::Done(agent_core::provider::FinishReason::ToolCalls),
                ],
                vec![
                    tool_call(0, "c3", "ok_tool"),
                    StreamEvent::Done(agent_core::provider::FinishReason::ToolCalls),
                ],
                vec![
                    StreamEvent::TextDelta("信息足够，直接作答。".into()),
                    StreamEvent::Done(agent_core::provider::FinishReason::Stop),
                ],
            ],
            LogConfig::default(),
        );
        let bus = event_bus();
        let mut sub = bus.subscribe();
        let mut spec = TaskSpec::new("探查任务", tmp.join("workspace").display().to_string());
        spec.max_iterations = 4;
        let outcome = runtime.run(spec, bus, RunControl::new()).await.unwrap();
        assert!(matches!(outcome, Outcome::Success { .. }));

        let mut per_snapshot: Vec<(u32, usize)> = Vec::new();
        while let Ok(ev) = sub.try_recv() {
            if let Event::ContextSnapshot {
                iteration, added, ..
            } = ev
            {
                let n = added
                    .iter()
                    .filter(|m| m.text().contains("[系统提示] 迭代预算只剩"))
                    .count();
                per_snapshot.push((iteration, n));
            }
        }
        per_snapshot.sort_by_key(|(i, _)| *i);
        assert!(!per_snapshot.is_empty(), "应有多轮增量快照");
        assert!(
            per_snapshot.iter().all(|(_, n)| *n == 0),
            "预算提醒是请求尾部临时消息，不得累积进历史：{per_snapshot:?}"
        );
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }
}
