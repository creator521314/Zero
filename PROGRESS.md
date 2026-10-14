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
| `agent-tui::app` | 多字节字符编辑；流式增量合并为一行；空输入不提交 |
| `agent-tui::theme` | 权限档位 → 徽章色/文案映射（三态、大小写不敏感） |
| `agent-tui::render` | 六类消息符号与缩进；工具名固定列对齐（含中文宽度）；宽字符 wrap 行数；鼠标区段分流；`TestBackend` 渲染快照 |
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

**后续**：P1（工具耗时/错误原因、Toast、`Ctrl+R` 历史、`/` 命令补全）、
P2（侧边栏、Markdown、diff 视图）见 `docs/ui-design.md` 第十章。

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
