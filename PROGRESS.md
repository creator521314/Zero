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
| `agent-tools::paths` | `../` 逃逸拒绝；工作区内路径接受 |
| `agent-tools::web` | HTML 标签与 script 剥离 |
| `agent-providers` | tool 消息转换；assistant tool_call 转换（arguments 必须为字符串） |
| `agent-runtime::context` | 上下文压缩保留固定区并生成摘要 |
| `agent-tui::app` | 多字节字符编辑；流式增量合并为一行；空输入不提交；工具四态流转（含迟到事件不覆盖已终结行） |
| `agent-tui::theme` | 权限档位 → 徽章色/文案映射（三态、大小写不敏感） |
| `agent-tui::render` | 六类消息符号与缩进；工具四态符号/耗时/原因（多行压单行、超长截断）；工具名固定列对齐（含中文宽度）；宽字符 wrap 行数；鼠标区段分流；`TestBackend` 渲染快照 |
| `agent-store` | 追加 + 重载；逻辑时钟单调；durability 保留 |

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

- Rust 源文件 36 个，约 **5200 行**
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
