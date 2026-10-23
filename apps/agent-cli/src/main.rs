//! agent —— 个人 Agent 入口（TUI 模式）。
//!
//! 装配职责：读配置 → 建 provider 路由 → 建工具集 → 建权限引擎 → 拉起 TUI + Agent 循环。

use std::path::{Path, PathBuf};
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

                        // 「批准本任务同类操作」的登记回调：让权限引擎记住
                        // (task, tool)，该任务后续同类调用在 L3 下直接放行，
                        // 不再逐一打扰用户。
                        let engine = perm2.clone();
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
                                batch_approve: Some(Arc::new(
                                    move |task_id: String, tool: String| {
                                        let eng = engine.clone();
                                        // 回调签名是同步 `Fn`，登记内部却是 async：
                                        // spawn 一个短任务执行，不阻塞审批分支。
                                        tokio::spawn(async move {
                                            eng.approve_for_task(&task_id, &tool).await;
                                        });
                                    },
                                )),
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
                    Command::Quit => break,
                }
            }
        });
    }

    let mut app = App::with_identity(cfg.agent.default_tier.clone(), router.available().join(","));
    app.set_log_path(log_file.display().to_string());

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
}
