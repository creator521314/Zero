//! UI 状态与事件处理（不含渲染，便于单测）。

use std::time::{Duration, Instant};

use agent_core::event::{Command, Event, Outcome, ToolOutcome};
use agent_core::ids::{ApprovalId, TaskId};
use agent_core::perm::{ApprovalRequest, PermissionTier};
use agent_core::store::TaskSummary;
use agent_core::task::{TaskSpec, TaskState};
use tokio::sync::mpsc;

use crate::theme::{glyph, tier_label};

/// 一期可执行的 `/` 命令（顺序即补全菜单的展示顺序）。
///
/// 只列**真正能落地**的命令：`/clear` 清空对话、`/help` 显示帮助、`/tier`
/// 切换权限档、`/tasks` 列历史任务、`/resume` 切到某个历史任务继续、`/news`
/// 填入新闻收集提示词，都只改本地状态或读 store 后回灌界面。设计稿里的
/// `/model`（切模型）、`/compact`（手动压缩上下文）依赖后端能力，尚未落地，
/// 故不列入——遵循「只提示已实现功能」的原则，避免补出执行不了的入口。
pub const SLASH_COMMANDS: &[&str] = &["/clear", "/help", "/tier", "/tasks", "/resume", "/news"];

/// 内置的新闻收集模板：`templates/news.md` 不存在时的兜底。
///
/// 直接编译进二进制，保证「零配置也能用」——仓库里那份是可改的副本，
/// 不是唯一来源。
const EMBEDDED_NEWS_TEMPLATE: &str = include_str!("../../../templates/news.md");

/// 右上角瞬态通知：`on_tick` 里到期自动清除，`text` 供渲染读取。
pub struct Toast {
    pub text: String,
    /// 到期时刻（`Instant` 单调时钟，不受系统改时间影响）。
    until: Instant,
}

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
    /// 把光标定位到第 `char_idx` 个字符**之后**（`char_idx` 可到 `char 总数`）。
    ///
    /// 供鼠标点击定位用：点击列先折算成字符索引，再落回字节偏移。
    pub fn set_char_index(&mut self, char_idx: usize) {
        self.cursor = self
            .buffer
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.buffer.len());
    }
    /// 光标所在的字符索引（用于渲染定位，按 char 计非字节）。
    pub fn cursor_char_index(&self) -> usize {
        self.buffer[..self.cursor].chars().count()
    }
    /// Ctrl+Enter：在光标处插入换行，支持多行输入。
    pub fn insert_newline(&mut self) {
        self.buffer.insert(self.cursor, '\n');
        self.cursor += 1;
    }
    /// 光标所在的硬行号（0 起）与该行内字符列（按 char 计）。
    ///
    /// 供多行渲染定位软光标用：行号用于垂直滚动，列号用于横向滚动与反显。
    pub fn cursor_line_col(&self) -> (usize, usize) {
        let before = &self.buffer[..self.cursor];
        let line = before.matches('\n').count();
        let col = before
            .rfind('\n')
            .map(|i| before[i + 1..].chars().count())
            .unwrap_or_else(|| before.chars().count());
        (line, col)
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
    /// 待转交编排层的 `/` 命令（`/tasks`、`/resume` 需要 store，UI 够不着）。
    pub pending_command: Option<Command>,
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
    /// `/` 命令补全菜单是否激活（输入 `/` 时打开，`Esc` / 执行后关闭）。
    pub completion_active: bool,
    /// `/` 命令补全菜单当前高亮的候选索引。
    pub completion_sel: usize,
    /// 已提交的输入历史（时间正序，末尾最新；连续重复只记一条）。
    history: Vec<String>,
    /// 历史回溯游标：`Some(i)` 表示正在查看 `history[i]`；用户编辑后置回 None。
    history_cursor: Option<usize>,
    /// footer 是否可见（`Ctrl+H` 切换）。
    pub footer_visible: bool,
    /// 右上角瞬态通知；None 表示当前没有。
    pub toast: Option<Toast>,
    /// 最近一次从 store 取回的历史任务（`/tasks` 与任务面板共用）。
    pub tasks: Vec<TaskSummary>,
    /// 任务面板是否打开（`Ctrl+G`）。
    pub panel_open: bool,
    /// 任务面板高亮行。
    pub panel_sel: usize,
    /// 当前对话区正在看的是哪个任务。
    ///
    /// 有值意味着「接着这条任务说」：下一次提交沿用它当任务 id，并把该任务的
    /// 上下文快照作为续跑上下文（由编排层补齐），所以对话是连续的，而不是
    /// 每按一次回车就开一条新任务、模型忘光前面做过什么。
    pub viewing: Option<(TaskId, String)>,
    /// 项目根（装配层注入）：`/news` 从这里读 `templates/news.md`。
    project_root: Option<std::path::PathBuf>,
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
            pending_command: None,
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
            completion_active: false,
            completion_sel: 0,
            history: Vec::new(),
            history_cursor: None,
            footer_visible: true,
            toast: None,
            tasks: Vec::new(),
            panel_open: false,
            panel_sel: 0,
            viewing: None,
            project_root: None,
        }
    }
}

/// 由按键解析出的动作。
///
/// 不派生 `PartialEq`：`Submit` 携带完整 [`TaskSpec`]（含 f64 / 消息列表），
/// 比较整个规格没有意义，测试里一律用 `matches!`。
#[derive(Debug, Clone)]
pub enum UiAction {
    None,
    Submit(TaskSpec),
    Cancel,
    Approve {
        remember: bool,
    },
    Reject,
    /// 需要编排层执行的动作（`/tasks`、`/resume`、面板选中某条任务）。
    Command(Command),
    Quit,
}

impl App {
    /// 应用一条 Agent 事件。
    pub fn apply_event(&mut self, ev: Event) {
        match ev {
            Event::TaskStarted { id, title } => {
                self.active_task = Some(id.clone());
                self.status = format!("执行中: {title}");
                // 对话区从此归这条任务：后续输入沿用它当任务 id 续跑。
                self.viewing = Some((id, title.clone()));
                self.panel_open = false;
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
            // 上下文快照只服务审计复盘，不进对话流。
            Event::ContextSnapshot { .. } => {}
            // 整轮回复：实时路径下它与逐字增量重合（增量已经拼好了），但
            // **历史转写**里通常只有它——`TextDelta` 默认不落盘。所以：内容
            // 与最后一行正文相同就跳过，不同才补，两条路径都不重复。
            Event::AssistantTurn { text, .. } => {
                if !text.trim().is_empty() {
                    let same = matches!(self.lines.last(), Some(Line::Assistant(t)) if t.trim() == text.trim());
                    if !same {
                        self.lines.push(Line::Assistant(text));
                        self.auto_scroll();
                    }
                }
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
            Event::TaskList { tasks } => {
                self.tasks = tasks;
                self.panel_sel = self.panel_sel.min(self.tasks.len().saturating_sub(1));
                // 面板开着就只刷新面板（用户正在里面挑）；否则是 `/tasks`，打到对话区。
                if !self.panel_open {
                    self.print_task_list();
                }
                self.wake_cursor();
            }
            Event::TaskOpened { id, title, events } => {
                self.panel_open = false;
                if events.is_empty() {
                    self.lines.push(Line::Error(format!(
                        "任务「{title}」没有可回放的历史（日志可能已被清理），仍可继续输入。"
                    )));
                    self.viewing = Some((id, title));
                    self.scroll_to_bottom();
                    return;
                }
                // 转写回放走与实时事件完全相同的规则（见 `replay_lines`），
                // 因此「切过去的对话」和「当时看到的对话」长得一样。
                self.lines = replay_lines(&events);
                self.lines.push(Line::System(format!(
                    "已切到任务 {}「{}」：继续输入即接着它的上下文。",
                    short_id(&id),
                    title
                )));
                self.status = format!("查看: {title}");
                self.viewing = Some((id, title));
                self.scroll_to_bottom();
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
        self.leave_history();
        self.wake_cursor();
    }

    pub fn on_backspace(&mut self) {
        self.input.backspace();
        self.leave_history();
        self.wake_cursor();
    }

    /// 软光标闪烁周期。
    const CURSOR_BLINK_MS: u64 = 530;

    /// 每个渲染 tick 调用一次：到点则翻转软光标可见性、清除过期 Toast。
    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        let elapsed = self.cursor_last_blink.elapsed().as_millis() as u64;
        if elapsed >= Self::CURSOR_BLINK_MS {
            self.cursor_visible = !self.cursor_visible;
            self.cursor_last_blink = Instant::now();
        }
        if let Some(t) = &self.toast {
            if t.until <= Instant::now() {
                self.toast = None;
            }
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

    /// 循环切换权限档位 L1 → L2 → L3 → L1（点击 header 徽章时调用）。
    ///
    /// 切换立即影响**之后提交的任务**（见 [`Self::submit`]），当前在跑的任务
    /// 不受影响——档位是任务创建时写进 `TaskSpec` 的。
    pub fn cycle_tier(&mut self) {
        self.tier = match self.tier.to_ascii_uppercase().as_str() {
            "L1" => "L2".into(),
            "L2" => "L3".into(),
            _ => "L1".into(),
        };
        // 切档是瞬态反馈，走右上角 Toast；`status` 语义是「运行状态」，
        // 不该被切档占用（否则会把「就绪 / 执行中」盖掉）。
        self.toast(format!("权限档位 {}", tier_label(&self.tier)));
        self.wake_cursor();
    }

    /// 当前显示档位对应的权限枚举（提交任务时写入 `TaskSpec.tier`）。
    pub fn tier_enum(&self) -> PermissionTier {
        match self.tier.to_ascii_uppercase().as_str() {
            "L1" => PermissionTier::L1,
            "L3" => PermissionTier::L3,
            _ => PermissionTier::L2,
        }
    }

    /// 当前输入匹配到的 `/` 命令候选（按前缀过滤）。
    ///
    /// 只有 buffer 以 `/` 开头（且不在审批弹层）才有候选；返回空即未激活。
    pub fn slash_candidates(&self) -> Vec<&'static str> {
        if self.pending.is_some() {
            return Vec::new();
        }
        let buf = self.input.buffer.trim_start();
        if !buf.starts_with('/') {
            return Vec::new();
        }
        let query = &buf[1..];
        SLASH_COMMANDS
            .iter()
            .filter(|c| c[1..].starts_with(query))
            .copied()
            .collect()
    }

    /// 输入 / 删除后刷新补全状态：有候选则激活并重置高亮，否则关闭。
    pub fn refresh_completion(&mut self) {
        self.completion_active = !self.slash_candidates().is_empty();
        self.completion_sel = 0;
    }

    /// 弹出右上角 Toast（默认 2 秒后自动消失）。
    pub fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast {
            text: text.into(),
            until: Instant::now() + Duration::from_secs(2),
        });
    }

    /// 切换 footer 显隐（`Ctrl+H`）。
    pub fn toggle_footer(&mut self) {
        self.footer_visible = !self.footer_visible;
        self.toast(if self.footer_visible {
            "footer 已显示"
        } else {
            "footer 已隐藏"
        });
        self.wake_cursor();
    }

    /// `Ctrl+R`：循环回溯输入历史（从最新往回，到最早后回到最新）。
    pub fn recall_history(&mut self) {
        if self.history.is_empty() {
            self.toast("没有历史记录");
            return;
        }
        let next = match self.history_cursor {
            None => self.history.len() - 1,    // 首次：最新一条
            Some(0) => self.history.len() - 1, // 已到最早，循环回最新
            Some(i) => i - 1,                  // 上一条更早的
        };
        self.history_cursor = Some(next);
        self.input.buffer = self.history[next].clone();
        self.input.move_end();
        self.wake_cursor();
    }

    /// 用户手动编辑了输入：退出历史回溯状态。
    ///
    /// 回溯游标只在 `recall_history` 里前进；一旦用户自己改了 buffer
    /// （打字、退格、Delete、补全、执行命令），继续按 `Ctrl+R` 就应从
    /// 最新历史重新开始，而不是接着上次的游标。
    pub fn leave_history(&mut self) {
        self.history_cursor = None;
    }

    /// 输入框内容是否是一条可直接执行的 `/` 命令。
    ///
    /// 带参数的两条（`/resume <id>`、`/news <主题>`）在补全菜单里没有意义——
    /// 一旦带上参数，前缀匹配就落空了，回车会把它当普通任务提交给模型。
    /// 所以回车时先用这个判据拦一道。
    pub fn is_slash_command(&self) -> bool {
        let buf = self.input.buffer.trim_start();
        SLASH_COMMANDS.contains(&buf) || buf.starts_with("/resume ") || buf.starts_with("/news ")
    }

    /// 执行一条 `/` 命令；成功执行后清空输入并关闭补全菜单。
    pub fn run_slash_command(&mut self, cmd: &str) -> bool {
        match cmd {
            "/clear" => self.clear_conversation(),
            "/help" => self.show_help(),
            "/tier" => self.cycle_tier(),
            // 这两个不直接改 UI，而是转成 Command 交给编排层（它才够得着 store）。
            "/tasks" => {
                self.panel_open = false;
                self.pending_command = Some(Command::ListTasks);
                self.input.take();
                return true;
            }
            _ if cmd.starts_with("/resume ") => {
                let id = cmd["/resume ".len()..].trim();
                if id.is_empty() {
                    self.toast("用法：/resume <任务ID前几位>");
                    return true;
                }
                self.pending_command = Some(Command::ResumeTask(id.to_string()));
                self.input.take();
                return true;
            }
            // `/news <主题>`：把模板渲染成提示词填进输入框（不直接发送——
            // 让用户先看一眼、改一改，避免一按回车就花掉一次联网任务）。
            _ if cmd == "/news" || cmd.starts_with("/news ") => {
                self.fill_news_prompt(cmd.trim_start_matches("/news").trim());
                return true;
            }
            _ => return false,
        }
        self.input.take();
        self.completion_active = false;
        self.completion_sel = 0;
        self.wake_cursor();
        true
    }

    /// 记录项目根（装配层注入）：`/news` 从这里读 `templates/news.md`。
    pub fn set_project_root(&mut self, root: impl Into<std::path::PathBuf>) {
        self.project_root = Some(root.into());
    }

    /// 新闻收集模板：优先用项目里的 `templates/news.md`（用户可改），
    /// 读不到就用编译进二进制的内置副本——零配置也必须能用。
    fn news_template(&self) -> String {
        let from_disk = self
            .project_root
            .as_ref()
            .map(|r| r.join("templates").join("news.md"))
            .and_then(|p| std::fs::read_to_string(p).ok());
        match from_disk {
            Some(s) if !s.trim().is_empty() => s,
            _ => EMBEDDED_NEWS_TEMPLATE.to_string(),
        }
    }

    /// `/news <主题>`：渲染模板并填进输入框（不自动发送）。
    pub fn fill_news_prompt(&mut self, topic: &str) {
        let topic = topic.trim();
        if topic.is_empty() {
            self.toast("用法：/news <主题>，例如 /news Rust");
            return;
        }
        let rendered = self
            .news_template()
            .replace("{topic}", topic)
            .replace("{period}", "最近 7 天")
            .replace("{slug}", &slugify(topic));
        self.input.buffer = rendered;
        self.input.move_end();
        self.completion_active = false;
        self.completion_sel = 0;
        self.leave_history();
        self.wake_cursor();
        self.toast("已填入新闻收集提示词：确认或修改后回车发送");
    }

    /// 清空对话并开一条新线：下一次提交会作为**新任务**开始（不再续跑当前任务）。
    pub fn clear_conversation(&mut self) {
        self.lines.clear();
        self.viewing = None;
        self.panel_open = false;
        self.lines.push(Line::System(
            "对话已清空（下一条指令将作为新任务开始）。".into(),
        ));
        self.scroll_to_bottom();
    }

    /// 在对话区打印 `/` 命令帮助。
    pub fn show_help(&mut self) {
        for l in [
            "/ 命令：",
            "  /clear  清空对话（开新任务）",
            "  /help   显示本帮助",
            "  /tier   切换权限档 L1→L2→L3",
            "  /tasks  列出历史任务",
            "  /resume <ID前几位>  切到某条历史任务继续",
            "  /news <主题>  填入新闻收集提示词（改完再发送）",
            "  Ctrl+G  打开任务面板（↑/↓ 选择，Enter 切换）",
        ] {
            self.lines.push(Line::System(l.into()));
        }
        self.scroll_to_bottom();
    }

    // ---- 任务面板（Ctrl+G）----

    /// 开关任务面板；打开时向编排层要一份最新列表。
    pub fn toggle_panel(&mut self) {
        self.panel_open = !self.panel_open;
        self.panel_sel = 0;
        if self.panel_open {
            self.pending_command = Some(Command::ListTasks);
            self.toast("任务面板：↑/↓ 选择，Enter 切换，Esc 关闭");
        }
        self.wake_cursor();
    }

    /// 面板内上下移动（到边界不循环：位置感比兜圈更有用）。
    pub fn panel_move(&mut self, delta: isize) {
        if self.tasks.is_empty() {
            return;
        }
        let last = self.tasks.len() - 1;
        let next = self.panel_sel as isize + delta;
        self.panel_sel = next.clamp(0, last as isize) as usize;
        self.wake_cursor();
    }

    /// 选中面板当前行：切到那条任务（Esc / 列表为空时返回 None）。
    pub fn panel_activate(&mut self) -> Option<Command> {
        let t = self.tasks.get(self.panel_sel)?.clone();
        self.panel_open = false;
        Some(Command::ResumeTask(t.id.0.clone()))
    }

    /// 关闭面板（Esc）。
    pub fn close_panel(&mut self) {
        self.panel_open = false;
        self.wake_cursor();
    }

    /// 把历史任务打印进对话区（`/tasks`；面板打开时不打印）。
    pub fn print_task_list(&mut self) {
        if self.tasks.is_empty() {
            self.lines.push(Line::System(
                "还没有历史任务（完成一个任务后就会出现在这里）。".into(),
            ));
            self.scroll_to_bottom();
            return;
        }
        self.lines
            .push(Line::System(format!("历史任务（{}）：", self.tasks.len())));
        for t in &self.tasks {
            self.lines.push(Line::System(format!(
                "  {}",
                task_row(t, self.viewing.as_ref().map(|(id, _)| id))
            )));
        }
        self.lines.push(Line::System(
            "用 /resume <ID前几位> 或 Ctrl+G 切到某条任务继续。".into(),
        ));
        self.scroll_to_bottom();
    }

    /// 提交当前输入；空输入返回 None。
    ///
    /// 返回的 `TaskSpec` 已经带好权限档位与续跑目标，编排层直接执行即可。
    pub fn submit(&mut self) -> Option<TaskSpec> {
        let text = self.input.take();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        // 记入历史（连续重复只记一条，避免连按回车刷出一串相同的）。
        if self.history.last().map(String::as_str) != Some(trimmed) {
            self.history.push(trimmed.to_string());
        }
        self.history_cursor = None;
        self.lines.push(Line::User(trimmed.to_string()));
        self.scroll_to_bottom();
        let mut spec = TaskSpec::new(trimmed.to_string(), String::new());
        spec.tier = self.tier_enum();
        // 工作区由编排层补齐。
        spec.workspace = String::new();

        // 对话区正看着某条任务：沿用它的 id 与标题，并让编排层补上它的上下文，
        // 这样「接着说」是连续的（模型记得刚才做过什么），面板里也仍是同一条任务。
        if let Some((id, title)) = &self.viewing {
            spec.id = id.clone();
            spec.title = title.clone();
            spec.resume_from = Some(id.clone());
        }
        Some(spec)
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
            // remember 必须一路带到编排层：否则 UI 的「批准本任务同类操作」
            // 只是看起来生效，后续同类调用照旧弹窗。
            Command::Approve {
                approval_id: p.id,
                remember,
            }
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

    /// 取走待编排层处理的 `/` 命令（`/tasks`、`/resume`）。
    ///
    /// 与 `take_approval` 同一套路：UI 只负责产生意图，真正要碰 store 的动作
    /// 交给编排层，TUI 因此不需要依赖 agent-store。
    pub fn take_pending_command(&mut self) -> Option<Command> {
        self.pending_command.take()
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

/// 回放一段历史事件，得到与实时渲染一致的对话行。
///
/// 复用 [`App::apply_event`]：增量合并、工具四态、收尾判重这些规则在「实时」与
/// 「回看」两条路径上必须一模一样，所以不复制一份渲染逻辑——复制出来的第二份
/// 迟早会跟第一份走散。
fn replay_lines(events: &[Event]) -> Vec<Line> {
    let mut ctx = App::default();
    ctx.lines.clear();
    for ev in events {
        // 面板/列表事件本身不属于对话内容（正常也不会落盘，这里防御性跳过）。
        if matches!(ev, Event::TaskList { .. } | Event::TaskOpened { .. }) {
            continue;
        }
        ctx.apply_event(ev.clone());
    }
    ctx.lines
}

/// 任务 id 的前 8 位——列表里够区分，又不会把整行撑满（uuid 有 36 字符）。
fn short_id(id: &TaskId) -> String {
    id.0.chars().take(8).collect()
}

/// 任务状态 → 2 字标签（全部等宽，列表才对得齐）。
fn state_label(state: TaskState) -> &'static str {
    match state {
        TaskState::Created => "新建",
        TaskState::Running => "运行",
        TaskState::Paused => "暂停",
        TaskState::Done => "完成",
        TaskState::Failed => "失败",
        TaskState::Cancelled => "取消",
    }
}

/// 毫秒时间戳 → 相对时间（面板里的「最近一次动过」）。
///
/// 刻意不引入时间库：任务列表关心的是「多久以前」，不是绝对时刻；而相对时间
/// 不需要时区换算，也就不会因为时区/夏令时把「昨天」显示成「今天」。
fn fmt_time(ms: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(ms);
    let secs = (now - ms).max(0) / 1000;
    match secs {
        0..=59 => "刚刚".into(),
        60..=3599 => format!("{} 分钟前", secs / 60),
        3600..=86_399 => format!("{} 小时前", secs / 3600),
        86_400..=2_591_999 => format!("{} 天前", secs / 86_400),
        _ => format!("{} 个月前", secs / 2_592_000),
    }
}

/// 一条历史任务的单行描述（`/tasks` 与任务面板共用）。
///
/// 当前正在看的那条用 `▸` 标出来，避免「我到底在哪条任务里」的困惑。
pub(crate) fn task_row(t: &TaskSummary, current: Option<&TaskId>) -> String {
    let marker = if current == Some(&t.id) { "▸" } else { " " };
    format!(
        "{} {}  {}  {}  {}",
        marker,
        short_id(&t.id),
        state_label(t.state),
        fmt_time(t.updated_at),
        t.title
    )
}

/// 主题 → 文件名安全的形式：空白换成短横线，去掉 Windows 不允许的字符。
///
/// 模板里 `artifacts/news-{slug}.md` 会用到它：`/news AI 芯片` 得到
/// `news-AI-芯片.md`，而不是一个带空格、在命令行里必须加引号的文件名。
fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            ' ' | '\t' | '\n' | '\r' => {
                if !out.is_empty() && !out.ends_with('-') {
                    out.push('-');
                }
            }
            // Windows 文件名非法字符 + 路径分隔符。
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => {}
            _ => out.push(c),
        }
    }
    let t = out.trim_matches('-');
    if t.is_empty() {
        "news".into()
    } else {
        t.to_string()
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

    // ---- Ctrl+R 历史 / Ctrl+H footer / Toast ----

    #[test]
    fn submit_records_history_and_dedups() {
        let mut app = App::default();
        app.input.buffer = "hello".into();
        assert!(app.submit().is_some());
        app.input.buffer = "hello".into();
        assert!(app.submit().is_some(), "连续重复应只记一条");
        app.input.buffer = "world".into();
        assert!(app.submit().is_some());
        assert_eq!(app.history, vec!["hello".to_string(), "world".to_string()]);
    }

    #[test]
    fn recall_history_cycles_newest_to_oldest() {
        let mut app = App::default();
        for t in ["one", "two", "three"] {
            app.input.buffer = t.into();
            app.submit();
        }
        app.recall_history();
        assert_eq!(app.input.buffer, "three", "首次回溯到最新");
        app.recall_history();
        assert_eq!(app.input.buffer, "two");
        app.recall_history();
        assert_eq!(app.input.buffer, "one");
        app.recall_history();
        assert_eq!(app.input.buffer, "three", "到最早后循环回最新");
    }

    #[test]
    fn recall_history_empty_toasts() {
        let mut app = App::default();
        app.recall_history();
        assert_eq!(app.toast.as_ref().unwrap().text, "没有历史记录");
    }

    #[test]
    fn editing_after_recall_restarts_from_newest() {
        let mut app = App::default();
        for t in ["one", "two"] {
            app.input.buffer = t.into();
            app.submit();
        }
        app.recall_history(); // -> "two"
        app.recall_history(); // -> "one"
        assert_eq!(app.input.buffer, "one");
        app.on_char('x'); // 用户编辑，退出回溯
        app.recall_history();
        assert_eq!(app.input.buffer, "two", "编辑后应从最新重新开始");
    }

    #[test]
    fn toggle_footer_flips_and_toasts() {
        let mut app = App::default();
        assert!(app.footer_visible);
        app.toggle_footer();
        assert!(!app.footer_visible);
        assert_eq!(app.toast.as_ref().unwrap().text, "footer 已隐藏");
        app.toggle_footer();
        assert!(app.footer_visible);
        assert_eq!(app.toast.as_ref().unwrap().text, "footer 已显示");
    }

    #[test]
    fn toast_expires_on_tick() {
        let mut app = App {
            toast: Some(Toast {
                text: "x".into(),
                until: Instant::now(), // 已到期
            }),
            ..Default::default()
        };
        app.on_tick();
        assert!(app.toast.is_none(), "到期 Toast 应被清除");

        app.toast("still here");
        app.on_tick();
        assert!(app.toast.is_some(), "未到期 Toast 应保留");
    }

    #[test]
    fn cycle_tier_toasts_instead_of_status() {
        let mut app = App::default();
        let status_before = app.status.clone();
        app.tier = "L1".into();
        app.cycle_tier();
        assert_eq!(app.tier, "L2");
        assert_eq!(app.status, status_before, "切档不应覆盖运行状态");
        // L2 的显示标签是「AUTO」（AI 审视）。
        assert!(app.toast.as_ref().is_some_and(|t| t.text.contains("AUTO")));
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
    fn take_approval_carries_remember_intent() {
        // 弹层 `a` = 批准并记住本任务同类操作；`y` = 只批准本次。
        let mut app = App {
            pending: Some(pending_approval()),
            ..Default::default()
        };
        let remember = match app.take_approval(true, true).expect("应有待审批项") {
            Command::Approve { remember, .. } => remember,
            _ => panic!("批准应返回 Command::Approve"),
        };
        assert!(
            remember,
            "remember 必须带到编排层，否则「批准本任务同类」只是看上去生效"
        );

        let mut app = App {
            pending: Some(pending_approval()),
            ..Default::default()
        };
        let remember = match app.take_approval(true, false).expect("应有待审批项") {
            Command::Approve { remember, .. } => remember,
            _ => panic!("批准应返回 Command::Approve"),
        };
        assert!(!remember, "只批准本次不应带上 remember");
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
    fn insert_newline_at_cursor() {
        let mut s = InputState::default();
        for c in "ab".chars() {
            s.insert_char(c);
        }
        s.move_left(); // cursor between a and b
        s.insert_newline();
        assert_eq!(s.buffer, "a\nb");
        assert_eq!(s.cursor, 2, "光标应落在换行符之后");
    }

    #[test]
    fn cursor_line_col_tracks_multiline() {
        let mut s = InputState::default();
        for c in "ab\ncd\n".chars() {
            s.insert_char(c);
        }
        // 光标在末尾：第 2 行（空行）第 0 列。
        assert_eq!(s.cursor_line_col(), (2, 0));

        s.move_left(); // 第二行 "cd" 末尾
        assert_eq!(s.cursor_line_col(), (1, 2));
        s.move_left(); // "c" 与 "d" 之间
        assert_eq!(s.cursor_line_col(), (1, 1));
        s.move_home();
        assert_eq!(s.cursor_line_col(), (0, 0));
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

    // ---- 任务面板 / 历史任务 / 续跑 ----

    fn task(id: &str, title: &str, state: TaskState) -> TaskSummary {
        TaskSummary {
            id: TaskId::from_raw(id),
            title: title.into(),
            state,
            created_at: 1_700_000_000_000,
            updated_at: 1_700_000_000_000,
        }
    }

    #[test]
    fn task_list_prints_into_conversation_when_panel_closed() {
        let mut app = App::default();
        app.apply_event(Event::TaskList {
            tasks: vec![task("aaaaaaaa-1111", "写脚本", TaskState::Done)],
        });
        assert_eq!(app.tasks.len(), 1);
        let text = app
            .lines
            .iter()
            .filter_map(|l| match l {
                Line::System(s) => Some(s.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("历史任务"), "{text}");
        assert!(text.contains("写脚本"), "列表要带标题：{text}");
        assert!(text.contains("aaaaaaaa"), "列表要带 id 前 8 位：{text}");
        assert!(text.contains("完成"), "列表要带状态：{text}");
    }

    #[test]
    fn task_list_does_not_spam_conversation_when_panel_open() {
        // 面板开着时列表只刷新面板：否则每开一次面板就往对话里灌一遍。
        let mut app = App::default();
        app.toggle_panel();
        app.apply_event(Event::TaskList {
            tasks: vec![task("aaaaaaaa-1111", "写脚本", TaskState::Done)],
        });
        assert!(
            !app.lines
                .iter()
                .any(|l| matches!(l, Line::System(s) if s.contains("历史任务"))),
            "面板打开时不该打印列表：{:?}",
            app.lines
        );
    }

    #[test]
    fn toggle_panel_requests_task_list() {
        let mut app = App::default();
        app.toggle_panel();
        assert!(app.panel_open);
        assert!(
            matches!(app.take_pending_command(), Some(Command::ListTasks)),
            "打开面板要向编排层要列表（UI 够不着 store）"
        );
        app.toggle_panel();
        assert!(!app.panel_open);
    }

    #[test]
    fn panel_move_and_activate_produce_resume_command() {
        let mut app = App {
            tasks: vec![
                task("aaaaaaaa-1111", "第一", TaskState::Done),
                task("bbbbbbbb-2222", "第二", TaskState::Done),
            ],
            panel_open: true,
            ..Default::default()
        };
        app.panel_move(1);
        assert_eq!(app.panel_sel, 1);
        app.panel_move(5);
        assert_eq!(app.panel_sel, 1, "到底不越界");
        match app.panel_activate() {
            Some(Command::ResumeTask(id)) => assert_eq!(id, "bbbbbbbb-2222"),
            other => panic!("应选中第二条并切换，实际 {other:?}"),
        }
        assert!(!app.panel_open, "切换后关闭面板");
    }

    #[test]
    fn panel_activate_with_empty_list_is_noop() {
        let mut app = App {
            panel_open: true,
            ..Default::default()
        };
        assert!(app.panel_activate().is_none());
    }

    /// 造一段「历史任务」的事件流：模型答复只通过 `AssistantTurn` 留下
    /// （默认不落逐字增量），这是回放最容易漏内容的地方。
    fn history_events(id: &TaskId) -> Vec<Event> {
        vec![
            Event::TaskStarted {
                id: id.clone(),
                title: "统计文件数".into(),
            },
            Event::AssistantTurn {
                id: id.clone(),
                iteration: 0,
                text: "我先看看目录里有什么。".into(),
                calls: vec![],
                usage: agent_core::provider::Usage::default(),
            },
            Event::ToolCallStart {
                id: id.clone(),
                call_id: agent_core::ids::ToolCallId::new(),
                name: "shell_exec".into(),
                args: serde_json::json!({ "cmd": "dir" }),
            },
            Event::ToolCallEnd {
                id: id.clone(),
                call_id: agent_core::ids::ToolCallId::new(),
                outcome: ToolOutcome::Ok,
                ms: 12,
                detail: None,
                output: Some("ok".into()),
            },
            Event::AssistantTurn {
                id: id.clone(),
                iteration: 1,
                text: "共 3 个文件。".into(),
                calls: vec![],
                usage: agent_core::provider::Usage::default(),
            },
            Event::TaskFinished {
                id: id.clone(),
                outcome: Outcome::Success {
                    summary: "共 3 个文件。".into(),
                    artifacts: vec!["artifacts/count.md".into()],
                },
            },
        ]
    }

    #[test]
    fn task_opened_replays_history_into_conversation() {
        let mut app = App::default();
        let id = TaskId::from_raw("cccccccc-3333");
        app.apply_event(Event::TaskOpened {
            id: id.clone(),
            title: "统计文件数".into(),
            events: history_events(&id),
        });

        assert_eq!(
            app.viewing.as_ref().map(|(i, _)| i.0.clone()),
            Some(id.0.clone())
        );
        assert!(!app.panel_open);
        let text: Vec<String> = app
            .lines
            .iter()
            .map(|l| match l {
                Line::User(s) | Line::Assistant(s) | Line::System(s) | Line::Done(s) => s.clone(),
                Line::Tool { name, .. } => name.clone(),
                Line::Reasoning(s) | Line::Error(s) => s.clone(),
            })
            .collect();
        let joined = text.join("\n");
        assert!(
            joined.contains("我先看看目录里有什么"),
            "整轮答复要回放：{joined}"
        );
        assert!(
            joined.contains("共 3 个文件"),
            "最后一轮答复也要在：{joined}"
        );
        assert!(joined.contains("shell_exec"), "工具调用要回放：{joined}");
        assert!(
            joined.contains("artifacts/count.md"),
            "产物要回放：{joined}"
        );
        // 同一段答复只出现一次（TextDelta 与 AssistantTurn 重合时不重复）
        assert_eq!(
            text.iter().filter(|t| t.contains("我先看看")).count(),
            1,
            "答复不该重复：{text:?}"
        );
    }

    #[test]
    fn resume_without_history_keeps_view_and_warns() {
        // 日志被清理时仍允许「接着这条任务说」，只是没有上下文可带。
        let mut app = App::default();
        let id = TaskId::from_raw("dddddddd-4444");
        app.apply_event(Event::TaskOpened {
            id: id.clone(),
            title: "旧任务".into(),
            events: vec![],
        });
        assert!(app
            .lines
            .iter()
            .any(|l| matches!(l, Line::Error(s) if s.contains("没有可回放"))));
        assert_eq!(app.viewing.as_ref().map(|(i, _)| i.0.clone()), Some(id.0));
    }

    #[test]
    fn submit_after_switch_resumes_the_same_task() {
        let mut app = App::default();
        let id = TaskId::from_raw("eeeeeeee-5555");
        app.apply_event(Event::TaskOpened {
            id: id.clone(),
            title: "统计文件数".into(),
            events: history_events(&id),
        });

        app.input.buffer = "再把结果写进 README".into();
        let spec = app.submit().expect("应生成任务规格");
        assert_eq!(spec.id.0, id.0, "沿用同一任务 id：面板里仍是一条任务");
        assert_eq!(spec.title, "统计文件数", "标题不随每次追问改名");
        assert_eq!(
            spec.resume_from.as_ref().map(|i| i.0.clone()),
            Some(id.0),
            "要告诉编排层从哪条任务续跑"
        );
        assert_eq!(spec.prompt, "再把结果写进 README");
    }

    #[test]
    fn fresh_app_submit_starts_a_new_task() {
        let mut app = App::default();
        app.input.buffer = "写个脚本".into();
        let spec = app.submit().expect("应生成任务规格");
        assert!(spec.resume_from.is_none(), "新对话不该带续跑上下文");
    }

    #[test]
    fn clear_conversation_starts_a_new_thread() {
        let mut app = App::default();
        let id = TaskId::from_raw("ffffffff-6666");
        app.apply_event(Event::TaskOpened {
            id,
            title: "旧任务".into(),
            events: vec![],
        });
        app.clear_conversation();
        assert!(app.viewing.is_none(), "/clear 之后是新话题");
        app.input.buffer = "换个话题".into();
        assert!(app.submit().unwrap().resume_from.is_none());
    }

    // ---- `/news`：新闻收集模板 ----

    #[test]
    fn news_command_fills_template_with_topic() {
        let mut app = App::default();
        assert!(app.run_slash_command("/news Rust"));

        let buf = &app.input.buffer;
        assert!(buf.contains("Rust"), "主题要填进去：{buf}");
        assert!(buf.contains("web_search"), "要引导先搜再抓：{buf}");
        assert!(
            buf.contains("artifacts/news-Rust.md"),
            "产物路径要具体：{buf}"
        );
        assert!(
            !buf.contains("{topic}") && !buf.contains("{slug}") && !buf.contains("{period}"),
            "占位符必须都被替换：{buf}"
        );
        assert!(
            !app.lines.iter().any(|l| matches!(l, Line::User(_))),
            "只填提示词，不自动提交（避免一按回车就烧钱）"
        );
        assert!(app.toast.is_some(), "要提示「填好了」");
    }

    #[test]
    fn news_without_topic_shows_usage() {
        let mut app = App::default();
        assert!(app.run_slash_command("/news"));
        assert!(app.input.buffer.is_empty(), "没主题时不该往输入框塞东西");
        assert!(
            app.toast.as_ref().is_some_and(|t| t.text.contains("用法")),
            "应提示怎么用"
        );
    }

    #[test]
    fn news_template_prefers_project_file() {
        // 用户可改 `templates/news.md`：改完不该还被内置副本盖住。
        let root = std::env::temp_dir().join(format!(
            "agent-tui-news-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(root.join("templates")).unwrap();
        std::fs::write(root.join("templates/news.md"), "自定义模板：{topic}").unwrap();

        let mut app = App::default();
        app.set_project_root(&root);
        assert!(app.run_slash_command("/news 芯片"));
        assert_eq!(app.input.buffer, "自定义模板：芯片");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn slugify_makes_safe_file_names() {
        assert_eq!(slugify("AI 芯片"), "AI-芯片");
        assert_eq!(slugify("a/b:c*d"), "abcd");
        assert_eq!(slugify("   "), "news");
    }

    #[test]
    fn streamed_then_assistant_turn_does_not_duplicate() {
        let mut app = App::default();
        let id = TaskId::new();
        app.apply_event(Event::TextDelta {
            id: id.clone(),
            text: "你好".into(),
        });
        app.apply_event(Event::AssistantTurn {
            id,
            iteration: 0,
            text: "你好".into(),
            calls: vec![],
            usage: agent_core::provider::Usage::default(),
        });
        assert_eq!(
            app.lines
                .iter()
                .filter(|l| matches!(l, Line::Assistant(_)))
                .count(),
            1,
            "实时路径下增量与整轮回复不该各占一行：{:?}",
            app.lines
        );
    }
}
