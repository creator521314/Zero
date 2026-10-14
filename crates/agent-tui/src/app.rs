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
    /// 字节偏移（始终位于 char boundary）。
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
    /// Delete：删光标右侧一个字符。
    pub fn delete(&mut self) {
        if self.cursor < self.buffer.len() {
            let next = next_char_boundary(&self.buffer, self.cursor);
            self.buffer.drain(self.cursor..next);
        }
    }
    /// 光标左移一个字符。
    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = prev_char_boundary(&self.buffer, self.cursor);
        }
    }
    /// 光标右移一个字符。
    pub fn move_right(&mut self) {
        if self.cursor < self.buffer.len() {
            self.cursor = next_char_boundary(&self.buffer, self.cursor);
        }
    }
    pub fn move_home(&mut self) {
        self.cursor = 0;
    }
    pub fn move_end(&mut self) {
        self.cursor = self.buffer.len();
    }
    /// 光标所在的字符索引（用于渲染定位，按 char 计非字节）。
    pub fn cursor_char_index(&self) -> usize {
        self.buffer[..self.cursor].chars().count()
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

fn next_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = (idx + 1).min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i.min(s.len())
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
    /// 软光标可见性（由 tick 驱动翻转，实现终端无关的闪烁）。
    pub cursor_visible: bool,
    /// 上次翻转软光标的时刻。
    cursor_last_blink: std::time::Instant,
}

impl App {
    /// 构造带状态栏标识的 App（权限档 / provider 名由装配层传入）。
    pub fn with_identity(tier: impl Into<String>, provider: impl Into<String>) -> Self {
        Self {
            tier: tier.into(),
            provider: provider.into(),
            ..Default::default()
        }
    }
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
            cursor_visible: true,
            cursor_last_blink: std::time::Instant::now(),
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
                self.lines
                    .push(Line::System(format!("▶ 任务开始: {title}")));
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
                if let Some(Line::Tool { done, ok: o, .. }) = self
                    .lines
                    .iter_mut()
                    .rev()
                    .find(|l| matches!(l, Line::Tool { done, .. } if !*done))
                {
                    *done = true;
                    *o = ok;
                }
                self.status = format!("工具完成 ({ms}ms)");
                self.auto_scroll();
            }
            Event::AwaitingApproval {
                approval_id, req, ..
            } => {
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
                self.lines
                    .push(Line::Error(format!("{}: {}", err.kind, err.message)));
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
        self.wake_cursor();
    }

    pub fn on_backspace(&mut self) {
        self.input.backspace();
        self.wake_cursor();
    }

    /// 软光标闪烁周期。
    const CURSOR_BLINK_MS: u64 = 530;

    /// 每个渲染 tick 调用一次：到点则翻转软光标可见性。
    pub fn on_tick(&mut self) {
        let elapsed = self.cursor_last_blink.elapsed().as_millis() as u64;
        if elapsed >= Self::CURSOR_BLINK_MS {
            self.cursor_visible = !self.cursor_visible;
            self.cursor_last_blink = std::time::Instant::now();
        }
    }

    /// 任意键盘活动后立即使光标常亮（避免闪到一半被按键打断的突兀感）。
    pub fn wake_cursor(&mut self) {
        self.cursor_visible = true;
        self.cursor_last_blink = std::time::Instant::now();
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

    // ---- 光标移动 ----

    #[test]
    fn cursor_moves_left_right_over_multibyte() {
        let mut s = InputState::default();
        for c in "你好ab".chars() {
            s.insert_char(c);
        }
        // 5 chars, cursor at end (byte 3+3+1+1 = 8)
        assert_eq!(s.cursor, s.buffer.len());
        assert_eq!(s.cursor_char_index(), 4);

        s.move_left();
        assert_eq!(s.cursor_char_index(), 3);
        s.move_left();
        s.move_left();
        assert_eq!(s.cursor_char_index(), 1);
        // 越界保护：不动
        s.move_left();
        s.move_left();
        s.move_left();
        assert_eq!(s.cursor_char_index(), 0);
        assert_eq!(s.cursor, 0);

        s.move_right();
        assert_eq!(s.cursor_char_index(), 1);
        s.move_right();
        s.move_right();
        s.move_right();
        assert_eq!(s.cursor_char_index(), 4);
        // 右边界保护
        s.move_right();
        assert_eq!(s.cursor, s.buffer.len());
    }

    #[test]
    fn cursor_home_end() {
        let mut s = InputState::default();
        for c in "你好".chars() {
            s.insert_char(c);
        }
        s.move_home();
        assert_eq!(s.cursor, 0);
        s.move_end();
        assert_eq!(s.cursor, s.buffer.len());
    }

    #[test]
    fn insert_at_cursor_not_append() {
        let mut s = InputState::default();
        for c in "ac".chars() {
            s.insert_char(c);
        }
        s.move_left(); // cursor between a and c
        s.insert_char('b');
        assert_eq!(s.buffer, "abc");
    }

    #[test]
    fn backspace_deletes_before_cursor() {
        let mut s = InputState::default();
        for c in "abc".chars() {
            s.insert_char(c);
        }
        s.move_home();
        s.move_right(); // cursor between a and b
        s.backspace();
        assert_eq!(s.buffer, "bc");
        assert_eq!(s.cursor_char_index(), 0);
    }

    #[test]
    fn delete_removes_char_at_cursor() {
        let mut s = InputState::default();
        for c in "abc".chars() {
            s.insert_char(c);
        }
        s.move_home();
        s.delete(); // remove 'a'
        assert_eq!(s.buffer, "bc");
        assert_eq!(s.cursor_char_index(), 0);
        s.delete();
        assert_eq!(s.buffer, "c");
        // 末尾 delete 是 no-op
        s.move_end();
        s.delete();
        assert_eq!(s.buffer, "c");
    }

    #[test]
    fn delete_multibyte_at_cursor() {
        let mut s = InputState::default();
        for c in "你好".chars() {
            s.insert_char(c);
        }
        s.move_home();
        s.delete();
        assert_eq!(s.buffer, "好");
        assert!(s.buffer.is_char_boundary(s.cursor));
    }

    // ---- 软光标闪烁 ----

    #[test]
    fn cursor_blink_toggles_after_period() {
        let mut app = App::default();
        assert!(app.cursor_visible, "初始应可见");
        // 刚启动未到周期：不翻转
        app.on_tick();
        assert!(app.cursor_visible, "未到周期不应翻转");
    }

    #[test]
    fn wake_cursor_forces_visible_and_resets_timer() {
        // 把上次翻转时间推到过去，模拟“该闪了”
        let mut app = App {
            cursor_last_blink: std::time::Instant::now()
                - std::time::Duration::from_millis(App::CURSOR_BLINK_MS + 50),
            cursor_visible: false,
            ..Default::default()
        };
        app.on_tick();
        assert!(app.cursor_visible, "到周期应翻转为可见");

        // 按键活动后立即可见
        app.cursor_visible = false;
        app.on_char('x');
        assert!(app.cursor_visible, "wake_cursor 应立即可见");
    }
}
