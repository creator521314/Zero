//! Markdown → 终端行（`RtLine` 序列，带样式）。
//!
//! 模型输出的是 Markdown。终端里原样平铺会把 `### **加粗**`、`| 列 |` 这类
//! 源码符号也印出来——读的人是在读源码，不是在读答案。
//!
//! 这里只渲染**终端表达得了的子集**：
//!
//! | 元素 | 呈现 |
//! |---|---|
//! | 标题 | 主色 / 强调色 + 加粗，上方留一空行 |
//! | 加粗 / 斜体 / 删除线 | `Bold` / `Italic` / `CrossedOut` 修饰 |
//! | 行内代码 | 强调色文字（**不用** success/warning/error——那是状态的语义色） |
//! | 围栏代码块 | 左侧竖条 + 整行铺底，与正文明显分层 |
//! | 有序 / 无序列表 | 缩进 + `•` / 序号，续行悬挂对齐 |
//! | 任务列表 | `✔` / `□` |
//! | 引用 | 左侧 `│` + 灰显竖条 |
//! | 分隔线 | 32 列细线（**不**铺满，铺满的分隔线留给「任务完成」） |
//! | 链接 | 文字加下划线，URL 与文字不同时附后 |
//!
//! 表格与 HTML **不渲染**：表格要算列宽，窄屏一定挤坏；HTML 事件直接丢弃
//! （终端不是浏览器，渲染 HTML 既没意义也不安全）。pulldown-cmark 会把表格
//! 当成普通段落文本交出来，所以**内容不丢**，只是保留源码形态。
//!
//! **流式友好**：LLM 是逐字吐的，片段常常是残缺的 Markdown——`**加粗` 没闭合、
//! ```` ``` ```` 围栏只写了一半。pulldown-cmark 对未闭合围栏会照代码块内容收，
//! 未闭合的行内标记会当成字面文本：不会崩、不会吞字符，代价是那一帧的样式
//! 会闪一下。**这个代价比丢内容可接受**，流式渲染的首要原则是不丢字符。

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as RtLine, Span};
use unicode_width::UnicodeWidthStr;

use crate::render::wrap_line;
use crate::theme::{glyph, ink, surface};

/// 正文前景。
const BODY_FG: Color = ink::FG;
/// 行内代码与代码块竖条的前景色。
const CODE_FG: Color = ink::ACCENT;
/// Markdown 分隔线的长度（列）。
const RULE_LEN: usize = 32;

/// 把一段 Markdown 渲染成终端行。
///
/// `width` 用于软换行；传 0 表示不换行（仅供测试）。行会计入容器缩进，
/// 续行按 bullet 之后对齐（悬挂缩进）——列表项换行后不该顶格。
pub fn parse(src: &str, width: usize) -> Vec<RtLine<'static>> {
    Renderer::new(width).run(src)
}

struct Renderer {
    width: usize,
    out: Vec<RtLine<'static>>,
    /// 当前行累积的 span（不含容器前缀与 bullet）。
    line: Vec<Span<'static>>,
    /// 仅当前列表项首行使用。
    bullet: Vec<Span<'static>>,
    /// 当前块的前景（标题会换色）。
    block_fg: Color,
    /// 行内代码的前景色（`None` = 用块前景）。
    fg: Option<Color>,
    /// 累积的行内修饰（粗 / 斜 / 删除线）。
    mods: Vec<Modifier>,
    /// 当前块是否铺底色（代码块）。
    bg: Option<Color>,
    /// 代码块内：由 flush_row 自己收行，内容按原样保留。
    quote: usize,
    list_depth: usize,
    ordered: Option<u64>,
    /// 外层列表的序号（回到外层时要恢复）。
    ordered_stack: Vec<Option<u64>>,
    /// 当前链接的 URL（进入 `[text](url)` 时记录，离开时决定是否补显示）。
    link_url: Option<String>,
    /// 当前链接已收集到的文字（用来判断「链接文字是否就是 URL」）。
    link_text: String,
    started: bool,
}

impl Renderer {
    fn new(width: usize) -> Self {
        Self {
            width,
            out: Vec::new(),
            line: Vec::new(),
            bullet: Vec::new(),
            block_fg: BODY_FG,
            fg: None,
            mods: Vec::new(),
            bg: None,
            quote: 0,
            list_depth: 0,
            ordered: None,
            ordered_stack: Vec::new(),
            link_url: None,
            link_text: String::new(),
            started: false,
        }
    }

    fn run(mut self, src: &str) -> Vec<RtLine<'static>> {
        let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
        for ev in Parser::new_ext(src, opts) {
            match ev {
                Event::Start(tag) => self.start(tag),
                Event::End(tag) => self.end(tag),
                Event::Text(t) => self.push(t.as_ref(), self.inline_style()),
                Event::Code(t) => self.push(t.as_ref(), Style::default().fg(CODE_FG)),
                Event::SoftBreak | Event::HardBreak => self.flush_row(),
                Event::Rule => {
                    self.flush_row();
                    self.push(&"─".repeat(RULE_LEN), Style::default().fg(surface::LINE));
                    self.flush_row();
                }
                Event::TaskListMarker(done) => {
                    let (mark, color) = if done {
                        (glyph::OK, ink::SUCCESS)
                    } else {
                        ("□", ink::FG_MUTED)
                    };
                    self.push(&format!("{mark} "), Style::default().fg(color));
                }
                // 公式：终端渲染不了 TeX，但**内容不能丢**，按字面文本输出。
                Event::InlineMath(t) | Event::DisplayMath(t) => {
                    self.push(t.as_ref(), self.inline_style())
                }
                // HTML：终端不是浏览器，渲染它既没意义也不安全，直接丢弃。
                Event::Html(_) | Event::InlineHtml(_) | Event::FootnoteReference(_) => {}
            }
        }
        self.flush_row();
        self.out
    }

    // ---- 行输出 ----

    /// 收当前行：补容器前缀与 bullet，软换行，续行走悬挂缩进。
    fn flush_row(&mut self) {
        if self.line.is_empty() && self.bullet.is_empty() {
            return; // 空行不输出尾随垃圾
        }
        let bullet = std::mem::take(&mut self.bullet);
        let content: Vec<Span<'static>> = std::mem::take(&mut self.line);
        let container = self.container_prefix();
        let head_w = width_of(&container) + width_of(&bullet);

        let rows = if self.width == 0 {
            vec![RtLine::from(content)]
        } else {
            let budget = self.width.saturating_sub(head_w).max(1);
            wrap_line(RtLine::from(content), budget)
        };

        for (i, r) in rows.into_iter().enumerate() {
            let mut spans = container.clone();
            if i == 0 {
                spans.extend(bullet.clone());
            } else if !bullet.is_empty() {
                spans.push(Span::raw(" ".repeat(width_of(&bullet))));
            }
            spans.extend(r.spans);
            self.out.push(self.pad(RtLine::from(spans)));
        }
    }

    /// 代码块需要整行铺底，否则底色到文字末尾就断了，右侧参差。
    fn pad(&self, line: RtLine<'static>) -> RtLine<'static> {
        let Some(bg) = self.bg else {
            return line;
        };
        let used: usize = line
            .spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        let pad = self.width.saturating_sub(used);
        let mut spans = line.spans;
        spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
        RtLine::from(spans)
    }

    /// 容器前缀：基础缩进 + 引用竖条 + 列表嵌套缩进。
    fn container_prefix(&self) -> Vec<Span<'static>> {
        let mut v = vec![Span::raw("  ")];
        if self.bg.is_some() {
            // 代码块：竖条 + 铺底
            v.push(Span::styled("▎ ", Style::default().fg(CODE_FG)));
            return v;
        }
        for _ in 0..self.quote {
            v.push(Span::styled("│ ", Style::default().fg(ink::FG_MUTED)));
        }
        if self.list_depth > 1 {
            v.push(Span::raw("  ".repeat(self.list_depth - 1)));
        }
        v
    }

    // ---- 样式 ----

    fn inline_style(&self) -> Style {
        let mut s = Style::default().fg(self.fg.unwrap_or(self.block_fg));
        for m in &self.mods {
            s = s.add_modifier(*m);
        }
        s
    }

    fn push(&mut self, content: &str, style: Style) {
        if content.is_empty() {
            return;
        }
        // 代码块的内容是**单个含换行的 Text 事件**——pulldown-cmark 不为它发
        // SoftBreak。直接整段塞进一个 span 会被 ratatui 当成同一行的单元格，
        // 代码三行就粘成一行了。所以这里自己按 \n 切。
        let mut first = true;
        for part in content.split('\n') {
            if !first {
                self.flush_row();
            }
            first = false;
            if part.is_empty() {
                continue;
            }
            self.started = true;
            // 链接文字要攒起来：离开 `End(Link)` 时用它判断「要不要补显示 URL」。
            if self.link_url.is_some() {
                self.link_text.push_str(part);
            }
            match self.line.last_mut() {
                Some(s) if s.style == style => s.content.to_mut().push_str(part),
                _ => self.line.push(Span::styled(part.to_string(), style)),
            }
        }
    }

    // ---- 块级事件 ----

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {
                // 段间距：不让所有段落粘成一大坨。heading / code 自带留白，
                // 不再额外补，否则标题与正文之间会空出一整个空行。
                self.flush_row();
                if self.started {
                    self.out.push(RtLine::from(""));
                }
            }
            Tag::Heading { level, .. } => {
                self.flush_row();
                if self.started {
                    self.out.push(RtLine::from(""));
                }
                self.block_fg = heading_fg(level);
                self.mods.push(Modifier::BOLD);
            }
            Tag::CodeBlock(_) => {
                self.flush_row();
                if self.started {
                    self.out.push(RtLine::from(""));
                }
                self.bg = Some(surface::FAINT);
            }
            Tag::BlockQuote(_) => {
                self.flush_row();
                self.quote += 1;
            }
            Tag::List(ord) => {
                self.flush_row();
                self.list_depth += 1;
                self.ordered_stack.push(self.ordered.take());
                self.ordered = ord;
            }
            Tag::Item => {
                self.flush_row();
                let bullet = match self.ordered {
                    Some(n) => {
                        self.ordered = Some(n + 1);
                        format!("{n}. ")
                    }
                    None => format!("{} ", glyph::BULLET),
                };
                self.bullet = vec![Span::styled(
                    bullet,
                    Style::default()
                        .fg(ink::FG_MUTED)
                        .add_modifier(Modifier::BOLD),
                )];
            }
            Tag::Emphasis => self.mods.push(Modifier::ITALIC),
            Tag::Strong => self.mods.push(Modifier::BOLD),
            Tag::Strikethrough => self.mods.push(Modifier::CROSSED_OUT),
            Tag::Link { dest_url, .. } => {
                // 链接：文字加下划线。URL 与文字不同才附后——大多数链接的
                // 文字已经是描述，重复贴一遍 URL 只是噪音。
                self.link_url = Some(dest_url.to_string());
                self.link_text.clear();
                if !self.mods.contains(&Modifier::UNDERLINED) {
                    self.mods.push(Modifier::UNDERLINED);
                }
            }
            Tag::Image { .. } => {}
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Item => self.flush_row(),
            TagEnd::CodeBlock => {
                // 底色必须在这里卸掉，否则代码块之后的所有行都还带着 ▎ 和铺底。
                self.flush_row();
                self.bg = None;
            }
            TagEnd::Heading(_) => {
                self.flush_row();
                self.block_fg = BODY_FG;
                self.mods.retain(|m| *m != Modifier::BOLD);
            }
            TagEnd::BlockQuote(_) => {
                self.flush_row();
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::List(_) => {
                self.flush_row();
                self.list_depth = self.list_depth.saturating_sub(1);
                self.ordered = self.ordered_stack.pop().flatten();
            }
            TagEnd::Emphasis => self.mods.retain(|m| *m != Modifier::ITALIC),
            TagEnd::Strong => self.mods.retain(|m| *m != Modifier::BOLD),
            TagEnd::Strikethrough => self.mods.retain(|m| *m != Modifier::CROSSED_OUT),
            TagEnd::Link => {
                self.mods.retain(|m| *m != Modifier::UNDERLINED);
                let url = self.link_url.take().unwrap_or_default();
                let shown = std::mem::take(&mut self.link_text);
                if shown != url && !url.is_empty() && !shown.is_empty() {
                    self.push(&format!(" ({url})"), Style::default().fg(ink::FG_MUTED));
                }
            }
            TagEnd::Image => {}
            _ => {}
        }
    }
}

/// 标题前景：一级二级用主色，更深层级用强调色（层级靠色相差分，不靠字号——
/// 终端没有字号）。
fn heading_fg(level: HeadingLevel) -> Color {
    match level {
        HeadingLevel::H1 | HeadingLevel::H2 => ink::PRIMARY,
        _ => ink::ACCENT,
    }
}

fn width_of(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(rows: &[RtLine<'static>]) -> String {
        rows.iter()
            .map(|r| {
                r.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn heading_drops_hash_marks_and_is_bold() {
        let rows = parse("## 标题\n", 40);
        let s = text(&rows);
        assert!(!s.contains('#'), "不能再把 # 印出来：{s:?}");
        assert!(s.contains("标题"), "{s:?}");
        let heading = rows
            .iter()
            .find(|r| r.spans.iter().any(|s| s.content.contains("标题")))
            .expect("有标题行");
        let style = heading
            .spans
            .iter()
            .find(|s| s.content.contains("标题"))
            .expect("标题 span")
            .style;
        assert!(style.add_modifier.contains(Modifier::BOLD), "标题要加粗");
        assert_eq!(style.fg, Some(ink::PRIMARY), "二级标题用主色");
    }

    #[test]
    fn deep_heading_uses_accent() {
        let rows = parse("#### 深层标题\n", 40);
        let style = rows
            .iter()
            .find(|r| r.spans.iter().any(|s| s.content.contains("深层")))
            .and_then(|r| r.spans.iter().find(|s| s.content.contains("深层")))
            .expect("标题 span")
            .style;
        assert_eq!(style.fg, Some(ink::ACCENT));
    }

    #[test]
    fn inline_markup_is_styled_not_printed() {
        let rows = parse("这是 **粗体** 和 `code` 的一段。\n", 40);
        let s = text(&rows);
        assert!(!s.contains("**"), "加粗标记不该印出来：{s:?}");
        assert!(!s.contains('`'), "反引号不该印出来：{s:?}");
        assert!(s.contains("粗体") && s.contains("code"));
        let all: Vec<Span<'static>> = rows.iter().flat_map(|r| r.spans.clone()).collect();
        assert!(
            all.iter().any(
                |s| s.content.contains("粗体") && s.style.add_modifier.contains(Modifier::BOLD)
            ),
            "粗体要带 Bold"
        );
        assert!(
            all.iter()
                .any(|s| s.content.contains("code") && s.style.fg == Some(CODE_FG)),
            "行内代码要用代码色"
        );
    }

    #[test]
    fn emphasis_is_italic() {
        let rows = parse("这是 *斜体*。\n", 40);
        let all: Vec<Span<'static>> = rows.iter().flat_map(|r| r.spans.clone()).collect();
        assert!(
            all.iter()
                .any(|s| s.content.contains("斜体")
                    && s.style.add_modifier.contains(Modifier::ITALIC)),
            "斜体要带 Italic：{:?}",
            all.iter().map(|s| s.content.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn code_block_keeps_content_and_layers() {
        let rows = parse("```rust\nfn main() {}\n```\n", 40);
        let s = text(&rows);
        assert!(!s.contains("```"), "围栏不该印出来：{s:?}");
        assert!(s.contains("fn main() {}"), "代码内容要保留：{s:?}");
        let code_row = rows
            .iter()
            .find(|r| r.spans.iter().any(|s| s.content.contains("fn main")))
            .expect("有代码行");
        assert!(
            code_row.spans.iter().any(|s| s.content.contains('▎')),
            "代码块要有左竖条：{:?}",
            code_row
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<Vec<_>>()
        );
        assert!(
            code_row
                .spans
                .iter()
                .any(|s| s.style.bg == Some(surface::FAINT)),
            "代码块要铺底色"
        );
    }

    #[test]
    fn unordered_list_uses_bullet() {
        let rows = parse("- 第一项\n- 第二项\n", 40);
        let s = text(&rows);
        assert!(!s.contains("- 第一"), "连字符要换成项目符号：{s:?}");
        assert!(s.contains(&format!("{} 第一项", glyph::BULLET)), "{s:?}");
        assert_eq!(rows.len(), 2, "两项各一行：{s:?}");
    }

    #[test]
    fn ordered_list_numbers_increment() {
        let s = text(&parse("1. 甲\n2. 乙\n3. 丙\n", 40));
        assert!(
            s.contains("1. 甲") && s.contains("2. 乙") && s.contains("3. 丙"),
            "{s:?}"
        );
    }

    #[test]
    fn task_list_shows_state() {
        let s = text(&parse("- [x] 已完成\n- [ ] 未完成\n", 40));
        assert!(s.contains(&format!("{} 已完成", glyph::OK)), "{s:?}");
        assert!(s.contains("□ 未完成"), "{s:?}");
    }

    #[test]
    fn blockquote_has_bar() {
        let rows = parse("> 引用一句话\n", 40);
        let s = text(&rows);
        assert!(!s.contains('>'), "尖括号要换成竖条：{s:?}");
        assert!(s.contains("│ 引用一句话"), "{s:?}");
    }

    #[test]
    fn thematic_break_becomes_rule() {
        let rows = parse("上文\n\n---\n\n下文\n", 40);
        let s = text(&rows);
        assert!(s.contains(&"─".repeat(RULE_LEN)), "要有分隔线：{s:?}");
        assert!(!s.contains("---"), "源码形态要换掉：{s:?}");
    }

    #[test]
    fn link_text_is_underlined() {
        let rows = parse("见 [官网](https://example.com)。\n", 60);
        let s = text(&rows);
        assert!(!s.contains("]("), "链接源码形态要换掉：{s:?}");
        let all: Vec<Span<'static>> = rows.iter().flat_map(|r| r.spans.clone()).collect();
        assert!(
            all.iter().any(|s| s.content.contains("官网")
                && s.style.add_modifier.contains(Modifier::UNDERLINED)),
            "链接文字要加下划线：{s:?}"
        );
    }

    /// 流式首要原则：**一个字符都不能丢**。片段残缺时样式可以闪，内容不能少。
    #[test]
    fn unclosed_markup_still_keeps_every_character() {
        // 未闭合的加粗、未闭合的围栏代码块、半个链接——都是流式中的常态
        let partials = [
            "正在算 **加粗没闭合",
            "```\nfn main() {", // 语言标注（` ```rust ` 的 rust）不属于内容，另行断言
            "见 [官网](https://exa",
            "### ",
            "> ",
            "- ",
        ];
        for p in partials {
            let rows = parse(p, 40);
            let s: String = text(&rows).chars().filter(|c| !c.is_whitespace()).collect();
            let src: String = p.chars().filter(|c| !c.is_whitespace()).collect();
            // 丢掉的只允许是 Markdown 的语法字符（本来就不该显示）
            for ch in src.chars() {
                if !"#*`>-[]() ".contains(ch) {
                    assert!(s.contains(ch), "字符 {ch:?} 丢了：原文 {p:?} → {s:?}");
                }
            }
        }
    }

    /// 围栏上的语言标注（` ```rust `）是语法元信息，**不属于**代码块内容，
    /// 不显示——真把它印出来，读者会以为那是代码的第一行。
    #[test]
    fn code_block_lang_tag_is_not_shown_as_content() {
        let s = text(&parse("```rust\nfn main() {}\n```\n", 40));
        assert!(!s.contains("rust"), "语言标注不该当内容显示：{s:?}");
        assert!(s.contains("fn main() {}"), "代码本身要在：{s:?}");
    }

    #[test]
    fn rows_never_exceed_width() {
        let src = "### 标题\n\n这是**很长的一段话**，用来验证软换行之后每一行都没有超出给定宽度，\
                   里面混了 `inline_code` 和中文标点。\n\n- 列表项也比较长，长到需要折行才能放得下才好看出续行对齐\n";
        let rows = parse(src, 30);
        for r in &rows {
            let w: usize = width_of(&r.spans);
            assert!(
                w <= 30,
                "行宽 {w} 超过 30：{:?}",
                text(std::slice::from_ref(r))
            );
        }
    }

    /// 列表项换行后，续行要对齐到 bullet 之后（悬挂缩进），不能顶格。
    #[test]
    fn wrapped_list_item_hangs_after_bullet() {
        let rows = parse(
            "- 这是一个很长的列表项，长到必须在终端里折行才可以放下\n",
            20,
        );
        assert!(rows.len() >= 2, "应该换行：{:?}", text(&rows));
        let second = &rows[1];
        let content = second
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(
            content.starts_with("    "),
            "续行要缩进到 bullet 之后：{content:?}"
        );
    }

    #[test]
    fn table_degrades_to_plain_text_without_losing_content() {
        // 表格不在终端的能力范围内：降级为原样文本，内容不丢
        let rows = parse("| 名称 | 说明 |\n|---|---|\n| a | b |\n", 40);
        let s = text(&rows);
        assert!(
            s.contains("名称") && s.contains("说明") && s.contains("b"),
            "{s:?}"
        );
    }

    #[test]
    fn html_is_dropped() {
        let rows = parse("正文<script>alert(1)</script>\n", 40);
        let s = text(&rows);
        assert!(!s.contains("script"), "HTML 要丢掉：{s:?}");
        assert!(s.contains("正文"), "{s:?}");
    }
}
