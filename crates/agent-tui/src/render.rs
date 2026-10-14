//! 渲染 + 事件循环。

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as RtLine, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use ratatui::Terminal;
use tokio::sync::mpsc;

use agent_core::event::{Command, Event};

use crate::app::{App, Line, UiAction};

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
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, &mut app, &mut ev_rx, &cmd_tx).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    ev_rx: &mut mpsc::Receiver<Event>,
    cmd_tx: &mpsc::Sender<Command>,
) -> anyhow::Result<()> {
    let tick = Duration::from_millis(120);
    loop {
        terminal.draw(|f| draw(f, app))?;

        // 非阻塞地抽干事件队列
        while let Ok(ev) = ev_rx.try_recv() {
            app.apply_event(ev);
        }

        if event::poll(tick)? {
            if let CtEvent::Key(key) = event::read()? {
                match handle_key(app, key) {
                    UiAction::Quit => {
                        let _ = cmd_tx.send(Command::Quit).await;
                        return Ok(());
                    }
                    UiAction::Submit(text) => {
                        let mut spec = agent_core::task::TaskSpec::new(text.clone(), String::new());
                        spec.title = text.chars().take(24).collect();
                        // 工作区由编排层填入；这里发送带 prompt 的任务
                        let _ = cmd_tx.send(Command::SubmitTask(spec)).await;
                    }
                    UiAction::Cancel => {
                        if let Some(c) = app.cancel_command() {
                            let _ = cmd_tx.send(c).await;
                        }
                    }
                    UiAction::Approve { remember } => {
                        if let Some(c) = app.take_approval(true, remember) {
                            let _ = cmd_tx.send(c).await;
                        }
                    }
                    UiAction::Reject => {
                        if let Some(c) = app.take_approval(false, false) {
                            let _ = cmd_tx.send(c).await;
                        }
                    }
                    UiAction::None => {}
                }
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

/// 按键 -> 动作。
fn handle_key(app: &mut App, key: KeyEvent) -> UiAction {
    // 弹层优先
    if app.pending.is_some() {
        return match key.code {
            KeyCode::Char('y') | KeyCode::Enter => UiAction::Approve { remember: false },
            KeyCode::Char('a') => UiAction::Approve { remember: true },
            KeyCode::Char('n') | KeyCode::Esc => UiAction::Reject,
            _ => UiAction::None,
        };
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => UiAction::Quit,
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
        (KeyCode::PageUp, _) => {
            app.scroll_up(5);
            UiAction::None
        }
        (KeyCode::PageDown, _) => {
            app.scroll_down(5);
            UiAction::None
        }
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
            app.on_char(c);
            UiAction::None
        }
        _ => UiAction::None,
    }
}

fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // 状态栏
            Constraint::Min(3),    // 对话流
            Constraint::Length(3), // 输入区
        ])
        .split(area);

    draw_status(f, app, chunks[0]);
    draw_conversation(f, app, chunks[1]);
    draw_input(f, app, chunks[2]);

    if let Some(p) = &app.pending {
        draw_approval(f, app, p, area);
    }
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let text = format!(
        " 个人 Agent  权限:{}  模型:{}  状态:{} ",
        app.tier, app.provider, app.status
    );
    let p = Paragraph::new(text).style(Style::default().bg(Color::Blue).fg(Color::White));
    f.render_widget(p, area);
}

fn draw_conversation(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<RtLine> = Vec::new();
    for l in &app.lines {
        match l {
            Line::User(s) => {
                lines.push(RtLine::from(vec![Span::styled(
                    "▌ 你: ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )]));
                for tl in s.lines() {
                    lines.push(RtLine::from(tl.to_string()));
                }
            }
            Line::Assistant(s) => {
                lines.push(RtLine::from(Span::styled(
                    "◇ Agent:",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )));
                for tl in s.lines() {
                    lines.push(RtLine::from(tl.to_string()));
                }
            }
            Line::Reasoning(s) => {
                for tl in s.lines() {
                    lines.push(RtLine::from(Span::styled(
                        format!("  {tl}"),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
            Line::Tool {
                name,
                summary,
                done,
                ok,
            } => {
                let mark = if !*done {
                    "▸"
                } else if *ok {
                    "✔"
                } else {
                    "✘"
                };
                let color = if !*done {
                    Color::Yellow
                } else if *ok {
                    Color::Green
                } else {
                    Color::Red
                };
                lines.push(RtLine::from(vec![
                    Span::styled(format!("  {mark} [工具] "), Style::default().fg(color)),
                    Span::styled(
                        name.clone(),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(summary.clone(), Style::default().fg(Color::Gray)),
                ]));
            }
            Line::System(s) => {
                for tl in s.lines() {
                    lines.push(RtLine::from(Span::styled(
                        format!("  {tl}"),
                        Style::default().fg(Color::Magenta),
                    )));
                }
            }
            Line::Error(s) => {
                for tl in s.lines() {
                    lines.push(RtLine::from(Span::styled(
                        format!("  {tl}"),
                        Style::default().fg(Color::Red),
                    )));
                }
            }
        }
    }

    let total = lines.len() as u16;
    let height = area.height.saturating_sub(2);
    let max_scroll = total.saturating_sub(height);
    let scroll = max_scroll.saturating_sub(app.scroll_from_bottom);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" 对话 ")
        .border_style(Style::default().fg(Color::DarkGray));
    let p = Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(p, area);
}

fn draw_input(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" 输入任务 (Enter 发送 / Esc 中断 / Ctrl+C 退出) ")
        .border_style(Style::default().fg(Color::Cyan));
    let p = Paragraph::new(app.input.buffer.clone()).block(block);
    f.render_widget(p, area);
    // 光标
    let cursor_x = area.x + 1 + app.input.buffer.chars().count() as u16;
    let cursor_y = area.y + 1;
    f.set_cursor_position((cursor_x.min(area.right().saturating_sub(2)), cursor_y));
}

fn draw_approval(f: &mut Frame, _app: &App, p: &crate::app::PendingApproval, area: Rect) {
    let w = area.width.min(72);
    let h = 9.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect::new(x, y, w, h);

    f.render_widget(Clear, popup);
    let risk = match p.req.risk {
        agent_core::perm::Risk::High => "高危",
        agent_core::perm::Risk::Medium => "中危",
        agent_core::perm::Risk::Low => "低危",
    };
    let args = truncate_json(&p.req.args, 200);
    let text = format!(
        "工具: {}\n风险: {}\n参数: {}\n\n[y]批准  [a]本任务此类全批准  [n]拒绝",
        p.req.tool, risk, args
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" 需要确认 ")
        .border_style(Style::default().fg(Color::Yellow));
    let para = Paragraph::new(text).block(block).wrap(Wrap { trim: true });
    f.render_widget(para, popup);
}

fn truncate_json(v: &serde_json::Value, max: usize) -> String {
    let s = v.to_string();
    if s.chars().count() <= max {
        s
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}
