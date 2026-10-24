//! 真实模型下的续跑冒烟：验证「切回历史任务继续」不是只把 id 传了一圈。
//!
//! 单元测试能证明「消息被拼进了请求」，但不能证明整条链路（真实端点 + 真实
//! store + 真实执行循环）跑得通；这条用例就是补这一层。
//!
//! 运行：
//! ```text
//! cargo test -p agent-cli --test resume_live -- --ignored --nocapture
//! ```
//!
//! 没配置 provider 时打印「跳过」并返回——不把没有 key 的机器判红，也不假装验过。

use std::sync::Arc;

use agent_core::event::event_bus;
use agent_core::perm::PermissionEngine;
use agent_core::task::{RunControl, TaskSpec};
use agent_core::AgentRunner;
use agent_runtime::runner::ApprovalMsg;
use agent_runtime::AgentRuntime;
use agent_store::JsonlStore;
use agent_tools::SearchBackend;
use tokio::sync::{mpsc, Mutex};

#[tokio::test]
#[ignore = "真实 LLM 调用：cargo test -p agent-cli --test resume_live -- --ignored"]
async fn resumed_task_really_carries_the_previous_turn() {
    // 集成测试的 cwd 是包目录（apps/agent-cli），不是仓库根；显式指到仓库的配置，
    // 否则会读到「默认模板」→ 只剩一个连不上的本地 ollama。
    let cfg_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/config.toml");
    let cfg_path = if cfg_path.exists() {
        cfg_path
    } else {
        agent_providers::ProvidersFile::config_path()
    };
    let cfg = agent_providers::ProvidersFile::load_or_default(&cfg_path).expect("读配置失败");
    let client = agent_net::build_client(&Default::default()).expect("建 HTTP 客户端失败");
    let router = agent_providers::ProviderRouter::from_config(&cfg, client.clone());
    if router.available().is_empty() {
        eprintln!("跳过：没有可用的 provider（请配置 DEEPSEEK_API_KEY 或 config/config.toml）");
        return;
    }
    let provider = router
        .resolve(None, None)
        .await
        .expect("解析 provider 失败");
    eprintln!("使用 provider: {}", provider.name());

    let tmp = std::env::temp_dir().join(format!(
        "agent-resume-live-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let ws = tmp.join("workspace");
    tokio::fs::create_dir_all(&ws).await.unwrap();
    let store = Arc::new(JsonlStore::new(&tmp));
    store.init().await.unwrap();
    // 落盘由 recorder 订阅总线完成（runner 不再自己写）；没有这一行 store 永远是空的。
    let bus = event_bus();
    agent_store::spawn_event_sink(bus.clone(), store.clone(), Default::default(), None);

    let registry = Arc::new(agent_tools::default_tools(
        client.clone(),
        SearchBackend::resolve(None, None),
    ));
    let reg = registry.clone();
    let lookup: agent_runtime::runner::ToolLookup = Arc::new(move |n: &str| reg.get(n));
    // 无人值守：不需要审批弹层，任何询问都当拒绝（这条用例本身不用工具）。
    let (_ap_tx, ap_rx) = mpsc::channel::<ApprovalMsg>(1);
    let perm = Arc::new(agent_perm::PermissionEngineImpl::new(
        agent_perm::rules::StaticRules::new(),
        agent_perm::PermConfig::default(),
    )) as Arc<dyn PermissionEngine>;

    let runtime = AgentRuntime::new(agent_runtime::RuntimeDeps {
        provider: provider.clone(),
        provider_name: provider.name().to_string(),
        tools: lookup,
        tool_defs: registry.defs(),
        perm,
        store: store.clone(),
        log_cfg: Default::default(),
        approvals: Arc::new(Mutex::new(ap_rx)),
        context_cfg: Default::default(),
        per_task_budget: 0.0,
        cost_sink: None,
    });

    // ---- 第一轮：一条普通任务，不调工具，只留下上下文快照 ----
    let mut first = TaskSpec::new(
        "记住这个数字：7319。只回复「收到」两个字。",
        ws.display().to_string(),
    );
    first.max_iterations = 2;
    let task_id = first.id.clone();
    runtime
        .run(first, bus.clone(), RunControl::new())
        .await
        .expect("第一轮任务失败");
    // 落盘是异步的：等 recorder 把快照写完再读。
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // ---- 续跑：真实地从 store 取回上下文，再追问 ----
    let history = store
        .resume_context(&task_id)
        .await
        .expect("第一轮应落上下文快照（续跑的唯一数据来源）");
    assert!(
        history.iter().any(|m| m.text().contains("7319")),
        "快照里应含第一轮的用户指令：{:?}",
        history.iter().map(|m| m.text()).collect::<Vec<_>>()
    );

    let mut second = TaskSpec::new(
        "我刚才让你记住的数字是多少？只回数字。",
        ws.display().to_string(),
    );
    second.id = task_id.clone();
    second.resume_messages = Some(history);
    second.max_iterations = 2;
    let outcome = runtime
        .run(second, bus.clone(), RunControl::new())
        .await
        .expect("续跑任务失败");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let summary = match outcome {
        agent_core::event::Outcome::Success { summary, .. } => summary,
        other => panic!("续跑应成功收尾，实际 {other:?}"),
    };
    eprintln!("续跑回答：{summary}");
    assert!(
        summary.contains("7319"),
        "续跑后模型应记得第一轮的数字（否则上下文根本没带上）：{summary}"
    );

    // 同一任务 id：面板里仍是同一条任务，而不是每追问一次多一条。
    let listed = store.recent_tasks(10).await;
    assert_eq!(
        listed.len(),
        1,
        "续跑应更新原任务，而不是新建一条：{listed:?}"
    );

    let session = agent_core::store::Store::load_session(
        store.as_ref(),
        &agent_core::ids::SessionId(task_id.0.clone()),
    )
    .await
    .expect("会话应可加载");
    let last_ctx = session
        .events
        .iter()
        .rev()
        .find_map(|e| match &e.payload {
            agent_core::event::Event::ContextSnapshot { messages, .. } => Some(messages.clone()),
            _ => None,
        })
        .expect("第二次请求前应有快照");
    let texts: Vec<String> = last_ctx.iter().map(|m| m.text()).collect();
    assert!(
        texts.iter().any(|t| t.contains("7319")),
        "续跑请求里必须包含第一轮的对话：{texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("我刚才让你记住")),
        "续跑请求里也必须包含这次的新指令：{texts:?}"
    );

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}
