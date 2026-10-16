//! 渲染 + 事件循环。

use std::io;
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as CtEvent, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line as RtLine, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use ratatui::Terminal;
use tokio::sync::mpsc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use agent_core::event::{Command, Event};

use crate::app::{App, Line, ToolStatus, UiAction};
use crate::theme::{glyph, ink, surface, tier_badge_color, tier_label, BG};

/// 启动 TUI 主循环。
///
/// - `ev_rx`：Agent 事件流
/// - `cmd_tx`：命令发送端（提交任务/审批/取消）
/// - `on_event`：把 UI 无关的事件转发给调用方（如落盘）
pub async fn run_tui(
    mut app: App,
    mut ev_rx: mpsc::Receiver<Event>,
    cmd_tx: mpsc::Sender<Command>,
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // 隐藏终端原生光标：输入框使用软光标（反显块）渲染，
    // 否则会与软光标重叠成"两个光标"，且原生光标在 alternate
    // screen 下多数终端不闪烁。
    // 启用鼠标捕获：对话区 / 输入区都支持滚轮滚动。
    execute!(
        stdout,
        EnterAlternateScreen,
        crossterm::cursor::Hide,
        EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, &mut app, &mut ev_rx, &cmd_tx).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        crossterm::cursor::Show,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

/// 四行布局：header(1) / 对话流(弹性) / 输入区(3) / footer(1)。
///
/// 索引约定（`handle_mouse` 依赖）：0=header，1=对话区，2=输入区，3=footer。
/// 终端高度过小时 ratatui 会自动压缩，但索引始终存在。
fn layout_chunks(area: Rect) -> [Rect; 4] {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(3),    // 对话流
            Constraint::Length(3), // 输入区
            Constraint::Length(1), // footer
        ])
        .split(area);
    [chunks[0], chunks[1], chunks[2], chunks[3]]
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    ev_rx: &mut mpsc::Receiver<Event>,
    cmd_tx: &mpsc::Sender<Command>,
) -> anyhow::Result<()> {
    let tick = Duration::from_millis(120);
    loop {
        // 当前终端尺寸（鼠标事件需要靠它判断指针落在哪个区域）
        let size = terminal.size()?;
        let area = Rect::new(0, 0, size.width, size.height);
        draw_frame(terminal, app)?;

        // 非阻塞地抽干事件队列。
        //
        // `Disconnected` 必须显式处理：编排层一旦退出（panic / 提前 return），
        // 事件流关闭，此后 `try_recv` 只返回 `Empty`，界面与「模型正在思考」
        // 完全无法区分——用户只能干等。这里落一条错误行点明后端已死。
        loop {
            match ev_rx.try_recv() {
                Ok(ev) => app.apply_event(ev),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    app.backend_disconnected();
                    break;
                }
            }
        }

        // 驱动软光标闪烁（与终端原生光标行为无关，跨平台一致）。
        app.on_tick();

        if event::poll(tick)? {
            match event::read()? {
                CtEvent::Key(key) => {
                    // Windows 的 crossterm 会对每次输入同时上报 Press 与 Release；
                    // 中文 IME 尤其明显（每个字符两条事件）。只处理 Press，
                    // 否则字符会被插入两次（“你好” → “你你好好”）。
                    //
                    // 别写成 `if !… { continue; }`——那会落在外层主循环上：
                    // 白画一帧不说，还跳过本轮末尾的退出检查。
                    if should_handle_key(&key) {
                        match handle_key(app, key) {
                            UiAction::Quit => {
                                let _ = cmd_tx.send(Command::Quit).await;
                                return Ok(());
                            }
                            UiAction::Submit(text) => {
                                let mut spec =
                                    agent_core::task::TaskSpec::new(text.clone(), String::new());
                                spec.title = text.chars().take(24).collect();
                                // 工作区由编排层填入；这里发送带 prompt 的任务
                                if cmd_tx.send(Command::SubmitTask(spec)).await.is_err() {
                                    app.backend_disconnected();
                                }
                            }
                            UiAction::Cancel => {
                                if let Some(c) = app.cancel_command() {
                                    if cmd_tx.send(c).await.is_err() {
                                        app.backend_disconnected();
                                    }
                                }
                            }
                            UiAction::Approve { remember } => {
                                if let Some(c) = app.take_approval(true, remember) {
                                    if cmd_tx.send(c).await.is_err() {
                                        app.backend_disconnected();
                                    }
                                }
                            }
                            UiAction::Reject => {
                                if let Some(c) = app.take_approval(false, false) {
                                    if cmd_tx.send(c).await.is_err() {
                                        app.backend_disconnected();
                                    }
                                }
                            }
                            UiAction::None => {}
                        }
                    }
                }
                // 终端缩放：物理屏幕会被终端重排，而 ratatui 只在「尺寸确实
                // 变了」时才自己 clear（窗口动画 / DPI 变化可能查不到差异）。
                // 这里直接清屏 + 重置后台缓冲，保证下一帧是干净的整屏重画。
                CtEvent::Resize(_, _) => terminal.clear()?,
                // 鼠标滚轮：按指针所在区域分流——对话区上下滚，输入区左右滚。
                CtEvent::Mouse(m) => handle_mouse(app, m, area),
                _ => {}
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

/// 是否应处理该按键事件。
///
/// crossterm 在 Windows 上会对每次输入同时上报 `Press` 与 `Release`（中文 IME
/// 尤其明显——每个字符两条事件）。只处理 `Press`，否则字符会被插入两次：
/// “你好” 变成 “你你好好”。
fn should_handle_key(key: &KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
}

/// 鼠标滚轮事件：按指针所在区域分流。
///
/// - 对话区：上下滚动历史输出；
/// - 输入区：左右移动光标（输入框是单行，超长时靠横向滚动查看）。
///
/// 注意：输入区**不是**最后一段（其后还有 footer），因此必须用
/// `[y, y+height)` 的闭开区间判断，不能用 `row >= y`。
fn handle_mouse(app: &mut App, m: MouseEvent, area: Rect) {
    let chunks = layout_chunks(area);
    let input = chunks[2];
    let in_input = m.row >= input.y && m.row < input.y.saturating_add(input.height);
    const WHEEL_STEP: u16 = 3;

    match m.kind {
        MouseEventKind::ScrollUp => {
            if in_input {
                for _ in 0..WHEEL_STEP {
                    app.input.move_left();
                }
            } else {
                app.scroll_up(WHEEL_STEP);
            }
            app.wake_cursor();
        }
        MouseEventKind::ScrollDown => {
            if in_input {
                for _ in 0..WHEEL_STEP {
                    app.input.move_right();
                }
            } else {
                app.scroll_down(WHEEL_STEP);
            }
            app.wake_cursor();
        }
        _ => {}
    }
}

/// 按键 -> 动作。
fn handle_key(app: &mut App, key: KeyEvent) -> UiAction {
    // 弹层优先：审批期间其它按键一律吞掉，避免误输入。
    if app.pending.is_some() {
        // Ctrl+C 始终可退出，否则弹层期间用户会被卡住，只能强杀进程。
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return UiAction::Quit;
        }
        // 大小写归一化：用户看到提示上的大写字母会按 Shift+Y，CapsLock 同理，
        // crossterm 上报的都是大写字符；只认小写会「按了没反应」。
        let lower = match &key.code {
            KeyCode::Char(c) => Some(c.to_ascii_lowercase()),
            _ => None,
        };
        let is_enter = key.code == KeyCode::Enter;
        let is_esc = key.code == KeyCode::Esc;
        return match (lower, is_enter, is_esc) {
            (Some('y'), _, _) | (_, true, _) => UiAction::Approve { remember: false },
            (Some('a'), _, _) => UiAction::Approve { remember: true },
            (Some('n'), _, _) | (_, _, true) => UiAction::Reject,
            _ => UiAction::None,
        };
    }

    match (key.code, key.modifiers) {
        // Ctrl+C：用 contains 而非精确匹配——Windows 上 Ctrl 事件可能
        // 附带 NumLock/CapsLock 等标志，精确相等会漏判。
        (code, m) if code == KeyCode::Char('c') && m.contains(KeyModifiers::CONTROL) => {
            UiAction::Quit
        }
        (KeyCode::Esc, _) => UiAction::Cancel,
        (KeyCode::Enter, _) => {
            // 提交
            match app.submit() {
                Some(Command::SubmitTask(spec)) => UiAction::Submit(spec.prompt),
                _ => UiAction::None,
            }
        }
        (KeyCode::Backspace, _) => {
            app.on_backspace();
            UiAction::None
        }
        (KeyCode::Delete, _) => {
            app.input.delete();
            app.wake_cursor();
            UiAction::None
        }
        (KeyCode::Left, _) => {
            app.input.move_left();
            app.wake_cursor();
            UiAction::None
        }
        (KeyCode::Right, _) => {
            app.input.move_right();
            app.wake_cursor();
            UiAction::None
        }
        // Home / Ctrl+A（readline 风格）
        (code, m)
            if code == KeyCode::Home
                || (code == KeyCode::Char('a') && m.contains(KeyModifiers::CONTROL)) =>
        {
            app.input.move_home();
            app.wake_cursor();
            UiAction::None
        }
        // End / Ctrl+E
        (code, m)
            if code == KeyCode::End
                || (code == KeyCode::Char('e') && m.contains(KeyModifiers::CONTROL)) =>
        {
            app.input.move_end();
            app.wake_cursor();
            UiAction::None
        }
        (KeyCode::PageUp, _) => {
            app.scroll_up(5);
            UiAction::None
        }
        (KeyCode::PageDown, _) => {
            app.scroll_down(5);
            UiAction::None
        }
        // Ctrl+L：整屏重绘。约定俗成的「刷新屏幕」键，屏幕被外部弄脏时手动救回。
        (KeyCode::Char('l'), m) if m.contains(KeyModifiers::CONTROL) => {
            app.request_repaint();
            UiAction::None
        }
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
            app.on_char(c);
            UiAction::None
        }
        _ => UiAction::None,
    }
}

/// 画一帧；App 请求了整屏重绘时，先擦屏。
///
/// 必须用 `Terminal::clear()`——它**同时**擦掉物理屏幕并重置后台缓冲，两者保持
/// 同步。别用 `swap_buffers()`：它只重置缓冲、不擦屏，于是下一帧的差分变成
/// 「空白 vs 新帧」，新帧留空的单元格不会被写——屏幕上那些位置仍是旧帧的字尾，
/// 表现为 `list_dir e. 0msng the project…` 这种两帧文字叠在一起。
fn draw_frame<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> io::Result<()> {
    if app.take_repaint() {
        terminal.clear()?;
    }
    terminal.draw(|f| draw(f, app))?;
    Ok(())
}

fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let chunks = layout_chunks(area);

    draw_header(f, app, chunks[0]);
    draw_conversation(f, app, chunks[1]);
    draw_input(f, app, chunks[2]);
    draw_footer(f, chunks[3]);

    if let Some(p) = &app.pending {
        draw_approval(f, app, p, area);
    }
}

/// 顶部信息栏：左「品牌 + 任务模式」，中「provider · 状态」，右「权限徽章」。
fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let base = Style::default().bg(surface::DIM).fg(ink::FG);

    // 左：品牌（紫，加粗）
    let brand = Span::styled(
        " ZERO ",
        Style::default()
            .fg(ink::PRIMARY)
            .add_modifier(Modifier::BOLD),
    );
    // 中：provider · 运行状态（灰）
    let meta = Span::styled(
        format!(" {} · {} ", app.provider, app.status),
        Style::default().fg(ink::FG_MUTED),
    );
    // 右：权限徽章（底色按档位三态）
    let badge = Span::styled(
        format!(" {} ", tier_label(&app.tier)),
        Style::default()
            .bg(tier_badge_color(&app.tier))
            .fg(BG)
            .add_modifier(Modifier::BOLD),
    );

    let left_w = 6usize;
    let meta_w = UnicodeWidthStr::width(meta.content.as_ref());
    let badge_w = UnicodeWidthStr::width(badge.content.as_ref());
    let pad = (area.width as usize)
        .saturating_sub(left_w + meta_w + badge_w + 1)
        .max(1);

    let line = RtLine::from(vec![brand, meta, Span::raw(" ".repeat(pad)), badge]);
    f.render_widget(Paragraph::new(line).style(base), area);
}

/// 底部快捷键提示条（灰字，溢出时由 ratatui 自动裁剪）。
fn draw_footer(f: &mut Frame, area: Rect) {
    let text = " ⏎ 发送  ⇧⏎ 换行  Esc 中断  PgUp/Dn 滚动  Ctrl+C 退出 ";
    f.render_widget(
        Paragraph::new(Span::styled(text, Style::default().fg(ink::FG_MUTED))),
        area,
    );
}

/// 构建对话区的渲染行（纯函数，便于单测）。
///
/// `inner_w` 为内容区宽度：用户消息整行铺底需要靠它算右侧填充。
fn build_conversation_lines(items: &[Line], inner_w: usize) -> Vec<RtLine<'static>> {
    let mut lines: Vec<RtLine> = Vec::new();
    for l in items {
        // 多轮之间留白：非首条用户消息前插一个空行，与上一条回答分隔。
        if matches!(l, Line::User(_)) && !lines.is_empty() {
            lines.push(RtLine::from(""));
        }
        match l {
            // 用户消息：左竖线（粉）+ 底色铺满整行。
            Line::User(s) => {
                for (i, tl) in s.lines().enumerate() {
                    let (bar, body) = if i == 0 {
                        (format!("{} ", glyph::USER_BAR), tl.to_string())
                    } else {
                        ("  ".to_string(), tl.to_string())
                    };
                    let used = UnicodeWidthStr::width(bar.as_str())
                        + UnicodeWidthStr::width(body.as_str());
                    let pad = inner_w.saturating_sub(used);
                    let bg = Style::default().bg(surface::FAINT);
                    lines.push(RtLine::from(vec![
                        Span::styled(bar, bg.fg(ink::ACCENT).add_modifier(Modifier::BOLD)),
                        Span::styled(body, bg.fg(ink::FG)),
                        Span::styled(" ".repeat(pad), bg),
                    ]));
                }
            }
            // 助手回答：缩进 2，正文近白。
            Line::Assistant(s) => {
                for tl in s.lines() {
                    lines.push(RtLine::from(Span::styled(
                        format!("  {tl}"),
                        Style::default().fg(ink::FG),
                    )));
                }
            }
            // 思考链：灰显，首行带 ⋯ 标记。
            Line::Reasoning(s) => {
                for (i, tl) in s.lines().enumerate() {
                    let prefix = if i == 0 {
                        format!("  {} ", glyph::REASONING)
                    } else {
                        "    ".to_string()
                    };
                    lines.push(RtLine::from(Span::styled(
                        format!("{prefix}{tl}"),
                        Style::default().fg(ink::FG_MUTED),
                    )));
                }
            }
            // 工具调用：状态符号 + 工具名固定列 + 参数摘要，按四态换色；
            // 成功附耗时，失败 / 拒绝附原因。
            Line::Tool {
                name,
                summary,
                status,
            } => {
                let (mark, color) = match status {
                    ToolStatus::Running => (glyph::RUNNING, ink::WARNING),
                    ToolStatus::Ok { .. } => (glyph::OK, ink::SUCCESS),
                    ToolStatus::Failed { .. } => (glyph::FAIL, ink::ERROR),
                    ToolStatus::Rejected { .. } => (glyph::REJECTED, ink::FG_MUTED),
                };
                let mut spans = vec![
                    Span::styled(format!("  {mark} "), Style::default().fg(color)),
                    Span::styled(
                        pad_display(name, TOOL_NAME_COL),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(summary.clone(), Style::default().fg(ink::FG_MUTED)),
                ];
                match status {
                    ToolStatus::Running => {}
                    ToolStatus::Ok { ms } => spans.push(Span::styled(
                        format!("  {ms}ms"),
                        Style::default().fg(ink::FG_MUTED),
                    )),
                    ToolStatus::Failed { reason, ms } => {
                        spans.push(Span::styled(
                            format!("  {ms}ms"),
                            Style::default().fg(ink::FG_MUTED),
                        ));
                        spans.push(Span::styled(
                            format!("  {}", truncate_display(&one_line(reason), REASON_COL)),
                            Style::default().fg(ink::ERROR),
                        ));
                    }
                    // 被拒即未执行，没有耗时可言，只给原因（灰显，引导看审批记录）。
                    ToolStatus::Rejected { reason } => spans.push(Span::styled(
                        format!("  {}", truncate_display(&one_line(reason), REASON_COL)),
                        Style::default().fg(ink::FG_MUTED),
                    )),
                }
                lines.push(RtLine::from(spans));
            }
            // 系统提示：灰显，首行带 · 标记。
            Line::System(s) => {
                for (i, tl) in s.lines().enumerate() {
                    let prefix = if i == 0 {
                        format!("  {} ", glyph::SYSTEM)
                    } else {
                        "    ".to_string()
                    };
                    lines.push(RtLine::from(Span::styled(
                        format!("{prefix}{tl}"),
                        Style::default().fg(ink::FG_MUTED),
                    )));
                }
            }
            // 错误：红字加粗，首行带 ✖ 标记。
            Line::Error(s) => {
                for (i, tl) in s.lines().enumerate() {
                    let prefix = if i == 0 {
                        format!("  {} ", glyph::FAIL)
                    } else {
                        "    ".to_string()
                    };
                    lines.push(RtLine::from(Span::styled(
                        format!("{prefix}{tl}"),
                        Style::default().fg(ink::ERROR).add_modifier(Modifier::BOLD),
                    )));
                }
            }
        }
    }
    lines
}

fn draw_conversation(f: &mut Frame, app: &App, area: Rect) {
    // inner_w 需在构建行时可用：用户消息整行铺底要填满内容宽度。
    let inner_w = area.width.saturating_sub(2) as usize;
    let lines = build_conversation_lines(&app.lines, inner_w);

    // Paragraph 启用了 Wrap，长行会被软换行成多行。滚动范围必须按
    // **wrap 后的实际行数**计算。之前按总宽度做数学估算，在 word wrap
    //（英文）场景下会严重低估，导致内容一长就滚不到底、底部被截断。
    let total: u16 = lines
        .iter()
        .map(|l| wrapped_line_count(l, inner_w))
        .sum::<usize>()
        .min(u16::MAX as usize) as u16;
    let height = area.height.saturating_sub(2);
    let max_scroll = total.saturating_sub(height);
    let scroll = max_scroll.saturating_sub(app.scroll_from_bottom);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(Span::styled(" 对话 ", Style::default().fg(ink::SECONDARY)))
        .border_style(Style::default().fg(surface::LINE));
    let p = Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(p, area);

    // 仅当内容溢出时显示右侧滚动条，避免常态噪音。
    if total > height {
        draw_scrollbar(f, area, total, height, scroll);
    }
}

/// 对话区右侧滚动条（1 列宽，贴在圆角边框内侧）。
fn draw_scrollbar(f: &mut Frame, area: Rect, total: u16, view: u16, offset: u16) {
    if area.width < 4 || area.height < 4 {
        return;
    }
    let track_h = area.height.saturating_sub(2);
    let x = area.x + area.width - 1;
    let y = area.y + 1;
    // 滑块高度按「可见/总」比例，至少 1 行。
    let thumb = ((view as u32 * track_h as u32) / total.max(1) as u32).max(1) as u16;
    let thumb = thumb.min(track_h);
    let span_pos = if total > view {
        (offset as u32 * track_h.saturating_sub(thumb) as u32 / (total - view) as u32) as u16
    } else {
        0
    };
    for i in 0..track_h {
        let cell = Rect::new(x, y + i, 1, 1);
        let (ch, color) = if i >= span_pos && i < span_pos + thumb {
            ("█", ink::PRIMARY)
        } else {
            ("│", surface::LINE)
        };
        f.render_widget(
            Paragraph::new(Span::styled(ch, Style::default().fg(color))),
            cell,
        );
    }
}

fn draw_input(f: &mut Frame, app: &App, area: Rect) {
    // 输入框是焦点区：主色圆角边框，与对话区的弱灰边框区分。
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(Span::styled(" 输入 ", Style::default().fg(ink::PRIMARY)))
        .border_style(Style::default().fg(ink::PRIMARY));

    // 软光标：把 buffer 按光标切成 前 / 光标处 / 后 三段，光标处用反显。
    // 不依赖 f.set_cursor_position()——alternate screen 下 Windows Terminal
    // 的原生光标常不闪烁，软光标在任何终端都有一致的闪烁行为。
    let buf = &app.input.buffer;
    let cursor_idx = app.input.cursor_char_index();
    let before: String = buf.chars().take(cursor_idx).collect();
    let mut rest = buf.chars().skip(cursor_idx);
    let at_char = rest.next().unwrap_or(' ');
    let after: String = rest.collect();

    // 输入框是单行且不 wrap：内容超过框宽时靠**横向滚动**保证光标可见，
    // 否则超长输入直接被裁掉、光标也跑出视野（之前正是如此）。
    let inner_w = area.width.saturating_sub(2) as usize;
    let cursor_col = UnicodeWidthStr::width(before.as_str());

    let mut spans: Vec<Span> = Vec::new();
    if buf.is_empty() {
        // 空输入：软光标 + 占位文字 + 反显 pill。
        // 只提示**已实现**的功能（Enter 发送），避免误导。
        spans.push(cursor_cell(app.cursor_visible, ' '));
        spans.extend(placeholder_spans());
    } else {
        spans.push(Span::raw(before.as_str()));
        spans.push(cursor_cell(app.cursor_visible, at_char));
        spans.push(Span::raw(after.as_str()));
    }
    // 光标 + 光标块占 cursor_col+1 列；放不下就往右滚，让光标贴右沿。
    // inner_w 为 0（极端窄终端）时 saturating_sub 自然得到 0。
    let hscroll = (cursor_col + 1).saturating_sub(inner_w) as u16;

    let p = Paragraph::new(RtLine::from(spans))
        .block(block)
        .scroll((0, hscroll));
    f.render_widget(p, area);
}

/// 工具名在列表中的固定显示列宽（超出截断，不足补空格）。
const TOOL_NAME_COL: usize = 12;

/// 工具失败/拒绝原因的最大显示列宽（超出截断，避免一行原因挤满整个对话区）。
const REASON_COL: usize = 60;

/// 按**显示宽度**填充或截断到固定列宽，使多行工具名左对齐。
fn pad_display(s: &str, col: usize) -> String {
    let w = UnicodeWidthStr::width(s);
    if w <= col {
        return format!("{s}{}", " ".repeat(col - w));
    }
    truncate_display(s, col)
}

/// 按**显示宽度**截断到指定列宽（超出以 `…` 收尾）；未超宽时原样返回。
fn truncate_display(s: &str, col: usize) -> String {
    if UnicodeWidthStr::width(s) <= col {
        return s.to_string();
    }
    // 保留 col-1 列内容 + 省略号
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > col.saturating_sub(1) {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('…');
    out
}

/// 把多行文本压成单行：工具报错常带换行与缩进，直接铺进摘要行会撑破布局。
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 软光标单元格：可见时反显（终端无关的闪烁，替代原生光标）。
fn cursor_cell(visible: bool, ch: char) -> Span<'static> {
    let mut span = Span::raw(ch.to_string());
    if visible {
        span.style = Style::default().add_modifier(Modifier::REVERSED);
    }
    span
}

/// 空输入时的占位提示：灰字 + 反显 pill。
///
/// 只提示**已实现**的功能，避免出现「提示了却做不到」的入口。
fn placeholder_spans() -> Vec<Span<'static>> {
    let hint = Style::default().fg(ink::FG_MUTED);
    let pill = Style::default()
        .fg(ink::FG)
        .add_modifier(Modifier::REVERSED);
    vec![
        Span::styled(" 输入任务… ", hint),
        Span::styled(" ⏎ ", pill),
        Span::styled(" 发送", hint),
    ]
}

/// 一行文本在给定宽度下 word-wrap 后占用的显示行数（至少 1 行）。
///
/// 使用 textwrap 按 Unicode 显示宽度做软换行，与 ratatui 的
/// `Wrap { trim: false }` 行为足够接近，避免之前按总宽度做数学估算
/// 在英文 word-wrap 场景下严重低估、导致底部内容被截断的问题。
fn wrapped_line_count(line: &RtLine, inner_w: usize) -> usize {
    if inner_w == 0 {
        return 1;
    }
    let text = line_as_str(line);
    if text.is_empty() {
        return 1;
    }
    textwrap::wrap(&text, inner_w).len().max(1)
}

/// 取出 RtLine 的纯文本（仅用于测宽，样式无关）。
fn line_as_str(line: &RtLine) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn draw_approval(f: &mut Frame, _app: &App, p: &crate::app::PendingApproval, area: Rect) {
    use agent_core::perm::Risk;

    let w = area.width.min(72);
    let h = 11.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect::new(x, y, w, h);

    f.render_widget(Clear, popup);

    // 高危用红、其余用橙——与 header 权限徽章同一套语义色。
    let high = matches!(p.req.risk, Risk::High);
    let accent = if high { ink::ERROR } else { ink::WARNING };
    let risk = match p.req.risk {
        Risk::High => "高危",
        Risk::Medium => "中危",
        Risk::Low => "低危",
    };
    let args = truncate_json(&p.req.args, 160);

    let label = Style::default().fg(ink::FG_MUTED);
    let key = Style::default()
        .fg(ink::FG)
        .add_modifier(Modifier::REVERSED);

    let mut body = vec![
        approval_field("工具", &p.req.tool),
        approval_field("风险", risk),
        approval_field("参数", &args),
        RtLine::from(""),
    ];
    if high {
        body.push(RtLine::from(Span::styled(
            " ⚠ 该操作可能不可恢复",
            Style::default().fg(ink::ERROR),
        )));
        body.push(RtLine::from(""));
    }
    body.push(RtLine::from(vec![
        Span::styled(" [y] ", key),
        Span::styled(" 批准   ", label),
        Span::styled(" [n] ", key),
        Span::styled(" 拒绝", label),
    ]));
    body.push(RtLine::from(Span::styled(
        " 也可按 Enter 批准、Esc 拒绝",
        Style::default().fg(ink::FG_MUTED),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(Span::styled(" 权限确认 ", Style::default().fg(accent)))
        .border_style(Style::default().fg(accent));
    let para = Paragraph::new(Text::from(body))
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(para, popup);
}

/// 审批弹层中的一行「字段  值」。
fn approval_field(key: &str, value: &str) -> RtLine<'static> {
    RtLine::from(vec![
        Span::styled(format!(" {key}  "), Style::default().fg(ink::FG_MUTED)),
        Span::styled(value.to_string(), Style::default().fg(ink::FG)),
    ])
}

fn truncate_json(v: &serde_json::Value, max: usize) -> String {
    let s = v.to_string();
    if s.chars().count() <= max {
        s
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;

    fn key_with_kind(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind,
            state: event::KeyEventState::NONE,
        }
    }

    #[test]
    fn press_is_handled() {
        let k = key_with_kind(KeyCode::Char('你'), KeyEventKind::Press);
        assert!(should_handle_key(&k));
    }

    #[test]
    fn release_is_ignored() {
        // 回归测试：Windows crossterm 同时上报 Press + Release。
        // 不过滤会导致中文 IME 每字符插入两次。
        let k = key_with_kind(KeyCode::Char('你'), KeyEventKind::Release);
        assert!(!should_handle_key(&k));
    }

    #[test]
    fn repeat_is_ignored() {
        let k = key_with_kind(KeyCode::Char('a'), KeyEventKind::Repeat);
        assert!(!should_handle_key(&k));
    }

    #[test]
    fn ime_press_release_pair_inserts_once() {
        // 模拟一次 IME 输入的完整事件流：Press + Release。
        let mut app = App::default();
        for kind in [KeyEventKind::Press, KeyEventKind::Release] {
            let k = key_with_kind(KeyCode::Char('好'), kind);
            if should_handle_key(&k) {
                if let KeyCode::Char(c) = k.code {
                    app.on_char(c);
                }
            }
        }
        assert_eq!(app.input.buffer, "好", "Press+Release 只应插入一个字符");
    }

    #[test]
    fn ascii_still_works_after_filter() {
        let mut app = App::default();
        for c in "hello".chars() {
            let k = key_with_kind(KeyCode::Char(c), KeyEventKind::Press);
            if should_handle_key(&k) {
                if let KeyCode::Char(ch) = k.code {
                    app.on_char(ch);
                }
            }
        }
        assert_eq!(app.input.buffer, "hello");
    }

    // ---- word-wrap 行数计算（对话区滚动范围） ----

    #[test]
    fn wrapped_line_count_short_line_is_one() {
        let l = RtLine::from("hello");
        assert_eq!(wrapped_line_count(&l, 80), 1);
    }

    #[test]
    fn wrapped_line_count_empty_line_is_one() {
        let l = RtLine::from("");
        assert_eq!(wrapped_line_count(&l, 80), 1);
    }

    #[test]
    fn wrapped_line_count_counts_cjk_by_display_width() {
        // 中文占 2 列：8 CJK + 5 ASCII = 21 列，在宽 8 的区域应占 3 行
        let l = RtLine::from("你好世界测试ABCDE".to_string());
        assert_eq!(wrapped_line_count(&l, 8), 3);
    }

    #[test]
    fn wrapped_line_count_english_word_wrap_not_underestimated() {
        // 旧逻辑按总宽度做数学估算：ceil(83 / 30) = 3 行；
        // 但 word wrap 留白后实际约 4 行，textwrap 应给出更接近的值。
        let l = RtLine::from(
            "This is a long sentence that will wrap across multiple lines in a narrow viewport.",
        );
        let rows = wrapped_line_count(&l, 30);
        assert!(rows >= 3, "word wrap 行数不应被低估，got {rows}");
    }

    // ---- 鼠标滚轮区域分流 ----

    fn rect(w: u16, h: u16) -> Rect {
        Rect::new(0, 0, w, h)
    }

    #[test]
    fn mouse_scroll_in_conversation_scrolls_history() {
        let mut app = App::default();
        let area = rect(80, 30); // 输入区从 row 27 开始
        let m = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            row: 10, // 对话区中部
            column: 5,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, m, area);
        assert_eq!(app.scroll_from_bottom, 3, "对话区滚轮应滚动历史");
    }

    #[test]
    fn mouse_scroll_in_input_moves_cursor() {
        let mut app = App::default();
        for c in "你好世界".chars() {
            app.on_char(c);
        }
        let area = rect(80, 30);
        let m = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            row: 28, // 输入区（27..30）
            column: 5,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, m, area);
        assert_eq!(
            app.input.cursor_char_index(),
            4 - 3,
            "输入区滚轮应左移光标 3 个字符"
        );
    }

    #[test]
    fn mouse_scroll_down_in_input_moves_cursor_right() {
        let mut app = App::default();
        for c in "你好世界".chars() {
            app.on_char(c);
        }
        app.input.move_home();
        let area = rect(80, 30);
        let m = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            row: 28,
            column: 5,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, m, area);
        assert_eq!(app.input.cursor_char_index(), 3);
    }

    #[test]
    fn mouse_scroll_down_in_conversation_returns_to_follow() {
        let mut app = App::default();
        app.scroll_up(10);
        assert!(!app.follow);
        let area = rect(80, 30);
        let m = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            row: 5,
            column: 5,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, m, area);
        // scroll_down(3) 不足以回到 0，但方向正确
        assert_eq!(app.scroll_from_bottom, 7);
    }

    #[test]
    fn mouse_other_kinds_ignored() {
        let mut app = App::default();
        let before = app.scroll_from_bottom;
        let m = MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            row: 5,
            column: 5,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, m, rect(80, 30));
        assert_eq!(app.scroll_from_bottom, before);
    }

    // ---- 工具名固定列（pad_display） ----

    #[test]
    fn pad_display_pads_short_ascii() {
        let out = pad_display("read_file", 12);
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 12);
        assert!(out.starts_with("read_file"));
    }

    #[test]
    fn pad_display_exact_width_unchanged() {
        let out = pad_display("123456789012", 12);
        assert_eq!(out, "123456789012");
    }

    #[test]
    fn pad_display_truncates_long_name_with_ellipsis() {
        let out = pad_display("very_long_tool_name", 12);
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 12);
        assert!(out.ends_with('…'), "截断应以省略号收尾: {out:?}");
    }

    #[test]
    fn pad_display_counts_cjk_by_display_width() {
        // 2 个中文 = 4 列，补足到 6 列应加 2 个空格（而非 4 个）。
        let out = pad_display("工具", 6);
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 6);
        assert_eq!(out, "工具  ");
    }

    // ---- 软光标与占位提示 ----

    #[test]
    fn cursor_cell_reversed_when_visible() {
        let s = cursor_cell(true, 'x');
        assert!(s.style.add_modifier.contains(Modifier::REVERSED));
        assert_eq!(s.content, "x");
    }

    #[test]
    fn cursor_cell_plain_when_hidden() {
        let s = cursor_cell(false, 'x');
        assert!(!s.style.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn placeholder_spans_hint_enter_only() {
        let spans = placeholder_spans();
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(joined.contains("⏎"), "应提示 Enter: {joined:?}");
        // 只提示已实现的功能，不应出现尚未落地的前缀入口。
        for not_yet in ["/", "@", "!", "$", "#"] {
            assert!(
                !joined.contains(not_yet),
                "P0 不应提示未实现入口 {not_yet}: {joined:?}"
            );
        }
    }

    // ---- 审批弹层字段行 ----

    #[test]
    fn approval_field_renders_key_and_value() {
        let line = approval_field("工具", "shell");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("工具"));
        assert!(text.contains("shell"));
    }

    // ---- 布局分区（鼠标分流依赖的索引约定） ----

    #[test]
    fn layout_has_four_regions_in_order() {
        let chunks = layout_chunks(rect(80, 30));
        assert_eq!(chunks[0].height, 1, "header 应为 1 行");
        assert_eq!(chunks[3].height, 1, "footer 应为 1 行");
        assert_eq!(chunks[2].height, 3, "输入区应为 3 行");
        assert!(chunks[1].height >= 3, "对话区至少 3 行");
        // 顺序校验：header < 对话 < 输入 < footer
        assert!(chunks[0].y < chunks[1].y);
        assert!(chunks[1].y < chunks[2].y);
        assert!(chunks[2].y < chunks[3].y);
    }

    // ---- 渲染快照（TestBackend，验证 P0 视觉验收项） ----

    /// 把整个界面渲染到内存缓冲并转成文本，用于断言实际输出。
    fn render_to_string(app: &App, w: u16, h: u16) -> String {
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| draw(f, app)).expect("draw");
        buffer_text(terminal.backend().buffer(), w, h)
    }

    /// 缓冲 → 文本。
    ///
    /// 宽字符（中文）会占用额外的「续行」cell，逐 cell 拼接会得到
    /// 「输 入」这种被撑开的文本；这里按显示宽度跳过续行 cell。
    fn buffer_text(buf: &Buffer, w: u16, h: u16) -> String {
        (0..h)
            .map(|y| {
                let mut row = String::new();
                let mut skip = 0u16;
                for x in 0..w {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    let sym = buf[(x, y)].symbol();
                    row.push_str(sym);
                    let cw = sym
                        .chars()
                        .next()
                        .and_then(UnicodeWidthChar::width)
                        .unwrap_or(1);
                    if cw > 1 {
                        skip = cw as u16 - 1;
                    }
                }
                row
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn app_with_all_kinds() -> App {
        let mut app = App::default();
        app.provider = "deepseek".into();
        app.status = "就绪".into();
        app.lines = vec![
            Line::User("帮我改一下配置".into()),
            Line::Assistant("好的，我来处理。".into()),
            Line::Reasoning("先看下现有配置".into()),
            Line::Tool {
                name: "read_file".into(),
                summary: "src/a.rs".into(),
                status: ToolStatus::Running,
            },
            Line::Tool {
                name: "edit_file".into(),
                summary: "src/a.rs".into(),
                status: ToolStatus::Ok { ms: 42 },
            },
            Line::Tool {
                name: "shell_exec".into(),
                summary: "rm -rf /".into(),
                status: ToolStatus::Failed {
                    reason: "命令退出码 1".into(),
                    ms: 12,
                },
            },
            Line::Tool {
                name: "web_fetch".into(),
                summary: "http://10.0.0.1".into(),
                status: ToolStatus::Rejected {
                    reason: "命中内网访问规则".into(),
                },
            },
            Line::System("任务已完成".into()),
            Line::Error("连接失败".into()),
        ];
        app
    }

    /// 去掉所有空白：宽字符（中文）在缓冲区里会占用额外的续行 cell，
    /// 逐 cell 拼接会得到「输 入 任 务」这种带空格的文本，先规范化再断言。
    fn squash(s: &str) -> String {
        s.chars().filter(|c| !c.is_whitespace()).collect()
    }

    #[test]
    fn render_shows_header_brand_and_tier_badge() {
        let out = render_to_string(&app_with_all_kinds(), 100, 40);
        let s = squash(&out);
        assert!(s.contains("ZERO"), "header 应含品牌名:\n{out}");
        assert!(s.contains("AUTO"), "L2 应显示 AUTO 徽章:\n{out}");
        assert!(s.contains("deepseek"), "header 应含 provider:\n{out}");
    }

    #[test]
    fn render_shows_all_message_glyphs() {
        let out = render_to_string(&app_with_all_kinds(), 100, 40);
        let s = squash(&out);
        for (g, what) in [
            ("▌", "用户消息左竖线"),
            ("⋯", "思考链标记"),
            ("⏺", "工具运行中"),
            ("✔", "工具成功"),
            ("✖", "工具失败"),
            ("⊘", "工具被拒"),
            ("·", "系统提示"),
        ] {
            assert!(s.contains(g), "缺少{what} {g}:\n{out}");
        }
    }

    // ---- 工具四态：耗时与原因（P1） ----

    /// 取构建结果中第一行包含 `needle` 的整行文本。
    fn tool_line(items: &[Line], needle: &str) -> String {
        build_conversation_lines(items, 96)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .find(|t| t.contains(needle))
            .unwrap_or_else(|| panic!("未找到含 {needle} 的工具行"))
    }

    #[test]
    fn render_running_tool_has_neither_ms_nor_reason() {
        let items = vec![Line::Tool {
            name: "read_file".into(),
            summary: "src/a.rs".into(),
            status: ToolStatus::Running,
        }];
        let line = tool_line(&items, "read_file");
        assert!(line.contains('⏺'), "运行中应显示 ⏺: {line}");
        assert!(!line.contains("ms"), "运行中不应显示耗时: {line}");
    }

    #[test]
    fn render_ok_tool_shows_elapsed_ms() {
        let items = vec![Line::Tool {
            name: "edit_file".into(),
            summary: "src/a.rs".into(),
            status: ToolStatus::Ok { ms: 42 },
        }];
        let line = tool_line(&items, "edit_file");
        assert!(line.contains('✔'), "成功应显示 ✔: {line}");
        assert!(line.contains("42ms"), "成功应显示耗时: {line}");
    }

    #[test]
    fn render_failed_tool_shows_elapsed_ms_and_reason() {
        let items = vec![Line::Tool {
            name: "shell_exec".into(),
            summary: "cargo test".into(),
            status: ToolStatus::Failed {
                reason: "命令退出码 1".into(),
                ms: 12,
            },
        }];
        let line = tool_line(&items, "shell_exec");
        assert!(line.contains('✖'), "失败应显示 ✖: {line}");
        assert!(line.contains("12ms"), "失败也应给出耗时: {line}");
        assert!(line.contains("命令退出码 1"), "失败应给出原因: {line}");
    }

    #[test]
    fn render_rejected_tool_shows_reason_without_ms() {
        // 被拒即未执行，显示耗时是误导。
        let items = vec![Line::Tool {
            name: "web_fetch".into(),
            summary: "http://10.0.0.1".into(),
            status: ToolStatus::Rejected {
                reason: "命中内网访问规则".into(),
            },
        }];
        let line = tool_line(&items, "web_fetch");
        assert!(line.contains('⊘'), "被拒应显示 ⊘: {line}");
        assert!(line.contains("命中内网访问规则"), "被拒应给出原因: {line}");
        assert!(!line.contains("ms"), "被拒不应显示耗时: {line}");
    }

    #[test]
    fn render_failed_tool_reason_is_flattened_to_one_line() {
        // 多行错误若原样铺开会撑破「一行一工具」的紧凑布局。
        let items = vec![Line::Tool {
            name: "shell_exec".into(),
            summary: "x".into(),
            status: ToolStatus::Failed {
                reason: "第一行\n  第二行".into(),
                ms: 1,
            },
        }];
        let line = tool_line(&items, "shell_exec");
        assert!(
            line.contains("第一行 第二行"),
            "多行原因应压成单行: {line:?}"
        );
    }

    #[test]
    fn render_long_reason_is_clipped() {
        let items = vec![Line::Tool {
            name: "shell_exec".into(),
            summary: "x".into(),
            status: ToolStatus::Failed {
                reason: "错误".repeat(200),
                ms: 1,
            },
        }];
        let line = tool_line(&items, "shell_exec");
        assert!(line.ends_with('…'), "超长原因应截断: {line:?}");
        assert!(
            UnicodeWidthStr::width(line.as_str()) < 200,
            "截断后宽度应显著变短: {line:?}"
        );
    }

    // ---- truncate_display / one_line ----

    #[test]
    fn truncate_display_keeps_short_text() {
        assert_eq!(truncate_display("短", 10), "短");
    }

    #[test]
    fn truncate_display_cjk_counts_by_display_width() {
        // 「工具」= 4 列；限宽 5 时应原样保留（不按字符数截断）。
        assert_eq!(truncate_display("工具", 5), "工具");
        let clipped = truncate_display("工具调用失败原因", 5);
        assert_eq!(UnicodeWidthStr::width(clipped.as_str()), 5);
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn one_line_collapses_whitespace_and_newlines() {
        assert_eq!(one_line("a\n  b\tc"), "a b c");
        assert_eq!(one_line("  两侧留白  "), "两侧留白");
    }

    #[test]
    fn render_shows_input_box_and_footer_hints() {
        let out = render_to_string(&app_with_all_kinds(), 100, 40);
        let s = squash(&out);
        assert!(s.contains("输入"), "应渲染输入框标题:\n{out}");
        assert!(s.contains("发送"), "footer 应含快捷键提示:\n{out}");
        assert!(s.contains("中断"), "footer 应含中断提示:\n{out}");
    }

    #[test]
    fn render_tool_name_is_column_aligned() {
        // read_file 与 edit_file 先补足到 12 列，摘要应起始于同一**显示列**。
        // 直接检查构建出的渲染行，避免受缓冲区宽字符占位影响。
        let items = app_with_all_kinds().lines;
        let built = build_conversation_lines(&items, 96);
        let text_of = |l: &RtLine| -> String {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        let r = built
            .iter()
            .map(text_of)
            .find(|t| t.contains("read_file"))
            .expect("read_file 行");
        let e = built
            .iter()
            .map(text_of)
            .find(|t| t.contains("edit_file"))
            .expect("edit_file 行");
        let col = |t: &str| UnicodeWidthStr::width(&t[..t.find("src/a.rs").expect("摘要")]);
        assert_eq!(col(&r), col(&e), "工具名对齐后摘要起始列应一致:\n{r}\n{e}");
    }

    #[test]
    fn render_empty_input_shows_placeholder_and_pill() {
        let out = render_to_string(&App::default(), 100, 40);
        let s = squash(&out);
        assert!(s.contains("输入任务"), "空输入应显示占位文字:\n{out}");
        assert!(s.contains("⏎"), "空输入应显示 Enter pill:\n{out}");
    }

    // ---- 审批弹层按键 ----

    fn key(code: KeyCode) -> KeyEvent {
        key_with_kind(code, KeyEventKind::Press)
    }

    /// 构造一个「正在等待高危审批」的 App。
    fn approval_app() -> App {
        let mut app = App::default();
        app.pending = Some(crate::app::PendingApproval {
            id: agent_core::ids::ApprovalId::new(),
            req: agent_core::perm::ApprovalRequest {
                id: agent_core::ids::ApprovalId::new(),
                task_id: agent_core::ids::TaskId::new(),
                tool: "shell".into(),
                args: serde_json::json!({ "command": "rm -rf /" }),
                risk: agent_core::perm::Risk::High,
                reason: "命中高危命令规则".into(),
                matched_rule: None,
            },
        });
        app
    }

    #[test]
    fn approval_lowercase_y_approves() {
        let mut app = approval_app();
        assert!(matches!(
            handle_key(&mut app, key(KeyCode::Char('y'))),
            UiAction::Approve { remember: false }
        ));
    }

    #[test]
    fn approval_uppercase_y_approves() {
        // 回归：弹层提示按键，用户按 Shift+Y 时 crossterm 上报 Char('Y')，
        // 只认小写会出现「按了没反应」。
        let mut app = approval_app();
        assert!(
            matches!(
                handle_key(&mut app, key(KeyCode::Char('Y'))),
                UiAction::Approve { remember: false }
            ),
            "Shift+Y 也应批准"
        );
    }

    #[test]
    fn approval_uppercase_n_rejects() {
        let mut app = approval_app();
        assert!(matches!(
            handle_key(&mut app, key(KeyCode::Char('N'))),
            UiAction::Reject
        ));
    }

    #[test]
    fn approval_enter_approves_and_esc_rejects() {
        let mut app = approval_app();
        assert!(matches!(
            handle_key(&mut app, key(KeyCode::Enter)),
            UiAction::Approve { .. }
        ));
        assert!(matches!(
            handle_key(&mut app, key(KeyCode::Esc)),
            UiAction::Reject
        ));
    }

    #[test]
    fn approval_ctrl_c_still_quits() {
        // 回归：弹层期间若不处理 Ctrl+C，用户会被卡住，只能强杀进程。
        let mut app = approval_app();
        let ctrl_c = KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: event::KeyEventState::NONE,
        };
        assert!(matches!(handle_key(&mut app, ctrl_c), UiAction::Quit));
    }

    #[test]
    fn approval_swallows_other_keys() {
        // 审批期间普通字符不应落进输入框。
        let mut app = approval_app();
        let _ = handle_key(&mut app, key(KeyCode::Char('x')));
        assert!(app.input.buffer.is_empty(), "弹层期间不应写入输入框");
    }

    #[test]
    fn render_approval_modal_overlays() {
        let app = approval_app();
        let out = render_to_string(&app, 100, 40);
        let s = squash(&out);
        assert!(s.contains("权限确认"), "应显示弹层标题:\n{out}");
        assert!(s.contains("高危"), "应显示风险等级:\n{out}");
        assert!(s.contains("批准"), "应显示操作提示:\n{out}");
    }

    #[test]
    fn only_later_user_messages_get_leading_blank_line() {
        let items = vec![
            Line::User("一".into()),
            Line::Assistant("答".into()),
            Line::User("二".into()),
        ];
        let built = build_conversation_lines(&items, 80);
        let text_of =
            |l: &RtLine| -> String { l.spans.iter().map(|s| s.content.as_ref()).collect() };
        let texts: Vec<String> = built.iter().map(text_of).collect();
        assert!(
            !texts[0].trim().is_empty(),
            "首条用户消息不应前置空行: {texts:?}"
        );
        let idx = texts
            .iter()
            .position(|t| t.contains('二'))
            .expect("第二条用户消息");
        assert!(
            texts[idx - 1].trim().is_empty(),
            "第二条用户消息前应有空行: {texts:?}"
        );
    }

    #[test]
    #[ignore = "仅用于人工目视校验布局"]
    fn preview_visual_layout() {
        println!("\n{}", render_to_string(&app_with_all_kinds(), 100, 22));
    }

    #[test]
    fn mouse_row_inside_footer_is_not_input() {
        // footer 是最后 1 行，落在 footer 的滚轮不应移动输入光标。
        let area = rect(80, 30);
        let mut app = App::default();
        for c in "abc".chars() {
            app.on_char(c);
        }
        let m = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            row: 29, // footer
            column: 5,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, m, area);
        assert_eq!(
            app.input.cursor_char_index(),
            3,
            "footer 区滚轮不应移动光标"
        );
    }

    /// 回归：任务收尾时，最后一段回答不该在帧里出现两次。
    ///
    /// `task_finished.summary` 与最后一轮 `assistant_turn.text` 字节相同
    /// （runner 直接 clone 的 `out.text`），TUI 曾把它整段再塞进一个灰显的
    /// 系统行——「任务完成之后还会输出这么多暗色内容」即此。
    #[test]
    fn task_finished_frame_shows_the_answer_once() {
        use agent_core::event::{Event, Outcome};
        use agent_core::ids::TaskId;

        let mut app = App::default();
        let id = TaskId::new();
        let text = "项目介绍：一个个人独占的 Agent。";
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

        let dump = render_to_string(&app, 80, 22);
        assert_eq!(
            dump.matches("一个个人独占").count(),
            1,
            "帧里只应出现一次：\n{dump}"
        );
    }

    // ---- 整屏重绘必须真的擦屏（旧帧字尾不能留在屏幕上）----

    /// 回归：`swap_buffers()` 只重置后台缓冲、不擦屏，于是下一帧的差分变成
    /// 「空白 vs 新帧」——新帧留空的单元格不写，屏幕上那些位置仍是旧帧的字尾，
    /// 表现为 `list_dir e. 0msng the project…` 这种两帧文字叠在一起。
    /// 整屏重绘必须走 `Terminal::clear()`：擦屏 + 重置缓冲，两者同步。
    #[test]
    fn forced_repaint_erases_stale_cells() {
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(60, 8)).expect("terminal");
        let mut app = App::default();
        app.lines = vec![Line::Assistant("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into())];
        draw_frame(&mut terminal, &mut app).expect("draw");

        // 新帧比旧帧短：旧帧的字尾必须被擦掉，而不是留在屏幕上。
        app.lines = vec![Line::Assistant("B".into())];
        app.request_repaint();
        draw_frame(&mut terminal, &mut app).expect("draw");

        let dump = buffer_text(terminal.backend().buffer(), 60, 8);
        assert!(!dump.contains("AAAA"), "旧帧的字尾没被擦掉：\n{dump}");
        assert!(dump.contains('B'), "新帧要画出来：\n{dump}");
    }

    // ---- 整屏重绘（Ctrl+L 手动救回黑屏）----

    #[test]
    fn ctrl_l_requests_repaint() {
        let mut app = App::default();
        let _ = app.take_repaint();
        let key = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert!(matches!(handle_key(&mut app, key), UiAction::None));
        assert!(app.take_repaint(), "Ctrl+L 应请求整屏重绘");
    }

    #[test]
    fn plain_l_is_still_a_normal_character() {
        // Ctrl+L 不能把普通输入吃掉。
        let mut app = App::default();
        let _ = app.take_repaint();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
        );
        assert_eq!(app.input.buffer, "l");
        assert!(!app.take_repaint());
    }
}
