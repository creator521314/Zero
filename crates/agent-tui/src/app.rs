//! UI 状态与事件处理（不含渲染，便于单测）。

use agent_core::event::{Command, Event, Outcome, ToolOutcome};
use agent_core::ids::{ApprovalId, TaskId};
use agent_core::perm::ApprovalRequest;
use agent_core::task::TaskSpec;
use tokio::sync::mpsc;

use crate::theme::glyph;

/// 一条对话行。
#[derive(Debug, Clone)]
pub enum Line {
    User(String),
    Assistant(String),
    /// 折叠的工具调用：名称 + 参数摘要 + 状态。
    Tool {
        name: String,
        summary: String,
        status: ToolStatus,
    },
    /// 思考链（灰显）。
    Reasoning(String),
    System(String),
    Error(String),
    /// 任务完成（成功收尾专用）。
    ///
    /// 与 `Error` 对称：一轮跑完要在满屏正文里一眼扫得到，所以不走
    /// `System` 的灰显——成功色 + 加粗 + 上下分隔线，见 `render.rs`。
    Done(String),
}

/// 工具调用的 UI 状态（四态）。
///
/// 比「成功 / 失败」二值布尔更细：被权限规则或用户拦下的调用**压根没执行**，
/// 需要与「执行了但失败」区分开——符号、配色、要不要显示耗时都不一样。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatus {
    /// 执行中。
    Running,
    /// 执行成功，携带耗时（毫秒）。
    Ok { ms: u64 },
    /// 执行失败，携带原因与耗时。
    Failed { reason: String, ms: u64 },
    /// 被拒绝（权限规则或用户），未执行。
    Rejected { reason: String },
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
    /// 帧计数（每 tick +1）：驱动工作指示器的旋转帧。
    tick: u32,
    /// 编排层事件流是否已断开（用于只提示一次，避免每帧刷屏）。
    backend_down: bool,
    /// 请求下一帧整屏重绘（实现为 `Terminal::clear()`，见 render.rs）。
    ///
    /// ratatui 只发送**差分**：物理屏幕一旦被外部改动（终端重排、别的进程
    /// 碰过它），未变化的单元格就再也不会被重画。但整屏重绘要擦屏、会闪，
    /// 所以只在**用户主动要求**时用：`Ctrl+L`。
    ///
    /// 工具执行完不再自动重绘——黑屏的根因（子进程共享控制台、`chcp` 连带
    /// 擦屏）已在 `agent-tools/src/shell.rs` 从源头修掉（`CREATE_NO_WINDOW`），
    /// 没必要让每次工具调用都闪一下。
    needs_repaint: bool,
    /// 日志文件路径：断连提示里直接给出，省得用户翻配置目录。
    log_path: Option<String>,
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
            tick: 0,
            backend_down: false,
            needs_repaint: false,
            log_path: None,
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
                    status: ToolStatus::Running,
                });
                self.status = "执行工具…".into();
                self.auto_scroll();
            }
            Event::ToolCallEnd {
                outcome,
                ms,
                detail,
                ..
            } => {
                // 认最后一个「执行中」的工具行。精确匹配 call_id 需要额外维护
                // 索引；一期工具串行执行，「最后一个未完成」即当前调用。
                let running = self.lines.iter_mut().rev().find(
                    |l| matches!(l, Line::Tool { status, .. } if *status == ToolStatus::Running),
                );
                if let Some(Line::Tool { status, .. }) = running {
                    *status = match outcome {
                        ToolOutcome::Ok => ToolStatus::Ok { ms },
                        ToolOutcome::Failed => ToolStatus::Failed {
                            reason: detail.unwrap_or_else(|| "未提供错误详情".into()),
                            ms,
                        },
                        ToolOutcome::Denied | ToolOutcome::Rejected => ToolStatus::Rejected {
                            reason: detail.unwrap_or_else(|| "未说明原因".into()),
                        },
                    };
                }
                self.status = match outcome {
                    ToolOutcome::Ok => format!("工具完成 ({ms}ms)"),
                    ToolOutcome::Failed => format!("工具失败 ({ms}ms)"),
                    ToolOutcome::Denied => "被权限规则拒绝".into(),
                    ToolOutcome::Rejected => "已拒绝".into(),
                };
                // 这里**不**请求整屏重绘：正确的整屏重绘要擦屏（`Terminal::clear`），
                // 每次工具调用都擦一下就是每调一次闪一次。黑屏的根因已在源头修掉
                // ——子进程带 `CREATE_NO_WINDOW` 起在自己的控制台里，改代码页、
                // 清屏都碰不到我们的屏幕（见 `agent-tools/src/shell.rs`）。
                // 屏幕万一还是脏了，Ctrl+L 手动救。
                self.auto_scroll();
            }
            // 上下文快照与整轮回复只服务审计复盘，不进对话流。
            Event::ContextSnapshot { .. } | Event::AssistantTurn { .. } => {}
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
                match &outcome {
                    Outcome::Success { summary, artifacts } => {
                        self.lines.push(Line::Done("✔ 任务完成。".into()));
                        // summary 就是最后一轮模型的原文（runner 直接 clone 的
                        // `out.text`），而原文已经作为 Line::Assistant 流式渲染
                        // 过一遍了。再整段塞一次，同一段话就显示两遍——而且还落
                        // 进灰显的系统行，读起来更费劲。
                        //
                        // 只有对不上时才补，两种情况：模型没有流式输出（重放、
                        // 断流），以及「连续多轮没有产出」那条合成摘要。补的时
                        // 候按正文渲染，别用系统提示的灰。
                        let shown = self
                            .lines
                            .iter()
                            .any(|l| matches!(l, Line::Assistant(t) if t.trim() == summary.trim()));
                        if !shown && !summary.trim().is_empty() {
                            self.lines.push(Line::Assistant(summary.clone()));
                        }
                        if !artifacts.is_empty() {
                            let mut s = String::from("产物：");
                            for a in artifacts {
                                s.push_str(&format!("\n  - {a}"));
                            }
                            self.lines.push(Line::System(s));
                        }
                    }
                    // 失败要看得见：灰显的系统提示会被当成一条普通记录划过去。
                    Outcome::Failed { reason } => {
                        self.lines
                            .push(Line::Error(format!("✘ 任务失败: {reason}")));
                    }
                    Outcome::Cancelled => self.lines.push(Line::System("⊘ 任务已取消".into())),
                    Outcome::Paused => self.lines.push(Line::System("⏸ 任务已暂停".into())),
                }
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

    /// 请求下一帧整屏重绘（用于从外部改动过的屏幕上自愈）。
    pub fn request_repaint(&mut self) {
        self.needs_repaint = true;
    }

    /// 取走重绘请求（取走即清零，避免每帧都整屏重画）。
    pub fn take_repaint(&mut self) -> bool {
        std::mem::take(&mut self.needs_repaint)
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
        self.tick = self.tick.wrapping_add(1);
        let elapsed = self.cursor_last_blink.elapsed().as_millis() as u64;
        if elapsed >= Self::CURSOR_BLINK_MS {
            self.cursor_visible = !self.cursor_visible;
            self.cursor_last_blink = std::time::Instant::now();
        }
    }

    /// agent 是否正在工作（有活动任务）。
    ///
    /// 等待审批**不算**工作——那时是球在用户手里。
    pub fn is_working(&self) -> bool {
        self.active_task.is_some() && self.pending.is_none()
    }

    /// 工作指示器当前的旋转帧（由 tick 驱动）。
    pub fn spinner(&self) -> &'static str {
        glyph::SPINNER[(self.tick % glyph::SPINNER.len() as u32) as usize]
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

    /// 记录日志文件路径（启动时由装配层注入）。
    ///
    /// 用途有二：启动横幅里告知位置；断连提示中直接引用完整路径，
    /// 省去用户去配置目录翻找。
    pub fn set_log_path(&mut self, path: impl Into<String>) {
        let path = path.into();
        self.lines.push(Line::System(format!("日志文件: {path}")));
        self.log_path = Some(path);
    }

    /// 编排层事件流已断开：TUI 必须显式告知用户。
    ///
    /// 事件流关闭后 `try_recv` 只会返回 `Empty`，界面表现与「模型正在思考」
    /// 完全一致——用户无从判断后端是死是活，只能干等。这里落一条醒目错误行，
    /// 并给出可执行的下一步（看日志 / 退出）。
    pub fn backend_disconnected(&mut self) {
        if self.backend_down {
            return; // 只提示一次，避免每帧刷屏
        }
        self.backend_down = true;
        self.pending = None;
        self.status = "后端已断开".into();
        let hint = match &self.log_path {
            Some(p) => format!("请查看日志定位原因：{p}"),
            None => "请查看日志定位原因。".to_string(),
        };
        self.lines.push(Line::Error(format!(
            "后端事件流已断开：Agent 循环已退出或崩溃，后续操作不会再有响应。\n\
             {hint}\n按 Ctrl+C 退出。"
        )));
        self.scroll_to_bottom();
    }

    /// 编排层是否已断开。
    pub fn is_backend_down(&self) -> bool {
        self.backend_down
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

    // ---- 工具调用四态流转 ----

    fn start_tool(app: &mut App, name: &str) {
        app.apply_event(Event::ToolCallStart {
            id: TaskId::new(),
            call_id: agent_core::ids::ToolCallId::new(),
            name: name.into(),
            args: serde_json::json!({ "command": "ls" }),
        });
    }

    fn end_tool(app: &mut App, outcome: ToolOutcome, ms: u64, detail: Option<&str>) {
        app.apply_event(Event::ToolCallEnd {
            id: TaskId::new(),
            call_id: agent_core::ids::ToolCallId::new(),
            outcome,
            ms,
            detail: detail.map(str::to_string),
            output: None,
        });
    }

    fn last_tool_status(app: &App) -> &ToolStatus {
        match app.lines.last() {
            Some(Line::Tool { status, .. }) => status,
            other => panic!("最后一行应为工具行，实际 {other:?}"),
        }
    }

    #[test]
    fn tool_call_start_enters_running() {
        let mut app = App::default();
        start_tool(&mut app, "shell_exec");
        assert_eq!(last_tool_status(&app), &ToolStatus::Running);
    }

    #[test]
    fn tool_call_end_ok_carries_ms() {
        let mut app = App::default();
        start_tool(&mut app, "shell_exec");
        end_tool(&mut app, ToolOutcome::Ok, 37, None);
        assert_eq!(last_tool_status(&app), &ToolStatus::Ok { ms: 37 });
        assert!(app.status.contains("37ms"), "状态栏应显示耗时");
    }

    #[test]
    fn tool_call_end_failed_carries_reason_and_ms() {
        let mut app = App::default();
        start_tool(&mut app, "shell_exec");
        end_tool(&mut app, ToolOutcome::Failed, 9, Some("命令退出码 1"));
        assert_eq!(
            last_tool_status(&app),
            &ToolStatus::Failed {
                reason: "命令退出码 1".into(),
                ms: 9,
            }
        );
    }

    #[test]
    fn tool_call_end_denied_and_rejected_share_rejected_status() {
        // 两者都是「未被许可、压根没执行」，UI 上归为一态。
        for outcome in [ToolOutcome::Denied, ToolOutcome::Rejected] {
            let mut app = App::default();
            start_tool(&mut app, "shell_exec");
            end_tool(&mut app, outcome, 0, Some("命中高危规则"));
            assert_eq!(
                last_tool_status(&app),
                &ToolStatus::Rejected {
                    reason: "命中高危规则".into()
                },
                "{outcome:?} 应映射为 Rejected"
            );
        }
    }

    #[test]
    fn tool_call_end_without_detail_falls_back_to_placeholder() {
        // 旧 jsonl 日志缺 detail 字段（serde 默认 None）时不应渲染出空原因。
        let mut app = App::default();
        start_tool(&mut app, "shell_exec");
        end_tool(&mut app, ToolOutcome::Failed, 5, None);
        assert_eq!(
            last_tool_status(&app),
            &ToolStatus::Failed {
                reason: "未提供错误详情".into(),
                ms: 5,
            }
        );
    }

    #[test]
    fn tool_call_end_does_not_overwrite_finished_line() {
        // 迟到／重复的 ToolCallEnd 不应改写已经终结的工具行。
        let mut app = App::default();
        start_tool(&mut app, "shell_exec");
        end_tool(&mut app, ToolOutcome::Ok, 1, None);
        end_tool(&mut app, ToolOutcome::Failed, 2, Some("迟到的事件"));
        assert_eq!(last_tool_status(&app), &ToolStatus::Ok { ms: 1 });
    }

    // ---- 任务收尾：不要把最后一段回答显示两遍 ----

    /// 回归：`task_finished.summary` 就是最后一轮模型的原文（runner 直接
    /// clone 的 `out.text`，实测与 `assistant_turn.text` 字节相同），而原文
    /// 已经流式渲染过一遍。收尾时再整段塞一遍，同一段话就出现两次，且第二次
    /// 是灰显的系统行——「任务完成之后还会输出这么多暗色内容」即此。
    #[test]
    fn task_finished_does_not_duplicate_streamed_text() {
        let mut app = App::default();
        let id = TaskId::new();
        let text = "## 项目介绍\n\n一个个人独占、跑在自有设备上的 Agent。";
        app.apply_event(Event::TextDelta {
            id: id.clone(),
            text: text.into(),
        });
        app.apply_event(Event::TaskFinished {
            id,
            outcome: Outcome::Success {
                summary: text.into(),
                artifacts: vec![],
            },
        });

        let shown = app
            .lines
            .iter()
            .filter(|l| match l {
                Line::Assistant(s) | Line::System(s) => s.contains("一个个人独占"),
                _ => false,
            })
            .count();
        assert_eq!(shown, 1, "同一段回答只应显示一次：{:?}", app.lines);
        assert!(
            app.lines
                .iter()
                .any(|l| matches!(l, Line::Done(s) if s.contains("✔ 任务完成"))),
            "完成标记仍要显示，且走 Done 的显眼样式而非 System 的灰"
        );
    }

    /// 反面：模型没有流式输出（重放、断流），或收尾摘要是「连续多轮没有产出」
    /// 那条合成文本时——summary 是唯一来源，必须补上，且按正文渲染。
    #[test]
    fn task_finished_shows_summary_that_was_never_streamed() {
        let mut app = App::default();
        app.apply_event(Event::TaskFinished {
            id: TaskId::new(),
            outcome: Outcome::Success {
                summary: "连续 4 轮没有产生新的文件或有效输出，任务可能已停滞。".into(),
                artifacts: vec!["artifacts/report.md".into()],
            },
        });
        assert!(
            app.lines
                .iter()
                .any(|l| matches!(l, Line::Assistant(s) if s.contains("任务可能已停滞"))),
            "没流式过的摘要要补上，且是正文（亮色）行：{:?}",
            app.lines
        );
        assert!(
            app.lines
                .iter()
                .any(|l| matches!(l, Line::System(s) if s.contains("report.md"))),
            "产物仍要列出"
        );
    }

    #[test]
    fn task_failed_is_rendered_as_error() {
        // 失败不该和普通系统提示一样灰着划过去。
        let mut app = App::default();
        app.apply_event(Event::TaskFinished {
            id: TaskId::new(),
            outcome: Outcome::Failed {
                reason: "provider 超时".into(),
            },
        });
        assert!(
            app.lines
                .iter()
                .any(|l| matches!(l, Line::Error(s) if s.contains("provider 超时"))),
            "失败应以错误样式显示：{:?}",
            app.lines
        );
    }

    #[test]
    fn repaint_request_is_one_shot() {
        // 取走即清零：否则每帧都整屏重画（擦屏会闪），白费带宽。
        let mut app = App::default();
        app.request_repaint();
        assert!(app.take_repaint());
        assert!(!app.take_repaint(), "第二次取应为空");
    }

    // ---- 后端断开 ----

    fn pending_approval() -> PendingApproval {
        PendingApproval {
            id: ApprovalId::new(),
            req: ApprovalRequest {
                id: ApprovalId::new(),
                task_id: TaskId::new(),
                tool: "shell".into(),
                args: serde_json::json!({ "command": "rm -rf /" }),
                risk: agent_core::perm::Risk::High,
                reason: "命中高危命令规则".into(),
                matched_rule: None,
            },
        }
    }

    #[test]
    fn backend_disconnected_reports_exactly_once() {
        // 事件流关闭后每帧的 try_recv 都返回 Disconnected，
        // 若不幂等，对话区会被同一条错误刷爆。
        let mut app = App::default();
        let before = app.lines.len();
        app.backend_disconnected();
        app.backend_disconnected();
        app.backend_disconnected();
        assert_eq!(app.lines.len(), before + 1, "重复调用应只落一条提示");
        assert!(app.is_backend_down());
        assert_eq!(app.status, "后端已断开");
    }

    #[test]
    fn backend_disconnected_dismisses_pending_approval() {
        // 后端已死，弹层还挂在那儿只会让用户白白按键。
        let mut app = App {
            pending: Some(pending_approval()),
            ..Default::default()
        };
        app.backend_disconnected();
        assert!(app.pending.is_none(), "断连后应撤下审批弹层");
    }

    #[test]
    fn disconnect_hint_carries_log_path() {
        // 提示里给出完整路径，用户才不必去翻配置目录找日志。
        let mut app = App::default();
        app.set_log_path(r"C:\tmp\agent.log");
        assert!(
            app.lines
                .iter()
                .any(|l| matches!(l, Line::System(s) if s.contains(r"C:\tmp\agent.log"))),
            "启动时应告知日志位置"
        );

        app.backend_disconnected();
        assert!(
            app.lines
                .iter()
                .any(|l| matches!(l, Line::Error(s) if s.contains(r"C:\tmp\agent.log"))),
            "断连提示应带上日志路径"
        );
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
