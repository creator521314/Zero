//! agent-server —— 服务器端（一期 headless 模式）。
//!
//! 一期定位为「普通终端」：与 CLI 共用同一套核心，仅去掉 TUI，改为读取任务并输出结果。
//! 二期将演化为数据中心 + 设备注册中心（见技术方案 4.2）。

use std::sync::Arc;

use agent_core::event::{event_bus, Event};
use agent_core::ids::SessionId;
use agent_core::perm::{PermissionEngine, PermissionTier};
use agent_core::task::{RunControl, TaskSpec};
use agent_core::AgentRunner;
use agent_perm::rules::StaticRules;
use agent_perm::{PermConfig, PermissionEngineImpl};
use agent_providers::wizard;
use agent_providers::ProviderRouter;
use agent_providers::ProvidersFile;
use agent_store::JsonlStore;
use tokio::sync::{mpsc, Mutex};

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

    let root = cfg.agent.resolved_root();
    let registry = Arc::new(agent_tools::default_tools(
        client.clone(),
        cfg.search.searxng_endpoint.clone(),
    ));
    let reg_for_lookup = registry.clone();
    let lookup: agent_runtime::runner::ToolLookup =
        Arc::new(move |name: &str| reg_for_lookup.get(name));

    let perm = Arc::new(PermissionEngineImpl::new(
        StaticRules::new(),
        PermConfig::default(),
    )) as Arc<dyn PermissionEngine>;

    let store = Arc::new(JsonlStore::new(&root));
    store.init().await?;

    let provider = router.resolve(None, None).await?;
    let bus = event_bus();

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
    let runtime = agent_runtime::AgentRuntime::new(agent_runtime::RuntimeDeps {
        provider: provider.clone(),
        provider_name: provider.name().to_string(),
        tools: lookup,
        tool_defs: registry.defs(),
        perm,
        store: store.clone(),
        approvals: Arc::new(Mutex::new(ap_rx)),
        context_cfg: Default::default(),
        batch_approve: None,
        per_task_budget: router.per_task_limit(),
    });

    let mut spec = TaskSpec::new(prompt, String::new());
    spec.tier = PermissionTier::L1; // headless 无人值守，默认全自动
    spec.max_iterations = cfg.agent.max_iterations;
    let ws = agent_store::workspace_of(&root, &spec.id);
    tokio::fs::create_dir_all(&ws).await.ok();
    spec.workspace = ws.display().to_string();

    let sid = SessionId(spec.id.0.clone());
    store
        .record(
            &sid,
            &spec.id,
            agent_core::store::Durability::Persistent,
            Event::TaskStarted {
                id: spec.id.clone(),
                title: spec.title.clone(),
            },
        )
        .await?;

    match runtime.run(spec, bus, RunControl::new()).await {
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
