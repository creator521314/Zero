//! agent-server —— 服务器端（一期 headless 模式）。
//!
//! 一期定位为「普通终端」：与 CLI 共用同一套核心，仅去掉 TUI，改为读取任务并输出结果。
//! 二期将演化为数据中心 + 设备注册中心（见技术方案 4.2）。

use std::sync::Arc;

use agent_core::event::{event_bus, Event};
use agent_core::perm::{PermissionEngine, PermissionTier};
use agent_core::task::{RunControl, TaskSpec};
use agent_core::AgentRunner;
use agent_perm::rules::StaticRules;
use agent_perm::{PermConfig, PermissionEngineImpl};
use agent_providers::router::budget_line;
use agent_providers::wizard;
use agent_providers::ProviderRouter;
use agent_providers::ProvidersFile;
use agent_store::JsonlStore;
use tokio::sync::mpsc;

/// 用户级记忆库路径（设计 O4：`~/.config/zero/memory.jsonl`）。
fn user_memory_path() -> std::path::PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home)
        .join(".config")
        .join("zero")
        .join("memory.jsonl")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // 任务来自 argv：agent-server "<任务描述>"
    let prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    if prompt.trim().is_empty() {
        eprintln!("用法: agent-server \"<任务描述>\" [--tier L1|L2|L3]");
        std::process::exit(2);
    }

    let cfg = wizard::ensure_loaded()?;
    let cfg_path = ProvidersFile::config_path();

    // 非致命警告（如已知厂商 base_url 缺路径段）
    for w in cfg.warnings() {
        tracing::warn!("{w}");
    }
    let client = agent_net::build_client(&Default::default())?;
    let router = Arc::new(ProviderRouter::from_config(&cfg, client.clone()));
    if router.available().is_empty() {
        anyhow::bail!(
            "没有可用的 LLM provider。\n\
             请设置至少一个 API key 环境变量（如 DEEPSEEK_API_KEY），\n\
             或启动本地 Ollama（http://localhost:11434）作为兜底。\n\
             配置文件位于: {}",
            cfg_path.display()
        );
    }
    tracing::info!(providers = ?router.available(), "available providers");
    tracing::info!(
        "{}",
        budget_line(router.spent_month_usd(), router.monthly_limit())
    );

    let root = cfg.agent.resolved_root();
    // 任务树共享预算单元 + 委派端口。
    //
    // 保留具体类型句柄：`register` 是装配层职责，不属于 `SubagentPort`
    // 的能力面（tools 只需要 `run_subtask`）。
    let budget_cell = agent_runtime::subagent::BudgetCell::default();
    let subagent_impl = Arc::new(agent_runtime::subagent::RuntimeSubagent::new(
        budget_cell.clone(),
    ));
    let subagent_port: Arc<dyn agent_core::subagent::SubagentPort> = subagent_impl.clone();

    // 记忆服务 + 工具注入（与 CLI 同一套规则）。
    let memory = Arc::new(agent_store::MemoryService::new(
        user_memory_path(),
        agent_core::memory::MemoryConfig::default(),
    ));
    let injected_registry = agent_tools::InjectedRegistry::new();

    let mut registry = agent_tools::default_tools(agent_tools::SearchBackend::resolve(
        cfg.search.backend.as_deref(),
        cfg.search.searxng_endpoint.as_deref(),
    ));
    registry.register_delegate(subagent_port.clone());
    registry.register_memory(memory.clone(), memory.clone(), injected_registry.clone());
    let registry = Arc::new(registry);
    let reg_for_lookup = registry.clone();
    let lookup: agent_runtime::runner::ToolLookup =
        Arc::new(move |name: &str| reg_for_lookup.get(name));

    // AI 审视器：L2 档在无人值守时不能变成「一律询问」——那会让任务卡死在
    // 没人点确认的地方。headless 下配一个审视器，它失败时自动降级为询问。
    let reviewer_provider = router.resolve(None, None).await?;
    let perm = Arc::new(
        PermissionEngineImpl::new(StaticRules::new(), PermConfig::default()).with_reviewer(
            Box::new(agent_providers::LlmReviewer::new(reviewer_provider.clone())),
        ),
    ) as Arc<dyn PermissionEngine>;

    let store = Arc::new(JsonlStore::new(&root));
    store.init().await?;
    // 过期清理：临时事件（含工具输出）按 `transient_retention_days` 删除。
    if let Err(e) = store
        .prune_transient(cfg.agent.transient_retention_days)
        .await
    {
        tracing::warn!(error = %e, "transient 事件清理失败（不影响启动）");
    }

    let provider = router.resolve(None, None).await?;
    let bus = event_bus();

    // jsonl 落盘：与 TUI 模式共用同一套规则（agent_store::recorder）
    agent_store::spawn_event_sink(bus.clone(), store.clone(), cfg.log.clone(), None);

    // 打印事件流
    {
        let mut sub = bus.subscribe();
        tokio::spawn(async move {
            while let Ok(ev) = sub.recv().await {
                match ev {
                    Event::TextDelta { text, .. } => print!("{text}"),
                    Event::ToolCallStart { name, args, .. } => {
                        eprintln!("\n[工具] {name} {args}");
                    }
                    Event::TaskFinished { outcome, .. } => {
                        println!("\n[完成] {outcome:?}");
                    }
                    Event::Error { err, .. } => eprintln!("\n[错误] {}: {}", err.kind, err.message),
                    _ => {}
                }
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
        });
    }

    let (_ap_tx, ap_rx) = mpsc::channel(1);
    // 工具输出脱敏：headless 同样不能把密钥交给模型或日志。
    let redactor = std::sync::Arc::new(agent_providers::SecretRedactor::from_config(&cfg));
    let redact: agent_runtime::runner::RedactFn =
        std::sync::Arc::new(move |s: &str| redactor.redact(s));
    // 预算熔断必须取消**执行循环正在监听**的那个令牌，因此先建好再交给
    // RunControl；自建一个没人监听的令牌等于没有熔断。
    let task_cancel = tokio_util::sync::CancellationToken::new();
    let cap = router.per_task_limit();
    // v1.2：窗口按实际 provider 取，compose 与 deps 共用同一份配置。
    let ctx_cfg =
        agent_runtime::context::ContextConfig::for_model(provider.capabilities().max_context);
    let deps = agent_runtime::RuntimeDeps {
        provider: provider.clone(),
        provider_name: provider.name().to_string(),
        tools: lookup,
        tool_defs: registry.defs(),
        perm,
        store: store.clone(),
        log_cfg: cfg.log.clone(),
        approvals: Arc::new(agent_runtime::runner::ApprovalInbox::new(ap_rx)),
        context_cfg: ctx_cfg.clone(),
        per_task_budget: router.per_task_limit(),
        // 每轮成本：报月度账本 + 记入任务树共享预算；合计超额取消令牌。
        cost_sink: Some(Arc::new({
            let router = router.clone();
            let cell = budget_cell.clone();
            let gate_cancel = task_cancel.clone();
            move |cost: f64| {
                router.record_spend(cost);
                cell.charge(cost);
                if cap > 0.0 && cell.spent_usd() > cap {
                    gate_cancel.cancel();
                }
            }
        })),
        redact,
    };
    let runtime = agent_runtime::AgentRuntime::new(deps.clone());

    let mut spec = TaskSpec::new(prompt, String::new());
    spec.tier = PermissionTier::L1; // headless 无人值守，默认全自动
    spec.max_iterations = cfg.agent.max_iterations;
    let ws = agent_store::workspace_of(&root, &spec.id);
    tokio::fs::create_dir_all(&ws).await.ok();
    spec.workspace = ws.display().to_string();
    // 工具沙箱 = 项目根（默认启动目录）；私有工作区只放系统数据。
    spec.project_root = Some(cfg.agent.resolved_project_root().display().to_string());
    // headless 单任务：登记一次即可，不需要 unregister。
    // 记忆注入快照（M2）：与 CLI 同一套规则；失败静默，不阻塞任务。
    let memory_port = &*memory;
    match agent_core::memory::MemoryPort::compose(
        memory_port,
        &agent_core::memory::MemoryScope::Project {
            root: cfg.agent.resolved_project_root().display().to_string(),
        },
        ctx_cfg.max_context,
    )
    .await
    {
        Ok(ctx) if !ctx.text.is_empty() => {
            tracing::info!(injected = ctx.injected_ids.len(), "memory composed");
            injected_registry.register(&spec.id.0, ctx.injected_ids.clone());
            spec.memory_context = Some(ctx);
        }
        _ => {}
    }
    subagent_impl
        .register(spec.clone(), deps, bus.clone())
        .await;

    // 注意：不再在此手写 `TaskStarted`——落盘统一由 recorder 订阅总线完成，
    // 否则同一条事件会被写两遍。
    match runtime
        .run(
            spec,
            bus,
            RunControl {
                cancel: task_cancel.clone(),
            },
        )
        .await
    {
        Ok(o) => {
            println!("\n最终结果: {o:?}");
            Ok(())
        }
        Err(e) => {
            eprintln!("任务失败: {e}");
            std::process::exit(1);
        }
    }
}
