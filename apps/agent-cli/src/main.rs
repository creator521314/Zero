//! agent —— 个人 Agent 入口（TUI 模式）。
//!
//! 装配职责：读配置 → 建 provider 路由 → 建工具集 → 建权限引擎 → 拉起 TUI + Agent 循环。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_core::event::{event_bus, Command, Event};
use agent_core::ids::SessionId;
use agent_core::perm::{PermissionEngine, PermissionTier, Risk};
use agent_core::provider::{ChatRequest, LlmProvider, Message, ToolCall};
use agent_core::task::{RunControl, TaskSpec};
use agent_core::AgentRunner;
use agent_perm::rules::StaticRules;
use agent_perm::{AiReviewer, PermConfig, PermissionEngineImpl, Review};
use agent_providers::wizard;
use agent_providers::ProviderRouter;
use agent_providers::ProvidersFile;
use agent_store::JsonlStore;
use agent_tui::{run_tui, App};
use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};

/// AI 审视器：用低价模型判定操作是否放行。
struct LlmReviewer {
    provider: Arc<dyn LlmProvider>,
}

#[async_trait]
impl AiReviewer for LlmReviewer {
    async fn review(
        &self,
        task: &TaskSpec,
        call: &ToolCall,
        static_risk: Risk,
        timeout: Duration,
    ) -> Result<Review, String> {
        let sys = "你是操作安全审查员。判断给定的工具调用是否服务于用户任务、是否可能造成不可逆危害。\
                   只输出 JSON：{\"decision\":\"allow|deny\",\"risk\":\"low|medium|high\",\"reason\":\"...\",\"confidence\":0.0}";
        let ctx = format!(
            "任务目标: {}\n工作区: {}\n静态风险: {:?}\n当前操作: 工具={} 参数={}",
            task.prompt, task.workspace, static_risk, call.name, call.arguments
        );
        let req = ChatRequest {
            model: String::new(),
            messages: vec![Message::system(sys), Message::user(ctx)],
            tools: vec![],
            temperature: Some(0.0),
            max_tokens: Some(200),
            stream: false,
            reasoning_effort: None,
            metadata: Default::default(),
        };
        let resp = tokio::time::timeout(timeout, self.provider.chat(req))
            .await
            .map_err(|_| "ai review timeout".to_string())?
            .map_err(|e| e.to_string())?;

        let text = resp.message.text();
        let start = text.find('{').ok_or("no json in review")?;
        let end = text.rfind('}').ok_or("no json in review")?;
        let json: serde_json::Value =
            serde_json::from_str(&text[start..=end]).map_err(|e| e.to_string())?;

        let allow = json
            .get("decision")
            .and_then(|d| d.as_str())
            .map(|d| d.eq_ignore_ascii_case("allow"))
            .unwrap_or(false);
        let risk = match json.get("risk").and_then(|r| r.as_str()) {
            Some("high") => Risk::High,
            Some("medium") => Risk::Medium,
            _ => Risk::Low,
        };
        Ok(Review {
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
        })
    }
}

fn parse_tier(s: &str) -> PermissionTier {
    match s.to_ascii_uppercase().as_str() {
        "L1" => PermissionTier::L1,
        "L3" => PermissionTier::L3,
        _ => PermissionTier::L2,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg_path = ProvidersFile::config_path();

    // 日志写文件，避免污染 TUI
    if let Some(dir) = cfg_path.parent() {
        let _ = std::fs::create_dir_all(dir.join("logs"));
        let file_appender = tracing_appender::rolling::never(dir.join("logs"), "agent.log");
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .with_writer(file_appender)
            .init();
    }

    // 启动期配置加载：缺/坏 → 自动进入 wizard（仅 stdin 是 TTY 时）。
    let cfg = wizard::ensure_loaded()?;

    let client = agent_net::build_client(&Default::default())?;
    let router = Arc::new(ProviderRouter::from_config(&cfg, client.clone()));

    if router.available().is_empty() {
        anyhow::bail!(
            "没有可用的 LLM provider。请设置环境变量，例如:\n  setx DEEPSEEK_API_KEY \"sk-xxx\"\n\
             配置文件: {}\n(可用 `cargo xtask gen-cfg` 重新生成模板)",
            cfg_path.display()
        );
    }

    let root = PathBuf::from(&cfg.agent.root);
    tokio::fs::create_dir_all(root.join("workspace")).await.ok();

    // 工具集
    let registry = Arc::new(agent_tools::default_tools(
        client.clone(),
        cfg.search.searxng_endpoint.clone(),
    ));
    let reg_for_lookup = registry.clone();
    let lookup: agent_runtime::runner::ToolLookup =
        Arc::new(move |name: &str| reg_for_lookup.get(name));
    let tool_defs = registry.defs();

    // 权限引擎：静态规则 + AI 审视（复用默认 provider，低价档）
    let reviewer_provider = router.resolve(None, None).await?;
    let perm = Arc::new(
        PermissionEngineImpl::new(StaticRules::new(), PermConfig::default()).with_reviewer(
            Box::new(LlmReviewer {
                provider: reviewer_provider,
            }),
        ),
    ) as Arc<dyn PermissionEngine>;

    // 存储
    let store = Arc::new(JsonlStore::new(&root));
    store.init().await?;

    // 全局限量通道：审批应答（单例，被所有任务共享）
    let (ap_tx, ap_rx) = mpsc::channel::<agent_runtime::runner::ApprovalMsg>(16);
    let approvals = Arc::new(Mutex::new(ap_rx));

    let bus = event_bus();

    // 命令通道
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Command>(64);
    // 事件通道（TUI 消费）
    let (ev_tx, ev_rx) = mpsc::channel::<Event>(2048);

    // broadcast -> mpsc 转发 + JSONL 落盘
    {
        let store = store.clone();
        let mut sub = bus.subscribe();
        tokio::spawn(async move {
            while let Ok(ev) = sub.recv().await {
                let _ = ev_tx.send(ev.clone()).await;
                let (tid, durable) = match &ev {
                    Event::TaskStarted { id, .. }
                    | Event::TaskFinished { id, .. }
                    | Event::Error { id, .. } => {
                        (id.clone(), agent_core::store::Durability::Persistent)
                    }
                    Event::TextDelta { id, .. }
                    | Event::ReasoningDelta { id, .. }
                    | Event::ToolCallStart { id, .. }
                    | Event::ToolCallEnd { id, .. }
                    | Event::AwaitingApproval { id, .. } => {
                        (id.clone(), agent_core::store::Durability::Transient)
                    }
                };
                let sid = SessionId(tid.0.clone());
                let _ = store.record(&sid, &tid, durable, ev).await;
            }
        });
    }

    // Agent 命令循环
    {
        let bus = bus.clone();
        let router = router.clone();
        let store = store.clone();
        let tool_defs2 = tool_defs.clone();
        let lookup2 = lookup.clone();
        let perm2 = perm.clone();
        let approvals2 = approvals.clone();
        let cfg_root = root.clone();
        let default_tier = parse_tier(&cfg.agent.default_tier);
        let max_iter = cfg.agent.max_iterations;

        tokio::spawn(async move {
            // 每任务一个取消令牌
            #[allow(unused_assignments)]
            let mut current: Option<tokio_util::sync::CancellationToken> = None;

            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    Command::SubmitTask(mut spec) => {
                        if spec.workspace.is_empty() {
                            let ws = agent_store::workspace_of(&cfg_root, &spec.id);
                            tokio::fs::create_dir_all(&ws).await.ok();
                            spec.workspace = ws.display().to_string();
                        }
                        spec.tier = default_tier;
                        spec.max_iterations = max_iter;

                        let provider = match router.resolve(None, spec.model.as_deref()).await {
                            Ok(p) => p,
                            Err(e) => {
                                let _ = bus.send(Event::Error {
                                    id: spec.id.clone(),
                                    err: agent_core::ErrorInfo::new("provider", e.to_string()),
                                });
                                continue;
                            }
                        };

                        let ctl = RunControl::new();
                        // current 供 Cancel 命令触发取消
                        current = Some(ctl.cancel.clone());

                        let runtime =
                            agent_runtime::AgentRuntime::new(agent_runtime::RuntimeDeps {
                                provider: provider.clone(),
                                provider_name: provider.name().to_string(),
                                tools: lookup2.clone(),
                                tool_defs: tool_defs2.clone(),
                                perm: perm2.clone(),
                                store: store.clone(),
                                approvals: approvals2.clone(),
                                context_cfg: Default::default(),
                                batch_approve: None,
                                per_task_budget: router.per_task_limit(),
                            });

                        if let Err(e) = runtime.run(spec, bus.clone(), ctl).await {
                            tracing::error!(error = %e, "task run failed");
                        }
                        // 任务结束后清空，避免 Cancel 命中已结束的任务
                        let _ = current.take();
                    }
                    Command::Approve(id) => {
                        let _ = ap_tx
                            .send((
                                id,
                                agent_runtime::runner::ApprovalReply::Approve { remember: false },
                            ))
                            .await;
                    }
                    Command::Reject {
                        approval_id,
                        reason,
                    } => {
                        let _ = ap_tx
                            .send((
                                approval_id,
                                agent_runtime::runner::ApprovalReply::Reject { reason },
                            ))
                            .await;
                    }
                    Command::Cancel(_) => {
                        if let Some(t) = &current {
                            t.cancel();
                        }
                    }
                    Command::Quit => break,
                }
            }
        });
    }

    let app = App {
        tier: cfg.agent.default_tier.clone(),
        provider: router.available().join(","),
        ..Default::default()
    };

    run_tui(app, ev_rx, cmd_tx).await?;
    Ok(())
}
