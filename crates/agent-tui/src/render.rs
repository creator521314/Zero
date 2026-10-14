//! 渲染 + 事件循环。

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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
    // 隐藏终端原生光标：输入框使用软光标（反显块）渲染，
    // 否则会与软光标重叠成"两个光标"，且原生光标在 alternate
    // screen 下多数终端不闪烁。
    execute!(stdout, EnterAlternateScreen, crossterm::cursor::Hide)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, &mut app, &mut ev_rx, &cmd_tx).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        crossterm::cursor::Show
    )?;
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

        // 驱动软光标闪烁（与终端原生光标行为无关，跨平台一致）。
        app.on_tick();

        if event::poll(tick)? {
            if let CtEvent::Key(key) = event::read()? {
                // Windows 的 crossterm 会对每次输入同时上报 Press 与 Release；
                // 中文 IME 尤其明显（每个字符两条事件）。只处理 Press，
                // 否则字符会被插入两次（“你好” → “你你好好”）。
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

    // 软光标：把 buffer 按光标切成 前 / 光标处 / 后 三段，光标处用反显。
    // 不依赖 f.set_cursor_position()——alternate screen 下 Windows Terminal
    // 的原生光标常不闪烁，软光标在任何终端都有一致的闪烁行为。
    let buf = &app.input.buffer;
    let cursor_idx = app.input.cursor_char_index();
    let before: String = buf.chars().take(cursor_idx).collect();
    let mut rest = buf.chars().skip(cursor_idx);
    let at_char = rest.next().unwrap_or(' ');
    let after: String = rest.collect();

    let mut spans = vec![Span::raw(before)];
    if app.cursor_visible {
        spans.push(Span::styled(
            at_char.to_string(),
            Style::default().add_modifier(Modifier::REVERSED),
        ));
    } else {
        spans.push(Span::raw(at_char.to_string()));
    }
    spans.push(Span::raw(after));

    let p = Paragraph::new(RtLine::from(spans)).block(block);
    f.render_widget(p, area);
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
