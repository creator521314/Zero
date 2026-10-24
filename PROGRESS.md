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
| `agent-tools`：8 个工具（含 finish）+ 写前快照 + 路径逃逸防护 | ✅ | `crates/agent-tools/src/` |
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
| `agent-tools::web` | HTML 标签与 script 剥离；搜索后端解析矩阵；搜索 URL 构造与查询串往返；Bing 结果页解析（实体解码/多字节/条数上限）；SearXNG JSON 解析与 HTML 错误页可读性；HTTP 错误文案与 SearXNG JSON 未启用提示；结果渲染；`backend=off` 与空 query 输入校验（另有 1 个 `--ignored` 真实网络冒烟） |
| `agent-tools::shell` | 本平台解释器能起命令；非 0 退出码标为错误；描述串与平台一致；中文按 OEM 代码页解出可读文本；UTF-8 输出不被二次损坏 |
| `agent-tools::fs_tools` | 相对路径落到项目根；无项目根时退回私有工作区；空目录明说；读目录给出替代工具；覆写前快照到私有工作区 `snapshots/` |
| `agent-tools`（集成，`tests/smoke.rs`） | 端到端冒烟：8 个工具都注册且 `defs()` 可用；声明参数与实际入参一致；缺必填参数报 `InvalidArgs`；真实读写/编辑/列目录/`shell_exec`/`finish` 产物登记；装配路径上的逃逸防护；`web_fetch` 抓到正文且标签剥离；**零配置 `web_search` 能搜出真实结果**；联网「跳过」判定不被放宽（详见 [工具端到端冒烟](#工具端到端冒烟2026-10-24)） |
| `agent-tools::finish` | 显式终止信号：总结 + 登记交付物路径；非法/逃逸路径跳过不掀翻任务 |
| `agent-providers` | tool 消息转换；assistant tool_call 转换（arguments 必须为字符串） |
| `agent-runtime::context` | 上下文压缩保留固定区并生成摘要 |
| `agent-tui::app` | 多字节字符编辑；流式增量合并为一行；空输入不提交；工具四态流转（含迟到事件不覆盖已终结行）；重绘请求取走即清零；任务收尾不重复渲染已流式输出的原文（对不上时仍补），失败按错误样式显示；工作中/等审批的判定；spinner 帧随 tick 转动；`/` 命令补全状态（候选过滤、选中、执行 `/clear` `/help` `/tier`）；输入历史记录/去重/`Ctrl+R` 循环召回；Toast 创建与到期清除；footer 显隐 |
| `agent-tui::theme` | 权限档位 → 徽章色/文案映射（三态、大小写不敏感） |
| `agent-tui::render` | 六类消息符号与缩进；工具四态符号/耗时/原因（多行压单行、超长截断）；工具名固定列对齐（含中文宽度）；软换行不丢字符、不超宽、样式跟随；滚到底能看到最后一行；鼠标交互（滚轮区段分流、点击定位光标、滚动条点击/拖拽、徽章切档、审批按钮点击）；`/` 命令补全浮层（前缀过滤、键盘选中、鼠标点击候选）；Toast 右上角反色覆盖渲染；footer 隐藏时布局收回该行；`Ctrl+H`/`Ctrl+R` 按键；`TestBackend` 渲染快照（任务收尾只出现一次、整屏重绘擦掉旧帧字尾） |
| `agent-tui::markdown` | 助手回答的 Markdown 渲染：标题/粗斜体/删除线/行内码/围栏代码块/有序无序列表/任务列表/引用/分隔线/链接；表格 HTML 降级不丢内容；流式残缺不崩不吞字符；软换行 + 列表悬挂缩进 |
| `agent-store` | 追加 + 重载；逻辑时钟单调；durability 保留 |
| `agent-store::recorder` | 落盘分类（持久/临时/不落）；总线滞后跳过而不退出；每条事件只落一次；增量默认不落 |
| `agent-core::event` | 上下文快照与整轮回复往返；工具输出留存；旧日志缺字段时兜底 |
| `agent-runtime::runner` | 每轮记录上下文快照与整轮回复；工具原文 + 自报失败 → Failed；关闭时不记录快照；项目根传到工具；提示词指向项目根且约束盲目重试；终止信号工具跳过权限并提前收尾、产物去重 |

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

## 缺陷修复：模型输出的最后一部分显示不出来（2026-10-16）

**症状**：任务答完，最后几行内容不在屏幕上；再发一条指令，新输出把那段内容
「推」上来才看得见。

**根因**：对话区的滚动上限是**估算**出来的。`draw_conversation` 用 `textwrap`
算每个逻辑行 wrap 后占几行，拿估算值当 `max_scroll`；而 `Paragraph` 自己还开着
`Wrap { trim: false }`，会再换一次行。两个库的断行策略不同，估算必然与实际
渲染行数对不上——真实会话实测：**估算 41 行、实际渲染 45 行**，`max_scroll`
少 4，滚到底时底部 4 行被切在视口外。新内容进来后总行数变大，那 4 行才被
「推」进视野，正是症状里描述的样子。

最小复现（宽 98，中英混合带空格的长句）：

```text
textwrap 算 3 行，ratatui 实际渲染 4 行
```

纯中文无空格的长段反而对得上（两边都按显示宽度硬切），所以这个 bug 只在中英
混合、markdown 列表这类带空格的行上发作。

**改动**：

| 项 | 位置 |
|---|---|
| 新增 `wrap_line`：自己按显示宽度软换行（保留样式、不丢字符、每行不超宽） | `agent-tui/src/render.rs` |
| `draw_conversation` 切完之后**不再给 `Paragraph` 设 `.wrap()`**——所见行数 = 滚动上限，不再失配 | 同上 |
| 删掉 `textwrap` 依赖（换行不再需要另一个库来估） | `Cargo.toml`、`agent-tui/Cargo.toml` |

**验收**：新增 2 个单测——换行不丢字符/不超宽/样式跟着字走，以及「跟随时滚到
底必须看得见最后一行」。后者做了反向验证：换回旧实现确实失败（帧里找不到收尾
标记）；`cargo test --workspace` **161 passed / 0 failed**。

## 改动：输入框右侧的发送指示器（2026-10-16）

输入框右沿固定一个状态指示器，一眼知道现在能不能发：

| 状态 | 显示 | 颜色 |
|---|---|---|
| agent 工作中 | `⠋ working`（spinner 随 tick 转） | 橙 `ink::WARNING` |
| 空闲 + 有输入 | `⏎ 发送` —— 可发送 | 绿 `ink::SUCCESS` |
| 空闲 + 空输入 | `⏎ 发送` | 灰 `ink::FG_MUTED` |

实现在 `agent-tui/src/render.rs`：

- `draw_input` 把输入区拆成**内容区 + 指示器区**两个 `Rect` 分别渲染。指示器
  不独立成区的话，会被超长输入的横向滚动一起带走。
- 指示器宽度固定 `SEND_COL = 11`：两种文案一长一短，宽度跟着变会让内容区在
  状态切换时抖动。
- 工作状态由 `App::is_working()` 给出：`active_task` 非空且**不在等审批**——
  等审批时球在用户手里，显示 working 是误导。spinner 帧取 `App::spinner()`。

占位提示里原来的 `⏎` pill 去掉了，提示已移到右侧，留着就重复。

**验收**：新增 5 个单测（三态的文案与颜色、任务结束后回到可发送态、spinner
随 tick 转动）；`cargo test --workspace` **166 passed / 0 failed**。

## 改动：任务完成标志更显眼（2026-10-16）

原来「✔ 任务完成。」走的是 `Line::System`——灰显，跟一条普通记录没区别，在一屏
近白的回答里直接划过去了。

新增 `Line::Done(String)`（成功收尾专用，与 `Line::Error` 对称），渲染成
**成功色 + 加粗，上下各一条整行分隔线**：

```text
│Agent：能读文件、改代码、跑命令，并把每轮对话落盘成…       │
│────────────────────────────────────────────────────█
│  ✔ 任务完成。                                        █
│────────────────────────────────────────────────────█
```

只换颜色不够——绿色的工具成功行已经有一堆了；靠「整行铺满的分隔线」把它从
正文里切出来，滚动时是一道一眼扫得到的带。

**验收**：新增 `task_done_marker_is_prominent`（断言完成行是成功色 + 加粗 +
上下分隔线）；`cargo test --workspace` **167 passed / 0 failed**。

## 改动：助手回答的 Markdown 渲染（2026-10-16）

原来助手输出是纯文本平铺——`### **加粗**`、`| 列 |` 这类源码符号原样印出来，
读的人是在读源码，不是在读答案。新增 `agent-tui/src/markdown.rs`
（`pulldown-cmark` 驱动），**只渲染终端表达得了的子集**：

| 元素 | 呈现 |
|---|---|
| 标题 | 主色（H1/H2）/ 强调色（H3+）+ 加粗，上方留一空行 |
| 加粗 / 斜体 / 删除线 | `Bold` / `Italic` / `CrossedOut` 修饰 |
| 行内代码 | 强调色文字（**不用** success/warning/error——那是状态的语义色） |
| 围栏代码块 | 左侧 `▎` 竖条 + 整行铺底 |
| 有序 / 无序列表 | 缩进 + `•` / 序号，续行悬挂对齐 |
| 任务列表 | `✔` / `□` |
| 引用 | 左侧 `│` 灰竖条 |
| 分隔线 | 32 列细线（不铺满，铺满的分隔线留给「任务完成」） |
| 链接 | 文字加下划线，URL 与文字不同时附后 |

**降级与安全**：表格不渲染（要算列宽，窄屏必挤坏）、HTML 丢弃（终端不是浏览器）、
公式按字面文本保留——共同点是**内容不丢**，只退回源码形态。

**流式容错**（首要原则）：LLM 逐字吐，片段常常残缺（`**加粗` 没闭合、围栏只写
一半）。`pulldown-cmark` 对未闭合围栏照代码块内容收，未闭合行内标记当字面文本：
不崩、不吞字符，代价是那一帧样式闪一下，**比丢内容可接受**。有单测专门跑一组
残缺片段，断言除语法字符外一个都不能丢。

**实现要点**：软换行 + 列表悬挂缩进在 `markdown.rs` 内完成（`wrap_line` 提到
crate 可见），外层再 wrap 一次是幂等的；代码块的换行是自己按 `\n` 切的——因为
`pulldown-cmark` 把代码块内容当作**单个含换行的 Text 事件**，直接塞进 span 会被
ratatui 当成同一行的单元格。

**验收**：新增 19 个单测（17 个元素渲染 + 2 个集成路径），覆盖各元素、流式残缺
不丢字符、行宽不超、代码块后样式不溢出；`cargo test --workspace` **186 passed / 0 failed**。

## 改动：鼠标点击交互（2026-10-16）

原来只支持滚轮（对话区上下滚、输入区左右移光标），其余鼠标事件一律忽略。
这次补齐**左键点击/拖拽**，并把 UI 动作派发抽成 `dispatch`，让鼠标和键盘走
同一条命令发送路径：

| 指针位置 | 交互 |
|---|---|
| 对话区滚动条 | 点击跳转、拖拽滚动（滑块顶部对齐指针） |
| 输入框内容区 | 点击定位光标（按显示列宽折算：中文 2 列、横向滚动已补偿） |
| header 权限徽章 | 点击循环切换档位 L1→L2→L3，**立即作用于之后提交的任务** |
| 审批弹层按钮 | 点击「批准 / 拒绝」（按钮固定在内区底部，坐标稳定可命中） |

footer 是纯快捷键提示条，没有可点击目标，故不参与命中分流。

**实现要点**：`handle_mouse` 返回 `Option<UiAction>`，与 `handle_key` 共用一个
`dispatch` 函数消费动作；审批弹层期间的其它鼠标交互一律吞掉，避免误触背后
界面。点击定位光标的核心是「显示列 → 字符索引」换算——中文等宽字符占 2 列，
且超长输入会被横向滚动，点击列要先加回滚动量再折算。审批弹层按钮从 body 里
拆出来、固定渲染在内区倒数第 2 行——否则参数超长 wrap 会把按钮往下挤，命中
坐标就没法稳定。

**验收**：新增 9 个单测（光标定位含中文宽度与横向滚动、字符索引落字节、档位
循环与枚举映射、徽章点击、按钮命中区域、滚动条点击跳转）；`cargo test --workspace`
**195 passed / 0 failed**。

## 改动：`/` 命令补全（2026-10-17）

设计稿的输入框 pill 早就画了个 `/`，但一直补不了——输入 `/` 就是普通字符。
这次把命令补全补齐：输入 `/` 弹出候选浮层（贴输入框左上角往上长），`↑/↓` 选、
`Tab` 补全、`Enter` 执行、`Esc` 关闭；鼠标点候选直接执行、点菜单外关闭。

**候选只列能真正落地的命令**：`/clear`（清空对话）、`/help`（打印帮助）、
`/tier`（切换权限档）。设计稿里的 `/model`（切模型）、`/compact`（手动压缩）
依赖后端能力，尚未实现故不列入——遵循「只提示已实现功能」，宁可少列也不补出
执行不了的入口。

**实现要点**：
- 候选是输入的纯函数：`slash_candidates()` 按 `/` 后前缀过滤 `SLASH_COMMANDS`，
  补全状态只存「是否激活 + 高亮索引」两个字段，不缓存候选列表。
- `Enter` 精确匹配才执行，前缀不完整时先补全再让用户确认一次——避免只输 `/`
  就误执行默认高亮的第一个命令（`/clear` 会清空对话）。
- 菜单渲染与鼠标命中共用 `completion_menu_rect` 保证坐标一致；候选带 1 个
  前导空格，外框宽度按 `max_w + 3` 算，否则 `/clear` 会被右框裁掉最后一个字符。
- 补全激活时鼠标点击一律吞掉（点候选执行、点菜单外关闭），不误触背后界面。

**验收**：新增 10 个单测（前缀过滤、审批弹层不补全、三命令执行语义、激活/过滤、
Enter 精确执行 vs 前缀补全、上下选与 Esc、Tab 补全、菜单坐标、鼠标点击、渲染
快照）；`cargo test --workspace` **205 passed / 0 failed**。

## 改动：P1 交互收尾（Toast / `Ctrl+R` 历史 / `Ctrl+H` 隐藏 footer）（2026-10-17）

P1 剩余三项交互一次落地：Toast 通知、`Ctrl+R` 输入历史、`Ctrl+H` 隐藏 footer。

| 项 | 说明 |
|---|---|
| Toast 通知 | `App::toast(text)` 创建 2 秒过期提示，`on_tick` 到期清除；右上角反色块覆盖渲染，叠在审批弹层之上，任意状态都看得见 |
| `Ctrl+R` 输入历史 | `submit` 记录历史并去重；`recall_history` 循环回溯；手动编辑（打字/退格/Delete/Tab/补全/提交）后 `leave_history` 归零，下次从最新开始 |
| `Ctrl+H` 隐藏 footer | `toggle_footer` 翻转显隐；`layout_chunks` 按 footer 高 1/0 收回该行还给对话区 |
| 切档反馈 | `cycle_tier` 由改写状态改为发 Toast，不再覆盖「运行状态」 |

**实现要点**：Toast 用 `Instant + Duration` 单调时钟到期，不依赖系统时间；`Ctrl+H`
在 crossterm 0.28 Windows 下解析为 `Char('h')+CONTROL`，与 Backspace 可区分；
带 `CONTROL` 的字符在普通输入分支之前单独拦截，不会误入正文。

**验收**：新增 12 个单测（app：提交去重/循环回溯/空历史 Toast/编辑后重开回溯/
footer 翻转/Toast 到期/切档 Toast；render：`Ctrl+H` 切 footer/`Ctrl+R` 召回历史/
隐藏布局收行/footer 不渲染/Toast 右上角渲染）；`cargo xtask check` 通过，
`cargo test --workspace` **217 passed / 0 failed**。

## 改动：M2 能力收尾（finish 工具 + 写前快照 + 端到端验收）（2026-10-17）

补齐 M2「一次对话内写文件 → 执行脚本 → 报告结果」的最后两块拼图，并跑通端到端验收。

| 项 | 说明 |
|---|---|
| `finish` 工具 | 显式终止信号：`summary` 给用户总结、`artifacts` 登记交付物路径；`Tool` trait 新增 `is_terminating()` 标记 |
| 执行循环收尾 | runner 遇到 `is_terminating` 工具：跳过权限裁决直接执行，执行完立即结束循环，汇总总结与产物（去重），不再多跑一轮 |
| 写前快照 | `file_write` / `file_edit` 覆写前把旧内容备份到私有工作区 `snapshots/`（`<文件名>.<毫秒>`）；新建文件不产生空快照 |
| 提示词引导 | system prompt 引导模型：产出写入 `artifacts/`，任务完成调用 `finish` 登记交付物 |

**实现要点**：`finish` 是纯收尾、无副作用，因此在权限引擎之前就放行（L3 也不会弹层打断收尾）；快照落在私有工作区而非项目根，避免污染用户仓库。

**验收**：新增 8 个单测（finish：登记总结/产物、缺省兜底、逃逸路径跳过；快照：write/edit 覆写备份、新建不备份；runner：终止信号跳过权限并提前收尾、真实工具端到端「写文件→执行脚本→报告」）；`cargo xtask check` 通过，`cargo test --workspace` **225 passed / 0 failed**。

## 改动：输入区支持 `Ctrl+Enter` 多行换行（2026-10-18）

输入框原先只有一行：footer 早就提示过换行，但 `handle_key` 里并没有换行分支，
属于「提示了却做不到」。

| 项 | 位置 |
|---|---|
| `Ctrl+Enter` 换行、`Enter` 提交（带 CONTROL 的 Enter 分支排在普通 Enter 之前） | `render.rs::handle_key` |
| 输入区高度 3 → 5（3 行可视内容） | `render.rs::layout_chunks` |
| 多行渲染：硬换行 + 垂直滚动跟随光标 + 光标行横向滚动 | `render.rs::input_lines` |
| 横向滚动按显示宽度裁剪（CJK 占 2 列），裁剪点不碎字符 | `render.rs::trim_left_by_width` |
| `insert_newline` / `cursor_line_col`（行号 + 行内列，供渲染定位软光标） | `agent-tui/src/app.rs` |

footer 提示同步为 `^⏎ 换行`（`^` 为 Ctrl 的传统记法，长度不变不影响布局）。

**已知限制**：部分 Unix 终端把 `Ctrl+Enter` 与 `Enter` 上报成同一字节 `\r`，
无法区分，属终端限制，应用层无从判断。

**验收**：新增 6 个单测；`cargo xtask check` 通过。

## 缺陷修复：`web_fetch` 处理含中文网页 panic（2026-10-18）

**症状**：抓取含中文的页面时工具线程 panic——
`start byte index 29135 is not a char boundary; it is inside '解'`。

**根因**：`strip_html` 按**字节**推进（`i += 1`、`bytes[i] as char`），却对 `&str`
做 `lower[i..]` 切片。`i` 落在多字节 UTF-8 字符中间时必然 panic；把字节当
Latin-1 输出还会让中文 / emoji 变乱码（「解」= `E8 A7 A3` 被拆成 `è § £`）。

**改动**：改为按 `char` 迭代（`i += c.len_utf8()`）；标签名是 ASCII，改用字节前缀
做大小写不敏感匹配（`starts_with_ignore_ascii_case`），不再对整篇文档
`to_lowercase()`——后者可能改变字节长度，与原 `input` 的索引错位。`i += 7`
一并修正为 `i += b"</script".len()`（8 字节）。

**回归保护**：新增 `strips_tags_with_multibyte_text`（中文 + emoji 不丢不乱码）；
顺带排查全仓同类「按字节索引切 `&str`」的位置，其余（`app.rs` 的 cursor、
`render.rs` 测试用 `find()`、`web.rs` 里以 ASCII 标记为锚点的 `attrs()` /
`first_anchor()` 扫描）索引均落在 char 边界。

**验收**：`agent-tools` 23 passed；`cargo xtask check` 通过。

## 改动：接通「批准本任务同类操作」（2026-10-18）

**症状**：L3 下在审批弹层按 `a`（批准本任务同类操作），后续同类工具调用**照旧弹窗**
——看起来生效了，实际没有。

**根因**：能力都在，最后一米没接线，链路断在三处：

```text
UI `a` → UiAction::Approve{remember}   ─┐
                                         ├─ take_approval 里 `let _ = remember` 吞掉意图
PermissionEngineImpl::approve_for_task  ┤   └─ 不在 trait 上，Arc<dyn PermissionEngine> 拿不到
runner deps.batch_approve              ┴─ apps 装配恒为 None，且转发硬编码 remember:false
```

**改动**：

| 项 | 位置 |
|---|---|
| `Command::Approve` 携带 `remember`（改结构体变体） | `agent-core/src/event.rs` |
| `take_approval` 不再吞掉 `remember` | `agent-tui/src/app.rs` |
| `PermissionEngine` trait 新增 `approve_for_task`（默认空实现，不破坏替身） | `agent-core/src/perm.rs` |
| trait 实现转发到固有方法，登记逻辑保持单点 | `agent-perm/src/lib.rs` |
| 转发透传 `remember`（原硬编码 `false`） | `apps/agent-cli/src/main.rs` |
| 装配 `batch_approve` 回调：`tokio::spawn` 调 `approve_for_task` | 同上 |

`agent-server` 是 headless 单次执行、没有审批交互（`_ap_tx` 未使用），故仍为 `None`。

**回归保护**：新增 2 个单测——UI 侧断言 `remember` 没被吞
（`take_approval_carries_remember_intent`）；runtime 侧端到端断言按 `a` 后回调
确实收到 `(task_id, tool)`（`approve_with_remember_registers_batch_approval`）。

**验收**：`cargo xtask check` 通过。

## 修复：`web_search` 总是失败（2026-10-24）

**症状**：无论什么查询，`web_search` 都返回同一句错误——
「web_search 未配置搜索后端」。工具在 8 个工具里唯一常年不可用。

**根因**：它是**唯一**的后端是自建 SearXNG 的 JSON API，而：

1. 默认配置、配置向导、`config.example.toml` 都不会写 `[search]` 段
   → `searxng_endpoint` 恒为 `None` → 每次调用直接返回错误；
2. 就算配置了，也还有三个放大问题的实现缺陷：
   - 不校验 HTTP 状态码，直接 `resp.json()`：SearXNG 未启用 JSON 输出时返回
     403 的 HTML 错误页，报错是「bad search response: expected value at line 1
     column 1」，完全指不出真实原因；
   - URL 用字符串拼接（`format!("{endpoint}?q=...")`），端点自带查询参数会拼出
     非法 URL，且手工百分号编码不正确处理多字节；
   - 不支持取消（对比 `web_fetch` 有 `ctx.cancel`）。

「必须有后端才能用」+「后端几乎没人有」= 工具 100% 失败。

**改动**：

| 项 | 位置 |
|---|---|
| 新增内置免 key 后端（Bing 结果页），零配置即可搜索 | `agent-tools/src/web.rs` |
| 后端变成有序列表，失败（HTTP 错误/不可解析/零结果）自动顺延下一个 | 同上 |
| 校验状态码并把状态码 + 正文摘要带进报错；识别 SearXNG JSON 输出未启用并给出具体改法 | `http_error` / `parse_searxng` |
| URL 改由 `reqwest::Url` 的 `query_pairs_mut` 构造（含原参数保留与编码） | `SearchBackend::build_url` |
| 显式 `Accept-Encoding: identity`——依赖未开 gzip 解压特性，否则取回压缩正文只会「解析出零结果」 | `WebSearch::run` |
| 接入 `ctx.cancel`（与 `web_fetch` 一致） | 同上 |
| `[search] backend`（auto/searxng/bing/off）+ 缺端点时的非致命警告 | `agent-providers/src/config.rs`、`config/config.example.toml` |
| 两个入口按配置解析后端列表 | `apps/agent-*`、`agent-tools/src/registry.rs` |

隐私取舍：`backend = "searxng"` 时**不回退**到内置后端，宁可失败也不把 query
发给第三方；默认 `auto` 才允许回退。

**回归保护**：新增 19 个单测——后端解析矩阵（含 `searxng` 无端点不得偷偷回退、
`off` 关闭）、URL 构造（保留原参数、中文查询往返一致、非法端点报错点名字段）、
Bing 解析（实体解码、多字节不乱码、切片不 panic、`count` 上限）、SearXNG JSON
解析与 HTML 错误页可读性、`http_error` 内容与提示、渲染格式、`backend=off`
与空 query 的输入校验。另有 1 个 `--ignored` 真实网络冒烟测试
（`searches_without_configuration_against_live_bing`），已实跑通过并拿到 5 条
真实结果（中文标题/摘要正常）。

**验收**：`cargo xtask check` 通过（fmt + clippy 0 warning + 测试）；
`cargo test --workspace` **257 passed / 0 failed / 2 ignored**（原 225 passed）。

## M3：安全闭环（2026-10-24）

**动机**：M3 的条目全是「代码在位但没验过」——和 `web_search` 出事前同一副样子。
这一轮把它们拿到真实环境里兑现，结论是**抓出 4 个真问题，其中 2 个是功能其实没生效**。

### 改动

| 项 | 位置 |
|---|---|
| 月度用量账本：按 `YYYY-MM` 分账、跨月自动归零、原子写盘 | `agent-providers/src/budget.rs`（新增） |
| 执行循环把每轮成本报给账本（此前流式路径从不记账） | `agent-runtime/src/runner.rs`（`cost_sink`） |
| 月度见底先退本地 provider，没有本地才拒绝 | `router.rs::resolve` |
| 成本可见性：启动打印 `月度预算 $x 已用 $y（n%）`，跨 80%/100% 告警 | `router.rs::budget_line` |
| L2 审视器从 `agent-cli` 抽到 `agent-providers`，两个入口共用 | `agent-providers/src/review.rs`（新增） |
| `AiReviewer` trait 移到 `agent-core`（入参出参全是 core 类型） | `agent-core/src/perm.rs` |
| 批量批准改为执行循环直接 await 引擎（去掉同步回调 + spawn） | `agent-runtime/src/runner.rs` |

### 抓到的 4 个真问题

1. **月度熔断从来没累计过**：`record_cost` 只在 `chat_with_failover` 里被调用，
   而执行循环是流式直连 provider 的、AI 审视走 `provider.chat`——
   两条主路径都不经过它。配好的月度预算**永远显示已用 $0**。
2. **月度用量不落盘**：`spent_month_usd` 是内存 `f64`，注释写着「可持久化」
   但没人实现，重启即清零。
3. **批量批准有竞态**：审批通过后走同步回调、回调内部再 `tokio::spawn` 去登记，
   下一次同类调用可能在登记落定前就查过了——用户按了 `a` 仍被再问一次。
   系统级测试实跑复现（询问 2 次而非 1 次），改为直接 await 引擎后修复。
4. **L2 审视的 `max_tokens` 太小**：实测（真实 DeepSeek）发现模型写长中文理由时
   JSON 断在 `"confidence":` 处，整条判定作废、白降级一次询问。

### 实测结论（真实 DeepSeek，`cargo test --test review_live -- --ignored`）

| 轮次 | 结果 | 调整 |
|---|---|---|
| 第 1 轮 | 7 例中 1 例失败：`web_fetch` 报 `no json` | 定位到 `max_tokens: 200` 截断 → 提到 400 + 提示词限制 reason 长度 + 截断时兜底捞 `decision` |
| 第 2 轮 | 7 例中 1 例失败：`web_fetch` 被拦（「与任务无关」） | 提示词改为「只判断危害，相关性仅作参考，不单独否决」 |
| 第 3 轮 | 9 例全对 | 判定理由合理（删库/读私钥/外传凭据 → deny；无害命令 → allow） |

顺带一个**策略确认**：误拦只是多问一次，误放行才是不可逆代价——所以审视器
「拿不准就问人」，`risk=high` 时即便 `allow` 也仍然询问。

### 回归保护

- `budget.rs`：跨月分账、重启后累计仍在、损坏文件不阻塞启动、0/NaN 不计账
- `router.rs`：超限退本地（并断言选择稳定——修掉了一个 HashMap 迭代顺序导致的不确定性）、
  无本地兜底时明确报 `BudgetExceeded`、`budget_line` 文案、阈值只报一次
- `runner.rs`：每轮成本确实上报给 sink；批量批准端到端只询问一次
- `review.rs`：JSON 解析（含围栏/废话）、思考链兜底、截断兜底、
  缺 `decision` 必须报错、`risk=high` 仍询问、超时降级、L3 不受审视器影响

**验收**：`cargo xtask check` 通过（fmt + clippy 0 warning）；
`cargo test --workspace` **297 passed / 0 failed / 3 ignored**（原 271）；
L2 审视真实模型实测 9/9 通过（连跑两次稳定）。

## 工具端到端冒烟（2026-10-24）

**动机**：`web_search` 100% 失败的那次，257 个单测是全绿的。复盘发现单元测试
天然看不见三类问题：

1. 不过 `default_tools()` 这条生产装配路径——工具漏注册、改名都照样绿；
2. 不比对「模型看到的 JSON Schema」与「`execute` 真正读的键」——只改一侧，模型
   永远收到 `InvalidArgs`，表现就是某个工具 100% 失败；
3. 不对**输出内容**下断言——「不报错」和「真的能用」是两件事。

这套 harness 的用途是让「工具能用」被**持续**验证，而不是等某天偶然撞见。

**改动**：新增 `crates/agent-tools/tests/smoke.rs`（14 个集成测试），
`cargo xtask smoke` 作为单独入口。它本身就是集成测试，已被 `cargo test --all`
带上、`cargo xtask check` 自动纳入门禁。

| 用例 | 拦截的问题 |
|---|---|
| `every_tool_is_registered` | 8 个工具漏注册/改名 |
| `llm_sees_a_usable_definition_for_every_tool` | 描述为空、`parameters` 不合 schema（模型不会用） |
| `tools_accept_exactly_the_args_they_advertise` | 声明参数与实际读取漂移（核心） |
| `missing_required_arg_is_rejected_not_ignored` | 必填校验被删后静默按空值跑 |
| `write_then_read_round_trips_content` | 中文/emoji 往返 |
| `edit_actually_rewrites_the_file` | 编辑没落盘、找不到 `old_str` 却报成功 |
| `list_dir_shows_what_was_written` | 列目录与写入不一致 |
| `escape_attempt_is_rejected_through_the_registry` | 装配路径上的逃逸防护 |
| `finish_reports_summary_and_real_artifacts` | 交付物没登记/字节数为 0 |
| `shell_exec_runs_a_real_command` | 真实命令执行 + 乱码 |
| `web_fetch_retrieves_a_real_page` | 抓得到但正文不对/标签没剥 |
| `web_search_returns_real_hits_without_configuration` | **零配置必须能搜** |
| `web_search_rejects_blank_query` | 空 query 不被静默接受 |
| `transport_failures_are_skipped_but_real_breakage_is_not` | 守护跳过判定，防止冒烟变走过场 |

**联网用例的判定**：只有**传输层**失败（DNS/连不上/超时/TLS/proxy/429）才**跳过**
并打印原因——离网 CI 不该误红，但也不该假装验过了；其余一律算失败。该判定由上面
最后一条用例 lock 住，避免放宽后整套冒烟变成摆设。已用不可达代理
（`ALL_PROXY=http://127.0.0.1:9`）实测：联网时 14 passed 且真的拿到结果，
离线时 14 passed 且两个联网用例打印「跳过」。

**验证**：`cargo xtask check` 通过（fmt + clippy 0 warning + 测试）；
`cargo test --workspace` **271 passed / 0 failed / 2 ignored**（原 257）。
8 个工具全部实跑通过，未发现新的产品缺陷。

## M4：MVP 验收线（2026-10-24）

M4 的定义是「跑通用户亲述的三类真实任务」，即一期完成的判据。这一轮把
4.1–4.6 全部落地，并在「验收」这件事上抓出 **5 个真问题**，其中 **3 个是功能
其实没生效**——和 M3 一样的味道，见下面的「抓到的 5 个真问题」。

### 4.1–4.3：三类真实任务

用真实 DeepSeek + 真实工具跑，产物留在 `tmp/m4/`（该目录不入库，只是本地证据）。

| # | 任务 | 实际跑出来的东西 | 证据 |
|---|---|---|---|
| 4.1 写程序 | 从零写一个统计脚本，并用它在测试数据上跑出结果 | `count_files.py`（4.4KB，argparse + 递归 + 中日韩对齐，只用标准库）＋ 造出的 `testdata/`（8 个文件、含子目录与无扩展名文件） | `tmp/m4/p1/` |
| 4.2 写脚本 | 写一个把当前目录 `*.txt` 备份到 `backups/` 并加时间戳前缀的批处理 | `backup.bat`（PowerShell 取时间戳 + 失败回退），实跑 3 次，`backups/` 里留下 3 组文件；`skip.md` 未被复制（`*.txt` 过滤生效） | `tmp/m4/p2/` |
| 4.3 收集新闻 | 联网搜 + 抓原文，汇总成一份带出处的报告 | `news.md`（14.7KB：1.99/1.99.1、i686 降级、供应链投毒、TWiR #674 等，逐条带链接与日期）＋ 抓回的原始页面 `raw/blog_index.txt` | `tmp/m4/p3/` |

三条都不是「让模型说说而已」：4.1 的脚本**真跑过**、4.2 的脚本**真跑过三次**、
4.3 的报告**逐条核过原文**（作者在报告里明确标了哪几条没能定位到具体文章路径）。
这三条就是「一期 MVP 达成」的直接证据。

### 4.4：任务面板 / 切换 / 恢复

上一版只做到「`/tasks`、`/resume` 两个命令名进了补全列表」，再往下**整条链路是断的**：

| 抓到的真问题 | 表现 | 根因 |
|---|---|---|
| 编译不过 | `cargo check` 直接报 `E0004`：`Command::ListTasks` / `ResumeTask` 没人处理 | 上一版只改了 UI 与 core，编排层（`agent-cli`）的 `match` 没跟上 |
| `tasks/` 永远是空的 | `/tasks` 列出来一片空白 | `upsert_task` 从 M1 就在，但**没有任何生产路径调用它** |
| `/tasks` 按了没反应 | 输入命令后什么都不发生 | `take_pending_command()` 写好了，但 `render.rs` 的事件循环从没调用过它 |
| 续跑会丢新指令 | 恢复后模型把上一轮的问题重答一遍 | runner 只在「有历史」时推历史，**新 prompt 没推进去** |
| 切档只是装饰 | header 徽章切了 L3，提交时仍是 L2 | `agent-cli` 用配置默认值覆盖了 `spec.tier`；`dispatch` 还另外重建了一份 `TaskSpec` |

**改动**：

| 项 | 位置 |
|---|---|
| 新增 `Event::TaskList` / `TaskOpened`（转写事件回灌界面） | `agent-core/src/event.rs` |
| `TaskSpec.resume_from`（界面只传 id，消息由编排层补） | `agent-core/src/task.rs` |
| 任务摘要落盘 `touch_task`（保留创建时间、临时文件 + 改名） | `agent-store/src/lib.rs` |
| `resume_context`（最后一份快照 + 快照之后的答复）、`task_events`（转写，剔除快照） | 同上 |
| 每轮开始/收尾登记任务状态（`Running` → `Done`/`Failed`/`Cancelled`） | `agent-runtime/src/runner.rs` |
| 续跑：历史之后**必须**再推入本次新指令 | 同上 |
| `resolve_task`（完整 id / 唯一前缀 / 歧义报数）、`apply_resume`（可单测） | `apps/agent-cli/src/main.rs` |
| `Ctrl+G` 任务面板：↑/↓ 选择、Enter 切换、Esc 关闭、鼠标点行切换 | `agent-tui/{app,render}.rs` |
| 切到历史任务：回放转写（复用 `apply_event`，不写第二套渲染）、对话连续 | 同上 |
| `/help` 补上新命令；footer 加 `^G 任务` | 同上 |

**为什么「同一任务 id 接着跑」**：面板里一条任务就是一个话题。每次追问都新建任务的话，
面板会被「继续」「再改一下」刷屏，而且模型每轮都失忆——用户看到的是一个连续对话，
agent 却每句话都从零开始，这是最伤信任的一种不一致。`/clear` 才是「开新话题」的入口。

**验证**：

- 单测：面板状态机 / 转写回放（含「只有 `AssistantTurn`、没有逐字增量」的历史）/
  切换后提交带 `resume_from` / 前缀歧义 / `apply_resume` 落上下文 / 任务状态落盘。
- 真实模型：`apps/agent-cli/tests/resume_live.rs`（`--ignored`）实跑通过——
  第一轮让模型记住 `7319`，第二轮从 store 取回上下文追问，回答 `7319`；
  并断言同一任务只更新一条、续跑请求里同时含旧对话与新指令。
- 端到端：`agent-server` 真实跑一次任务，`tasks/<id>.json` 与 jsonl 正常落盘
  （本机 DSH 沙箱下写 `~/agent` 会被拦，属环境限制，非代码问题）。

### 4.5：Markdown 表格

原来表格整段按源码平铺（`| a | b |`）。现在：

| 宽度 | 呈现 |
|---|---|
| 够宽 | 对齐成列：表头主色加粗 + 一条 `─` 分隔线 + 数据行，列间 ` │ ` |
| 窄屏 | 降级成「列名: 值」的竖排记录（`• 首列` + 缩进的 `列名: 值`），折行悬挂对齐 |

判据看**最长一行**（各列最大宽度之和）而不是平均值：只要一格特别长，对齐后整张表必然越界。
两种情况都不丢内容；新增 4 个单测（对齐、窄屏降级、行宽不超、行内码）。

### 4.6：新闻收集模板

`/news <主题>`：把 `templates/news.md` 渲染进输入框（`{topic}` / `{period}` / `{slug}`），
**不自动发送**——先让人看一眼、改一改，避免一按回车就花掉一次联网任务。

模板本身把 4.3 踩过的坑固化成要求：先搜再抓、每条要有标题/日期/链接、不许凭印象拼 URL、
拿不到确切链接就写「来源：首页」、交付到 `artifacts/news-<slug>.md` 并在 `finish` 里登记。
`templates/news.md` 优先于编译进二进制的内置副本（用户改完重启即生效），
内置副本保证零配置可用。

**顺带修掉的坑**：`/news Rust` 这类**带参数**的命令在补全菜单里匹配不到（前缀对不上），
回车会被当成普通任务原样发给模型。现在回车前先用 `is_slash_command()` 拦一道。

## P2 收尾：`@` 文件补全与编辑 diff（2026-10-24）

M4 之后按「已知偏差」清单补 P2 的两项 UI 欠账。**sqlite 索引仍然没做**，理由见最后一节。

### `@` 文件补全

输入 `@` 从项目根扫出候选（相对路径），按 `路径前缀 → 文件名前缀 → 包含` 排序；
`↑/↓` 选、`Tab`/`Enter` 补全、`Esc` 关闭，鼠标点候选也补全。

几个不那么显眼、但直接决定好不好用的取舍：

| 决策 | 原因 |
|---|---|
| 只认「行首或空白后的 `@`」，且到光标之间无空白 | `a@b.com` 是邮箱，弹菜单只会碍事 |
| 跳过 `.git` / `target` / `node_modules` / 点开头目录；深度 8、上限 2 万条、候选 50 条 | 按一下 `@` 不该让 TUI 卡住；`@` 是用来选「要 agent 看的文件」，不是目录浏览器 |
| 补全后自动补一个空格 | 否则接着打字会把字符拼进路径，菜单又弹出来——实测很烦 |
| `Enter` 只补全、**不提交** | 挑文件挑到一半把任务发出去是最坏的意外 |
| 排序固定（分数 → 长度 → 字典序） | 否则两次按键之间候选换位，高亮落在哪条全看运气 |
| 没有项目根（headless）就不弹 | 弹一个永远空的菜单比不弹更糟 |

扫描是同步 IO，只在「刚输入 `@`」时发生一次，关闭即丢弃清单——不常驻内存，
也就不会拿到过期结果。

### 编辑 diff 预览

`file_edit` 成功后在工具行下面展开改动（`- 旧行` / `+ 新行`，每侧 12 行封顶）：

```
  ✔ file_edit  crates/agent-tui/src/app.rs  42ms
     - let x = 1;
     + let x = 2;
```

- **只对 `file_edit`**：它入参里就有 `old_str`/`new_str`，不用读盘、不用翻快照；
  `file_write` 没有「改前」文本，硬做只能是全 `+` 的假 diff。
- **只在成功时展开**：失败 / 被拒时「改了什么」并不成立，展示会误导。
- 不加整行底色（已有用户消息、代码块两种底色，再加一种会糊），只靠红 `-` 绿 `+`。

### 为什么 sqlite 索引仍然推迟

文档里的触发条件是「任务量上来后」，现在没到：

- 本机 `tasks/` 目录至今为空（`/tasks` 刚修好），没有可索引的真实数据；
- 内存索引已经存在（`JsonlStore` 的 `cache` + `seq`），单用户单机在几百个任务内
  读目录 + 反序列化是毫秒级；
- 接入 `rusqlite` 要引入 C 依赖并重写 `Cargo.lock`，为一项目前没有收益的优化
  付编译与跨平台成本不值。

真要接的时候，落点是 `agent-store`：jsonl 仍是唯一真相源，sqlite 只做可重建的
索引缓存（设计文档已注明），坏了删掉重建即可。

### 验证

新增 20 个单测（`@` 的触发/排序/补全/键鼠/渲染/扫盘跳过与上限，diff 的生成/截断/
成功才展示）；`cargo xtask check` 通过，全量 **352 passed / 0 failed / 4 ignored**。

顺带补上占位提示：空输入时渲染 `/ 命令` 与 `@ 引用文件` 两个反显 pill（此前两个
功能都没落地，所以一个 pill 都不显示；现在落地了就该让人看得见），`!` `$` `#`
仍然不显示。

## 已知限制（一期）

| 限制 | 说明 |
|---|---|
| 上下文压缩为轻量摘录 | 非 LLM 摘要，二期可增强 |
| 工具调用为串行执行 | 设计上支持并行，一期按顺序执行 |
| `web_search` 内置后端依赖 Bing 结果页结构 | HTML 结构变了会退化为「无结果」，此时可改用自建 SearXNG（JSON API，稳定契约）；该退化由 `cargo xtask smoke` 拦截 |
| 历史任务转写依赖 jsonl 未清理 | 日志被清掉时仍能切过去继续，但没有可回放的对话（界面会明确说明） |
| 任务面板不显示运行中任务的实时进度 | 单任务串行执行，面板用于「回看 / 切换」，实时进度在对话区 |

## 待办（M4 之后）

| 里程碑 | 内容 |
|---|---|
| 二期 | 分级同步、AI 合并 + 回滚、设备注册中心（`SyncEndpoint` 接真实实现） |
| 三期 | 手机端 / 哑终端（新增一个 `Frontend` 实现，core 零改动） |
