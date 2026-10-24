//! agent —— 个人 Agent 入口（TUI 模式）。
//!
//! 装配职责：读配置 → 建 provider 路由 → 建工具集 → 建权限引擎 → 拉起 TUI + Agent 循环。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_core::event::{event_bus, Command, Event};
use agent_core::perm::PermissionEngine;
use agent_core::task::{RunControl, TaskSpec};
use agent_core::AgentRunner;
use agent_perm::rules::StaticRules;
use agent_perm::{PermConfig, PermissionEngineImpl};
use agent_providers::router::budget_line;
use agent_providers::wizard;
use agent_providers::ProviderRouter;
use agent_providers::ProvidersFile;
use agent_store::JsonlStore;
use agent_tui::{run_tui, App};
use tokio::sync::{mpsc, Mutex};

/// 把「从哪个历史任务接上」落成实际上下文。
///
/// 界面只传 id（TUI 不依赖 store），消息由这里补：取该任务最后一份
/// `ContextSnapshot` + 快照之后的答复。顺带把任务 id 与标题锁定为原任务——
/// 否则每追问一句就会在面板里多出一条同名任务。
///
/// 返回 `Err(提示)` 表示续跑不可用（日志被清理 / 从没落过快照），调用方按新任务继续。
async fn apply_resume(store: &JsonlStore, spec: &mut TaskSpec) -> Result<(), String> {
    let Some(id) = spec.resume_from.clone() else {
        return Ok(());
    };
    spec.id = id.clone();
    match store.resume_context(&id).await {
        Some(msgs) if !msgs.is_empty() => {
            tracing::info!(task = %id.0, messages = msgs.len(), "resuming task with stored context");
            spec.resume_messages = Some(msgs);
            if let Some(s) = store.task_summary(&id).await {
                spec.title = s.title;
            }
            Ok(())
        }
        _ => {
            tracing::warn!(task = %id.0, "resume requested but no context snapshot found");
            spec.resume_from = None;
            Err("该任务没有可用的上下文快照，已按新任务继续。".into())
        }
    }
}

/// 按 id 或唯一前缀找任务（`/resume` 与任务面板共用）。
///
/// 规则：先精确匹配完整 id；否则按前缀匹配。0 个 / 多个都算失败——多个时
/// 报出「匹配到几个」，让用户多打几位，而不是随便挑一个切过去。
fn resolve_task(
    prefix: &str,
    tasks: &[agent_core::TaskSummary],
) -> Result<agent_core::TaskSummary, String> {
    let needle = prefix.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return Err("用法：/resume <任务ID前几位>（用 /tasks 查看）".into());
    }
    if let Some(t) = tasks
        .iter()
        .find(|t| t.id.0.eq_ignore_ascii_case(needle.as_str()))
    {
        return Ok(t.clone());
    }
    let hits: Vec<&agent_core::TaskSummary> = tasks
        .iter()
        .filter(|t| t.id.0.to_ascii_lowercase().starts_with(&needle))
        .collect();
    match hits.len() {
        0 => Err(format!(
            "找不到 id 以「{prefix}」开头的任务；用 /tasks 查看历史任务"
        )),
        1 => Ok(hits[0].clone()),
        n => Err(format!(
            "前缀「{prefix}」匹配到 {n} 个任务，请多输入几位；用 /tasks 查看"
        )),
    }
}

/// 解析日志目录：`AGENT_LOG_DIR` 优先，否则 `agent.root/logs`。
///
/// 不再放 `config/` 下——那是代码仓库里的配置目录，运行期产物不该混进去，
/// 也不该跟着 `config/` 一起被 gitignore 之外的规则误伤。
///
/// 此刻配置可能还没加载（甚至尚未生成、语法有误），所以只「看一眼」root，
/// 取不到就用默认值：日志功能不该被配置问题连带拖死。
fn resolve_log_dir(cfg_path: &Path, env_override: Option<String>) -> PathBuf {
    if let Some(d) = env_override.filter(|s| !s.trim().is_empty()) {
        return PathBuf::from(d);
    }
    ProvidersFile::load_or_default(cfg_path)
        .unwrap_or_default()
        .agent
        .resolved_log_dir()
}

/// 初始化文件日志（stdout 被 TUI 的 alternate screen 占用，只能写文件），
/// 返回日志文件路径，供界面展示。
fn init_tracing(cfg_path: &Path) -> PathBuf {
    let mut dir = resolve_log_dir(cfg_path, std::env::var("AGENT_LOG_DIR").ok());
    if std::fs::create_dir_all(&dir).is_err() {
        // 宁可换个地方，也不要把日志静默丢进写不进去的目录。
        let fallback = std::env::temp_dir().join("agent-logs");
        if std::fs::create_dir_all(&fallback).is_ok() {
            eprintln!(
                "[警告] 日志目录不可写：{}，改用 {}",
                dir.display(),
                fallback.display()
            );
            dir = fallback;
        }
    }
    let file_appender = tracing_appender::rolling::never(&dir, "agent.log");
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(file_appender)
        .init();
    dir.join("agent.log")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg_path = ProvidersFile::config_path();
    let log_file = init_tracing(&cfg_path);

    // 启动期配置加载：缺/坏 → 自动进入 wizard（仅 stdin 是 TTY 时）。
    let cfg = wizard::ensure_loaded()?;

    // 非致命警告（如已知厂商 base_url 缺路径段）——TUI 下写日志文件。
    for w in cfg.warnings() {
        tracing::warn!("{w}");
        eprintln!("[警告] {w}");
    }

    let client = agent_net::build_client(&Default::default())?;
    let router = Arc::new(ProviderRouter::from_config(&cfg, client.clone()));
    // 本月已花多少/还剩多少：成本一眼可见，而不是等预算见底才从报错里猜。
    tracing::info!(
        "{}",
        budget_line(router.spent_month_usd(), router.monthly_limit())
    );

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

    // 工具集。搜索后端按 [search] 段解析：未配置时走内置免 key 后端，
    // 保证 web_search 开箱可用。
    let registry = Arc::new(agent_tools::default_tools(
        client.clone(),
        agent_tools::SearchBackend::resolve(
            cfg.search.backend.as_deref(),
            cfg.search.searxng_endpoint.as_deref(),
        ),
    ));
    let reg_for_lookup = registry.clone();
    let lookup: agent_runtime::runner::ToolLookup =
        Arc::new(move |name: &str| reg_for_lookup.get(name));
    let tool_defs = registry.defs();

    // 权限引擎：静态规则 + AI 审视（复用默认 provider，低价档）
    let reviewer_provider = router.resolve(None, None).await?;
    let perm = Arc::new(
        PermissionEngineImpl::new(StaticRules::new(), PermConfig::default()).with_reviewer(
            Box::new(agent_providers::LlmReviewer::new(reviewer_provider)),
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

                        // 续跑：界面只传「从哪个任务接上」，消息由编排层从 store 取。
                        // 同一任务 id 继续追加事件，面板里仍是同一条任务，而不是
                        // 每说一句话就多出一条「新任务」。
                        if let Err(msg) = apply_resume(&store, &mut spec).await {
                            let _ = bus.send(Event::Error {
                                id: spec.id.clone(),
                                err: agent_core::ErrorInfo::new("resume_unavailable", msg),
                            });
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
                        // 权限档位由界面随任务带来（header 徽章可切档，「立即作用于
                        // 之后提交的任务」）；这里不再用配置默认值覆盖——覆盖了徽章
                        // 就只是装饰，切了档提交时仍走默认 L2。
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

                        // 「批准本任务同类操作」的登记由执行循环直接 await 引擎完成
                        // （曾经过一个同步回调再 spawn，会在登记落定前就查下一次，
                        // 用户按了 `a` 仍被再问一次）。
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
                                per_task_budget: router.per_task_limit(),
                                // 每轮成本报到账本，月度熔断才有数据可判。
                                cost_sink: Some(Arc::new({
                                    let router = router.clone();
                                    move |cost: f64| router.record_spend(cost)
                                })),
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
                    Command::Approve {
                        approval_id: id,
                        remember,
                    } => {
                        tracing::info!(approval = %id.0, remember, "approve forwarded to runner");
                        if ap_tx
                            .send((
                                id,
                                agent_runtime::runner::ApprovalReply::Approve { remember },
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
                    // `/tasks` 与任务面板：读 store 回灌事件。
                    // 这两个命令只是「看一眼」，不启动任务，因此不受串行限制。
                    Command::ListTasks => {
                        let tasks = store.recent_tasks(50).await;
                        tracing::info!(count = tasks.len(), "task list requested");
                        let _ = bus.send(Event::TaskList { tasks });
                    }
                    // 切到历史任务：回灌转写事件，界面据此还原对话区。
                    Command::ResumeTask(prefix) => {
                        if current.is_some() {
                            let _ = bus.send(Event::Error {
                                id: agent_core::TaskId::new(),
                                err: agent_core::ErrorInfo::new(
                                    "busy",
                                    "任务运行中无法切换，请等它结束或按 Esc 取消。",
                                ),
                            });
                            continue;
                        }
                        let tasks = store.recent_tasks(200).await;
                        match resolve_task(&prefix, &tasks) {
                            Ok(t) => {
                                let events = store.task_events(&t.id).await;
                                tracing::info!(
                                    task = %t.id.0,
                                    events = events.len(),
                                    "task opened"
                                );
                                let _ = bus.send(Event::TaskOpened {
                                    id: t.id.clone(),
                                    title: t.title.clone(),
                                    events,
                                });
                            }
                            Err(msg) => {
                                let _ = bus.send(Event::Error {
                                    id: agent_core::TaskId::new(),
                                    err: agent_core::ErrorInfo::new("task_not_found", msg),
                                });
                            }
                        }
                    }
                    Command::Quit => break,
                }
            }
        });
    }

    let mut app = App::with_identity(cfg.agent.default_tier.clone(), router.available().join(","));
    app.set_log_path(log_file.display().to_string());
    // `/news` 从项目根读 `templates/news.md`（读不到用内置副本）。
    app.set_project_root(project_root.clone());

    tracing::info!("tui starting");
    run_tui(app, ev_rx, cmd_tx).await?;
    tracing::info!("tui exited");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_cfg(dir: &Path, root: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let cfg = dir.join("config.toml");
        // TOML 基本串里反斜杠是转义符，Windows 路径要换成正斜杠。
        std::fs::write(
            &cfg,
            format!("[agent]\nroot = \"{}\"\n", root.replace('\\', "/")),
        )
        .unwrap();
        cfg
    }

    /// 回归：日志曾写到代码仓库里的 `config/logs/agent.log`。
    #[test]
    fn log_dir_is_not_under_config() {
        let d = resolve_log_dir(Path::new("config/config.toml"), None);
        assert!(d.ends_with("logs"), "{d:?}");
        assert!(!d.starts_with("config"), "日志不该落在 config/ 下: {d:?}");
    }

    /// 配置里改了 root，日志目录要跟着走。
    #[test]
    fn log_dir_follows_configured_root() {
        let tmp = std::env::temp_dir().join("agent-cli-logdir-test");
        let data = tmp.join("data");
        let cfg = write_cfg(&tmp, &data.display().to_string());
        assert_eq!(
            resolve_log_dir(&cfg, None),
            data.join("logs"),
            "日志目录应挂在 agent.root 下"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 配置坏掉（语法错）时也不能让日志功能连带失效。
    #[test]
    fn broken_config_falls_back_to_default_root() {
        let tmp = std::env::temp_dir().join("agent-cli-logdir-broken-test");
        std::fs::create_dir_all(&tmp).unwrap();
        let cfg = tmp.join("config.toml");
        std::fs::write(&cfg, "这不是 TOML = = =\n").unwrap();
        let d = resolve_log_dir(&cfg, None);
        assert!(d.ends_with("logs"), "{d:?}");
        assert!(!d.starts_with(&tmp), "不该把日志丢进坏配置所在目录");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn env_override_wins_and_blank_is_ignored() {
        let cfg = Path::new("config/config.toml");
        assert_eq!(
            resolve_log_dir(cfg, Some("/tmp/agent-logs".into())),
            PathBuf::from("/tmp/agent-logs")
        );
        // 空白串视为未设置，继续走配置
        let d = resolve_log_dir(cfg, Some("   ".into()));
        assert!(d.ends_with("logs"), "{d:?}");
    }

    // ---- /resume 的任务前缀解析 ----

    fn summary(id: &str, title: &str) -> agent_core::TaskSummary {
        agent_core::TaskSummary {
            id: agent_core::ids::TaskId::from_raw(id),
            title: title.into(),
            state: agent_core::task::TaskState::Done,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn resolve_task_accepts_full_id_and_unique_prefix() {
        let tasks = vec![
            summary("7f3a9c2b-1111-2222", "写脚本"),
            summary("b4d5e6f7-3333-4444", "收集新闻"),
        ];
        assert_eq!(
            resolve_task("7f3a9c2b-1111-2222", &tasks).unwrap().title,
            "写脚本"
        );
        assert_eq!(resolve_task("7f3a", &tasks).unwrap().title, "写脚本");
        // 大小写不敏感：用户从日志里复制来的 id 常常是大写
        assert_eq!(resolve_task("B4D5", &tasks).unwrap().title, "收集新闻");
    }

    #[test]
    fn ambiguous_prefix_is_rejected_instead_of_guessing() {
        let tasks = vec![summary("abc1-0000", "一"), summary("abc2-0000", "二")];
        let err = resolve_task("abc", &tasks).unwrap_err();
        assert!(err.contains("匹配到 2 个"), "应报出歧义：{err}");
        assert!(err.contains("/tasks"), "应指路怎么查：{err}");
    }

    #[test]
    fn unknown_or_empty_prefix_explains_what_to_do() {
        let tasks = vec![summary("abc1-0000", "一")];
        let err = resolve_task("zzz", &tasks).unwrap_err();
        assert!(err.contains("找不到"), "{err}");
        assert!(
            resolve_task("  ", &tasks).is_err(),
            "空参数不该被当成任意任务"
        );
    }

    // ---- 续跑：把 id 落成真实上下文 ----

    #[tokio::test]
    async fn apply_resume_loads_snapshot_and_locks_task_identity() {
        let tmp = std::env::temp_dir().join(format!(
            "agent-cli-resume-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let store = JsonlStore::new(&tmp);
        let tid = agent_core::ids::TaskId::new();
        let sid = agent_core::ids::SessionId(tid.0.clone());
        store
            .record(
                &sid,
                &tid,
                agent_core::store::Durability::Transient,
                Event::ContextSnapshot {
                    id: tid.clone(),
                    iteration: 1,
                    model: "m".into(),
                    messages: vec![
                        agent_core::provider::Message::system("你是助理"),
                        agent_core::provider::Message::user("统计文件数"),
                    ],
                    tools: vec![],
                },
            )
            .await
            .unwrap();
        store
            .touch_task(&tid, "统计文件数", agent_core::task::TaskState::Done)
            .await
            .unwrap();

        // 界面只会给出「从哪条任务接上」和一个新的追问。
        let mut spec = TaskSpec::new("再把结果写进 README", String::new());
        spec.resume_from = Some(tid.clone());
        apply_resume(&store, &mut spec).await.expect("应能续跑");

        assert_eq!(spec.id.0, tid.0, "续跑沿用原任务 id（面板里不该多出一条）");
        assert_eq!(spec.title, "统计文件数", "标题沿用原任务");
        let msgs = spec.resume_messages.expect("应带上上下文");
        assert_eq!(msgs.len(), 2);
        assert_eq!(spec.prompt, "再把结果写进 README", "新指令不能被上下文覆盖");

        // 找不到快照：按新任务继续，并明确告知。
        let mut fresh = TaskSpec::new("x", String::new());
        fresh.resume_from = Some(agent_core::ids::TaskId::new());
        let err = apply_resume(&store, &mut fresh).await.unwrap_err();
        assert!(err.contains("上下文快照"), "{err}");
        assert!(
            fresh.resume_from.is_none(),
            "续跑失败要清掉标记，避免死循环重试"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
