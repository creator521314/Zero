# 实现进度（M1 骨架）

> 对应《个人Agent技术方案设计文档_V1.md》第八章里程碑。

## 已完成（M1）

| 项 | 状态 | 位置 |
|---|---|---|
| cargo workspace + 9 crate 划分 | ✅ | `Cargo.toml` |
| `agent-core`：Event 总线 + 7 大 trait（冻结签名） | ✅ | `crates/agent-core/src/` |
| 逻辑时钟 + schema_version + durability 分层 | ✅ | `agent-core/src/store.rs` |
| `agent-store`：jsonl append-only + 内存索引 | ✅ | `crates/agent-store/src/lib.rs` |
| `agent-net`：reqwest 封装 + SSRF 防护 | ✅ | `crates/agent-net/src/lib.rs` |
| `agent-perm`：静态规则 + 三档权限决策流 | ✅ | `crates/agent-perm/src/` |
| `agent-tools`：7 个工具 + 路径逃逸防护 | ✅ | `crates/agent-tools/src/` |
| `agent-providers`：OpenAI 兼容适配器 + 路由 + 预算 | ✅ | `crates/agent-providers/src/` |
| `agent-runtime`：执行循环 + 上下文三层 + 防护 | ✅ | `crates/agent-runtime/src/` |
| `agent-tui`：ratatui 界面 + 审批弹层 + 事件解耦 | ✅ | `crates/agent-tui/src/` |
| `agent-sync`：二期占位（trait 冻结） | ✅ | `crates/agent-sync/src/lib.rs` |
| `agent-cli` / `agent-server` 装配入口 | ✅ | `apps/` |
| MIT 许可、xtask 辅助脚本、README | ✅ | 根目录 |

## 单测覆盖

| 模块 | 测试内容 |
|---|---|
| `agent-net` | SSRF 阻断内网/非 http(s)；放行公网 |
| `agent-perm::rules` | 8 类高危命令命中；4 类正常命令放行；受保护路径 |
| `agent-perm` | L1 高危拦截；L2 无审视器降级；L3 逐个确认 + 批量批准 |
| `agent-tools::paths` | `../` 逃逸拒绝；工作区内路径接受；项目根为基准；私有工作区仍可达；Windows 盘符大小写/斜杠方向不误判 |
| `agent-tools::web` | HTML 标签与 script 剥离 |
| `agent-tools::shell` | 本平台解释器能起命令；非 0 退出码标为错误；描述串与平台一致；中文按 OEM 代码页解出可读文本；UTF-8 输出不被二次损坏 |
| `agent-tools::fs_tools` | 相对路径落到项目根；无项目根时退回私有工作区；空目录明说；读目录给出替代工具 |
| `agent-providers` | tool 消息转换；assistant tool_call 转换（arguments 必须为字符串） |
| `agent-runtime::context` | 上下文压缩保留固定区并生成摘要 |
| `agent-tui::app` | 多字节字符编辑；流式增量合并为一行；空输入不提交；工具四态流转（含迟到事件不覆盖已终结行）；重绘请求取走即清零；任务收尾不重复渲染已流式输出的原文（对不上时仍补），失败按错误样式显示 |
| `agent-tui::theme` | 权限档位 → 徽章色/文案映射（三态、大小写不敏感） |
| `agent-tui::render` | 六类消息符号与缩进；工具四态符号/耗时/原因（多行压单行、超长截断）；工具名固定列对齐（含中文宽度）；宽字符 wrap 行数；鼠标区段分流；`TestBackend` 渲染快照 |
| `agent-store` | 追加 + 重载；逻辑时钟单调；durability 保留 |
| `agent-store::recorder` | 落盘分类（持久/临时/不落）；总线滞后跳过而不退出；每条事件只落一次；增量默认不落 |
| `agent-core::event` | 上下文快照与整轮回复往返；工具输出留存；旧日志缺字段时兜底 |
| `agent-runtime::runner` | 每轮记录上下文快照与整轮回复；工具原文 + 自报失败 → Failed；关闭时不记录快照；项目根传到工具；提示词指向项目根且约束盲目重试 |

## M1 验收标准

- [x] `cargo build` 通过（全部 crate，**0 warning**）
- [x] `cargo clippy --all-targets` 通过（**0 warning**）
- [x] `cargo test` 通过（**92 passed / 0 failed**，含 1 个 `--ignored` 目视预览）
- [x] 二进制产出：`target/debug/agent`（TUI）、`target/debug/agent-server`（headless）
- [x] 启动路径验证：首次运行生成配置模板；无 key 时给出明确提示并警告禁用 provider
- [x] 配置向导：缺配置 / 配置无效 → 自动进入 wizard（dialoguer）；非 TTY 写模板退出
- [x] 端到端真实对话：2026-10-14 本地验证通过（DeepSeek 真实端点，
      `agent-server "hi"` 获得正常回复；配置见 config/config.toml，
      支持 api_key_env 环境变量优先 + api_key 明文兜底）

### 规模

- Rust 源文件 39 个，约 **8900 行**（含测试模块）
- 9 个库 crate + 2 个二进制 + xtask

## TUI 界面改造（P0 已完成）

方案见 [`docs/ui-design.md`](docs/ui-design.md)（参考 `iCode`，Dracula 系配色）。
P0 仅改渲染层，未动 `app.rs` 的事件与状态机。

| 项 | 状态 | 位置 |
|---|---|---|
| 主题模块：语义色板 / 预混合表面色 / 视觉符号 / 档位徽章映射 | ✅ | `crates/agent-tui/src/theme.rs`（新增） |
| 四区布局：header(1) / 对话流 / 输入区(3) / footer(1) | ✅ | `render.rs::layout_chunks` |
| header：品牌 + provider·状态 + 权限徽章（三态配色） | ✅ | `render.rs::draw_header` |
| 六类消息样式：`▌` 用户 + 底色铺满、助手缩进、`⋯` 思考、`·` 系统、`✖` 错误 | ✅ | `render.rs::build_conversation_lines` |
| 工具三态配色：`⏺` 运行橙 / `✔` 成功绿 / `✖` 失败红 + 工具名 12 列对齐 | ✅ | 同上 |
| 对话区右侧滚动条（仅溢出时显示） | ✅ | `render.rs::draw_scrollbar` |
| 输入区圆角边框（焦点主色）+ 空输入占位 pill | ✅ | `render.rs::draw_input` |
| 审批弹层：圆角卡片 + 按风险着色（高危红 / 其余橙）+ pill 按钮 | ✅ | `render.rs::draw_approval` |
| 鼠标滚轮区段判断改为闭开区间（footer 不再被误判为输入区） | ✅ | `render.rs::handle_mouse` |

**P0 验收**：`cargo clippy --workspace --all-targets` 0 warning；
`cargo test --workspace` 92 passed / 0 failed；
渲染快照测试（`TestBackend`）覆盖 header、六类符号、工具列对齐、占位提示、审批弹层。

## TUI 界面增强（P1 部分完成：工具四态）

2026-10-15 完成 P1 首项——工具调用从「成功 / 失败」二值升级为四态，并带上
耗时与失败原因。改动链路：`agent-core`（事件扩字段）→ `agent-runtime`（填值）
→ `agent-tui`（状态与渲染）。

| 项 | 状态 | 位置 |
|---|---|---|
| `Event::ToolCallEnd` 新增 `outcome`（Ok/Failed/Denied/Rejected）与 `detail` | ✅ | `agent-core/src/event.rs` |
| runner 四路分支填入状态与原因（Deny / Reject / Ok / Err） | ✅ | `agent-runtime/src/runner.rs` |
| `Line::Tool` 的 `done`/`ok` 布尔替换为 `ToolStatus` 四态 | ✅ | `agent-tui/src/app.rs` |
| 渲染耗时（成功 / 失败）与原因（失败 / 拒绝）；被拒不显示耗时 | ✅ | `agent-tui/src/render.rs` |
| 多行原因压成单行 + 截断 60 显示列，避免撑破「一行一工具」布局 | ✅ | `render.rs::one_line` / `truncate_display` |

**兼容性**：`outcome` 与 `detail` 均带 `#[serde(default)]`，旧 jsonl 审计日志仍可
反序列化（兜底为 `Ok` / `None`）；`agent-cli` 等消费方仅解构 `id`，无需改动。

**P1 验收**：`cargo clippy --workspace --all-targets` 0 warning；
`cargo test --workspace` **121 passed / 0 failed**（新增 agent-core 2 + app 6 + render 9 + openai-compat 3）。

**后续**：P1 剩余项（Toast、`Ctrl+R` 历史、`/` 命令补全、`Ctrl+H` 隐藏 footer）、
P2（侧边栏、Markdown、diff 视图）见 `docs/ui-design.md` 第十章。

## 缺陷修复：审批自锁（2026-10-15）

**症状**：任务一旦走到需要审批的工具调用就彻底卡住。界面上既没有「任务完成」标志，
按 `y` 批准 / `Esc` 取消都没有任何反应，之后提交的问题也永远等不到回应，且**毫无错误提示**。

**根因**：`apps/agent-cli/src/main.rs` 的命令循环把 `runtime.run(...).await` 直接写在
`match cmd` 分支里。而 runner 的 `wait_approval` 所等待的应答，其唯一发送方 `ap_tx`
正由这个循环持有——**循环等任务结束，任务等循环转发审批，循环等待自己**：

```
cmd_rx.recv() ──► Command::SubmitTask ──► runtime.run().await ─┐
      ▲                                                          │ 阻塞
      └──◄── Command::Approve / Cancel 堆积于此，永不处理 ◄───────┘
                                        │
                            ap_tx.send() 永不执行
                                        ▼
                        wait_approval 永久挂起 → TaskFinished 永不发出
```

**修复**：

| 改动 | 说明 |
|---|---|
| 任务改为 `tokio::spawn` 独立执行 | 命令循环不再被任务阻塞，Approve/Reject/Cancel 可即时转发 |
| 新增运行中任务句柄并在循环内收割 | 任务结束后释放槽位，避免陈旧句柄挡住后续提交 |
| 任务忙时显式拒绝并回报 `Event::Error` | 原先静默排队，用户会误以为提交没生效 |
| TUI 处理 `TryRecvError::Disconnected` | 事件流关闭时落错误行提示「后端已断开」，不再无声卡住 |
| 断连提示附带完整日志路径 | 用户无需自行翻找配置目录 |

**配套日志**（默认写配置文件同级的 `logs/agent.log`，`RUST_LOG=debug` 可加详）：

- 任务生命周期：`run start` / `task started` / `task run finished` / `task run failed`
- 审批全链路：`awaiting user approval` → `waiting for approval reply` → `approval reply received`
- 异常分支：`stale approval reply dropped`（审批 ID 错配）、`approval channel closed`、
  `approval wait cancelled`、`rejected: another task is running`
- 命令收发：`command received`（debug 级）、审批转发与取消

**回归保护**：新增 3 个单测（断连提示幂等、撤下残留弹层、提示携带日志路径）；
全量 **121 passed / 0 failed**，clippy 0 warning。

## UI 重复输出修复：content / reasoning_content 重复（2026-10-15）

**症状**：模型回复正常输出后，**同一段文字又以暗色（灰显）重复输出一遍**。

**根因**：DeepSeek 等 OpenAI 兼容厂商会把同一段内容同时填入流式响应的
`delta.content` 和 `delta.reasoning_content` 字段。`agent-providers` 原先的
解析逻辑对两者都原样发送成 `StreamEvent`，于是 TUI 把同一段文本渲染为
`Line::Assistant`（正常色）和 `Line::Reasoning`（暗色）各一次。

**修复**：`crates/agent-providers/src/openai_compat.rs` 中提取
`split_content_reasoning(delta)` 函数：当 `reasoning_content` 与 `content`
内容完全相同时，只保留正文；仅在两者不同或正文为空时才发送思考链。

**回归保护**：新增 3 个单测（重复去重、内容不同保留、正文为空时保留思考链）；
`agent-providers` 33 passed / 0 failed，全量 **121 passed / 0 failed**。

## 日志增强：对话上下文入审计流（2026-10-16）

**动机**：定位「工具为什么没跑成」时，日志里只有 `tool_call_start/end`
与一堆逐字增量，三个关键问题都答不上来：模型当时**看到了什么**、**回了什么**、
**工具返回了什么**。逐字增量占了 95% 的体积（21 次工具调用 ≈ 400KB /
687 条事件），却是最没信息量的那部分。

**改动**：

| 项 | 位置 |
|---|---|
| 新增 `LogConfig`（`save_context` / `save_stream_deltas`） | `agent-core/src/store.rs` |
| 新增 `Event::ContextSnapshot`（system + 全部历史消息 + 可用工具） | `agent-core/src/event.rs` |
| 新增 `Event::AssistantTurn`（正文 + 工具调用 + 用量） | 同上 |
| `Event::ToolCallEnd` 增加 `output`（工具返回原文） | 同上 |
| 落盘规则集中到 `classify` + `spawn_event_sink`（含滞后容错） | `agent-store/src/recorder.rs` |
| 每轮请求前记录快照、请求后记录整轮回复；工具原文回传执行循环 | `agent-runtime/src/runner.rs` |
| CLI / server 统一接入 recorder（不再各处手写落盘） | `apps/agent-*` |
| `[log]` 配置段 + `cargo xtask log`（`--full` / `--errors`） | `config/*` + `xtask/src/log_view.rs` |

**顺带修掉的两个缺陷**：

1. **同一条事件落两遍**：`execute_tool` 曾直写 store，总线订阅者又写一遍，
   成功的 `ToolCallEnd` 在 jsonl 里成对出现（且两处结论相反：store 记
   `Failed`、总线记 `Ok`）。现在事件一律只从总线走，落盘单点裁决。
2. **工具自报失败被记成成功**：工具返回 `is_error=true`（如 `web_search`
   未配置后端）时，结局曾是 `Ok`。现在统一判 `Failed` 并带上原文。

**兼容性**：`output` 带 `#[serde(default)]`，新字段缺失时兜底为 `None`，
旧 jsonl 仍可反序列化（已用 10-16 的历史日志验证阅读器输出正常）。

**验收**：`cargo xtask check` 通过（fmt + clippy 0 warning + 测试）；
`cargo test --workspace` **130 passed / 0 failed**（新增 11 个）。

## P0 修复：让工具真正作用于项目（2026-10-16）

**症状**：连续三次会话「介绍当前项目」，工具失败率 8/11；`list_dir` 全部
返回空，`file_read` 全部报「文件不存在」，`shell_exec` 100% spawn 失败，
模型最后只能回答「工作区里没有项目文件」。

**根因 A —— 工作区从来就不是项目目录**：每提交一个任务就
`create_dir_all` 一个全新的空目录 `<root>/workspace/<uuid>`，路径护栏又
把读写锁死在里面，于是「当前文件夹」= 一个刚创建的空文件夹。

**根因 B —— `shell_exec` 硬编码 `/bin/sh`**：Windows 上没有它，每次调用
在 spawn 阶段就失败（os error 3），模型想用 `ls`/`dir` 兜底的路全部落空。

**改动**：

| 项 | 位置 |
|---|---|
| `TaskSpec.project_root`（可选）+ `ToolCtx.project_root` | `agent-core/src/{task,tool}.rs` |
| `ToolCtx::sandbox_root` / `allowed_roots`（双根） | 同上 |
| 多根解析 `resolve_in` / `resolve_in_ctx`；Windows 盘符大小写与斜杠方向不参与路径语义 | `agent-tools/src/paths.rs` |
| `#[cfg(windows)]` → `cmd /C`（并切 65001 代码页取 UTF-8 输出）；工具描述随平台变 | `agent-tools/src/shell.rs` |
| 相对路径基于项目根；空目录明说；读目录给出「请用 list_dir」 | `agent-tools/src/fs_tools.rs` |
| 提示词给项目根 + 「目标不存在别再猜路径」 | `agent-runtime/src/runner.rs` |
| `agent.project_root`（默认 `.` = 启动目录）+ 目录不存在时告警 | `agent-providers/src/config.rs` |

**为什么是两个根**：项目根只负责工具读写；`ctx/` 外置输出等私有数据仍放
`<root>/workspace/<task-id>`，不会在用户仓库里留垃圾。私有工作区始终保持
可达，否则外置到 `ctx/` 的内容连工具自己也读不回来。

**验收**：`cargo xtask check` 通过；新增 13 个单测（含 3 个真实进程 /
真实 IO 用例），`cargo test --workspace` **143 passed / 0 failed**。

## 日志目录归位（2026-10-16）

**症状**：TUI 的 `list_dir` 里赫然出现 `config` —— 运行期日志写到了
`<仓库>/config/logs/agent.log`。

**根因**：日志目录取自**配置文件所在目录**（`cfg_path.parent().join("logs")`），
把「配置放在哪」当成了「数据放哪」。代码仓库里的 `config/` 是配置目录，
运行期产物混进去既脏，又只靠 `.gitignore` 的 `*.log` 侥幸没被提交。

**改动**：

| 项 | 位置 |
|---|---|
| `AgentConfig::resolved_log_dir()` = `agent.root/logs` | `agent-providers/src/config.rs` |
| `resolve_log_dir`（`AGENT_LOG_DIR` > 配置 root > 默认）+ `init_tracing` | `apps/agent-cli/src/main.rs` |

与 jsonl 会话日志同处 `<root>/logs/`（`JsonlStore` 本就是 `root/logs`）。

两个健壮性细节：配置此时可能**尚未生成或语法有误**，所以只「看一眼」root、
失败即退回默认值（日志功能不该被配置问题连带拖死）；目录不可写时退到临时
目录并告警，而不是把日志静默丢进黑洞。

**验收**：`cargo test -p agent-cli -p agent-providers` 新增 6 个单测全过；
旧的 `config/logs/` 已删除。

## 黑屏与乱码：子进程碰了 TUI 的控制台（2026-10-16）

**症状**：执行任务途中「大部分区域突然变黑」，只剩工具调用列表还在刷新；
同一次会话里 `dir /a` 的返回是一堆乱码（`������ D �еľ��� Data`）。

### 黑屏：共享控制台 + 差分重绘

`shell_exec` 的子进程与 TUI 共享同一个控制台，而 `chcp 65001` 改的正是
这个共享控制台的代码页——**屏幕被连带擦掉**。而 TUI 走 ratatui，只发送
**差分**：被擦掉的静态区域（边框、输入框、页脚）内容未变，永远不会被
重画，于是看起来就是「大片变黑，只剩还在刷新的那几行」。

**证据链**：

1. 把真实会话（`5fb53ab3`，24 次工具调用全部 ok）重放进 TUI 缓冲：
   每一帧都完整（非空白字符 585 → 1274 单调增长），**App 绘制本身没问题**；
2. 截图里可见的 9 行，正好等于重放帧的第 5–13 行（自 `shell_exec` 起）：
   它上面的 `list_dir .`、任务起始行、边框全黑，而输入框/页脚在启动后
   再未变化过——**清屏恰好发生在第一次 `shell_exec` 那一刻**；
3. 时间上也吻合：这是 `shell_exec` 第一次真正跑通，也是第一次出现黑屏。

### 乱码：`chcp` 管不了管道

实测（字节级）：

```text
echo 中文ABC               → D6 D0 CE C4 41 42 43   （GBK）
chcp 65001 >nul && echo …  → D6 D0 CE C4 41 42 43   （一模一样）
```

`chcp` 只改控制台**显示**用的代码页，对管道里的字节毫无影响。子进程按
**OEM 代码页**输出，之前硬按 UTF-8 解就是满屏 `U+FFFD`。

### 改动

| 项 | 位置 |
|---|---|
| 子进程 `CREATE_NO_WINDOW`：起在自己的控制台里，改代码页/清屏都碰不到 TUI | `agent-tools/src/shell.rs` |
| 去掉 `chcp 65001` 装饰（无效，且是黑屏元凶） | 同上 |
| `decode_output`：UTF-8 严格解码 → 失败按 OEM 代码页（`MultiByteToWideChar(CP_OEMCP)`） | 同上 |
| 整屏重绘请求 + `Ctrl+L` 手动触发 | `agent-tui/src/app.rs` |
| `draw_frame()`：重绘走 `terminal.clear()`，`Resize` 同样清屏 | `agent-tui/src/render.rs` |

> 上面那条「整屏重绘」最初的实现用了 `swap_buffers()`（以为能不闪地整屏重画），
> 是错的，代价见下一节——已改成 `clear()`。

## 缺陷修复：旧帧字尾留在屏幕上（2026-10-16）

**症状**：任务收尾前后，对话区出现两帧文字叠在一起，例如
`✔ list_dir e. 0msng the project to understand its current state.`，右侧还散着
`字增量`、`ws 缺` 这类旧帧片段。

**根因**：黑屏修复里用了 `Terminal::swap_buffers()` 来「强制整屏重绘」。
ratatui 的缓冲配对是 `previous = 屏幕上那一帧`、`current = 待渲染的空缓冲`，
`flush()` 做 `previous.diff(current)`。而 `swap_buffers()`
**只重置缓冲、不擦屏**：

```rust
// ratatui-0.29.0/src/terminal/terminal.rs
pub fn swap_buffers(&mut self) { self.buffers[1 - self.current].reset(); self.current = 1 - self.current; }
```

它把「代表屏幕」的那个缓冲重置成空白，于是下一帧的差分变成「空白 vs 新帧」，
**新帧留空的单元格不会被写**——屏幕上那些位置保留着旧帧的内容，正是字尾残留。
工具每调用一次就触发一次，所以工具行最明显。

**反向验证**（把 `clear()` 换回 `swap_buffers()` 跑新用例）：

```text
│  BAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA                        │
    ↑ 新帧的 B        ↑ 旧帧残留的字尾
```

**改动**：

| 项 | 位置 |
|---|---|
| `draw_frame()`：重绘用 `terminal.clear()`（擦屏 + 重置缓冲，两者同步） | `agent-tui/src/render.rs` |
| 工具结束 / 任务收尾**不再**自动重绘：正确重绘要擦屏、会闪，而黑屏根因已在源头修掉（`CREATE_NO_WINDOW`）；要救有 `Ctrl+L` | `agent-tui/src/app.rs` |

**验收**：新增 `forced_repaint_erases_stale_cells`（`TestBackend` 双缓冲，断言
旧帧字尾被擦掉，且旧实现下确实失败）；`cargo xtask check` 通过，
`cargo test --workspace` **159 passed / 0 failed**（自动重绘的两条用例随功能
移除）。

编码不能「一律按 GBK 解」：`type` / `cat` / `git` / `rg` 本身吐 UTF-8，
再按 GBK 解会把它们弄坏。所以先严格试 UTF-8，失败才退回 OEM 代码页。

**验收**：`dir /a` 输出恢复为「驱动器 D 中的卷是 Data」；`cargo xtask check`
通过，`cargo test --workspace` **156 passed / 0 failed**（新增 7 个：2 个真实
进程用例 + 5 个 TUI 用例）。

## 缺陷修复：任务收尾把最后一段回答又输出一遍（2026-10-16）

**症状**：任务完成后，「✔ 任务完成」下面又整段冒出刚回答过的内容，而且是暗色
（截图里那份 2000+ 字的项目介绍）。

**根因**：`TaskFinished` 的 `summary` **就是最后一轮模型的原文**——`runner.rs`
直接 `out.text.clone()`。而这份原文早已通过 `Event::TextDelta` 流式渲染成
`Line::Assistant`（亮色正文，`runner.rs` 每段增量都往总线发一份）。TUI 收尾时
又把 `summary` 拼进一个 `Line::System` 推了第二次，于是同一段话显示两遍，且
第二遍落在灰显的系统行里——原 `Line::System` 是给「系统提示」用的。

实测（会话 `4863457a`）：`task_finished.summary` 与最后一次 `assistant_turn.text`
长度同为 2122，`-eq` 判等为真。

**改动**（`agent-tui/src/app.rs`）：

| 收尾分支 | 现在 |
|---|---|
| `Success` | 只推「✔ 任务完成。」；`summary` 与已有 `Line::Assistant` **内容相同就不再显示**；对不上时才补（模型未流式输出、断流重放，以及「连续多轮没有产出」的合成摘要），且按正文亮色渲染；产物另起一行 |
| `Failed` | 改用 `Line::Error`（红）：失败原先也是灰显系统行，和普通记录混在一起 |

关键是**按内容判重，而不是无条件跳过**：`summary` 是模型没有流式输出时唯一的
正文来源，无条件跳过会直接把内容弄丢。

**验收**：新增 4 个单测（含一个 `TestBackend` 帧快照，断言答案在整帧里只出现
一次）；`cargo xtask check` 通过，`cargo test --workspace` **160 passed / 0 failed**。

## 已知限制（一期）

| 限制 | 说明 |
|---|---|
| 上下文压缩为轻量摘录 | 非 LLM 摘要，二期可增强 |
| 工具调用为串行执行 | 设计上支持并行，一期按顺序执行 |
| `web_search` 需自备 SearXNG | 未配置时返回明确提示，不静默失败 |
| 批量批准未接 UI | `a` 键回传 `remember`，编排层登记逻辑待 M3 补齐 |
| TUI 任务面板未实现 | 单任务流，多任务切换在 M4 |

## 待办（M2 起）

| 里程碑 | 内容 |
|---|---|
| M2 | 工具链打磨、"写文件→执行脚本→汇报"链路打通 |
| M3 | L2 AI 审视实测调优、审计日志完善、预算熔断验证 |
| M4 | 任务面板/切换/恢复、Markdown 渲染增强、新闻收集模板 |
| 二期 | 分级同步、AI 合并 + 回滚、设备注册中心 |
