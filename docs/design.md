# 个人 Agent 技术方案设计文档 V1

| 项目 | 内容 |
|---|---|
| 文档版本 | V1（方案设计阶段产出） |
| 编制日期 | 2026-10-14 |
| 上游文档 | 《个人Agent需求分析文档_V1.md》 |
| 编制方式 | 4 个专家方向并行设计（系统架构 / Agent运行时 / LLM Provider / TUI），由 PM 统一整合并归一接口 |
| 文档状态 | 待评审 |

---

## 一、方案总览

### 1.1 设计目标（对应需求文档 MVP 范围）

一期交付一个 **Rust 单机通用 Agent**：TUI 交互、接入多云端 LLM（预留本地接口）、能读写文件/执行命令/联网，完成任务级三档权限管控，成功跑通「写代码 / 写脚本 / 收集新闻」三类真实任务。

### 1.2 技术栈总表

| 层 | 选型 | 理由 |
|---|---|---|
| 语言 | Rust（stable） | 用户指定；跨 PC/服务器，二期可裁剪至嵌入式 |
| 异步运行时 | tokio（multi_thread） | 生态标准 |
| TUI | ratatui + crossterm | 纯 Rust、跨平台、SSH 可用 |
| HTTP / SSE | reqwest + eventsource-stream | 连接池/TLS 成熟，SSE 分帧可靠 |
| 序列化 | serde + serde_json / toml | 标准 |
| 持久化 | **jsonl（事件真相源）+ sqlite（索引）** | 追加无锁、崩溃不丢、天然适配二期同步 |
| 密钥 | 环境变量优先（服务器）+ `secrecy` 包裹 + 日志脱敏 | Linux 服务器常无 Secret Service，keyring 不可靠 |
| 日志/错误 | tracing + thiserror/anyhow 分层 | 标准 |
| Markdown | pulldown-cmark + syntect | 流式增量友好 + 代码高亮 |
| 其他 | unicode-width、tui-textarea、tiktoken-rs、backoff、tokio-util(CancellationToken)、DashMap | 见各节 |

### 1.3 系统分层图

```
┌─────────────────────────────────────────────────────┐
│  agent-tui（渲染 + 输入）      agent-cli / agent-server（装配入口）│
│         ↕ Command                      ↕ 依赖注入                        │
├─────────────────────────────────────────────────────┤
│  agent-core（纯逻辑枢纽：Event 总线 / 7 大 trait）              │
├──────────┬──────────┬───────────┬─────────┬──────────┤
│ agent-   │ agent-   │ agent-    │ agent-  │ agent-   │
│ runtime  │ tools    │ perm      │providers│ store    │
│ 执行循环  │ 工具集    │ 权限引擎   │ LLM适配  │ 持久化    │
├──────────┴──────────┴───────────┴─────────┴──────────┤
│  agent-net（reqwest 出站封装，工具与 provider 共用）            │
│  agent-sync（二期占位：trait 冻结，空实现）                     │
└─────────────────────────────────────────────────────┘
```

---

## 二、Workspace 结构（架构专家方案）

```
agent-workspace/
├── Cargo.toml                    # [workspace] resolver = "2"
├── apps/
│   ├── agent-cli/      (bin)     # 入口：装配组件，拉起 TUI
│   └── agent-server/   (bin)     # 一期 = headless 模式；二期演化为数据中心
├── crates/
│   ├── agent-core/     (lib)     # 领域核心：Event/7大trait，零IO依赖
│   ├── agent-runtime/  (lib)     # Agent 执行循环、上下文管理
│   ├── agent-tools/    (lib)     # file/shell/web 工具实现
│   ├── agent-perm/     (lib)     # 权限引擎（三档 + AI 审视）
│   ├── agent-providers/(lib)     # LLM Provider 抽象 + 各厂商 adapter
│   ├── agent-tui/      (lib)     # ratatui 界面
│   ├── agent-store/    (lib)     # jsonl + sqlite 持久化
│   ├── agent-net/      (lib)     # reqwest 出站封装
│   └── agent-sync/     (lib)     # 二期占位：SyncEndpoint trait + 空实现
└── xtask/              (bin)     # 构建/校验脚本
```

**依赖铁律**（单人维护的关键）：

| 规则 | 说明 |
|---|---|
| 单向无环 | 所有实现 crate 只依赖 `agent-core`，互相不依赖 |
| core 零 IO | `agent-core` 永不依赖 ratatui/reqwest/DB —— 二期换传输层、三期换运行环境只替换叶子 crate |
| 演进锚点 | 一期只需保证 core 的 trait 与 Event 稳定；二期加 sync，三期手机 App = 另一个 `Frontend` 实现，核心零改动 |

---

## 三、核心抽象（已归一的统一接口）

> 四位专家分别给出的 trait 已由 PM 统一命名与签名，以下为**冻结版**。

### 3.1 事件总线（全系统唯一解耦机制）

```rust
pub enum Event {                       // Agent → 外界（TUI/落盘/同步）
    TaskStarted { id: TaskId, title: String },
    TextDelta { id: TaskId, text: String },        // 流式正文
    ReasoningDelta { id: TaskId, text: String },   // 思考链（TUI 灰显）
    ToolCallStart { id: TaskId, call_id: ToolCallId, name: String, args: Value },
    ToolCallEnd {                                  // 终结状态 + 耗时 + 原因
        id: TaskId, call_id: ToolCallId,
        outcome: ToolOutcome, ms: u64, detail: Option<String>,
    },
    AwaitingApproval { id: TaskId, req: ApprovalRequest },
    TaskFinished { id: TaskId, outcome: Outcome },
    Error { id: TaskId, err: ErrorInfo },
}

/// 工具终结状态：区分「跑了但失败」与「压根没跑」。
pub enum ToolOutcome { Ok, Failed, Denied, Rejected }

pub type EventBus = tokio::sync::broadcast::Sender<Event>;

pub enum Command {                     // 外界 → Agent
    SubmitTask(TaskSpec), Approve(TaskId), Reject(TaskId),
    Cancel(TaskId), Quit,
}
```

### 3.2 七大 trait 边界

```rust
// 1) LLM Provider（详见第五章）
#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> Capabilities;
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError>;
    async fn chat_stream(&self, req: ChatRequest)
        -> Result<BoxStream<'static, Result<StreamEvent, LlmError>>, LlmError>;
    fn price(&self, model: &str) -> Option<ModelPrice>;
}

// 2) 工具（详见第四章）
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;   // JSON Schema
    fn risk_level(&self) -> Risk;                // Low / Medium / High
    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError>;
}

// 3) 权限引擎（详见第四章）
#[async_trait]
pub trait PermissionEngine: Send + Sync {
    async fn check(&self, call: &ToolCall, task: &TaskSpec) -> Decision;
}
pub enum Decision { Allow, Deny(String), AskUser }

// 4) Agent 执行循环
#[async_trait]
pub trait AgentRunner: Send + Sync {
    async fn run(&self, task: TaskSpec, bus: EventBus, ctl: RunControl) -> Result<Outcome, Error>;
}

// 5) 存储
#[async_trait]
pub trait Store: Send + Sync {
    async fn append_event(&self, sid: SessionId, ev: &StoredEvent) -> Result<(), StoreError>;
    async fn load_session(&self, sid: SessionId) -> Result<Session, StoreError>;
    async fn list_tasks(&self, limit: usize) -> Result<Vec<TaskSummary>, StoreError>;
}

// 6) 前端（TUI 只是其中一种实现）
pub trait Frontend: Send {
    fn commands(&mut self) -> Receiver<Command>;
    fn render(&mut self, ev: &Event);
}

// 7) 同步（一期冻结签名、空实现，二期接入）
#[async_trait]
pub trait SyncEndpoint: Send + Sync {
    async fn push(&self, changed: Vec<StoredEvent>) -> Result<(), SyncError>;
    async fn pull(&self, since: Clock) -> Result<Vec<StoredEvent>, SyncError>;
}
```

### 3.3 异步模型

| 机制 | 选型 | 用途 |
|---|---|---|
| Event 分发 | `broadcast::channel(1024)` | TUI / JSONL 落盘 / tracing 桥接多订阅；TUI 丢帧（lagged）不阻塞 Agent |
| 命令/审批 | `mpsc` + `oneshot` 回执 | 单消费者；权限确认双向流 |
| 取消 | `tokio_util::sync::CancellationToken` | 每任务一个，Cancel 只触发 token 不 abort，保证落盘完整 |
| TUI 主线程 | ratatui 事件循环 + `recv_timeout(250ms)` tick | 渲染永不 `.await` 网络 IO |
| 背压 | TextDelta 每 16ms/32 token 合批 | 防 CPU 飙高与内存膨胀 |

---

## 四、Agent 执行核心（运行时专家方案）

### 4.1 执行循环状态机

```
Idle ──输入──▶ Planning ──tool_calls──▶ 权限裁决 ──Allow──▶ Executing
                 │                                        │
                 ├─final text──▶ Done                观察写回 messages
                 └─错误/超时──▶ Failed                     │
                                                Iterating ──<MAX_ITER──▶ Planning
权限 Deny ─▶ 注入拒绝观察 ─▶ Iterating（模型换策略，非静默失败）
```

**终止与防死循环**：

| 机制 | 规则 |
|---|---|
| 硬上限 | MAX_ITER = 25，超限转 Failed 并附未完成原因 |
| 重复检测 | `(tool, 规范化参数)` 哈希，连续 3 次相同 → 注入"请换策略"提示 |
| 无进展检测 | 连续 4 轮无新产物/成功命令 → 降级为"向用户汇报请求指导" |

### 4.2 上下文三层管理（长任务不膨胀）

1. **全量层**：最近 ~6 轮原始 messages；
2. **摘要层**：更早轮次 LLM 滚动摘要（压缩调用不计入 MAX_ITER）；system 固化「永不丢弃：原始任务、工作目录、产物路径」；
3. **文件外置层**：单工具输出 >8KB 写入 `ctx/`，上下文只留 200 字摘要 + 路径，需要时 `file_read` 取回。

触发：token > 模型窗口 70%。

### 4.3 一期工具清单

| 工具 | 风险 | 安全要点 |
|---|---|---|
| `file_read` / `list_dir` | Low | realpath 校验在工作区内，防 `../` 逃逸 |
| `file_write` / `file_edit` | Medium | 覆写前自动快照到 snapshots/；禁写 /etc、~/.ssh 等系统敏感目录 |
| `shell_exec` | **High** | 参数数组 exec（不经 `sh -c` 拼接）；cwd 锁定工作区；超时强杀 |
| `web_fetch` | Medium | 仅 http/https，禁 file:// 与内网地址（SSRF 防护）；限响应体大小 |
| `web_search` | Low | 可配置后端（SearXNG / API），结果裁剪入上下文 |
| `finish` | — | 显式终止信号，产出物登记 |

**MCP 兼容结论**：一期不实现 MCP（避免单人开发复杂度爆炸），但 `Tool` trait 的 name/description/JSON-Schema 语义与 MCP 对齐，二期一个 `McpToolAdapter` 即可零改动接入。

### 4.4 权限引擎（三档实现）

**双层危险识别**（静态规则先跑、不可绕过）：

- 第一层静态正则：`rm -rf /`、`sudo/su`、`mkfs/dd if=`、`> /dev/sdX`、fork bomb、`curl | sh`、写 /etc /boot /usr /~/.ssh 等；
- 第二层 AI 审视（仅 L2 且静态未命中时）。

**决策流**：

```rust
fn authorize(op) -> Decision {
    if static_rules.match(op) {                    // 命中高危静态规则
        return match tier { L1 => AllowLogged,     // 仍记审计
                            L2|L3 => AskUser }     // 强制人工
    }
    match tier {
        L1 => AllowLogged,
        L3 => AskUser,
        L2 => match ai_review(op).await {
            Ok(r) if r.decision==Allow && r.risk!=High => Allow,
            _ => AskUser,   // AI deny/高危/超时(>8s)/解析失败 → 一律问用户
        }
    }
}
```

> **第一性原则：无沙箱下「误放行」的不可逆代价 > 「多问一次」的骚扰成本。AI 审批任何故障默认降级为 AskUser，绝不默认放行。**

**AI 审视的输入**是精简「操作上下文包」（任务目标/最近动作/当前操作/工作区/静态风险），非全量对话；输出严格 JSON（decision/risk/reason/confidence），复用低价模型执行以控成本。

### 4.5 任务生命周期与目录结构

```
~/agent/
├── workspace/<task-id>/
│   ├── artifacts/     # 交付物（持久化，二期同步）
│   ├── ctx/           # 外置大输出（临时）
│   └── snapshots/     # file_write 前旧版本备份（临时，支持回滚）
├── tasks/<task-id>.json    # 任务元数据
├── logs/<task-id>.jsonl    # 审计日志（append-only，只记摘要+哈希防泄密）
└── config.toml
```

生命周期：`Created → Running ⇄ Paused → Done | Failed | Cancelled`，支持从 messages 快照检查点恢复。**持久化 vs 临时的区分字段一期即写入审计事件**，作为二期「重要性×紧急性」分级同步的输入。

### 4.6 新闻收集 = 通用工具组合（无需专用代码）

`web_search` → 逐条 `web_fetch` → LLM 提取去重分类 → `file_write(artifacts/news-<date>.md)` → `finish`。以预置任务模板（prompt 层）实现。

---

## 五、LLM Provider 抽象层（Provider 专家方案）

### 5.1 统一类型要点

- `Message { role, content: Vec<ContentPart>, tool_calls, tool_call_id, reasoning }` —— **ContentPart 枚举预留多模态**；
- `ToolCall.arguments` 统一为已解析 JSON（OpenAI 返回字符串、Anthropic 返回对象，adapter 抹平）；
- `StreamEvent { Start, TextDelta, ReasoningDelta, ToolCallDelta, Usage, Done, Error }` —— 思考链独立通道，不混入正文；
- `Usage { input, output, reasoning_tokens, cached_tokens }` 各家字段名归一；
- `Capabilities` 声明式能力（tools/vision/reasoning/json_mode/max_context），上层据此降级而非硬编码。

**关键差异抹平**：OpenAI 的 `tool_calls` vs Anthropic 的 `tool_use` 块、独立 tool 消息 vs user 内 `tool_result`、system 位置、流式工具参数增量格式——全部在 adapter 内双向转换。

### 5.2 一期接入策略（最小工作量覆盖最多厂商）

| 优先级 | 厂商 | 方式 |
|---|---|---|
| 1 | **DeepSeek**（默认档） | OpenAI 兼容 adapter；价格低、工具调用稳、国内直连 |
| 2 | **通义千问 DashScope**（代码任务/故障转移） | 同一 OpenAI 兼容 adapter，换 base_url |
| 3 | **Ollama**（本地插件位，兑现 FR-06） | 原生 OpenAI 兼容，复用同一 adapter |
| 可选 | OpenAI / Anthropic | 前者复用兼容 adapter；后者独立 adapter，可延后 |

> **一期只需写 2 个 adapter（OpenAiCompat + Anthropic），覆盖 5 家。**

### 5.3 配置与路由（P1/P2 未定的应对）

```toml
[providers.deepseek]
kind = "openai_compat"
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"

[router]
default = "deepseek"
[router.by_task]
code     = ["deepseek", "qwen", "openai"]   # 有序候选，失败顺延
sensitive = ["ollama"]                       # 隐私数据强制本地（应对 P3）

[budget]
monthly_limit_usd  = 20.0   # P2 未定 → 全部可配置，保守默认
per_task_limit_usd = 0.5
```

**密钥安全**：分层 fallback `环境变量 > keyring(桌面) > 加密配置`；内存中 `secrecy` 包裹防 Debug 泄漏；日志统一脱敏。

### 5.4 成本与故障控制

| 机制 | 设计 |
|---|---|
| 计量 | Usage 逐响应累计；流式缺 usage 用 tiktoken-rs 估算兜底 |
| 单任务熔断 | 超过 per_task_limit 立即中止并提示 |
| 月度熔断 | BudgetGuard 持久化月度用量（跨重启），超限降级到本地模型或拒绝+TUI 告警 |
| 重试 | 指数退避+抖动（backoff crate），仅网络/429/5xx 重试，401/400 不重试 |
| 故障切换 | 路由候选顺延 + 内存熔断器（连续失败标记降级，半开探测恢复） |
| 流保护 | SSE 空闲 60s 超时报错；工具参数增量只在 Done 后整体 parse |

---

## 六、TUI 交互层（TUI 专家方案）

### 6.1 主界面布局

```
┌─────────────────────────────────────────────────────────────┐
│ 个人 Agent   任务:3-收集新闻   权限:L3   模型:deepseek  ●在线 │
├──────────────────────────────────┬──────────────────────────┤
│ 对话流区（虚拟滚动）             │ 侧边任务面板              │
│ ▌用户: 帮我抓今天的科技新闻汇总  │  ▶ 1 写部署脚本  完成     │
│ ◇Agent: 我来帮你…               │    2 分析日志    运行中   │
│   ▸[工具] shell: curl…  ✓0.8s   │  ★ 3 收集新闻    当前     │
│   ▾[工具] write_file →out.md    │                          │
├──────────────────────────────────┴──────────────────────────┤
│ > 输入任务…（Alt+Enter 换行）                                │
└─────────────────────────────────────────────────────────────┘

权限弹层（L3 模态）：
┌────────── 需要确认 ──────────┐
│ ⚠ 危险操作 (shell)           │
│ 命令: rm -rf /tmp/build/*    │
│ [y]批准 [n]拒绝 [e]编辑 [d]详情 [a]本任务此类全批 │
└──────────────────────────────┘
```

### 6.2 关键交互

| 键 | 动作 | 键 | 动作 |
|---|---|---|---|
| Enter | 发送 | Esc | 中断当前任务 |
| Alt+Enter | 换行 | Ctrl+C×2 | 退出 |
| Ctrl+N/P | 切换任务 | PageUp/Down | 滚动 |
| Tab | 折叠/展开工具块 | ↑↓(空输入) | 翻历史 |

拒绝操作后 Agent 将 Deny 结果回传模型让其换策略，**不静默失败**。

### 6.3 渲染架构

- AgentEvent 只更新轻量状态、不触发重绘；重绘由 250ms tick + ratatui diff buffer 驱动；
- 流式 Markdown：缓冲区 + "未闭合段落"策略（空行/代码块闭合才固化 AST，半截语法按纯文本渲染，避免闪断）；
- 长输出虚拟滚动：界面只留可视窗口，原始输出落盘，显示"已省略 N 行，Ctrl+O 查看"。

### 6.4 SSH/服务器降级

COLORTERM/TERM 探测降级 24bit→256→16 色；宽度 <80 自动隐藏侧面板；检测 TMUX 避开前缀键冲突；SIGHUP 落盘会话、重连可恢复。

### 6.5 一期降级建议（控制 R6 风险）

**先把 channel 协议与状态机打磨稳定，Markdown 语法高亮可先纯文本降级、二期增强。**

---

## 七、跨模块数据流（总装配）

```
用户输入(TUI) ──Command::SubmitTask──▶ AgentRunner(tokio::spawn)
                                          │
                    ┌─────────────────────┼──────────────────┐
                    ▼                     ▼                  ▼
              ProviderRouter        PermissionEngine     EventBus(broadcast)
              (chat_stream)         (静态规则+AI审视)      │      │      │
                    │                     ▲               ▼      ▼      ▼
              StreamEvent          ApprovalRequest      TUI   jsonl   tracing
                    │             (oneshot 回执◀─TUI按键)  渲染   落盘   日志
                    ▼
              工具执行(agent-tools, 工作区锁定)
                    │
              ToolOutput →(快照/审计)→ artifacts + 审计事件
```

---

## 八、实施计划建议

| 里程碑 | 内容 | 验收标准 |
|---|---|---|
| **M1 骨架** | workspace 搭建、core trait 与 Event、provider 抽象 + DeepSeek adapter、最简 TUI（纯文本流式对话） | TUI 里和云端模型完成一次流式对话 |
| **M2 能力** | 工具系统（file/shell/web）、执行循环、工作区与快照 | 一次对话内完成"写文件→执行脚本→报告结果" |
| **M3 安全** | 权限引擎三档 + 静态规则 + AI 审视、审计日志、预算熔断 | L3 模式弹层确认；高危命令 L1 也被拦截 |
| **M4 交付** | 任务面板/切换/恢复、Markdown 渲染增强、新闻收集模板、agent-server headless | **跑通三类真实任务（写程序/写脚本/收集新闻）= MVP 验收线** |

---

## 九、风险登记（合并各专家识别）

| # | 风险 | 等级 | 应对 |
|---|---|---|---|
| A1 | 事件总线膨胀为"上帝对象" | 中 | Event 定义由 core 评审把关；模块只经 trait 边界通信 |
| A2 | jsonl 事件格式定型难迁移（影响二期 AI 合并） | 高 | 每行带 schema_version + 逻辑时钟；一期即按"可回滚可合并"设计 |
| A3 | 无沙箱 + 误操作不可逆（需求 R4） | 高 | 双层权限、写前快照、shell 参数化执行、SSRF/路径逃逸防护 |
| A4 | 成本失控（P2 未定） | 中 | 单任务/月度双熔断，阈值全可配，默认保守 |
| A5 | 厂商 API 演进 → 抽象泄漏 | 中 | extra 透传字段 + Capabilities 声明式降级 + 契约测试 |
| A6 | SSE 边界情况（截断/心跳/半截 JSON） | 中 | eventsource-stream + 空闲超时 + Done 后整体 parse |
| A7 | TUI 中文 IME 预编辑不可见 | 低 | 一期接受；二期增强 |
| A8 | 流式高频刷新 CPU 飙高 | 中 | 16ms 合批 + diff 渲染 + 虚拟滚动 |

---

## 十、遗留决策项（进入编码前需用户拍板）

| # | 事项 | 当前方案内默认值 |
|---|---|---|
| P1 | 优先接入厂商 | DeepSeek 默认 + 通义 + Ollama 本地 |
| P2 | 成本预算 | 月 $20 / 单任务 $0.5（可配置，保守默认） |
| P3 | 隐私底线 | `router.by_task.sensitive → ollama` 强制本地路由开关 |
| P4 | 临时信息保留时长 | 7 天默认 |
| — | 工作区根目录 | `~/agent/`（可配置） |

> **P2/P3 仍为需求文档标记的关键空白**。本方案已将两者做成配置项——即使暂不决策也不阻塞开发，但正式拍板前请知悉默认值含义。

---

**文档结束**
