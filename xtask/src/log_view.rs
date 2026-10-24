//! 会话日志阅读器 —— 把 jsonl 事件流还原成人能读的时间线。
//!
//! 事件流是唯一真相源，但原始 jsonl 一行一事件、字段层层嵌套，直接 `cat`
//! 根本看不出「模型看到了什么 / 工具返回了什么」。本模块负责把它们串成
//! 一条时间线，让定位问题变成看一段文本。
//!
//! 用法：
//!   cargo xtask log <session.jsonl | task-id> [--full] [--errors]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use agent_core::event::{Event, Outcome, ToolOutcome};
use agent_core::ids::ToolCallId;
use agent_core::provider::Role;
use agent_core::store::{Durability, StoredEvent};
use agent_providers::config::{expand_home, ProvidersFile};

/// 渲染选项。
#[derive(Debug, Clone, Copy, Default)]
pub struct ViewOpts {
    /// 展开完整上下文（每条消息正文）与工具返回原文。
    pub full: bool,
    /// 只看失败项（工具失败 / 被拦 / 报错 / 任务失败）。
    pub errors_only: bool,
}

/// 摘要模式下的截断长度；`--full` 用更长的上限。
const BRIEF: usize = 240;
const VERBOSE: usize = 4000;
/// 工具参数摘要长度。
const ARGS: usize = 160;

/// 定位会话文件：直接给路径，或给任务 ID（自动到 `~/agent/logs/` 下找）。
pub fn resolve(arg: &str) -> PathBuf {
    if arg.ends_with(".jsonl") {
        return PathBuf::from(arg);
    }
    let root = ProvidersFile::load(&ProvidersFile::config_path())
        .map(|c| c.agent.resolved_root())
        .unwrap_or_else(|_| expand_home("~/agent"));
    root.join("logs").join(format!("{arg}.jsonl"))
}

/// 读取并渲染。
pub fn render(path: &Path, opts: &ViewOpts) -> anyhow::Result<String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("读取 {} 失败：{e}", path.display()))?;
    let mut events = Vec::new();
    for line in content.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<StoredEvent>(line) {
            Ok(e) => events.push(e),
            Err(err) => eprintln!("[跳过损坏行] {err}"),
        }
    }
    Ok(render_events(&events, opts))
}

fn render_events(events: &[StoredEvent], opts: &ViewOpts) -> String {
    let limit = if opts.full { VERBOSE } else { BRIEF };
    let mut s = String::new();

    if let Some(first) = events.first() {
        let persistent = events
            .iter()
            .filter(|e| e.durability == Durability::Persistent)
            .count();
        s.push_str(&format!(
            "会话 {} · 任务 {}\n共 {} 条事件（持久 {} / 临时 {}）\n{}\n",
            first.session_id.0,
            first.task_id.0,
            events.len(),
            persistent,
            events.len() - persistent,
            "─".repeat(64)
        ));
    }

    // call_id -> (工具名, 参数)：ToolCallEnd 只带 call_id，展示时要回查。
    let mut calls: HashMap<ToolCallId, (String, serde_json::Value)> = HashMap::new();

    for stored in events {
        match &stored.payload {
            Event::TaskStarted { title, .. } => {
                if !opts.errors_only {
                    s.push_str(&format!("▶ 任务：{title}\n"));
                }
            }
            Event::ContextSnapshot {
                iteration,
                model,
                messages,
                tools,
                ..
            } => {
                if !opts.errors_only {
                    s.push_str(&format!(
                        "\n[轮 {iteration} · {model}] 上下文 {} 条消息 / 工具 {} 个\n",
                        messages.len(),
                        tools.len()
                    ));
                    if opts.full {
                        for m in messages {
                            s.push_str(&format!(
                                "    ▸ {}：{}\n",
                                role_name(&m.role),
                                clip(&m.text(), limit)
                            ));
                        }
                    }
                }
            }
            Event::AssistantTurn {
                text, calls: cs, ..
            } => {
                if !opts.errors_only && !text.trim().is_empty() {
                    s.push_str(&format!("    ◆ 助手：{}\n", clip(text, limit)));
                }
                for c in cs {
                    if !opts.errors_only {
                        s.push_str(&format!(
                            "    → 调用 {}({})\n",
                            c.name,
                            compact(&c.arguments)
                        ));
                    }
                }
            }
            Event::ToolCallStart {
                call_id,
                name,
                args,
                ..
            } => {
                calls.insert(call_id.clone(), (name.clone(), args.clone()));
            }
            Event::ToolCallEnd {
                call_id,
                outcome,
                ms,
                detail,
                output,
                ..
            } => {
                let failed = *outcome != ToolOutcome::Ok;
                if opts.errors_only && !failed {
                    continue;
                }
                let (name, args) = match calls.get(call_id) {
                    Some(v) => v.clone(),
                    None => ("?".to_string(), serde_json::Value::Null),
                };
                match outcome {
                    ToolOutcome::Ok => {
                        s.push_str(&format!(
                            "    ✔ {}({}) → ok ({}ms)\n",
                            name,
                            compact(&args),
                            ms
                        ));
                    }
                    ToolOutcome::Failed => {
                        s.push_str(&format!(
                            "    ✖ {}({}) → 失败 ({}ms)\n",
                            name,
                            compact(&args),
                            ms
                        ));
                    }
                    ToolOutcome::Denied => {
                        s.push_str(&format!(
                            "    ⊘ {}({}) → 被权限规则拦截\n",
                            name,
                            compact(&args)
                        ));
                    }
                    ToolOutcome::Rejected => {
                        s.push_str(&format!(
                            "    ⊘ {}({}) → 被用户拒绝\n",
                            name,
                            compact(&args)
                        ));
                    }
                }
                if let Some(d) = detail {
                    s.push_str(&format!("        原因：{}\n", clip(d, limit)));
                }
                // 返回原文：失败时必看（含工具给出的错误文本），成功时按需。
                if let Some(o) = output {
                    if Some(o) != detail.as_ref() {
                        s.push_str(&format!("        返回：{}\n", clip(o, limit)));
                    }
                }
            }
            Event::AwaitingApproval { req, .. } => {
                if !opts.errors_only {
                    s.push_str(&format!(
                        "    ⏸ 等待审批：{}({})\n",
                        req.tool,
                        compact(&req.args)
                    ));
                }
            }
            Event::TaskFinished { outcome, .. } => match outcome {
                Outcome::Success { summary, artifacts } => {
                    if opts.errors_only {
                        continue;
                    }
                    s.push_str(&format!("\n✔ 任务完成：{}\n", clip(summary, limit)));
                    for a in artifacts {
                        s.push_str(&format!("    产物：{a}\n"));
                    }
                }
                Outcome::Failed { reason } => {
                    s.push_str(&format!("\n✘ 任务失败：{}\n", clip(reason, limit)));
                }
                Outcome::Cancelled => s.push_str("\n⊘ 任务已取消\n"),
                Outcome::Paused => s.push_str("\n⏸ 任务已暂停\n"),
            },
            Event::Error { err, .. } => {
                s.push_str(&format!(
                    "\n✖ 错误[{}]：{}\n",
                    err.kind,
                    clip(&err.message, limit)
                ));
            }
            // 逐字增量默认不落盘；即便落了也不进时间线（内容已在整轮回复里）。
            Event::TextDelta { .. } | Event::ReasoningDelta { .. } => {}
            // 纯界面事件不落盘，也不会出现在审计日志里。
            Event::TaskList { .. } | Event::TaskOpened { .. } => {}
        }
    }
    s
}

fn role_name(r: &Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn compact(v: &serde_json::Value) -> String {
    clip(&serde_json::to_string(v).unwrap_or_default(), ARGS)
}

/// 截断：中间内容换成省略号，尾部保留（错误原因常在末尾）。
fn clip(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let head: String = chars[..max * 2 / 3].iter().collect();
    let tail: String = chars[chars.len() - max / 3..].iter().collect();
    format!("{head} …{tail}")
}
