# Zero Subagent（子任务委派）设计方案

> 版本：v0.1（设计稿，未实施）
> 日期：2026-11-25
> 范围：`creator521314/Zero` 一期架构
> 定位：作为 AI Agent 架构专家，基于对当前代码的逐文件感知给出可落地方案，不改动任何现有代码。
> 实施说明：本文是**设计决策记录**；具体落地请以 [`docs/subagent-implementation.md`](./subagent-implementation.md) 为准。该文档核对了真实代码后修正了本文若干伪代码级细节（`deps.with_tools()` 并不存在、事件加字段要改约 19 处构造点、父层 120s 工具超时会掐断 300s 子任务等），差异与原因见其 §3。

---

## 0. 结论先行

**能做，而且这个项目的架构恰好为它留了很干净的挂点**：`AgentRunner` 已经是「给一份 `TaskSpec` 就跑完一个任务」的纯函数式边界，子任务天然就是**另一个 `TaskSpec` + 另一个 `RunControl` + 复用同一条 `EventBus`**。真正需要新增的东西很少，难点全在「接线」和「防失控」，不在「再造一个运行时」。

一期建议只做 **串行委派（delegate）**：父任务调用一个工具，暂停等待，子任务跑完，把**结构化结果摘要**回填父上下文。理由是硬约束而非偷懒——TUI 的审批弹窗是单槽的（`pending: Option<PendingApproval>`），并行子任务同时要确认会互相覆盖；CLI 编排层也明确保留了「同一时刻只跑一个任务」的串行闸门。

一句话概括设计哲学：

> **委派是一个工具，不是第二种运行时；子任务是一个新的 TaskSpec，不是新的 Agent 类型。**

---

## 1. 现状盘点（代码证据）

### 1.1 可以直接复用的挂点

| 挂点 | 位置 | 为什么关键 |
|---|---|---|
| `AgentRunner::run(task, bus, ctl) -> Outcome` | `crates/agent-core/src/task.rs:104-111` | 已经是「输入规格 → 输出结果」的封闭边界。子任务 = 再调一次 `run()`，无需改执行循环 |
| `TaskSpec` 已有 `model` / `max_iterations` / `budget_usd` / `project_root` / `workspace` | `task.rs:26-62` | 子任务的模型选择、迭代上限、预算、共享目录**字段都已存在**，只需加 `parent_id` / `depth` |
| `EventBus = broadcast::Sender<Event>`，容量 1024 | `event.rs:188-194` | 父子共用一条总线即可，事件天然带 `TaskId` 区分来源 |
| `ApprovalInbox`（按 `approval_id` 匹配 + 错配暂存） | `runner.rs:73-140` | 上一轮刚修的错配缓冲，正好是多任务/子任务共用审批通道的前置条件 |
| `ToolLookup` + `tool_defs` 注入 | `runner.rs:144-163` | 子任务可以拿到**裁剪过的工具集**（去掉 `delegate`，防递归） |
| `cost_sink` 回调 → `router.record_spend` | `runner.rs:479-504`、`apps/agent-cli/src/main.rs:363` | 月度账本是全局共享的，子任务花费**天然不会漏记** |
| `redact` / SSRF 守卫 / 路径 realpath 校验 | `runner.rs:876-935`、`agent-net/src/lib.rs`、`agent-tools/src/paths.rs` | 子任务沿用同一套工具与脱敏，安全能力自动继承，零重复实现 |
| `finish` 的 `is_terminating()` | `agent-core/src/tool.rs:114-116` | 子任务同样用 `finish` 收敛，结果已是 `Outcome::Success { summary, artifacts }` |
| `ToolOutput` / `Event::ToolCallEnd.output` | `tool.rs:53-61`、`event.rs:76-78` | 委派结果走标准工具回填路径，不需要新通道 |

### 1.2 必须处理的障碍（都是真实代码事实，不是假想）

1. **TUI 目前按事件渲染、不按 `id` 过滤**。`crates/agent-tui/src/app.rs:349-360` 的 `Event::TextDelta { text, .. }` 直接忽略 `id`，`TaskStarted` 还会把 `active_task` 改掉。子任务一旦发流式事件，会串进父任务的对话区并抢走焦点。**这是必须先修的前置项。**
2. **事件枚举是穷尽匹配，加变体会连锁改 4 处**：`agent-tui/src/app.rs::apply_event`、`agent-store/src/recorder.rs::classify`、`agent-store/src/lib.rs:235` 的回放过滤、`apps/agent-server/src/main.rs:101` 的打印循环。好处是编译器会强制全覆盖；代价是改动面大。
3. **`ToolCtx` 不是全局对象**，由 `execute_tool` 每次构造（`runner.rs:891`），全仓共 11 处构造点。往里加字段要改一堆测试。**方案选择不改 `ToolCtx`**（见 §4.3）。
4. **子任务读不到父任务外置的 `ctx/*.txt`**。`ToolCtx::allowed_roots()`（`tool.rs:36-43`）只允许 `project_root` 和本任务 `workspace`。父任务把大输出外置到父 `workspace/ctx/`，子任务若用独立 workspace 就读不到。**方案选择子任务复用父 workspace**（见 §4.5）。
5. **权限批量批准按 `task_id` 隔离**（`agent-perm/src/lib.rs:49-75` 的 `batch_approved` 键是 `(task_id, tool)`）。子任务不继承父的「本任务同类全批准」——安全上正确（更严格），但要在文档里说清，避免用户困惑。
6. **单任务预算是 `run_inner` 的局部 `spent`**（`runner.rs:419, 494`），子任务独立跑 `run()` 会从 0 重新计。月度账本共享没问题，**但「单任务预算」会被绕过**。需要显式共享。
7. **CLI 的 `Command::SubmitTask` 有串行闸门**（`agent-cli/src/main.rs:299-311`，`current.is_some()` 直接回 `busy`）。**子任务绝不能走 Command 通道**，必须走 runtime 内部直调，否则会被自己的父任务挡住。
8. **事件总线容量 1024，慢订阅会 `Lagged`**（`event.rs:188`）。高频子任务流式增量可能挤掉父事件。建议子任务不广播 `TextDelta`。

### 1.3 依赖铁律的约束

`agent-tools` 当前只依赖 `agent-core` + `agent-net`（`crates/agent-tools/Cargo.toml`）。若把 `delegate` 工具直接调用 `agent-runtime`，就形成 `tools → runtime` 的反向依赖，破坏分层。**必须用依赖倒置**：trait 定义在 `agent-core`，实现在 `agent-runtime`，装配层注入。

---

## 2. 设计目标与非目标

### 目标

- **上下文隔离**：子任务只拿到明确给出的目标 + 可选背景，不继承父的完整历史。父任务只收到结果摘要。这是 subagent 的核心价值——把「查资料 / 跑测试 / 读一堆文件」这类高 token 的脏活切出去，不污染主上下文。
- **共享同一片空间**：父子在同一 `project_root` 上工作，子任务写出的文件父任务立刻可见，无需「传递文件」机制。（直接回应「多个任务能否共享空间」）
- **模型可分层**：父用均衡档，子任务可指定更便宜/更专的模型（`TaskSpec.model` 已支持，`router.resolve(None, model)` 已实现）。
- **权限不放大**：子任务档位 ≤ 父档位，静态高危规则照跑，审批走同一通道。
- **成本可控、可审计**：父子花费进同一月度账本，子任务有独立上限；委派关系进 jsonl，可回放。

### 非目标（一期明确不做）

- 多 Agent 协商 / 投票 / 自组织团队
- 自动任务拆分（是否委派、委派什么，由模型显式调用 `delegate` 决定）
- 跨进程 / 跨设备子任务（那是二期同步 + 三期手机节点的事）
- 并行委派（P2 再议，见 §6 风险）
- 让子任务再开子任务（深度硬限 1）

---

## 3. 核心抽象（新增，全部放在 `agent-core`）

```rust
// crates/agent-core/src/subagent.rs（新文件）

/// 委派请求：模型通过 `delegate` 工具的参数构造。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtaskSpec {
    /// 子任务目标（一句话，必填）。模型必须写清「要什么结果」。
    pub goal: String,
    /// 可选背景材料。不传 = 完全隔离；父模型需自己把必要信息摘出来。
    #[serde(default)]
    pub context: Option<String>,
    /// 期望的交付物形态提示，例如 "一份对比表" / "修改后的文件路径列表"。
    #[serde(default)]
    pub expected_output: Option<String>,
    /// 模型覆盖：允许给子任务指定便宜模型 / 专用模型。
    #[serde(default)]
    pub model: Option<String>,
    /// 迭代上限（钳制到 1..=12，防模型乱填）。
    #[serde(default)]
    pub max_iterations: Option<usize>,
    /// 子任务预算上限（USD），None = 由编排层按父剩余额度分配。
    #[serde(default)]
    pub budget_usd: Option<f64>,
}

/// 委派结果：只回传摘要，绝不回传子任务全量对话。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtaskResult {
    pub child_id: TaskId,
    pub outcome: Outcome,          // 复用现有 Outcome::Success/Failed/Cancelled
    /// 给父模型看的最终文本（已是脱敏后的内容）。
    pub summary: String,
    /// 子任务登记的交付物路径（相对 project_root）。
    #[serde(default)]
    pub artifacts: Vec<String>,
    /// 让父任务知道花了多少，便于决定是否继续委派。
    pub cost_usd: f64,
    pub iterations: u32,
}

/// 依赖倒置端口：实现在 agent-runtime，装配层注入给工具。
///
/// 放在 agent-core 是为了守住依赖铁律：agent-tools 不需要知道 runtime 存在。
#[async_trait]
pub trait SubagentPort: Send + Sync {
    async fn run_subtask(
        &self,
        parent: &TaskSpec,
        spec: SubtaskSpec,
        cancel: CancellationToken,
    ) -> Result<SubtaskResult, AgentError>;
}
```

`TaskSpec` 追加两个**向后兼容**字段（旧 jsonl 必须仍能反序列化，项目已有 `#[serde(default)]` 的先例，如 `event.rs:222-246` 的 legacy 测试）：

```rust
/// 父任务 id；None = 顶层任务。用于事件关联、TUI 折叠、审计回放。
#[serde(default)]
pub parent_id: Option<TaskId>,
/// 委派深度：顶层 0，子任务 1。硬上限 1（一期）。
#[serde(default)]
pub depth: u8,
```

事件侧**不新增枚举变体**，改为给 `TaskStarted` / `TaskFinished` 加一个可选字段，把改动面从 4 处穷尽匹配压到 0 处：

```rust
TaskStarted { id: TaskId, title: String,
              #[serde(default)] parent_id: Option<TaskId> },
TaskFinished { id: TaskId, outcome: Outcome,
               #[serde(default)] parent_id: Option<TaskId> },
```

> 取舍说明：新增 `SubagentStarted/Finished` 语义更清晰，但要同步改 TUI、recorder、store 回放、server 四处穷尽匹配。复用现有事件 + `parent_id` 更贴合本项目「务实、少改动、向后兼容」的风格，且 `StoredEvent` 已带 `schema_version` / `Clock` / `durability`，父子关系天然可同步。

---

## 4. 接线与控制流

### 4.1 依赖倒置接线图

```
agent-core      SubagentPort (trait) / SubtaskSpec / SubtaskResult / TaskSpec.parent_id
    ▲                    ▲                          ▲
    │ implements         │ holds Arc<dyn ...>       │ 注入
agent-runtime       agent-tools::DelegateTool   apps/*::main（装配层）

装配层唯一需要「同时认识 runtime 和 tools」的地方：
  let port: Arc<dyn SubagentPort> = Arc::new(RuntimeSubagent::new(deps.clone(), bus.clone()));
  registry.register_delegate(port);   // 新增注册函数，不改 default_tools 签名
```

`default_tools(search_backends)` 现有 12 处调用点不动，新增 `ToolRegistry::register_delegate(port)` 单独注入。这样 `harness()` 等测试装配可以不注入委派工具（保持现有冒烟测试全绿），委派测试单独构造带 port 的注册表。

### 4.2 `RuntimeSubagent::run_subtask` 的实现骨架

```rust
async fn run_subtask(&self, parent: &TaskSpec, spec: SubtaskSpec,
                     parent_cancel: CancellationToken) -> Result<SubtaskResult, AgentError> {
    // 1) 防失控：深度、长度、数量
    if parent.depth >= MAX_DEPTH { return Err(...); }
    if self.live_children.load() >= MAX_CHILDREN { return Err(...); }
    let goal = spec.goal.trim();
    if goal.is_empty() || goal.len() > 2000 { return Err(InvalidArgs); }

    // 2) 构造子 TaskSpec —— 关键：共享 project_root 与 workspace
    let child_id = TaskId::new();
    let mut child = TaskSpec::new(goal, /* workspace = */ parent.workspace.clone());
    child.id = child_id.clone();
    child.parent_id = Some(parent.id.clone());
    child.depth = parent.depth + 1;
    child.project_root = parent.project_root.clone();       // 同一片空间
    child.tier = parent.tier;                              // 权限不放大
    child.model = spec.model.or_else(|| parent.model.clone());
    child.max_iterations = spec.max_iterations.unwrap_or(8).clamp(1, 12);
    child.budget_usd = Some(self.grant_budget(parent, spec.budget_usd));
    // 不设 resume_from：子任务永远是新上下文

    // 3) 裁剪工具集：去掉 delegate（递归防护的第二道闸）
    let child_deps = self.deps.with_tools(
        self.deps.tool_defs.iter().filter(|d| d.name != "delegate").cloned().collect(),
        /* lookup 同样过滤 */ filtered_lookup,
    );
    // 共享：store / perm / approvals / cost_sink / redact / log_cfg
    // 独立：ContextManager（run_inner 内部新建）、spent（由共享 BudgetCell 接管）

    // 4) 取消级联 + 整体硬超时
    let child_cancel = parent_cancel.child_token();
    let ctl = RunControl { cancel: child_cancel.clone() };
    let outcome = tokio::time::timeout(
        Duration::from_secs(SUBTASK_WALL_TIMEOUT),
        AgentRuntime::new(child_deps).run(child, self.bus.clone(), ctl),
    ).await;

    // 5) 结果收敛：只取 summary + artifacts，脱敏后回填
    ...
}
```

### 4.3 为什么不动 `ToolCtx`

`DelegateTool` 在**构造时**持有 `Arc<dyn SubagentPort>`，`execute()` 里只用它发请求；父 `TaskSpec` 通过 `ToolCtx` 已有的 `task_id` 反查即可（或由 port 自己从内部映射取）。这样：

- `ToolCtx` 12 处构造点零改动；
- `agent-tools` 依旧只依赖 `agent-core`；
- 单元测试可以注入一个 `FakeSubagentPort`，不需要起真实 runtime。

### 4.4 串行委派时序（一期）

```
父 Runner 第 N 轮
  └─ assistant(tool_calls: [delegate{goal:"...", model:"cheap"}])
       ├─ 权限裁决：delegate 定为 Risk::Medium（L3 需确认，L2 走 AI 审视）
       ├─ Event::ToolCallStart ──► TUI 显示「委派：...」
       ├─ DelegateTool::execute
       │    └─ port.run_subtask(parent, spec, cancel)
       │         ├─ Event::TaskStarted{ id: child, parent_id: parent }
       │         │    └─ TUI：按 parent_id 折叠成子树，不改 active_task
       │         ├─ 子 Runner 独立循环（不发 TextDelta，只发 AssistantTurn
       │         │    / ToolCallStart/End / ContextSnapshot，全部带 child id）
       │         ├─ finish → Outcome::Success{summary, artifacts}
       │         └─ Event::TaskFinished{ id: child, parent_id: parent }
       ├─ 结果走标准工具回填：ToolCallEnd.output + Role::Tool 消息
       └─ 父 Runner 第 N+1 轮：看到摘要，继续主任务
```

父任务**阻塞等待**（`await`），期间不发自己的模型请求，因此：
- 审批单槽不会被并发占用；
- CLI 的 `current` 串行闸门完全不感知子任务（不走 Command 通道）；
- 父子不会同时写同一个文件（一期最重要的冲突规避）。

### 4.5 空间模型：共享同一片空间（重点）

直接回应「能否让多个任务共享同一片空间」——**在 Zero 里这应该是默认，而不是可选项**。

| 区域 | 父子关系 | 依据 |
|---|---|---|
| `project_root`（项目根/交付物） | **完全相同** | `ToolCtx::sandbox_root()` 以它优先，子任务写的文件父任务立刻可读，`finish` 的 artifacts 用 `resolve_in_ctx` 解析到同一基准 |
| `workspace`（私有系统数据：`snapshots/`、`ctx/`） | **一期直接复用父的同一目录** | 见下方分析 |

为什么 `workspace` 也复用而不是给子任务单独开目录：

- 父任务把超大工具输出外置到 `<workspace>/ctx/<uuid>.txt`（`runner.rs:923-931`，文件名已是 UUID）；
- `allowed_roots()` 只允许 `project_root` + 本任务 `workspace`；子任务若用独立目录，**读不到父的外置文件**；
- 上一轮刚修好的快照命名是 `<相对路径>.<纳秒>.<序号>`（`fs_tools.rs:264-306`），加上 `ctx/` 的 UUID 文件名，**并发写也不会互撞**——现有代码其实已经为共享 workspace 做好了准备，只是当时没意识到。

也就是说：**子任务 `TaskSpec.workspace = parent.workspace.clone()` 一行就实现了「多任务共享同一片空间」，零额外代码**。

对比 WorkBuddy 的两层模型（`/workspace` 项目级共享 + 会话私有 scratchpad）：Zero 是**同一进程内**的委派，没有必要引入跨会话沙箱隔离，共享成本为零；隔离真正需要的是**上下文**，不是文件系统。这是本方案最重要的判断。

**残余风险（一期接受，P2 处理）**：父子共用 `snapshots/` 会让 `file_restore` 的快照列表混在一起（按 label 匹配，会看到对方的历史版本）。因一期串行且子任务以只读/生成新文件为主，冲突概率低；P2 若做并行，改为 `<workspace>/subagents/<child_id>/` + 在 `ToolCtx` 增加显式 `shared_roots`。

### 4.6 上下文与结果回传（省 token 的关键）

- **下行**：子任务 system prompt 由 `AgentRuntime::system_prompt(&child)` 生成，只含 `goal` + 工作目录 + 规则；`spec.context` 作为一条 `Message::user` 追加。**不复制父的历史消息、不复制父的 `resume_messages`**。
- **上行**：只回传 `SubtaskResult.summary` + `artifacts` + `cost_usd` + `iterations`。子任务的完整对话留在它自己的 `ContextSnapshot` / `AssistantTurn` 里（jsonl 按 `child_id` 独立成 session），父上下文里永远只有一条工具结果。
- **超长结果**：复用父 `ContextManager::should_externalize` → 外置到 `ctx/<uuid>.txt`，父只拿 500 字摘要 + 路径。
- **脱敏**：结果在回填前过同一个 `redact`（密钥精确值 + 形态识别两层），不外泄到父上下文与日志。

### 4.7 事件与 TUI（P0 前置，必须先修）

1. `Event::TaskStarted/TaskFinished` 加 `#[serde(default)] parent_id`（旧日志兼容，补 legacy 反序列化测试）。
2. **TUI `apply_event` 增加按 `id` 过滤**：只有 `id == active_task` 且 `parent_id.is_none()` 的流式事件才进主对话区；带 `parent_id` 的事件进「子任务面板」。
3. `TaskStarted`：仅当 `parent_id.is_none()` 时更新 `active_task` / `viewing`；带 `parent_id` 时往对话区插一条可折叠的 `Line::System("↳ 子任务: …")`。
4. `replay_lines` / `TaskOpened` 的历史回放同样按 `parent_id` 折叠，保证「切过去的对话」和实时一致。
5. `agent-store::recorder::classify`：子任务事件按 `child_id` 落盘为独立 session，`durability` 沿用现有规则（`TaskStarted/Finished/Error` = Persistent，过程事件 = Transient）。**二期同步时父子关系天然带 `parent_id`，可还原任务树**。
6. 子任务**不广播 `TextDelta` / `ReasoningDelta`**：避免挤占 1024 容量并把正文串进主对话；保留 `AssistantTurn` 即可完整复盘。

### 4.8 权限继承

| 项 | 策略 |
|---|---|
| 权限档位 | `child.tier = parent.tier`，**不允许升级**；L1 父任务不会因子任务变成无人值守 |
| 静态高危规则 | 照常运行（`perm.check(call, &child_task)`），高危命令仍强制询问 |
| 审批通道 | 共用 `Arc<ApprovalInbox>`；`AwaitingApproval` 事件带 `child_id`，TUI 弹窗需显示「这是子任务的请求」 |
| 批量批准 | **不继承**（`batch_approved` 键含 `task_id`）——更严格，安全上正确；文档需向用户说明 |
| `delegate` 调用本身 | 定为 `Risk::Medium` 并加入 `static_risk`；建议在静态规则里对「一次委派 3 个以上子任务」「goal 里含高危命令字符串」做拦截 |
| 工具集 | 子任务裁剪掉 `delegate`（递归第二道闸），其余 9 个工具全部保留，安全守卫自动继承 |

### 4.9 预算与账本

现状：`spent` 是 `run_inner` 局部变量、`per_task_budget` 在 `RuntimeDeps`，月度账本靠 `cost_sink → router.record_spend`（全局共享，子任务不会漏记）。

新增一个**任务树共享预算单元**：

```rust
/// 用 AtomicU64 存「微美元」，父子共享同一个实例。
#[derive(Clone, Default)]
pub struct BudgetCell(Arc<AtomicU64>);

impl BudgetCell {
    fn remaining_usd(&self, cap: f64) -> f64;
    fn charge(&self, usd: f64) -> bool;   // 超预算返回 false → 子任务提前收尾
}
```

- 父任务创建 `BudgetCell`，注入 `RuntimeDeps.budget_cell`；
- 子任务 `run_subtask` 共享同一实例，`charge()` 由每轮成本回调触发；
- 子任务独立上限 = `min(spec.budget_usd, 父剩余额度)`；
- `per_task_budget` 校验改为读 `BudgetCell`，从而**父子合计不超父的单任务预算**，同时月度熔断继续由全局账本兜底。

### 4.10 防失控清单（硬限制，不可由模型覆盖）

| 闸 | 值 | 位置 |
|---|---|---|
| 最大深度 | 1（子任务不可再委派） | `parent.depth` 检查 + 子工具集裁剪，双保险 |
| 单父并发子任务 | 1（一期串行） | `live_children` 原子计数 |
| 单任务树累计子任务数 | 8 | 父 `run_inner` 计数，超限回填明确错误 |
| 子任务迭代上限 | `clamp(1, 12)` | 覆盖模型传值 |
| 子任务整体墙钟超时 | 300s | `tokio::time::timeout` 包住 `run()` |
| goal 长度 | ≤ 2000 字符 | 防把整份文件塞进委派参数 |
| 取消级联 | `parent_cancel.child_token()` | 父取消 → 子立即停；子取消不影响父 |
| 重复委派检测 | 复用现有 `hash_call` 的 3 次重复拦截 | `runner.rs:589-605` |

---

## 5. 分期实施计划

### P0：前置兼容（1 个 commit，行为零变化）

**改动**：`agent-core/src/event.rs`（加 `parent_id` + legacy 测试）、`agent-core/src/task.rs`（加 `parent_id`/`depth`）、`agent-core/src/subagent.rs`（新类型与 trait）、`agent-tui/src/app.rs`（按 id 过滤 + 折叠）、`agent-store` 回放过滤、`agent-server` 打印循环。

**验收**：
- `cargo test --workspace` 全绿（当前基线 389 通过）；
- 新增测试：旧 jsonl 里没有 `parent_id` 的 `task_started` 仍能反序列化；
- TUI 单测：带 `parent_id` 的 `TaskStarted` 不改变 `active_task`。

### P1：串行委派 MVP（3~4 个 commit，每 commit 可独立 review）

| commit | 内容 |
|---|---|
| 1 | `agent-runtime`：新增 `RuntimeSubagent`（`SubagentPort` 实现）、`BudgetCell`、子工具集裁剪、取消级联、墙钟超时 |
| 2 | `agent-tools`：`DelegateTool` + `ToolRegistry::register_delegate`；`agent-perm/src/rules.rs` 注册 `delegate` 风险等级与静态规则 |
| 3 | 装配层（`agent-cli` / `agent-server`）：构造 port 注入注册表，`tool_defs` 随之包含 delegate |
| 4 | 冒烟与集成测试 |

**测试清单（P1 必须全绿）**：
- 委派结果回填：父上下文里出现 `SubtaskResult.summary`，且**不含子任务的中间消息**（上下文隔离断言）；
- 共享空间：子任务写入 `project_root/artifacts/x.md` 后，父任务 `file_read` 能读到；
- 外置文件可读：父任务外置的 `ctx/*.txt`，子任务能读（验证 workspace 复用）；
- 深度限制：`depth>=1` 时委派被拒；子任务工具集里没有 `delegate`；
- 预算：`BudgetCell` 父子合计超限时子任务提前收尾，月度账本金额 = 父子之和；
- 取消：父 `cancel()` 后子任务 `Outcome::Cancelled`；
- 超时：子任务挂起时 300s 内返回明确错误而非无限等待；
- 权限：L3 下子任务的工具请求弹审批；L1 下高危命令仍被静态规则拦；
- 脱敏：子任务输出里的 `sk-` 形态密钥回填前被替换为 `<redacted>`；
- 冒烟：`EXPECTED_TOOLS` 从 9 → 10，契约样例覆盖 `delegate`。

### P2：体验与并行（可选，二期前完成）

- TUI 子任务面板：树形折叠、展开看子任务完整转写、显示各自 cost；
- 并行委派 `delegate_many`：需要先把 TUI 审批从单槽改成队列（这是并行的真正前置条件）；
- 文件写冲突：引入 `<workspace>/subagents/<child_id>/` 隔离 + 写路径分区 / 文件锁；
- 子任务模板：预置「调研」「测试」「代码审查」三类 goal 提示，降低模型乱写委派参数的概率。

### P3：与二期同步衔接

`StoredEvent` 已带 `schema_version` / `Clock` / `durability`；`parent_id` 随事件落盘后，二期「手机权威节点 + AI 辅助合并」可以直接按任务树还原，手机端可以把一个子任务当作哑终端节点执行。**本方案不改动任何同步接口**。

---

## 6. 风险与缓解

| 风险 | 严重度 | 缓解 |
|---|---|---|
| TUI 事件串台（子任务文本污染主对话） | 高 | P0 先修按 `id` 过滤，未修完不进 P1 |
| 递归委派 / 扇出失控烧钱 | 高 | depth=1 + 工具裁剪双闸 + `BudgetCell` + 累计上限 8 |
| 父子同时写同一文件 | 中 | 一期串行；P2 上路径分区/文件锁；`file_restore` 快照已唯一命名不会误覆盖 |
| 审批单槽被并发占用 | 中 | 一期串行规避；并行前先改审批队列 |
| 子任务读不到父的外置上下文 | 中 | 复用父 workspace（§4.5），已由 UUID 文件名 + 唯一快照名保证不撞 |
| 事件总线 1024 容量被流式增量挤爆 | 低 | 子任务不广播 `TextDelta`，只留整轮 `AssistantTurn` |
| 模型把整份文件塞进 `goal` | 低 | 长度钳制 2000 字符 + 超长明确报错 |
| 快照列表父子混杂导致误恢复 | 低 | 一期接受；文档说明；P2 隔离目录 |
| 月度预算被绕过 | 中 | `cost_sink` 全局共享（现状已满足）+ `BudgetCell` 管单任务 |

---

## 7. 关键决策记录

1. **不新增 `Subagent` 运行时类型**：`AgentRunner::run` 已经是最好的抽象，再包一层只会制造两套执行循环。
2. **委派做成工具，不做成 Command**：CLI 的 `SubmitTask` 有串行闸门，走它会被父任务自己挡住；且工具形态让模型能自然决定何时分身，并复用权限裁决、审计、回填全链路。
3. **不新增事件枚举变体**：`parent_id` 可选字段把改动面从 4 处穷尽匹配压到 0，且与项目已有的 legacy jsonl 兼容风格一致。
4. **不改 `ToolCtx`**：改用「工具构造时注入 port」，避免 12 处构造点改动，也守住 `agent-tools → agent-core` 的单向依赖。
5. **共享 workspace 而非隔离**：这是「多任务共享同一片空间」的最简实现，且现有 UUID 外置 + 唯一快照命名已经把冲突风险处理掉了。隔离的是上下文，不是磁盘。
6. **一期只做串行**：不是能力不够，是 TUI 审批单槽 + 文件写冲突两个硬约束；串行已能拿到 subagent 八成收益（上下文隔离 + 模型分层 + 并行开发体验）。

---

## 8. 落地前置条件

- P0/P1 不引入任何新外部依赖（`async_trait`、`tokio`、`serde` 均已在工作区）；
- 全程遵守 `cargo fmt` + `cargo clippy --workspace --all-targets` 零警告 + `cargo test --workspace` 全绿；
- 每个 commit 保持可独立 review，不混无关改动；
- 建议先在 `ScriptedProvider`（`runner.rs` 测试模块已有）上跑通委派闭环，再接真实模型（真实模型用例继续放 `--ignored`，不消耗你的 token）。
