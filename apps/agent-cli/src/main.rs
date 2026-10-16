//! agent —— 个人 Agent 入口（TUI 模式）。
//!
//! 装配职责：读配置 → 建 provider 路由 → 建工具集 → 建权限引擎 → 拉起 TUI + Agent 循环。

use std::sync::Arc;
use std::time::Duration;

use agent_core::event::{event_bus, Command, Event};
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

    // 日志写文件，避免污染 TUI（stdout 已被 alternate screen 占用）
    let log_file = cfg_path.parent().map(|dir| {
        let logs_dir = dir.join("logs");
        let _ = std::fs::create_dir_all(&logs_dir);
        let file_appender = tracing_appender::rolling::never(&logs_dir, "agent.log");
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .with_writer(file_appender)
            .init();
        logs_dir.join("agent.log")
    });

    // 启动期配置加载：缺/坏 → 自动进入 wizard（仅 stdin 是 TTY 时）。
    let cfg = wizard::ensure_loaded()?;

    // 非致命警告（如已知厂商 base_url 缺路径段）——TUI 下写日志文件。
    for w in cfg.warnings() {
        tracing::warn!("{w}");
        eprintln!("[警告] {w}");
    }

    let client = agent_net::build_client(&Default::default())?;
    let router = Arc::new(ProviderRouter::from_config(&cfg, client.clone()));

    if router.available().is_empty() {
        anyhow::bail!(
            "没有可用的 LLM provider。请设置环境变量，例如:\n  setx DEEPSEEK_API_KEY \"sk-xxx\"\n\
             配置文件: {}\n(可用 `cargo xtask gen-cfg` 重新生成模板)",
            cfg_path.display()
        );
    }

    let root = cfg.agent.resolved_root();
    tokio::fs::create_dir_all(root.join("workspace")).await.ok();
    // 工具沙箱：默认启动目录，这样「介绍当前项目」才有东西可看。
    let project_root = cfg.agent.resolved_project_root();

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

    // 总线 → TUI 转发 + jsonl 落盘（落盘规则集中在 agent_store::recorder）
    agent_store::spawn_event_sink(bus.clone(), store.clone(), cfg.log.clone(), Some(ev_tx));

    // Agent 命令循环
    {
        let bus = bus.clone();
        let router = router.clone();
        let store = store.clone();
        let tool_defs2 = tool_defs.clone();
        let lookup2 = lookup.clone();
        let perm2 = perm.clone();
        let approvals2 = approvals.clone();
        let log_cfg2 = cfg.log.clone();
        let cfg_root = root.clone();
        let proj_root = project_root.clone();
        let default_tier = parse_tier(&cfg.agent.default_tier);
        let max_iter = cfg.agent.max_iterations;

        tokio::spawn(async move {
            // 当前运行中的任务：取消令牌 + 句柄。
            //
            // 任务必须 spawn 到独立 task 执行，**不能在本循环里 await**：
            // 本循环同时负责把 Approve/Reject/Cancel 转发给 runner，而 runner
            // 的 wait_approval 正等着这些应答。若在此 await 任务结束，就形成
            // 「循环等任务结束、任务等循环转发审批」的自锁——表现为任务永远
            // 不结束、审批按了没反应、取消也无效。
            type RunningTask = (
                tokio_util::sync::CancellationToken,
                tokio::task::JoinHandle<()>,
            );
            let mut current: Option<RunningTask> = None;

            while let Some(cmd) = cmd_rx.recv().await {
                tracing::debug!(?cmd, "command received");

                // 先收割已结束的任务：否则陈旧句柄会把后续提交一直挡在门外。
                if current.as_ref().is_some_and(|(_, h)| h.is_finished()) {
                    if let Some((_, h)) = current.take() {
                        if let Err(e) = h.await {
                            tracing::error!(error = %e, "task join failed");
                        }
                    }
                }

                match cmd {
                    Command::SubmitTask(mut spec) => {
                        // 一期为串行执行：同一时刻只跑一个任务。明确拒绝并回报，
                        // 而不是静默排队——静默排队会让用户以为提交没生效。
                        if current.is_some() {
                            tracing::warn!(task = %spec.id.0, "rejected: another task is running");
                            let _ = bus.send(Event::Error {
                                id: spec.id.clone(),
                                err: agent_core::ErrorInfo::new(
                                    "busy",
                                    "上一个任务仍在执行。请等它结束，或按 Esc 取消后再提交。",
                                ),
                            });
                            continue;
                        }

                        if spec.workspace.is_empty() {
                            let ws = agent_store::workspace_of(&cfg_root, &spec.id);
                            tokio::fs::create_dir_all(&ws).await.ok();
                            spec.workspace = ws.display().to_string();
                        }
                        // 工具沙箱 = 项目根（默认启动目录）；私有工作区只放系统数据。
                        if spec.project_root.is_none() {
                            spec.project_root = Some(proj_root.display().to_string());
                        }
                        spec.tier = default_tier;
                        spec.max_iterations = max_iter;

                        let provider = match router.resolve(None, spec.model.as_deref()).await {
                            Ok(p) => p,
                            Err(e) => {
                                tracing::error!(error = %e, "provider resolve failed");
                                let _ = bus.send(Event::Error {
                                    id: spec.id.clone(),
                                    err: agent_core::ErrorInfo::new("provider", e.to_string()),
                                });
                                continue;
                            }
                        };

                        let ctl = RunControl::new();
                        let cancel = ctl.cancel.clone();
                        let bus_task = bus.clone();
                        let task_id = spec.id.0.clone();
                        let provider_name = provider.name().to_string();

                        let runtime =
                            agent_runtime::AgentRuntime::new(agent_runtime::RuntimeDeps {
                                provider: provider.clone(),
                                provider_name: provider_name.clone(),
                                tools: lookup2.clone(),
                                tool_defs: tool_defs2.clone(),
                                perm: perm2.clone(),
                                store: store.clone(),
                                log_cfg: log_cfg2.clone(),
                                approvals: approvals2.clone(),
                                context_cfg: Default::default(),
                                batch_approve: None,
                                per_task_budget: router.per_task_limit(),
                            });

                        tracing::info!(task = %task_id, provider = %provider_name, "task started");
                        let handle = tokio::spawn(async move {
                            match runtime.run(spec, bus_task, ctl).await {
                                Ok(_) => tracing::info!(task = %task_id, "task run finished"),
                                Err(e) => {
                                    tracing::error!(task = %task_id, error = %e, "task run failed")
                                }
                            }
                        });
                        current = Some((cancel, handle));
                    }
                    Command::Approve(id) => {
                        tracing::info!(approval = %id.0, "approve forwarded to runner");
                        if ap_tx
                            .send((
                                id,
                                agent_runtime::runner::ApprovalReply::Approve { remember: false },
                            ))
                            .await
                            .is_err()
                        {
                            tracing::error!("approval channel closed; runner gone");
                        }
                    }
                    Command::Reject {
                        approval_id,
                        reason,
                    } => {
                        tracing::info!(approval = %approval_id.0, "reject forwarded to runner");
                        if ap_tx
                            .send((
                                approval_id,
                                agent_runtime::runner::ApprovalReply::Reject { reason },
                            ))
                            .await
                            .is_err()
                        {
                            tracing::error!("approval channel closed; runner gone");
                        }
                    }
                    Command::Cancel(id) => {
                        tracing::info!(task = %id.0, "cancel requested");
                        if let Some((t, _)) = &current {
                            t.cancel();
                        }
                    }
                    Command::Quit => break,
                }
            }
        });
    }

    let mut app = App::with_identity(cfg.agent.default_tier.clone(), router.available().join(","));
    if let Some(p) = &log_file {
        app.set_log_path(p.display().to_string());
    }

    tracing::info!("tui starting");
    run_tui(app, ev_rx, cmd_tx).await?;
    tracing::info!("tui exited");
    Ok(())
}
