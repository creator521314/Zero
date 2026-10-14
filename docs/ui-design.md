# Zero TUI 界面设计方案 V1

| 项目 | 内容 |
|---|---|
| 文档版本 | V1（UI 方案阶段产出） |
| 编制日期 | 2026-10-14 |
| 设计参考 | `D:\git_rc\iCode-main`（Python + Textual 实现，Dracula 系主题） |
| 适用范围 | `crates/agent-tui`（ratatui + crossterm） |
| 关联里程碑 | M2（P0/P1 落地）、M4（P2 落地） |
| 文档状态 | P0 已实施（2026-10-14），P1 / P2 待评审 |

---

## 一、设计目标与参考分析

### 1.1 目标

在**不改变 `crates/agent-tui/src/app.rs` 事件逻辑**的前提下，为现有 TUI 补上「视觉表达层」：
让消息类型一眼可辨、工具调用状态清晰、输入与快捷键可发现，并保留后续（M4）扩展侧边栏、Markdown、diff 视图的空间。

### 1.2 参考项目 iCode 的设计语言

| 维度 | iCode 的做法 | 借鉴结论 |
|---|---|---|
| 色板 | Dracula 系，7 个语义色 + 5 个表面色，变量化 | **直接采用**，见第二章 |
| 消息区分 | 左侧**粗竖线** + 底色，而非整框或缩进 | **采用**，省横向空间，中文场景友好 |
| 工具卡片 | 边框嵌标题与耗时，按状态换色 | **采用**，信息密度最高的一处 |
| 输入栏 | 圆角框 + 反显 pill 提示 ` # ` ` $ ` ` / ` ` @ ` ` ! ` | **采用**，零学习成本 |
| 屏幕布局 | header / (sidebar + chat) / inputbar / statusbar / footer | **裁剪采用**，见第三章 |
| 主题系统 | CSS 变量全局换肤 | 预留（P2），一期用常量 `Theme` |
| 过渡动画 | 150ms 补间 | **不采用**，终端兼容性差、收益低 |
| 视觉符号 | `▌` `⏺` `✔` `✖` 等纯文本符号 | **采用**，见附录 A |

### 1.3 现状差距分析

现有 `Line` 枚举（`app.rs`）已覆盖 6 类消息语义，与 iCode 的消息体系天然对齐：

```rust
pub enum Line {
    User(String),
    Assistant(String),
    Tool { name: String, summary: String, done: bool, ok: bool },
    Reasoning(String),
    System(String),
    Error(String),
}
```

结论：**Zero 缺的是「样式层」而非「结构层」**。P0 仅需新增 `theme.rs` 并重写 `render.rs` 的绘制函数，`app.rs` 的 `UiAction` / 事件处理完全不动。

---

## 二、配色系统

### 2.1 语义色板（Dracula 系，源自 iCode `_CHRYS_COLORS`）

集中定义于新文件 `crates/agent-tui/src/theme.rs`：

```rust
use ratatui::style::Color;

// 十六进制 → Color::Rgb 对照
// primary   #AF87FF → Rgb(175, 135, 255)  紫
// secondary #5F5FAF → Rgb( 95,  95, 175)  深紫
// accent    #FF87D7 → Rgb(255, 135, 215)  粉
// success   #5FFF87 → Rgb( 95, 255, 135)  绿
// warning   #FFAF5F → Rgb(255, 175,  95)  橙
// error     #FF5F5F → Rgb(255,  95,  95)  红
// fg        #EEEEEE → Rgb(238, 238, 238)  近白
// bg        #121212 → Rgb( 18,  18,  18)  最深
// surface   #303030 → Rgb( 48,  48,  48)  输入框/卡片
// surf_mut  #262626 → Rgb( 38,  38,  38)  用户消息底
// overlay   #3A3A3A → Rgb( 58,  58,  58)  弹窗底
// fg_muted  #585858 → Rgb( 88,  88,  88)  辅助文字

pub struct Theme {
    pub primary:   Color,
    pub secondary: Color,
    pub accent:    Color,
    pub success:   Color,
    pub warning:   Color,
    pub error:     Color,
    pub fg:        Color,
    pub bg:        Color,
    pub surface:   Color,
    pub surface_muted: Color,
    pub overlay:   Color,
    pub fg_muted:  Color,
}
```

### 2.2 语义用途表

| 色 | 用途 |
|---|---|
| `primary` 紫 | 焦点边框、模式徽章、选中项高亮 |
| `secondary` 深紫 | 次级按钮、非焦点边框 |
| `accent` 粉 | 用户消息左竖线、标签（tag）文字 |
| `success` 绿 | 工具成功、审批 MANUAL、检查项通过 |
| `warning` 橙 | 工具运行中、审批 AUTO、警告提示 |
| `error` 红 | 工具失败、错误消息、审批 BYPASS |
| `fg` 近白 | 正文（助手回答） |
| `fg_muted` 灰 | 思考链、系统提示、footer 提示文字 |

### 2.3 关键技术转换：alpha 混合

iCode 大量使用 Textual 的 alpha 混合语法（如 `background: $foreground 15%`），**ratatui 不支持运行时混合**，须在 `#121212` 背景上预计算为固定色值：

| iCode 写法 | 预计算色值 | Zero 用途 |
|---|---|---|
| `$foreground 8%` | `#242424` | 用户消息背景、列表 hover |
| `$foreground 15%` | `#333333` | 表头、次级控件底 |
| `$primary 18%` | `#2E2638` | 选中轮次高亮 |
| `$foreground 25%` | `#494949` | 分隔线、弱边框 |

---

## 三、布局方案

### 3.1 主界面分区

```
┌──────────────────────────────────────────────────────────────┐
│ ZERO ▌            deepseek-chat · M2             MANUAL      │ 1行 header
├──────────────────────────────────────────────────────────────┤
│ ▌ 帮我把 base_url 的问题修一下                                │
│                                                               │
│   问题在于 base_url 缺少 /v1 路径段，DeepSeek 对该路径仍先做   │
│   认证，返回的却是 key invalid…                                │ 对话流
│   ⏺ read_file  src/config.rs                         ✔ 12ms   │ (弹性 Min 3)
│   ⏺ edit_file  src/config.rs                         ⏺ …      │
│   ⋯ 正在核对配置…                                              │
├──────────────────────────────────────────────────────────────┤
│ ╭────────────────────────────────────────────────────────────╮│
│ │ 输入任务…          # agent  $ model  / cmd  @ file  ! shell││ 3行 input
│ ╰────────────────────────────────────────────────────────────╯│
├──────────────────────────────────────────────────────────────┤
│ ⏎ 发送  ⇧⏎ 换行  Esc 中断  PgUp/Dn 滚动  Ctrl+R 历史  ^Q 退出 │ 1行 footer
└──────────────────────────────────────────────────────────────┘
```

### 3.2 分区规格

| 区域 | 高度 | 变化（相对现状） |
|---|---|---|
| header | 1 | **新增**。左：`ZERO ▌` 品牌 + 任务模式；中：`provider · tier`；右：审批模式徽章（三态配色） |
| 对话流 | 弹性（`Min(3)`） | 保留。新增右侧滚动条（仅溢出时显示） |
| 输入区 | 3 | 保留。升级为圆角框 + 占位符 pill 提示 |
| footer | 1 | **新增**。常驻快捷键提示，`Ctrl+H` 可隐藏（P1） |

### 3.3 header 审批模式徽章

沿用 iCode `#approval-badge` 的三态语义配色，直接复用 `agent-perm` 的三档概念：

| 模式 | 底色 | 文字色 | 含义 |
|---|---|---|---|
| MANUAL | `primary` 紫 | `#121212` | 逐个确认（L3） |
| AUTO | `warning` 橙 | `#121212` | AI 审视（L2） |
| BYPASS | `error` 红 | `#121212` | 高危放行（L1，需显著警告） |

> 注：一期 `agent-perm` 仅暴露三档决策流，`tier` 与档位的映射以现有 `App::tier` 字段为准。

### 3.4 未来扩展位（P2 / M4）

`Ctrl+G` 唤起左侧边栏，宽度 24–32，复用现有对话流高度：

```
├──────────┬───────────────────────────────────────────────────┤
│ 会话      │                                                   │
│ ▸ 当前    │              对话流                                │
│   TODO   │                                                   │
│   ⏺ 1/3  │                                                   │
```

---

## 四、消息渲染规范

### 4.1 六类消息样式

对齐 `Line` 枚举，逐类定义前缀、颜色、缩进：

| `Line` 变体 | 前缀 | 缩进 | 样式 | 参考 iCode |
|---|---|---|---|---|
| `User` | `▌ ` | 0 | 前/背景 `#242424`，左竖线 `accent` 粉 | `UserMessage` border-left thick |
| `Assistant` | 无 | 2 | 前景 `fg` 近白 | `AssistantMessage` |
| `Reasoning` | `⋯ ` | 2 | 前景 `fg_muted` 灰 | dim text |
| `Tool` | `⏺ ` | 2 | **按状态换色**（见第五章） | `ToolCall` -success/-error |
| `System` | `· ` | 2 | 前景 `fg_muted` 灰 | notice |
| `Error` | `✖ ` | 2 | 前景 `error` 红，加粗 | `ErrorMessage` |

### 4.2 用户消息整行着色

用户消息需**整行铺底**（不只是前缀），实现方式：

1. 计算该行 wrap 后的片段范围；
2. 对每行铺 `Style::default().bg(Rgb(36,36,36))`；
3. 首列绘制 `▌` 字符，前景设 `accent`。

> ratatui 无「左侧粗边框」概念，用 `▌`（U+258C 左半块）单字符代替，比自定义 `BorderSet` 更简单且等宽可靠。

### 4.3 空行与间距

- 用户消息**上**留 1 空行（与上一条回答分隔），**下**不留；
- 助手回答紧跟工具卡片，不留空行；
- 错误消息前后各留 1 空行，强化视觉打断。

---

## 五、工具卡片规范

工具调用是信息密度最高的一处，按 iCode `ToolCall` 的状态换色机制完整落地。

### 5.1 四种状态

| 状态 | 符号 | 颜色 | 行内容 |
|---|---|---|---|
| 运行中 | `⏺` | `warning` 橙 | `⏺ read_file  src/config.rs` |
| 成功 | `✔` | `success` 绿 | `✔ read_file  src/config.rs           12ms` |
| 失败 | `✖` | `error` 红 | `✖ read_file  src/config.rs           拒绝访问` |
| 被拒 | `⊘` | `warning` 橙 | `⊘ shell  rm -rf /                    用户拒绝` |

### 5.2 字段布局

单行内按固定列对齐，最大化可扫读性：

```
[状态符] [工具名 12列]  [参数摘要 ...flex...]  [耗时/结果 右对齐]
   ⏺      read_file       src/config.rs                12ms
```

- 工具名左对齐，宽度 `12`（超长截断为 `…`）；
- 参数摘要取 `Line::Tool.summary`，超宽则中间省略；
- 右侧列固定宽度 `12`，右对齐（耗时或错误短语）。

### 5.3 与现有结构的关系

现有 `Line::Tool { name, summary, done, ok }` 已可表达四态：

| `done` | `ok` | 推导状态 |
|---|---|---|
| `false` | — | 运行中 |
| `true` | `true` | 成功 |
| `true` | `false` | 失败 |

「被拒」态需在 P1 扩展为 `Line::Tool { status: ToolStatus }`（`Running` / `Ok(ms)` / `Failed(reason)` / `Rejected`），届时补耗时与原因字段。

---

## 六、输入区规范

### 6.1 视觉结构

```
╭────────────────────────────────────────────────────────────╮
│ 输入任务…          # agent  $ model  / cmd  @ file  ! shell│
╰────────────────────────────────────────────────────────────╯
```

- 边框：`BorderType::Rounded`，颜色 `surface` `#303030`；**获得焦点时**变 `primary` 紫；
- 内容区首行绘制提示文字，前景 `fg_muted` 灰；
- 光标沿用现有软光标（`REVERSED` 反显块）。

### 6.2 占位符 pill 提示

参考 iCode `_build_prompt_placeholder` 的**反显 pill**（`(" # ", "r")` 即 reverse 样式）：

| pill | 含义 | 落地阶段 |
|---|---|---|
| `/` | 命令：`/clear` `/help` `/model` `/compact` | P1 |
| `@` | 文件路径补全 | P2 |
| `!` | Shell 直通 | P2 |
| `#` | 切换智能体 | 二期 |
| `$` | 切换模型 | P2 |

**仅当输入为空时显示**。pill 字符设为 `Style::default().add_modifier(Modifier::REVERSED)`，后接灰字说明。

> 一期只需实现 `/`，其余 pill 待对应功能落地后再显示（避免提示了做不到的功能）。

### 6.3 横向滚动

保留现有实现（`hscroll` + `Paragraph::scroll`），但改为在**边框内**滚动，边框本身不滚动。

---

## 七、审批弹层规范

### 7.1 视觉结构

沿用 `ratatui` 居中浮层，改为 iCode `approval.tcss` 的「卡片 + 危险度着色」：

```
        ╭──────────────── 权限确认 ─────────────────╮
        │                                            │
        │  工具    shell                             │
        │  命令    rm -rf ./build                    │
        │  路径    ./build                           │
        │                                            │
        │  ⚠ 该命令将不可恢复地删除目录                │
        │                                            │
        │  [Y] 批准    [A] 记住并批准    [N] 拒绝     │
        ╰────────────────────────────────────────────╯
```

- 外框：`BorderType::Rounded`，颜色按危险度——高危 `error` 红，普通 `warning` 橙；
- 标题嵌在顶部边框：`─ 权限确认 ─`（`Block::title` + `Position::Top`）；
- 底部按钮行：反显 pill 样式，与输入栏 pill 视觉统一；
- 弹层下方加 `Clear` 清屏，避免与对话流互相干扰。

### 7.2 键位

**沿用现有键位，本方案不改动**（现有 `UiAction::{Approve { remember }, Reject}` 已覆盖三个动作）。上图中的 `[Y]/[A]/[N]` 为视觉提示文字，须与 `render.rs` 实际绑定保持一致后再定稿。

---

## 八、快捷键体系

### 8.1 设计原则

iCode 有 F1–F12 十余个面板，Zero 当前仅单对话流。**只登记未来可兑现的**，避免「提示了却没有功能」。

### 8.2 键位表

| 键 | 功能 | 阶段 |
|---|---|---|
| `Enter` | 发送 | 现有 |
| `Shift+Enter` / `Ctrl+J` | 换行 | 现有 |
| `Esc` / `Ctrl+B` | 中断当前任务 | 现有 |
| `PgUp` / `PgDn` | 对话翻页 | 现有 |
| `Ctrl+End` | 滚到底部 | P0 |
| `Ctrl+H` | 显示/隐藏 footer | P1 |
| `Ctrl+R` | 输入历史回溯 | P1 |
| `F1` | 帮助 | P1 |
| `F2` | 模型 / 配置（复用 wizard） | P1 |
| `F6` | 日志面板 | P2 |
| `Ctrl+G` | 侧边栏（会话 / TODO） | P2（M4） |
| `Ctrl+Q` | 退出 | 现有 |

### 8.3 footer 渲染

- 单行，居中或左对齐，前景 `fg_muted` 灰；
- 每组「`键 描述`」之间以 2 空格分隔；
- 溢出时按优先级从右向左裁剪（先裁 `F6`/`F1` 等低频项）。

---

## 九、ratatui 实现要点

Textual → ratatui 的逐项转换对照：

| iCode / Textual | ratatui 实现 |
|---|---|
| `border: round` | `Block::border_type(BorderType::Rounded)` |
| `border-left: thick` | 首列绘制 `▌`（U+258C），不做真边框 |
| `border-title` / `border-subtitle` | `Block::title(Title::from(" x ").position(Position::Top))` |
| `background: $x 15%` | 预计算固定色值表（见 2.3） |
| `text-style: dim` | `Modifier::DIM` |
| `text-style: bold` | `Modifier::BOLD` |
| Toast（右上角） | `Clear` + 定位 `Paragraph` 叠加，2s 后消失（需 `App` 加时间戳） |
| 150ms 过渡动画 | **不实现** |
| 滚轮 / 点击事件 | 已有 `EnableMouseCapture` + `handle_mouse`，扩展为点击 pill 触发补全（P1） |

### 9.1 代码组织建议

```
crates/agent-tui/src/
├── theme.rs        # 新增：Theme 常量 + 语义色
├── render.rs       # 改造：draw_header / draw_conversation / draw_input / draw_footer / draw_approval
├── app.rs          # 基本不动（仅 P1 扩展 Line::Tool 状态与 toast 状态）
└── glyphs.rs       # 可选：符号常量集中管理（▌ ⏺ ✔ ✖ ⊘ ⋯ ·）
```

### 9.2 兼容性约束

- **Terminal 兼容**：`Color::Rgb` 在 256 色终端会自动降级，无需特殊处理；但显示宽度必须用 `unicode-width` 计算（中文 2 列），现有 `wrapped_line_count` 已处理，新样式不得破坏该逻辑。
- **测试**：现有 71 个测试不依赖颜色，P0 改造后须全绿；新增纯函数（符号选择、状态→颜色映射）应补单测。

---

## 十、实施路线

| 阶段 | 内容 | 改动范围 | 风险 | 状态 |
|---|---|---|---|---|
| **P0** | `theme.rs` 主题化；6 类消息样式；工具三态配色；输入框圆角 + pill；header + footer；滚动条；审批弹层升级 | `render.rs`、新增 `theme.rs` | 低（纯渲染层） | ✅ 2026-10-14 |
| **P1** | 工具卡片耗时/错误原因（扩展 `Line::Tool`）；Toast；`Ctrl+R` 历史；`/` 命令与补全；`Ctrl+H` 隐藏 footer | `render.rs`、`app.rs` | 中（需扩 `App` 状态） | 待实施 |
| **P2** | 侧边栏（会话 / TODO 树）；`@` 文件补全；Markdown 渲染；diff 视图；主题换肤 | 新增 `widgets/` 模块 | 高（M4） | 待实施 |

### P0 验收标准

- [x] `cargo build` / `cargo clippy --workspace --all-targets` 零警告
- [x] `cargo test --workspace` 全绿（**92 passed / 0 failed**）
- [x] 6 类消息在 80 列终端下正确渲染，中文不错位
- [x] 工具三态颜色随 `done`/`ok` 正确切换
- [x] 80×24 与 120×40 两种尺寸下布局不溢出

> 验收方式：`crates/agent-tui/src/render.rs` 内新增 `TestBackend` 渲染快照测试，
> 断言 header 品牌/徽章、六类消息符号、工具名固定列对齐、空输入占位、审批弹层；
> 另可用 `cargo test -p agent-tui preview_visual_layout -- --ignored --nocapture` 目视校验。

---

## 附录 A：视觉符号表

| 符号 | 码位 | 用途 |
|---|---|---|
| `▌` | U+258C | 用户消息左竖线 |
| `⏺` | U+23FA | 工具运行中 |
| `✔` | U+2714 | 工具成功 |
| `✖` | U+2716 | 工具失败 / 错误消息 |
| `⊘` | U+2298 | 工具被拒 |
| `⋯` | U+22EF | 思考链 |
| `·` | U+00B7 | 系统提示 |
| `⚠` | U+26A0 | 高危警告 |
| `╭╮╰╯` | U+256D/256E/2570/256F | 圆角边框 |
| `─│` | U+2500/2502 | 直角边框 / 分隔线 |

> 注：`⏺`/`✔`/`✖` 均为**单列宽**符号，中文终端下需实测；若出现占位异常，可降级为 `>` / `v` / `x` / `-` ASCII 备选组（在 `glyphs.rs` 中统一切换）。

## 附录 B：与 iCode 的取舍对照

| iCode 能力 | Zero 是否采纳 | 说明 |
|---|---|---|
| Dracula 语义色板 | ✅ 全盘采纳 | 直接映射为 `Theme` |
| 左竖线消息区分 | ✅ | 用 `▌` 单字符实现 |
| 工具卡片状态色 | ✅ | 四态 |
| 圆角输入框 + pill | ✅ | 一期只显示 `/` |
| header / footer | ✅ | 裁剪为各 1 行 |
| 侧边栏（会话/TODO 树） | ⏸ P2 | 依赖 M4 多任务 |
| F1–F12 面板群 | ⏸ 部分 | 仅登记可兑现项 |
| alpha 混合 | ❌ | 预计算色值替代 |
| 过渡动画 / 富 Markdown | ❌ / ⏸ | 兼容性与成本考量，Markdown 延至 M4 |
| 主题换肤文件（YAML） | ⏸ P2 | 一期用常量 |
