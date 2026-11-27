//! UI 状态与事件处理（不含渲染，便于单测）。

use std::time::{Duration, Instant};

use agent_core::event::{Command, Event, Outcome, ToolOutcome};
use agent_core::ids::{ApprovalId, TaskId};
use agent_core::perm::{ApprovalRequest, PermissionTier};
use agent_core::question::{QuestionAnswer, QuestionRequest};
use agent_core::store::TaskSummary;
use agent_core::task::{TaskSpec, TaskState};
use tokio::sync::mpsc;

use crate::theme::{glyph, tier_label};

/// 一期可执行的 `/` 命令（顺序即补全菜单的展示顺序）。
///
/// 只列**真正能落地**的命令：`/clear` 清空对话、`/help` 显示帮助、`/tier`
/// 切换权限档、`/sessions` 打开会话面板、`/resume` 切到某个历史任务继续、`/news`
/// 填入新闻收集提示词，都只改本地状态或读 store 后回灌界面。设计稿里的
/// `/model`（切模型）、`/compact`（手动压缩上下文）依赖后端能力，尚未落地，
/// 故不列入——遵循「只提示已实现功能」的原则，避免补出执行不了的入口。
pub const SLASH_COMMANDS: &[&str] = &[
    "/clear",
    "/help",
    "/tier",
    "/sessions",
    "/resume",
    "/news",
    "/remember",
    "/memory",
    "/improve",
    "/ctx",
    "/exit",
    "/quit",
];

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
    /// 折叠的工具调用：名称 + 参数摘要 + 状态（成功时附 diff 预览）。
    Tool {
        name: String,
        summary: String,
        status: ToolStatus,
        /// 编辑类工具的改动预览；只有 `file_edit` 有（入参里就带改前/改后文本）。
        diff: Option<DiffPreview>,
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
    // ---- 文本选区与复制（移植自 sh0）----
    /// 文本选区；坐标是**对话内容坐标**（wrap 后的绝对行号 + 显示列），
    /// 滚动时高亮跟着内容走。None 表示没有选区。
    pub selection: Option<Selection>,
    /// 正在拖拽框选（Down 定锚点后置位，Up 收尾）。
    pub drag_select: bool,
    /// 键盘选择模式（Alt+V 进入）：方向键 / hjkl 扩选。
    pub selection_keyboard: bool,
    /// 选区右键菜单；Some 时独占鼠标与键盘。
    pub context_menu: Option<ContextMenu>,
    /// 最近一次复制的内容（剪贴板不可用时可用 `p` 粘回输入框）。
    pub last_copy: String,
    /// 剪贴板门面（OSC 52 + arboard + 命令行兜底），App 长期持有。
    pub clipboard: crate::clipboard::Clipboard,
    /// 鼠标捕获是否生效。Shift + 鼠标交回父终端时挂起，按键事件再恢复。
    pub mouse_captured: bool,
    /// 最近一次对话区内容宽度（wrap / 提取用），由渲染循环每帧更新。
    pub conv_width: usize,
    pub pending: Option<PendingApproval>,
    /// AI 的提问弹层：候选选择 / 自定义输入 / 取消。None = 没有待回答问题。
    pub pending_question: Option<PendingQuestion>,
    pub active_task: Option<TaskId>,
    /// 待转交编排层的 `/` 命令（`/resume`、面板刷新需要 store，UI 够不着）。
    pub pending_command: Option<Command>,
    pub tier: String,
    pub provider: String,
    pub status: String,
    /// 最近一轮的 (缓存命中 token, 输入 token)：header 显示前缀缓存命中率。
    /// None 表示厂商没回报该字段。
    pub cache_hit: Option<(u64, u64)>,
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
    /// 最近一次从 store 取回的历史会话（会话面板唯一数据源）。
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
    /// 项目根（装配层注入）：`/news` 从这里读 `templates/news.md`，`@` 补全扫这里。
    project_root: Option<std::path::PathBuf>,
    /// `@` 文件补全状态。
    pub at: AtCompletion,
    /// 记忆门面（装配层注入；None = 记忆不可用，相关命令给提示）。
    pub memory: Option<std::sync::Arc<dyn agent_core::memory::MemoryPort>>,
    /// `/memory` 面板状态；None = 面板关闭。
    pub memory_panel: Option<MemoryPanel>,
    /// 上下文调试视图：随 [`Event::ContextSnapshot`] 增量累积的
    /// 「实际发给模型的消息序列」，与对话区显示互相独立、常驻累积。
    pub ctx: CtxView,
    /// `/ctx` 全屏调试页的 UI 状态；None = 页面关闭。
    pub ctx_debug: Option<CtxDebugUi>,
    /// 待确认候选条数（header 角标）。
    pub memory_candidates: usize,
    /// 异步记忆动作暂存（run_slash_command 同步，真正执行在事件循环里 await）。
    pub pending_memory: Option<MemoryPending>,
}

/// `/memory` 面板状态。
pub struct MemoryPanel {
    /// true = 生效页（Active），false = 候选页（Candidate）。
    pub active_page: bool,
    pub sel: usize,
    /// 面板内搜索词（输入即过滤）。
    pub query: String,
    pub entries: Vec<agent_core::memory::MemoryEntry>,
    pub scope: agent_core::memory::MemoryScope,
}

/// 需要在异步上下文里执行的记忆动作。
#[derive(Debug, Clone)]
pub enum MemoryPending {
    /// `/remember <内容>` 显式写入。
    Remember(String),
    /// 打开 `/memory` 面板。
    Open,
    /// 重新加载面板列表 / 候选角标。
    Reload,
    /// 确认当前选中候选 → Active。
    Confirm,
    /// 候选页批量确认：全部可见候选 → Active（P0-1B，避免逐条按）。
    ConfirmAll,
    /// 丢弃当前选中候选 → Archived。
    Discard,
    /// 删除当前选中条目（生效页）。
    Delete,
    /// 清空当前页可见条目（生效页全部 Active / 候选页全部 Candidate）。
    Clear,
}

/// 上下文调试视图（`/ctx` 的数据源）。
///
/// **设计**：真实上下文由 runtime 的 `ContextManager` 持有，TUI 够不着；
/// 但 v1.2 增量协议下，[`Event::ContextSnapshot`] 已经携带了还原完整序列
/// 所需的全部信息（`added` 增量 + `reset` 全量基线标记）——与 store 的
/// `resume_context` 是同一套重建逻辑。所以调试视图直接在 TUI 侧按增量
/// 累积，runtime/store 零改动、分层不被破坏，界面看到的和模型吃到的
/// 天然同源。
#[derive(Debug, Default)]
pub struct CtxView {
    /// 实际发给模型的消息序列（含固定 system）。
    pub messages: Vec<agent_core::provider::Message>,
    /// 最近一份快照的模型名 / 可用工具 / 轮次 / 是否压缩重置。
    pub model: String,
    pub tools: Vec<String>,
    pub iteration: u32,
    pub last_reset: bool,
    /// 载入的是没有上下文快照的旧日志（v1.2 之前）：调试页应说明原因，
    /// 而不是含糊地显示「尚无上下文」。
    pub legacy: bool,
}

impl CtxView {
    /// 应用一份增量快照（与 store 的 `resume_context` 同一语义）。
    pub fn apply_snapshot(
        &mut self,
        model: &str,
        iteration: u32,
        added: &[agent_core::provider::Message],
        reset: bool,
        tools: &[String],
    ) {
        if reset {
            // 压缩重写了历史：整份替换为压缩后的基线。
            self.messages = added.to_vec();
        } else {
            self.messages.extend(added.iter().cloned());
        }
        self.model = model.to_string();
        self.iteration = iteration;
        self.tools = tools.to_vec();
        self.last_reset = reset;
        self.legacy = false;
    }

    /// 切到历史任务时清空：只在没有快照可回放时标 legacy。
    pub fn reset(&mut self, legacy: bool) {
        *self = Self {
            legacy,
            ..Self::default()
        };
    }

    /// 当前估算 token（与 runtime 压缩判据同一实现，见 agent-core）。
    pub fn est_tokens(&self) -> usize {
        agent_core::provider::estimate_messages_tokens(&self.messages)
    }
}

/// `/ctx` 全屏调试页的 UI 状态（数据在 [`App::ctx`]）。
#[derive(Debug, Default)]
pub struct CtxDebugUi {
    /// 消息列表高亮行。
    pub sel: usize,
    /// 底部预览窗格的滚动偏移（行）。
    pub preview_scroll: usize,
}

/// `@` 文件补全状态。
///
/// 生命周期跟着输入框里的 `@` 走：输入 `@` 打开、补全/ Esc / 删掉 `@` 关闭。
/// 文件清单在打开时扫一次（关闭即丢弃）——不常驻内存，也就不会拿到过期的清单。
#[derive(Default)]
pub struct AtCompletion {
    pub active: bool,
    /// 当前高亮的候选。
    pub sel: usize,
    /// `@` 在输入框里的字节位置（补全时整段替换 `@query`）。
    pub token_start: usize,
    /// `@` 之后的查询串。
    pub query: String,
    /// 候选（相对路径，`/` 分隔），最多 [`AT_MAX_CANDIDATES`] 条。
    pub candidates: Vec<String>,
}

/// 候选上限：够用且不会把浮层撑到天上去；再多就靠把查询打细。
pub const AT_MAX_CANDIDATES: usize = 50;
/// 扫描上限：防止在超大仓库上打开输入框就卡住（超出部分直接不看）。
const AT_SCAN_LIMIT: usize = 20_000;
/// 扫描时跳过的目录名（构建产物 / 依赖 / 版本库内部）。
const AT_SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "target-ws",
    "node_modules",
    "tmp",
    ".codebuddy",
    ".vscode",
];

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
            selection: None,
            drag_select: false,
            selection_keyboard: false,
            context_menu: None,
            last_copy: String::new(),
            clipboard: crate::clipboard::Clipboard::new(),
            mouse_captured: true,
            conv_width: 80,
            pending: None,
            pending_question: None,
            active_task: None,
            pending_command: None,
            tier: "L2".into(),
            provider: "-".into(),
            status: "就绪".into(),
            cache_hit: None,
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
            at: AtCompletion::default(),
            memory: None,
            memory_panel: None,
            ctx: CtxView::default(),
            ctx_debug: None,
            memory_candidates: 0,
            pending_memory: None,
        }
    }
}

/// 文本选区（移植自 sh0）。
///
/// 坐标是**对话内容坐标**：行 = 对话区软换行后的绝对行号，列 = 显示列
/// （CJK 占 2 列）。滚动不改变选区——高亮跟着内容走，而不是钉在屏幕上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// 锚点（按下 / 进入选择模式时的位置）。
    pub anchor: (usize, usize),
    /// 活动端（拖动 / 方向键移动到的位置）。
    pub head: (usize, usize),
}

impl Selection {
    /// 规整后的行范围（闭区间）。
    fn rows(&self) -> (usize, usize) {
        (
            self.anchor.0.min(self.head.0),
            self.anchor.0.max(self.head.0),
        )
    }

    /// 某一行被选中的列范围（闭区间）；不在选区内返回 None。
    ///
    /// 只有起止行按列裁剪，中间行整行选中（与 sh0 / 常见终端一致）。
    pub fn cols_for(&self, row: usize) -> Option<(usize, usize)> {
        let (r0, r1) = self.rows();
        if row < r0 || row > r1 {
            return None;
        }
        let (c0, c1) = (
            self.anchor.1.min(self.head.1),
            self.anchor.1.max(self.head.1),
        );
        if row == r0 && row == r1 {
            Some((c0, c1))
        } else if row == r0 {
            Some((c0, usize::MAX))
        } else if row == r1 {
            Some((0, c1))
        } else {
            Some((0, usize::MAX))
        }
    }

    /// 坐标是否落在选区内（右键弹菜单用）。
    pub fn contains(&self, row: usize, col: usize) -> bool {
        self.cols_for(row)
            .is_some_and(|(c0, c1)| col >= c0 && col <= c1)
    }

    /// 选区是否一个字符都没选到（单击）：按下与松手位置相同。
    pub fn is_empty_click(&self) -> bool {
        self.anchor == self.head
    }
}

/// 选区右键菜单里的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextAction {
    Copy,
    Clear,
}

impl ContextAction {
    pub const ALL: [ContextAction; 2] = [ContextAction::Copy, ContextAction::Clear];

    pub fn label(self) -> &'static str {
        match self {
            ContextAction::Copy => "复制",
            ContextAction::Clear => "取消选择",
        }
    }
}

/// 选区右键菜单（位置是按下右键时的屏幕坐标）。
#[derive(Debug, Clone, Copy)]
pub struct ContextMenu {
    pub x: u16,
    pub y: u16,
    /// 当前高亮的菜单项索引（↑↓ 选择，Enter 执行）。
    pub selected: usize,
}

/// 一次待回答的提问：候选索引与自定义输入缓冲区。
pub struct PendingQuestion {
    pub req: QuestionRequest,
    /// 选中项：0..options.len()-1 是候选，等于 options.len() 表示「自己输入」。
    pub sel: usize,
    /// 鼠标悬停项（仅影响高亮，不改 sel；离开列表时为 None）。
    pub hover: Option<usize>,
    pub custom: InputState,
    pub editing: bool,
}

impl PendingQuestion {
    fn new(req: QuestionRequest) -> Self {
        Self {
            req,
            sel: 0,
            hover: None,
            custom: InputState::default(),
            editing: false,
        }
    }

    /// 可选项目总数（最后一项是「自己输入」，若允许）。
    pub fn option_count(&self) -> usize {
        self.req.options.len() + usize::from(self.req.allow_custom)
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
    /// 需要编排层执行的动作（`/resume`、面板刷新/选中某条任务）。
    Command(Command),
    Quit,
}

impl App {
    /// 应用一条 Agent 事件。
    pub fn apply_event(&mut self, ev: Event) {
        match ev {
            Event::TaskStarted { id, title } => {
                // 子任务（id 与当前活跃任务不同）不抢 active_task / viewing，
                // 只在对话区插一条折叠提示。
                if self.active_task.as_ref().is_some_and(|a| a != &id) {
                    self.lines
                        .push(Line::System(format!("↳ 子任务开始: {title}")));
                    self.auto_scroll();
                } else {
                    self.active_task = Some(id.clone());
                    self.status = format!("执行中: {title}");
                    // 对话区从此归这条任务：后续输入沿用它当任务 id 续跑。
                    self.viewing = Some((id, title.clone()));
                    self.panel_open = false;
                    self.lines
                        .push(Line::System(format!("▶ 任务开始: {title}")));
                    self.scroll_to_bottom();
                }
            }
            // v1.2：用户输入独立落盘，回放时才能还原「当时到底问了什么」。
            // 实时提交时 submit() 已 push 过一行，故就近去重；回放路径
            // （lines 被整体替换）没有这一行，能正常补上。
            Event::UserPrompt { text, .. } => {
                if !text.trim().is_empty() {
                    let already = self
                        .lines
                        .iter()
                        .rev()
                        .take(6)
                        .any(|l| matches!(l, Line::User(t) if t.trim() == text.trim()));
                    if !already {
                        self.lines.push(Line::User(text));
                        self.scroll_to_bottom();
                    }
                }
            }
            Event::SubtaskLinked { goal, .. } => {
                self.lines
                    .push(Line::System(format!("↳ 委派子任务：{goal}")));
                self.auto_scroll();
            }
            Event::TextDelta { id, text } => {
                // 子任务的流式增量不进主对话区（runtime 侧还会主动抑制广播，双保险）。
                if self.active_task.as_ref() != Some(&id) {
                    return;
                }
                match self.lines.last_mut() {
                    Some(Line::Assistant(s)) => s.push_str(&text),
                    _ => self.lines.push(Line::Assistant(text)),
                }
                self.auto_scroll();
            }
            Event::ReasoningDelta { id, text } => {
                if self.active_task.as_ref() != Some(&id) {
                    return;
                }
                match self.lines.last_mut() {
                    Some(Line::Reasoning(s)) => s.push_str(&text),
                    _ => self.lines.push(Line::Reasoning(text)),
                }
                self.auto_scroll();
            }
            Event::ToolCallStart { name, args, .. } => {
                let summary = summarize_args(&args);
                let diff = diff_for_tool(&name, &args);
                self.lines.push(Line::Tool {
                    name,
                    summary,
                    status: ToolStatus::Running,
                    diff,
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
            // 上下文快照：按增量协议累积调试视图（`/ctx` 数据源）；
            // 依旧不进对话流。
            Event::ContextSnapshot {
                model,
                iteration,
                added,
                reset,
                tools,
                ..
            } => {
                self.ctx
                    .apply_snapshot(&model, iteration, &added, reset, &tools);
            }
            // 整轮回复：实时路径下它与逐字增量重合（增量已经拼好了），但
            // **历史转写**里通常只有它——`TextDelta` 默认不落盘。所以：内容
            // 与最后一行正文相同就跳过，不同才补，两条路径都不重复。
            Event::AssistantTurn { text, usage, .. } => {
                // 前缀缓存命中率可观测：追问是否真的省钱，界面上要看得见。
                if usage.input_tokens > 0 {
                    self.cache_hit = usage.cached_tokens.map(|hit| (hit, usage.input_tokens));
                }
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
            // 审批结果只服务审计（日志/复盘）；界面上的「待审批」在用户按键
            // 那一刻就已清掉，这里无需再动状态。
            Event::ApprovalDecided { .. } => {}
            Event::QuestionAsked { req, .. } => {
                self.status = "等待回答…".into();
                self.pending_question = Some(PendingQuestion::new(req));
                self.request_repaint();
            }
            // 回答在用户按键那一刻已从界面清掉；这里按 id 再清一次，兜住
            // 其它任务/其它前端回灌的答案，避免旧弹层留着不消失。
            Event::QuestionAnswered { question_id, .. } => {
                if self
                    .pending_question
                    .as_ref()
                    .is_some_and(|p| p.req.id == question_id)
                {
                    self.pending_question = None;
                    self.status = "就绪".into();
                    self.request_repaint();
                }
            }
            Event::TaskFinished { id, outcome } => {
                // 子任务收尾：只插一行状态，绝不碰 active_task / status。
                if self.active_task.as_ref() != Some(&id) {
                    self.lines
                        .push(Line::System(format!("↳ 子任务结束: {outcome:?}")));
                    self.auto_scroll();
                    return;
                }

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
                // 会话面板是唯一展示方式：列表只刷新面板，不往对话区灌一遍。
                self.wake_cursor();
            }
            // 系统通知（如 /improve 产物路径）：进对话区，不落盘。
            Event::System { message } => {
                self.lines.push(Line::System(message));
                self.auto_scroll();
                self.wake_cursor();
            }
            Event::TaskOpened { id, title, events } => {
                self.panel_open = false;
                self.pending_question = None;
                self.request_repaint();
                // 历史回放现在带 ContextSnapshot（store::task_events 保留），
                // 按与实时/续跑相同的增量协议重建调试视图；没有快照的旧日志
                // 标记为 legacy，界面给出明确说明而不是空白。
                self.ctx.reset(
                    !events
                        .iter()
                        .any(|e| matches!(e, Event::ContextSnapshot { .. })),
                );
                for ev in &events {
                    if let Event::ContextSnapshot {
                        model,
                        iteration,
                        added,
                        reset,
                        tools,
                        ..
                    } = ev
                    {
                        self.ctx
                            .apply_snapshot(model, *iteration, added, *reset, tools);
                    }
                }
                self.ctx_debug = None;
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
        // `@` 的打开/过滤跟着输入走（`/` 补全由调用方再刷一次）。
        self.refresh_at();
    }

    pub fn on_backspace(&mut self) {
        self.input.backspace();
        self.leave_history();
        self.wake_cursor();
        self.refresh_at();
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
        if let Some(t) = &self.toast
            && t.until <= Instant::now()
        {
            self.toast = None;
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
        SLASH_COMMANDS.contains(&buf)
            || buf.starts_with("/resume ")
            || buf.starts_with("/news ")
            || buf.starts_with("/remember ")
            || buf.starts_with("/improve ")
    }

    /// 执行一条 `/` 命令；成功执行后清空输入并关闭补全菜单。
    pub fn run_slash_command(&mut self, cmd: &str) -> bool {
        match cmd {
            "/clear" => self.clear_conversation(),
            "/help" => self.show_help(),
            "/tier" => self.cycle_tier(),
            // 这两个不直接改 UI，而是转成 Command 交给编排层（它才够得着 store）。
            // `/sessions` 与 Ctrl+G 同一入口、同一面板，不保留第二套列表展示。
            "/sessions" => {
                self.toggle_panel();
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
            // `/remember <内容>`：显式写入长期记忆（立即生效）。真正执行在
            // 事件循环的异步上下文里（MemoryPort 是 async trait）。
            _ if cmd == "/remember" || cmd.starts_with("/remember ") => {
                let content = cmd.trim_start_matches("/remember").trim();
                if content.is_empty() {
                    self.toast("用法：/remember <要长期记住的内容>");
                    return true;
                }
                self.pending_memory = Some(MemoryPending::Remember(content.to_string()));
                self.input.take();
                return true;
            }
            // `/memory`：打开管理面板（生效页 / 候选页，Tab 切换）。
            "/memory" => {
                self.pending_memory = Some(MemoryPending::Open);
                self.input.take();
                return true;
            }
            // `/ctx`：打开上下文调试页（本地纯展示，不碰后端）。
            "/ctx" => {
                self.open_ctx_debug();
                self.input.take();
                return true;
            }
            // 退出：与 Ctrl+C 等价，走统一的退出路径（编排层收到 Quit 后收尾）。
            "/exit" | "/quit" => {
                self.should_quit = true;
                self.input.take();
                return true;
            }
            // `/improve <目标>`：记忆驱动的改进建议（编排层旁路生成 .patch）。
            _ if cmd == "/improve" || cmd.starts_with("/improve ") => {
                let goal = cmd.trim_start_matches("/improve").trim();
                if goal.is_empty() {
                    self.toast("用法：/improve <改进目标>");
                    return true;
                }
                self.pending_command = Some(Command::Improve {
                    goal: goal.to_string(),
                });
                self.input.take();
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

    /// 记录项目根（装配层注入）：`/news` 从这里读 `templates/news.md`，
    /// `@` 文件补全从这里扫文件。
    pub fn set_project_root(&mut self, root: impl Into<std::path::PathBuf>) {
        self.project_root = Some(root.into());
    }

    // ---- `@` 文件补全 ----

    /// 光标所在的 `@` 提及：返回 `@` 的字节位置与查询串。
    ///
    /// 只认「行首或空白之后的 `@`」且到光标之间没有空白：`a@b.com` 这种
    /// 邮箱不该弹菜单，`看看 @app` 才该弹。
    pub fn at_token(&self) -> Option<(usize, String)> {
        let before = &self.input.buffer[..self.input.cursor];
        let at = before.rfind('@')?;
        if at > 0 {
            let prev = before[..at].chars().next_back()?;
            if !prev.is_whitespace() {
                return None;
            }
        }
        let query = &before[at + 1..];
        if query.chars().any(char::is_whitespace) {
            return None;
        }
        Some((at, query.to_string()))
    }

    /// 当前查询命中的候选：先「路径前缀」，再「文件名前缀」，最后「包含」。
    ///
    /// 排序还带上路径长度与字典序，保证同样的查询每次顺序一致（否则列表会
    /// 在两次按键之间随机换位，高亮落在哪条全看运气）。
    pub fn at_candidates(&self) -> Vec<&str> {
        let q = self.at.query.to_ascii_lowercase();
        let mut hits: Vec<(u8, &str)> = self
            .at
            .candidates
            .iter()
            .filter_map(|p| {
                let lower = p.to_ascii_lowercase();
                let name = lower.rsplit('/').next().unwrap_or(lower.as_str());
                let score = if q.is_empty() || lower.starts_with(&q) {
                    0
                } else if name.starts_with(&q) {
                    1
                } else if lower.contains(&q) {
                    2
                } else {
                    return None;
                };
                Some((score, p.as_str()))
            })
            .collect();
        hits.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.len().cmp(&b.1.len()))
                .then_with(|| a.1.cmp(b.1))
        });
        hits.into_iter()
            .take(AT_MAX_CANDIDATES)
            .map(|(_, p)| p)
            .collect()
    }

    /// 重算 `@` 补全状态（输入 / 退格 / 删除 / 移动光标后调用）。
    pub fn refresh_at(&mut self) {
        // `/` 补全与审批弹层优先：两个浮层同时弹出来只会让人不知道在选什么。
        if self.completion_active || self.pending.is_some() {
            self.close_at();
            return;
        }
        let Some((start, query)) = self.at_token() else {
            self.close_at();
            return;
        };
        let Some(root) = self.project_root.clone() else {
            // 没有项目根（headless / 无目录）就没什么可补全的。
            self.close_at();
            return;
        };
        if self.at.candidates.is_empty() {
            self.at.candidates = scan_project_files(&root, AT_SCAN_LIMIT);
        }
        let changed = !self.at.active || self.at.token_start != start || self.at.query != query;
        self.at.token_start = start;
        self.at.query = query;
        if changed {
            self.at.sel = 0;
        }
        self.at.active = !self.at_candidates().is_empty();
        let last = self.at_candidates().len().saturating_sub(1);
        self.at.sel = self.at.sel.min(last);
    }

    /// 关闭 `@` 补全，并丢掉这次扫出来的文件清单。
    pub fn close_at(&mut self) {
        self.at = AtCompletion::default();
    }

    /// 上下移动 `@` 补全高亮。
    pub fn at_move(&mut self, delta: isize) {
        let last = self.at_candidates().len().saturating_sub(1);
        let next = self.at.sel as isize + delta;
        self.at.sel = next.clamp(0, last as isize) as usize;
        self.wake_cursor();
    }

    /// 用高亮项替换光标前的 `@查询`；成功返回 true。
    pub fn complete_at(&mut self) -> bool {
        let Some(cand) = self.at_candidates().get(self.at.sel).map(|s| s.to_string()) else {
            return false;
        };
        let start = self.at.token_start;
        let end = self.input.cursor;
        if start > end || end > self.input.buffer.len() {
            return false;
        }
        let insert = format!("@{cand}");
        self.input.buffer.replace_range(start..end, &insert);
        // 补全后自动补一个空格（若后面不是空白）：否则继续打字会把新字符拼进
        // 路径，`@src/app.rs` 后面刚打一个字母就又把菜单弹出来。
        let mut cursor = start + insert.len();
        if self.input.buffer[cursor..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_whitespace())
        {
            self.input.buffer.insert(cursor, ' ');
            cursor += 1;
        }
        self.input.cursor = cursor;
        self.close_at();
        self.leave_history();
        self.wake_cursor();
        true
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
        // 新话题：旧的上下文快照序列不再适用，调试页一并回到空状态。
        self.ctx.reset(false);
        self.ctx_debug = None;
        self.pending_question = None;
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
            "  /sessions  打开会话面板（与 Ctrl+G 相同）",
            "  /exit      退出（/quit 等价）",
            "  /resume <ID前几位>  切到某条历史任务继续",
            "  /news <主题>  填入新闻收集提示词（改完再发送）",
            "  Ctrl+G  打开会话面板（↑/↓ 选择，Enter 切换）",
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
            self.toast("会话面板：↑/↓ 选择，Enter 切换，Esc 关闭");
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

    // ---- 文本选区与复制（移植自 sh0）----

    /// 在某处按下左键：以该点为锚点开一个空选区并进入拖拽框选。
    pub fn begin_selection(&mut self, row: usize, col: usize) {
        self.selection = Some(Selection {
            anchor: (row, col),
            head: (row, col),
        });
        self.drag_select = true;
        self.selection_keyboard = false;
    }

    /// 拖拽更新选区活动端。
    pub fn extend_selection(&mut self, row: usize, col: usize) {
        if let Some(sel) = &mut self.selection {
            sel.head = (row, col);
        }
    }

    /// 键盘扩选：把活动端按 (drow, dcol) 移动，clamp 到对话内容范围内。
    pub fn move_selection_head(
        &mut self,
        drow: isize,
        dcol: isize,
        max_row: usize,
        max_col: usize,
    ) {
        let Some(sel) = &mut self.selection else {
            return;
        };
        let (r, c) = sel.head;
        let nr = (r as isize + drow).clamp(0, max_row as isize) as usize;
        let nc = (c as isize + dcol).clamp(0, max_col as isize) as usize;
        sel.head = (nr, nc);
    }

    /// 拖拽收尾：单击（没拖动）视为取消选区；真的选到了就**保留高亮**，
    /// 不自动复制（sh0 的取舍：松手即复制会让「只是想看看」的操作污染剪贴板）。
    pub fn finish_drag(&mut self) {
        self.drag_select = false;
        if let Some(sel) = self.selection
            && sel.is_empty_click()
        {
            self.selection = None;
        }
    }

    /// 清掉选区（Esc / 点选区外 / 其它按键接管时）。
    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.selection_keyboard = false;
        self.drag_select = false;
    }

    /// 收下要复制的内容：记录 last_copy、清选区、toast。返回内容行数。
    ///
    /// 剪贴板写入（`flush_clipboard`）与这里分开：提取文本在渲染层
    /// （wrap 逻辑在那边），单测也可以只验证收下内容而不真的写剪贴板。
    pub fn stage_copy(&mut self, text: String) -> usize {
        let n = text.lines().count().max(1);
        self.last_copy = text;
        self.clear_selection();
        self.toast(format!("已复制 {n} 行（若剪贴板为空，可按 p 粘回输入框）"));
        n
    }

    /// 把 last_copy 写进剪贴板（OSC 52 + 本机 + 命令行兜底）。
    /// 返回本机剪贴板是否成功。
    pub fn flush_clipboard(&mut self) -> bool {
        self.clipboard.copy(&self.last_copy)
    }

    /// 把最近复制的内容粘回输入框（剪贴板不可用时的兜底路径，sh0 的
    /// `p` 在这里是粘回会话；Zero 的等价落点是输入框）。
    pub fn paste_last_copy(&mut self) {
        if self.last_copy.is_empty() {
            return;
        }
        let text = self.last_copy.clone();
        for c in text.chars() {
            if c == '\n' {
                self.input.insert_newline();
            } else {
                self.input.insert_char(c);
            }
        }
        self.toast("已粘贴最近复制的内容到输入框");
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

    /// 问答弹层：鼠标悬停的候选（列表滚动偏移已折算成内容索引）。
    ///
    /// hover 没变就不请求重绘：鼠标移动会产生密集的 Moved 事件流，而
    /// request_repaint 会让下一帧整屏擦除重画——hover 不变时重画纯属浪费，
    /// 还会造成肉眼可见的频闪。
    pub fn question_hover(&mut self, idx: Option<usize>) {
        if let Some(p) = &mut self.pending_question {
            let next = idx.filter(|i| *i < p.option_count());
            if p.hover != next {
                p.hover = next;
                self.request_repaint();
            }
        }
    }

    /// 问答弹层：移动候选项（含末尾的「自己输入」）。
    pub fn question_move(&mut self, delta: i32) {
        if let Some(p) = &mut self.pending_question {
            let n = p.option_count().saturating_sub(1) as i64;
            let cur = p.sel as i64;
            p.sel = (cur + delta as i64).max(0).min(n) as usize;
            p.editing = false;
            self.request_repaint();
        }
    }

    /// Tab / `e`：在候选列表与自定义输入之间切换。
    pub fn question_toggle_edit(&mut self) {
        let allowed = self
            .pending_question
            .as_ref()
            .is_some_and(|p| p.req.allow_custom);
        if allowed && let Some(p) = &mut self.pending_question {
            p.editing = !p.editing;
            if p.editing {
                p.sel = p.req.options.len();
            }
            self.request_repaint();
        }
    }

    /// 自定义输入：直接打字即进入编辑（省一次 Tab）。
    pub fn question_char(&mut self, c: char) {
        if let Some(p) = &mut self.pending_question {
            if !p.req.allow_custom {
                return;
            }
            p.editing = true;
            p.sel = p.req.options.len();
            p.custom.insert_char(c);
            self.request_repaint();
        }
    }

    pub fn question_backspace(&mut self) {
        if let Some(p) = &mut self.pending_question {
            p.editing = true;
            p.custom.backspace();
            self.request_repaint();
        }
    }

    pub fn question_delete(&mut self) {
        if let Some(p) = &mut self.pending_question {
            p.editing = true;
            p.custom.delete();
            self.request_repaint();
        }
    }

    pub fn question_move_cursor(&mut self, delta: i32) {
        if let Some(p) = &mut self.pending_question {
            p.editing = true;
            if delta < 0 {
                p.custom.move_left();
            } else {
                p.custom.move_right();
            }
            self.request_repaint();
        }
    }

    /// 提交回答并清掉弹层；`Cancelled` 也是正式回答，等待中的工具会立刻结束。
    pub fn take_question(&mut self, answer: QuestionAnswer) -> Option<Command> {
        let p = self.pending_question.take()?;
        self.status = "就绪".into();
        self.request_repaint();
        Some(Command::AnswerQuestion {
            question_id: p.req.id,
            answer,
        })
    }

    /// 当前选中项对应的回答；选中「自己输入」时返回 None（调用方切编辑）。
    pub fn question_selected_answer(&self) -> Option<QuestionAnswer> {
        let p = self.pending_question.as_ref()?;
        if p.sel >= p.req.options.len() {
            return None;
        }
        Some(QuestionAnswer::Selected {
            index: p.sel,
            label: p.req.options[p.sel].clone(),
        })
    }

    /// 中断当前任务。
    pub fn cancel_command(&self) -> Option<Command> {
        self.active_task.clone().map(Command::Cancel)
    }

    /// 取走待编排层处理的命令（面板刷新、`/resume`）。
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
        // 问答事件在实时流里只弹层、不写对话区；回放时没有人再回答它们，
        // 必须转成文本行，否则历史会话里「当时问了什么、答了什么」凭空消失。
        match ev {
            Event::QuestionAsked { req, .. } => ctx.lines.push(Line::System(format!(
                "❓ AI 提问：{}（{} 个候选）",
                req.question,
                req.options.len()
            ))),
            Event::QuestionAnswered { answer, .. } => ctx.lines.push(Line::System(format!(
                "✍ 用户回答：{}",
                answer.as_model_text()
            ))),
            _ => ctx.apply_event(ev.clone()),
        }
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

/// 一条历史任务的单行描述（会话面板专用）。
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

/// 扫描项目文件，返回相对路径（`/` 分隔、按字典序）。
///
/// 跳过构建产物与版本库内部（见 [`AT_SKIP_DIRS`]）与所有点开头目录：`@` 补全
/// 是为了选「我要让 agent 看的文件」，不是给 `target/debug/deps/...` 做目录浏览。
/// 有深度与数量上限，避免在超大仓库上按一下 `@` 就卡住。
fn scan_project_files(root: &std::path::Path, limit: usize) -> Vec<String> {
    const MAX_DEPTH: usize = 8;
    let mut out: Vec<String> = Vec::new();
    let mut stack: Vec<(std::path::PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if out.len() >= limit || depth > MAX_DEPTH {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            if out.len() >= limit {
                break;
            }
            let Ok(ft) = entry.file_type() else { continue };
            let name = entry.file_name().to_string_lossy().to_string();
            if ft.is_dir() {
                // 点开头（.git/.github/.venv…）与构建产物一律不进。
                if name.starts_with('.') || AT_SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                stack.push((entry.path(), depth + 1));
            } else if ft.is_file() {
                let rel = entry
                    .path()
                    .strip_prefix(root)
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| name.clone());
                out.push(rel);
            }
        }
    }
    out.sort();
    out
}

/// 编辑类工具的改动预览。
///
/// 只对 `file_edit` 生成：它的入参里就带 `old_str` / `new_str`，不需要读盘、
/// 也不需要翻快照就能算出来。`file_write` 没有「改前」文本，故不做假 diff。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffPreview {
    /// 被删掉的行。
    pub removed: Vec<String>,
    /// 新增的行。
    pub added: Vec<String>,
    /// 因过长被省略的行数（两侧合计）。
    pub omitted: usize,
}

/// 每侧最多展示的 diff 行数（再长就只报「还有 N 行」）。
const MAX_DIFF_LINES: usize = 12;

/// 从工具入参里提取改动预览；不可用返回 None。
fn diff_for_tool(name: &str, args: &serde_json::Value) -> Option<DiffPreview> {
    if name != "file_edit" {
        return None;
    }
    let old = args.get("old_str")?.as_str()?;
    let new = args.get("new_str").and_then(|v| v.as_str()).unwrap_or("");
    let mut removed: Vec<String> = old.lines().map(str::to_string).collect();
    let mut added: Vec<String> = new.lines().map(str::to_string).collect();
    if removed.is_empty() && added.is_empty() {
        return None;
    }
    let mut omitted = 0;
    for side in [&mut removed, &mut added] {
        if side.len() > MAX_DIFF_LINES {
            omitted += side.len() - MAX_DIFF_LINES;
            side.truncate(MAX_DIFF_LINES);
        }
    }
    Some(DiffPreview {
        removed,
        added,
        omitted,
    })
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

// ---- 记忆系统（M4）----

impl App {
    /// 当前作用域：有项目根 → 项目级；否则用户级（与注入口径一致）。
    fn memory_scope(&self) -> agent_core::memory::MemoryScope {
        match self.project_root.as_ref() {
            Some(r) if !r.as_os_str().is_empty() => agent_core::memory::MemoryScope::Project {
                root: r.display().to_string(),
            },
            _ => agent_core::memory::MemoryScope::User,
        }
    }

    /// 取走暂存的异步记忆动作（事件循环每轮调用一次）。
    pub fn take_pending_memory(&mut self) -> Option<MemoryPending> {
        self.pending_memory.take()
    }

    /// 候选页当前选中条目 id。
    pub fn memory_selected_id(&self) -> Option<agent_core::memory::MemoryId> {
        let p = self.memory_panel.as_ref()?;
        p.entries.get(p.sel).map(|e| e.id.clone())
    }

    /// 面板搜索词追加/回退一个字符（同步）；返回 true 表示需要重新加载。
    pub fn memory_panel_edit_query(&mut self, ch: Option<char>) -> bool {
        let Some(p) = self.memory_panel.as_mut() else {
            return false;
        };
        match ch {
            Some(c) => p.query.push(c),
            None => {
                p.query.pop();
            }
        }
        p.sel = 0;
        true
    }

    /// 翻转生效页/候选页；返回 true 表示需要重新加载。
    pub fn memory_panel_toggle_page(&mut self) -> bool {
        let Some(p) = self.memory_panel.as_mut() else {
            return false;
        };
        p.active_page = !p.active_page;
        p.sel = 0;
        true
    }

    pub fn memory_panel_move(&mut self, delta: i32) {
        let Some(p) = self.memory_panel.as_mut() else {
            return;
        };
        let len = p.entries.len();
        if len == 0 {
            return;
        }
        let cur = p.sel as i32 + delta;
        p.sel = cur.clamp(0, len as i32 - 1) as usize;
    }

    pub fn close_memory_panel(&mut self) {
        self.memory_panel = None;
    }

    // ---- `/ctx` 上下文调试页 ----

    /// 打开 `/ctx`：高亮默认落在最后一条消息（模型最新吃到的内容）。
    pub fn open_ctx_debug(&mut self) {
        let sel = self.ctx.messages.len().saturating_sub(1);
        self.ctx_debug = Some(CtxDebugUi {
            sel,
            preview_scroll: 0,
        });
        // 弹层叠在对话区之上：整屏清一次，避免旧帧宽字符续行 cell
        // 被 ratatui 的 diff 跳过而留在物理屏幕上。
        self.request_repaint();
    }

    pub fn close_ctx_debug(&mut self) {
        self.ctx_debug = None;
    }

    /// 列表上下移动（夹取范围）；移动后预览回到顶部。
    pub fn ctx_debug_move(&mut self, delta: i32) -> bool {
        let Some(ui) = &mut self.ctx_debug else {
            return false;
        };
        let max = self.ctx.messages.len().saturating_sub(1) as i64;
        let next = (ui.sel as i64 + delta as i64).clamp(0, max);
        if next as usize == ui.sel {
            return false;
        }
        ui.sel = next as usize;
        ui.preview_scroll = 0;
        self.request_repaint();
        true
    }

    /// 预览窗格滚动（行）：正数向下翻，负数向上翻。
    pub fn ctx_debug_scroll(&mut self, delta: i32) {
        if let Some(ui) = &mut self.ctx_debug {
            let next = ui.preview_scroll as i64 + delta as i64;
            let next = next.max(0) as usize;
            if next != ui.preview_scroll {
                ui.preview_scroll = next;
                self.request_repaint();
            }
        }
    }

    /// 直接选中某条消息（鼠标点击列表）；选中后预览回到顶部。
    pub fn ctx_debug_select(&mut self, idx: usize) -> bool {
        if idx >= self.ctx.messages.len() {
            return false;
        }
        if let Some(ui) = &mut self.ctx_debug {
            if ui.sel == idx {
                return false;
            }
            ui.sel = idx;
            ui.preview_scroll = 0;
            self.request_repaint();
        }
        true
    }

    /// 执行一个暂存的记忆动作。只允许在异步上下文（TUI 事件循环）里调用。
    pub async fn handle_memory(&mut self, act: MemoryPending) {
        use agent_core::memory::MemoryPort;
        let Some(port) = self.memory.clone() else {
            self.toast("记忆功能未启用");
            return;
        };
        match act {
            MemoryPending::Remember(content) => {
                let scope = self.memory_scope();
                match port.remember(scope, content, vec![]).await {
                    Ok(_) => {
                        self.toast("已记住（立即生效）");
                        self.refresh_memory_candidates().await;
                        if self.memory_panel.is_some() {
                            self.reload_memory_panel().await;
                        }
                    }
                    Err(agent_core::memory::MemoryError::SecretDetected) => {
                        self.toast("拒绝写入：内容疑似凭据")
                    }
                    Err(agent_core::memory::MemoryError::TooLong { max }) => {
                        self.toast(format!("拒绝写入：超过 {max} 字符"))
                    }
                    Err(e) => self.toast(format!("写入失败：{e}")),
                }
            }
            MemoryPending::Open => {
                self.memory_panel = Some(MemoryPanel {
                    active_page: true,
                    sel: 0,
                    query: String::new(),
                    entries: Vec::new(),
                    scope: self.memory_scope(),
                });
                self.reload_memory_panel().await;
                self.refresh_memory_candidates().await;
            }
            MemoryPending::Reload => {
                self.reload_memory_panel().await;
                self.refresh_memory_candidates().await;
            }
            MemoryPending::Confirm => {
                if let Some(id) = self.memory_selected_id() {
                    match MemoryPort::confirm(port.as_ref(), &[id]).await {
                        Ok(n) if n > 0 => self.toast("已确认生效"),
                        Ok(_) => {}
                        Err(e) => self.toast(format!("确认失败：{e}")),
                    }
                    self.reload_memory_panel().await;
                    self.refresh_memory_candidates().await;
                }
            }
            MemoryPending::ConfirmAll => {
                // P0-1B：候选页一键全部确认（旧实现只能逐条按，候选无限堆积）。
                let ids: Vec<agent_core::memory::MemoryId> = {
                    let Some(p) = self.memory_panel.as_ref() else {
                        return;
                    };
                    p.entries.iter().map(|e| e.id.clone()).collect()
                };
                if ids.is_empty() {
                    return;
                }
                match MemoryPort::confirm(port.as_ref(), &ids).await {
                    Ok(n) if n > 0 => self.toast(format!("已确认 {n} 条生效")),
                    Ok(_) => {}
                    Err(e) => self.toast(format!("确认失败：{e}")),
                }
                self.reload_memory_panel().await;
                self.refresh_memory_candidates().await;
            }
            MemoryPending::Discard => {
                if let Some(id) = self.memory_selected_id() {
                    match MemoryPort::discard(port.as_ref(), &[id]).await {
                        Ok(n) if n > 0 => self.toast("已丢弃"),
                        Ok(_) => {}
                        Err(e) => self.toast(format!("丢弃失败：{e}")),
                    }
                    self.reload_memory_panel().await;
                    self.refresh_memory_candidates().await;
                }
            }
            MemoryPending::Delete => {
                if let Some(id) = self.memory_selected_id() {
                    match MemoryPort::delete(port.as_ref(), &id).await {
                        Ok(_) => self.toast("已删除（可追溯归档）"),
                        Err(e) => self.toast(format!("删除失败：{e}")),
                    }
                    self.reload_memory_panel().await;
                    self.refresh_memory_candidates().await;
                }
            }
            MemoryPending::Clear => {
                let ids: Vec<agent_core::memory::MemoryId> = {
                    let Some(p) = self.memory_panel.as_ref() else {
                        return;
                    };
                    p.entries.iter().map(|e| e.id.clone()).collect()
                };
                if ids.is_empty() {
                    return;
                }
                let mut n = 0usize;
                for id in &ids {
                    if MemoryPort::delete(port.as_ref(), id).await.is_ok() {
                        n += 1;
                    }
                }
                self.toast(format!("已清空 {n} 条"));
                self.reload_memory_panel().await;
                self.refresh_memory_candidates().await;
            }
        }
    }

    pub async fn refresh_memory_candidates(&mut self) {
        use agent_core::memory::{MemoryPort, MemoryQuery, MemoryState};
        let Some(port) = self.memory.clone() else {
            return;
        };
        let q = MemoryQuery {
            keyword: None,
            scope: Some(self.memory_scope()),
            state: Some(MemoryState::Candidate),
            limit: 0,
        };
        if let Ok(v) = MemoryPort::panel(port.as_ref(), q).await {
            self.memory_candidates = v.len();
        }
    }

    async fn reload_memory_panel(&mut self) {
        use agent_core::memory::{MemoryPort, MemoryQuery, MemoryState};
        let Some(port) = self.memory.clone() else {
            return;
        };
        let Some(p) = self.memory_panel.as_ref() else {
            return;
        };
        let q = MemoryQuery {
            keyword: (!p.query.is_empty()).then(|| p.query.clone()),
            scope: Some(p.scope.clone()),
            state: Some(if p.active_page {
                MemoryState::Active
            } else {
                MemoryState::Candidate
            }),
            limit: 0,
        };
        match MemoryPort::panel(port.as_ref(), q).await {
            Ok(v) => {
                if let Some(p) = self.memory_panel.as_mut() {
                    p.entries = v;
                    p.sel = p.sel.min(p.entries.len().saturating_sub(1));
                }
            }
            Err(e) => self.toast(format!("读取记忆失败：{e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::provider::Message;

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
        app.apply_event(Event::TaskStarted {
            id: id.clone(),
            title: "t".into(),
        });
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
        app.apply_event(Event::TaskStarted {
            id: id.clone(),
            title: "t".into(),
        });
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
        let id = TaskId::new();
        app.apply_event(Event::TaskStarted {
            id: id.clone(),
            title: "t".into(),
        });
        app.apply_event(Event::TaskFinished {
            id,
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
        let id = TaskId::new();
        app.apply_event(Event::TaskStarted {
            id: id.clone(),
            title: "t".into(),
        });
        app.apply_event(Event::TaskFinished {
            id,
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
                source: None,
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
    fn task_list_never_prints_into_conversation() {
        let mut app = App::default();
        let before = app.lines.len();
        app.apply_event(Event::TaskList {
            tasks: vec![task("aaaaaaaa-1111", "写脚本", TaskState::Done)],
        });
        assert_eq!(app.tasks.len(), 1);
        assert_eq!(app.lines.len(), before, "会话列表只进面板，不刷对话区");
    }

    #[test]
    fn sessions_slash_command_opens_same_panel_as_ctrl_g() {
        let mut app = App::default();
        assert!(app.run_slash_command("/sessions"));
        assert!(app.panel_open);
        assert!(matches!(
            app.take_pending_command(),
            Some(Command::ListTasks)
        ));
        assert!(!app.run_slash_command("/tasks"), "旧命令已移除");
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
        assert!(
            app.lines
                .iter()
                .any(|l| matches!(l, Line::Error(s) if s.contains("没有可回放")))
        );
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

    // ---- `/ctx` 上下文调试页 ----

    fn snapshot(
        id: &TaskId,
        iteration: u32,
        added: Vec<agent_core::provider::Message>,
        reset: bool,
    ) -> Event {
        Event::ContextSnapshot {
            id: id.clone(),
            iteration,
            model: "deepseek-chat".into(),
            added,
            reset,
            tools: vec!["list_dir".into(), "file_read".into()],
        }
    }

    /// 增量累积：首轮（system+user）之后每轮只追加新增量。
    #[test]
    fn ctx_view_accumulates_incremental_snapshots() {
        let mut app = App::default();
        let id = TaskId::from_raw("ctx-1");
        app.apply_event(snapshot(
            &id,
            0,
            vec![
                agent_core::provider::Message::system("固定区"),
                agent_core::provider::Message::user("任务"),
            ],
            false,
        ));
        app.apply_event(snapshot(
            &id,
            1,
            vec![agent_core::provider::Message::assistant("答")],
            false,
        ));
        assert_eq!(app.ctx.messages.len(), 3);
        assert_eq!(app.ctx.messages[0].text(), "固定区");
        assert_eq!(app.ctx.model, "deepseek-chat");
        assert_eq!(app.ctx.tools.len(), 2);
        assert!(!app.ctx.last_reset);
    }

    /// 压缩重置：整份替换为全量基线，而不是继续追加。
    #[test]
    fn ctx_view_reset_replaces_baseline() {
        let mut app = App::default();
        let id = TaskId::from_raw("ctx-2");
        app.apply_event(snapshot(
            &id,
            0,
            vec![Message::system("旧"), Message::user("旧任务")],
            false,
        ));
        app.apply_event(snapshot(
            &id,
            5,
            vec![Message::system("固定区"), Message::user("[历史摘要]")],
            true,
        ));
        assert_eq!(app.ctx.messages.len(), 2, "reset 必须整份替换");
        assert_eq!(app.ctx.messages[0].text(), "固定区");
        assert!(app.ctx.last_reset);
    }

    /// `/ctx` 打开调试页：默认选中最后一条，↑↓ 移动并复位预览滚动。
    #[test]
    fn ctx_debug_opens_and_navigates() {
        let mut app = App::default();
        let id = TaskId::from_raw("ctx-3");
        app.apply_event(snapshot(
            &id,
            0,
            vec![
                Message::system("S"),
                Message::user("U"),
                Message::assistant("A"),
            ],
            false,
        ));
        assert!(app.run_slash_command("/ctx"));
        let ui = app.ctx_debug.as_ref().expect("页面应已打开");
        assert_eq!(ui.sel, 2, "默认高亮最后一条");
        app.ctx_debug_move(-1);
        assert_eq!(app.ctx_debug.as_ref().unwrap().sel, 1);
        app.ctx_debug_scroll(5);
        app.ctx_debug_move(-1);
        let ui = app.ctx_debug.as_ref().unwrap();
        assert_eq!(ui.sel, 0);
        assert_eq!(ui.preview_scroll, 0, "换消息后预览回到顶部");
        assert!(!app.ctx_debug_move(-1), "已到顶不再移动");
    }

    /// 切到历史会话：回放里的快照序列重建调试视图（增量 + reset 语义一致）。
    #[test]
    fn task_opened_replays_context_snapshots() {
        let mut app = App::default();
        let id = TaskId::from_raw("ctx-4");
        app.apply_event(snapshot(&id, 0, vec![Message::system("旧会话")], false));
        app.apply_event(Event::TaskOpened {
            id: id.clone(),
            title: "旧任务".into(),
            events: vec![
                snapshot(
                    &id,
                    0,
                    vec![Message::system("S"), Message::user("U")],
                    false,
                ),
                snapshot(&id, 3, vec![Message::assistant("A")], false),
            ],
        });
        assert_eq!(app.ctx.messages.len(), 3, "历史快照按增量重建");
        assert_eq!(app.ctx.messages[0].text(), "S");
        assert_eq!(app.ctx.messages[2].text(), "A");
        assert!(!app.ctx.legacy);
    }

    /// 没有快照的旧日志：标记 legacy，调试页给出明确说明。
    #[test]
    fn task_opened_without_snapshots_marks_legacy() {
        let mut app = App::default();
        app.apply_event(Event::TaskOpened {
            id: TaskId::from_raw("ctx-5"),
            title: "旧任务".into(),
            events: vec![Event::UserPrompt {
                id: TaskId::from_raw("ctx-5"),
                text: "老日志".into(),
            }],
        });
        assert!(app.ctx.messages.is_empty());
        assert!(app.ctx.legacy, "老日志要标注无快照，而不是空白");
    }

    /// token 估算与 runtime 共用同一实现：中文按 1/字符、ASCII 按 1/3。
    #[test]
    fn ctx_est_tokens_matches_core_estimator() {
        let msgs = vec![Message::user("你好世界")];
        assert_eq!(
            agent_core::provider::estimate_messages_tokens(&msgs),
            4,
            "4 个中文字符 ≈ 4 token"
        );
        assert_eq!(
            agent_core::provider::estimate_messages_tokens(&[Message::user("abcdef")]),
            3
        );
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

    // ---- 编辑类工具的 diff 预览 ----

    fn tool_start(app: &mut App, name: &str, args: serde_json::Value) {
        app.apply_event(Event::ToolCallStart {
            id: TaskId::new(),
            call_id: agent_core::ids::ToolCallId::new(),
            name: name.into(),
            args,
        });
    }

    #[test]
    fn file_edit_carries_diff_preview() {
        let mut app = App::default();
        tool_start(
            &mut app,
            "file_edit",
            serde_json::json!({
                "path": "a.rs",
                "old_str": "fn old() {}\nlet x = 1;",
                "new_str": "fn new() {}\nlet x = 2;"
            }),
        );
        match app.lines.last() {
            Some(Line::Tool { diff: Some(d), .. }) => {
                assert_eq!(d.removed, vec!["fn old() {}", "let x = 1;"]);
                assert_eq!(d.added, vec!["fn new() {}", "let x = 2;"]);
                assert_eq!(d.omitted, 0);
            }
            other => panic!("file_edit 应带 diff 预览，实际 {other:?}"),
        }
    }

    #[test]
    fn non_edit_tool_has_no_diff_preview() {
        let mut app = App::default();
        tool_start(
            &mut app,
            "file_write",
            serde_json::json!({ "path": "a.rs", "content": "hello" }),
        );
        match app.lines.last() {
            Some(Line::Tool { diff, .. }) => assert!(diff.is_none(), "file_write 没有「改前」文本"),
            other => panic!("应为工具行，实际 {other:?}"),
        }
    }

    #[test]
    fn long_edit_reports_omitted_lines() {
        let mut app = App::default();
        let old = (0..30)
            .map(|i| format!("old {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = "new".to_string();
        tool_start(
            &mut app,
            "file_edit",
            serde_json::json!({ "path": "a.rs", "old_str": old, "new_str": new }),
        );
        match app.lines.last() {
            Some(Line::Tool { diff: Some(d), .. }) => {
                assert_eq!(d.removed.len(), MAX_DIFF_LINES, "最多展示 12 行");
                assert_eq!(d.omitted, 30 - MAX_DIFF_LINES, "其余报数");
            }
            other => panic!("应带 diff，实际 {other:?}"),
        }
    }

    // ---- `@` 文件补全 ----

    /// 造一个「项目根已注入、候选已就绪」的 App（不真的扫盘）。
    fn at_app(files: &[&str]) -> App {
        let mut app = App::default();
        app.set_project_root(std::env::temp_dir());
        app.at.candidates = files.iter().map(|s| s.to_string()).collect();
        app
    }

    #[test]
    fn at_token_only_recognises_word_start_mentions() {
        let mut app = App::default();
        // 行首
        app.input.buffer = "@app".into();
        app.input.move_end();
        assert_eq!(app.at_token(), Some((0, "app".to_string())));

        // 空白之后（"看看 " = 3+3+1 = 7 字节）
        app.input.buffer = "看看 @app".into();
        app.input.move_end();
        assert_eq!(app.at_token(), Some((7, "app".to_string())));

        // 邮箱不该弹菜单
        app.input.buffer = "a@b.com".into();
        app.input.move_end();
        assert_eq!(app.at_token(), None);

        // 查询里出现空格 = 已经不是在提文件了
        app.input.buffer = "看 @app 文件".into();
        app.input.move_end();
        assert_eq!(app.at_token(), None);

        // 光标在 `@` 之前
        app.input.buffer = "@app".into();
        app.input.move_home();
        assert_eq!(app.at_token(), None);
    }

    #[test]
    fn at_candidates_rank_path_prefix_then_name_then_contains() {
        let mut app = at_app(&[
            "src/app.rs",
            "docs/application.md",
            "crates/app/mod.rs",
            "app.rs",
            "README.md",
        ]);
        app.at.query = "app".into();
        assert_eq!(
            app.at_candidates(),
            vec![
                "app.rs",              // 路径前缀
                "src/app.rs",          // 文件名前缀（app.rs）
                "docs/application.md", // 文件名前缀（application.md 也是 app 开头）
                "crates/app/mod.rs",   // 仅路径中包含
            ],
            "README.md 不该入列"
        );

        // 空查询 = 全量（按长度、字典序稳定排序）
        app.at.query.clear();
        assert_eq!(app.at_candidates().len(), 5);
        assert_eq!(app.at_candidates()[0], "app.rs");
    }

    #[test]
    fn at_candidates_are_case_insensitive() {
        let mut app = at_app(&["Src/App.Rs"]);
        app.at.query = "APP".into();
        assert_eq!(app.at_candidates(), vec!["Src/App.Rs"]);
    }

    #[test]
    fn complete_at_replaces_the_mention_text() {
        let mut app = at_app(&["src/app.rs"]);
        app.input.buffer = "看看 @ap 这个文件".into();
        app.input.move_end();
        // 光标挪到 `ap` 之后（"这个文件" 之前）
        for _ in 0.." 这个文件".chars().count() {
            app.input.move_left();
        }
        app.refresh_at();
        assert!(app.at.active, "光标在 @ap 之后应激活");
        assert!(app.complete_at());
        assert_eq!(app.input.buffer, "看看 @src/app.rs 这个文件");
        assert!(!app.at.active, "补全后关闭菜单");
        assert_eq!(
            app.input.cursor,
            "看看 @src/app.rs".len(),
            "光标落在插入内容之后"
        );
    }

    #[test]
    fn refresh_at_closes_without_project_root() {
        // 没有项目根（headless）就别弹一个永远空的菜单。
        let mut app = App::default();
        app.input.buffer = "@".into();
        app.input.move_end();
        app.refresh_at();
        assert!(!app.at.active);
    }

    #[test]
    fn scan_project_files_skips_build_vcs_and_dependency_dirs() {
        let root = std::env::temp_dir().join(format!(
            "agent-tui-atscan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        for (p, body) in [
            ("src/main.rs", "x"),
            ("README.md", "x"),
            ("target/debug/agent.exe", "x"),
            (".git/config", "x"),
            ("node_modules/pkg/index.js", "x"),
            (".venv/bin/python", "x"),
        ] {
            let path = root.join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }

        let files = scan_project_files(&root, 100);
        assert!(files.contains(&"src/main.rs".to_string()), "{files:?}");
        assert!(files.contains(&"README.md".to_string()), "{files:?}");
        for skipped in ["target", ".git", "node_modules", ".venv"] {
            assert!(
                !files.iter().any(|f| f.starts_with(skipped)),
                "{skipped}/ 不该被扫进来：{files:?}"
            );
        }
        // 相对路径一律 `/` 分隔（Windows 上也不能冒出 `\`）
        assert!(!files.iter().any(|f| f.contains('\\')), "{files:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_respects_limit() {
        let root = std::env::temp_dir().join(format!(
            "agent-tui-atlimit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..10 {
            std::fs::write(root.join(format!("f{i}.txt")), "x").unwrap();
        }
        assert_eq!(scan_project_files(&root, 4).len(), 4, "上限要生效");
        let _ = std::fs::remove_dir_all(&root);
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

    // ---- AI 问答（ask_user）----

    fn question_req(id: &str) -> agent_core::question::QuestionRequest {
        agent_core::question::QuestionRequest {
            id: agent_core::ids::QuestionId::from_raw(id),
            task_id: TaskId::from_raw("t-ask"),
            question: "用哪种数据库？".into(),
            header: Some("技术选型".into()),
            options: vec!["SQLite".into(), "Postgres".into()],
            allow_custom: true,
        }
    }

    #[test]
    fn question_asked_opens_popup_and_matching_answer_closes_it() {
        let mut app = App::default();
        app.apply_event(Event::QuestionAsked {
            id: TaskId::from_raw("t-ask"),
            req: question_req("q-1"),
        });
        assert!(app.pending_question.is_some());
        assert_eq!(app.status, "等待回答…");

        // 别的问题的回答不能把当前弹层关掉。
        app.apply_event(Event::QuestionAnswered {
            id: TaskId::from_raw("t-ask"),
            question_id: agent_core::ids::QuestionId::from_raw("other"),
            answer: agent_core::question::QuestionAnswer::Cancelled,
        });
        assert!(app.pending_question.is_some());
        app.apply_event(Event::QuestionAnswered {
            id: TaskId::from_raw("t-ask"),
            question_id: agent_core::ids::QuestionId::from_raw("q-1"),
            answer: agent_core::question::QuestionAnswer::Cancelled,
        });
        assert!(app.pending_question.is_none());
        assert_eq!(app.status, "就绪");
    }

    #[test]
    fn question_move_and_custom_buffer_are_bounded() {
        let mut app = App::default();
        app.apply_event(Event::QuestionAsked {
            id: TaskId::from_raw("t-ask"),
            req: question_req("q-2"),
        });
        assert_eq!(app.pending_question.as_ref().unwrap().option_count(), 3);
        app.question_move(10);
        assert_eq!(
            app.pending_question.as_ref().unwrap().sel,
            2,
            "clamp 到自定义项"
        );
        app.question_move(-10);
        assert_eq!(app.pending_question.as_ref().unwrap().sel, 0);

        app.question_toggle_edit();
        assert!(app.pending_question.as_ref().unwrap().editing);
        app.question_char('你');
        app.question_char('好');
        app.question_backspace();
        assert_eq!(app.pending_question.as_ref().unwrap().custom.buffer, "你");
    }

    #[test]
    fn take_question_returns_command_and_clears_state() {
        let mut app = App::default();
        app.apply_event(Event::QuestionAsked {
            id: TaskId::from_raw("t-ask"),
            req: question_req("q-3"),
        });
        let cmd = app
            .take_question(agent_core::question::QuestionAnswer::Custom {
                text: "都要".into(),
            })
            .unwrap();
        match cmd {
            Command::AnswerQuestion {
                question_id,
                answer,
            } => {
                assert_eq!(question_id.0, "q-3");
                assert_eq!(
                    answer,
                    agent_core::question::QuestionAnswer::Custom {
                        text: "都要".into()
                    }
                );
            }
            _ => panic!("应是问答命令"),
        }
        assert!(app.pending_question.is_none());
        assert!(app.take_repaint(), "弹层消失要整屏重绘");
    }

    #[test]
    fn replay_shows_question_and_answer_as_history_lines() {
        let lines = replay_lines(&[
            Event::QuestionAsked {
                id: TaskId::from_raw("t-ask"),
                req: question_req("q-history"),
            },
            Event::QuestionAnswered {
                id: TaskId::from_raw("t-ask"),
                question_id: agent_core::ids::QuestionId::from_raw("q-history"),
                answer: agent_core::question::QuestionAnswer::Selected {
                    index: 0,
                    label: "SQLite".into(),
                },
            },
        ]);
        let text = lines
            .iter()
            .filter_map(|l| match l {
                Line::System(s) => Some(s.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("用哪种数据库"), "{text}");
        assert!(text.contains("SQLite"), "{text}");
        assert!(text.contains("用户回答"), "{text}");
    }
}
