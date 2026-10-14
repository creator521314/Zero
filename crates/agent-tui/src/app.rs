//! UI 状态与事件处理（不含渲染，便于单测）。

use agent_core::event::{Command, Event, Outcome};
use agent_core::ids::{ApprovalId, TaskId};
use agent_core::perm::ApprovalRequest;
use agent_core::task::TaskSpec;
use tokio::sync::mpsc;

/// 一条对话行。
#[derive(Debug, Clone)]
pub enum Line {
    User(String),
    Assistant(String),
    /// 折叠的工具调用：名称 + 参数摘要 + 状态。
    Tool {
        name: String,
        summary: String,
        done: bool,
        ok: bool,
    },
    /// 思考链（灰显）。
    Reasoning(String),
    System(String),
    Error(String),
}

/// 待审批弹层。
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub id: ApprovalId,
    pub req: ApprovalRequest,
}

/// 输入框状态。
#[derive(Default)]
pub struct InputState {
    pub buffer: String,
    pub cursor: usize,
}

impl InputState {
    pub fn insert_char(&mut self, c: char) {
        self.buffer.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let prev = prev_char_boundary(&self.buffer, self.cursor);
            self.buffer.drain(prev..self.cursor);
            self.cursor = prev;
        }
    }
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.buffer)
    }
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.saturating_sub(1);
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// UI 应用状态。
pub struct App {
    pub lines: Vec<Line>,
    pub input: InputState,
    pub pending: Option<PendingApproval>,
    pub active_task: Option<TaskId>,
    pub tier: String,
    pub provider: String,
    pub status: String,
    pub scroll_from_bottom: u16,
    pub should_quit: bool,
    /// 是否跟随输出（自动滚到底）。
    pub follow: bool,
}

impl Default for App {
    fn default() -> Self {
        Self {
            lines: vec![Line::System(
                "个人 Agent 已启动。输入任务并回车提交；Esc 中断；Ctrl+C 两次退出。".into(),
            )],
            input: InputState::default(),
            pending: None,
            active_task: None,
            tier: "L2".into(),
            provider: "-".into(),
            status: "就绪".into(),
            scroll_from_bottom: 0,
            should_quit: false,
            follow: true,
        }
    }
}

/// 由按键解析出的动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    None,
    Submit(String),
    Cancel,
    Approve { remember: bool },
    Reject,
    Quit,
}

impl App {
    /// 应用一条 Agent 事件。
    pub fn apply_event(&mut self, ev: Event) {
        match ev {
            Event::TaskStarted { id, title } => {
                self.active_task = Some(id);
                self.status = format!("执行中: {title}");
                self.lines.push(Line::System(format!("▶ 任务开始: {title}")));
                self.scroll_to_bottom();
            }
            Event::TextDelta { text, .. } => {
                match self.lines.last_mut() {
                    Some(Line::Assistant(s)) => s.push_str(&text),
                    _ => self.lines.push(Line::Assistant(text)),
                }
                self.auto_scroll();
            }
            Event::ReasoningDelta { text, .. } => {
                match self.lines.last_mut() {
                    Some(Line::Reasoning(s)) => s.push_str(&text),
                    _ => self.lines.push(Line::Reasoning(text)),
                }
                self.auto_scroll();
            }
            Event::ToolCallStart { name, args, .. } => {
                let summary = summarize_args(&args);
                self.lines.push(Line::Tool {
                    name,
                    summary,
                    done: false,
                    ok: true,
                });
                self.status = "执行工具…".into();
                self.auto_scroll();
            }
            Event::ToolCallEnd { ok, ms, .. } => {
                if let Some(Line::Tool { done, ok: o, .. }) =
                    self.lines.iter_mut().rev().find(|l| matches!(l, Line::Tool { done, .. } if !*done))
                {
                    *done = true;
                    *o = ok;
                }
                self.status = format!("工具完成 ({ms}ms)");
                self.auto_scroll();
            }
            Event::AwaitingApproval { approval_id, req, .. } => {
                self.status = "等待确认…".into();
                self.pending = Some(PendingApproval {
                    id: approval_id,
                    req,
                });
            }
            Event::TaskFinished { outcome, .. } => {
                let msg = match &outcome {
                    Outcome::Success { summary, artifacts } => {
                        let mut s = format!("✔ 任务完成。\n{summary}");
                        if !artifacts.is_empty() {
                            s.push_str("\n产物：");
                            for a in artifacts {
                                s.push_str(&format!("\n  - {a}"));
                            }
                        }
                        s
                    }
                    Outcome::Failed { reason } => format!("✘ 任务失败: {reason}"),
                    Outcome::Cancelled => "⊘ 任务已取消".into(),
                    Outcome::Paused => "⏸ 任务已暂停".into(),
                };
                self.lines.push(Line::System(msg));
                self.active_task = None;
                self.status = "就绪".into();
                self.auto_scroll();
            }
            Event::Error { err, .. } => {
                self.lines.push(Line::Error(format!("{}: {}", err.kind, err.message)));
                self.auto_scroll();
            }
        }
    }

    fn auto_scroll(&mut self) {
        if self.follow {
            self.scroll_from_bottom = 0;
        }
    }

    fn scroll_to_bottom(&mut self) {
        self.scroll_from_bottom = 0;
        self.follow = true;
    }

    /// 处理普通字符输入。
    pub fn on_char(&mut self, c: char) {
        self.input.insert_char(c);
    }

    pub fn on_backspace(&mut self) {
        self.input.backspace();
    }

    /// 提交当前输入；空输入返回 None。
    pub fn submit(&mut self) -> Option<Command> {
        let text = self.input.take();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        self.lines.push(Line::User(trimmed.to_string()));
        self.scroll_to_bottom();
        let mut spec = TaskSpec::new(trimmed.to_string(), String::new());
        spec.workspace = String::new(); // 由编排层补齐
        Some(Command::SubmitTask(spec))
    }

    /// 滚动。
    pub fn scroll_up(&mut self, n: u16) {
        self.follow = false;
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(n);
    }
    pub fn scroll_down(&mut self, n: u16) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(n);
        if self.scroll_from_bottom == 0 {
            self.follow = true;
        }
    }

    /// 审批应答。
    pub fn take_approval(&mut self, approve: bool, remember: bool) -> Option<Command> {
        let p = self.pending.take()?;
        self.status = "就绪".into();
        Some(if approve {
            let _ = remember; // 由编排层根据 remember 登记批量批准
            Command::Approve(p.id)
        } else {
            Command::Reject {
                approval_id: p.id,
                reason: "用户拒绝".into(),
            }
        })
    }

    /// 中断当前任务。
    pub fn cancel_command(&self) -> Option<Command> {
        self.active_task.clone().map(Command::Cancel)
    }
}

fn summarize_args(args: &serde_json::Value) -> String {
    // shell_exec 显示命令；文件操作显示路径；其余显示紧凑 JSON
    if let Some(cmd) = args.get("cmd").and_then(|v| v.as_str()) {
        return truncate(cmd, 60);
    }
    if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
        return truncate(p, 60);
    }
    if let Some(u) = args.get("url").and_then(|v| v.as_str()) {
        return truncate(u, 60);
    }
    truncate(&args.to_string(), 60)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

/// 前端命令通道包装。
pub struct FrontendHandle {
    pub tx: mpsc::Sender<Command>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_editing_handles_multibyte() {
        let mut s = InputState::default();
        for c in "写个脚本".chars() {
            s.insert_char(c);
        }
        assert_eq!(s.buffer, "写个脚本");
        s.backspace();
        assert_eq!(s.buffer, "写个脚");
        assert!(s.buffer.is_char_boundary(s.cursor));
    }

    #[test]
    fn stream_deltas_merge_into_one_line() {
        let mut app = App::default();
        let id = TaskId::new();
        for t in ["Hel", "lo", " world"] {
            app.apply_event(Event::TextDelta {
                id: id.clone(),
                text: t.into(),
            });
        }
        let assistant = app
            .lines
            .iter()
            .filter(|l| matches!(l, Line::Assistant(_)))
            .count();
        assert_eq!(assistant, 1, "连续增量应合并为一行");
    }

    #[test]
    fn submit_empty_returns_none() {
        let mut app = App::default();
        assert!(app.submit().is_none());
        app.on_char('a');
        assert!(app.submit().is_some());
    }
}
