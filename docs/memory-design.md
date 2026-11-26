# Zero 记忆系统 · 设计文档

> 状态：**v1.2 上下文协议修订**（2026-11-27）。v1.0 实施后在真实使用中暴露「只写不读」等缺陷，见 `docs/memory-issues.md`；本文 §4 已按修复结果回写。v1.2 按 `docs/context-audit.md` 把记忆注入从 system 固定区移到首轮 user，并改用增量上下文快照协议，§4.2 / §5.1 已同步。
> 上游：`docs/memory-requirements.md` v1.0（已裁决）。本文只做设计展开，**不是逐行实施说明书**；若需要低能力模型照做，另出实施说明书。
> 本文所有「已裁决」条目均来自需求澄清文档 §10，不再重新讨论取舍。

---

## 0. 设计定位与硬约束

| 约束 | 来源 | 本文怎么守 |
|---|---|---|
| 依赖铁律：实现 crate 只依赖 `agent-core` | 项目架构 | 全部边界类型与 trait 放 `agent-core`；实现放 `agent-store`；`agent-tools` / `agent-tui` 只依赖 core |
| 不改 `Tool::execute` 签名、不给 `ToolCtx` 加字段 | `docs/interface-hardening.md` §9 | 记忆工具像 `DelegateTool` 一样**构造时注入 port** |
| 不给 `RuntimeDeps` 加字段（14 个构造点） | `docs/subagent-implementation.md` | 注入内容随 `TaskSpec` 走；提炼走事件总线旁路 worker，不进执行循环 |
| 子任务完全隔离 | 需求 §10 | `tool_allowlist` 同时排除 `delegate` 与记忆类工具 |
| Event 仍是单枚举 | `docs/interface-hardening.md` §9 | 本期**尽量不新增 Event/Command 变体**；记忆写操作本身即审计记录（append-only jsonl） |
| 密钥与凭据绝不入库 | 需求 O1 | 准入管道第一关；纯函数扫描，零新依赖 |
| 每条写入限长 + 来源/时间戳 | 需求 O1 兜底 | `MemoryEntry` 强制字段，写入端无法绕过 |

**一句话架构**：记忆是**装配层编排的旁路能力**——写由 TUI/提炼 worker 发起，读由「任务提交时算好快照塞进 `TaskSpec`」和「按需检索工具」两路提供，执行循环本身只多读一个字段。

---

## 1. 目标 / 非目标

**目标**：跨任务保留用户偏好、历史结论、环境事实；支持从经历中产出代码改进建议（`.patch`）；可写入、可注入、可检索、可管理、可追溯。

**非目标（一期）**：真实 embedding / 向量库、跨设备同步、子任务记忆继承、自动合并或推送代码、自动跑测试、`Event` 三域拆分。

**已知限制（评审确认接受）**：PII、源码原文、工具大输出**允许**入库；冲突记忆会同时注入；注入量随 `max_context` 浮动。TUI 需明示这三点。

---

## 2. 架构总览

```
                        ┌──────────────────── agent-core（零 IO）────────────────────┐
                        │ MemoryEntry / MemoryScope / MemoryState / MemorySource     │
                        │ MemoryIndex trait      MemoryStore trait      MemoryContext │
                        │ scan_secret() 纯函数  MemoryConfig                       │
                        └───────────────▲──────────────────▲──────────────────────┘
                                        │                  │
        ┌───────────────────────────────┴───┐   ┌──────────┴──────────────────────────┐
        │ agent-store（实现）                │   │ 装配层（apps/*）                      │
        │ JsonlMemoryStore：两层 jsonl       │◄──┤ compose() 组装注入快照               │
        │ KeywordIndex：关键词/标签检索       │   │ register(task_id, injected_ids)      │
        │ 去重 / 版本 / LRU / 压缩           │   │ 提炼 worker 订阅 EventBus（旁路）    │
        └───────────────▲───────────────────┘   └──────────┬──────────────────────────┘
                        │                                   │
     ┌──────────────────┴──────────┐   ┌───────────────────┴──────────────────────┐
     │ agent-tui                    │   │ agent-runtime                            │
     │ /remember、/memory 面板、     │   │ ContextManager::new(pinned + memory_text)│
     │ /improve 提示、候选批量确认    │   │ TaskSpec.memory_context（serde default）│
     │ 通过 MemoryPort（core trait） │   │ 子任务 allowlist 排除记忆工具            │
     └──────────────────────────────┘   └──────────────────────────────────────────┘
                        ▲                                   ▲
                        │                                   │
                ┌───────┴───────────────────────────────────┴───────┐
                │ agent-tools：memory_search（构造时注入 MemoryIndex）│
                │ 被 tool_allowlist 管住可见性与可执行性              │
                └────────────────────────────────────────────────────┘
```

### 2.1 crate 归属表

| crate | 新增内容 | 禁止事项 |
|---|---|---|
| `agent-core` | `src/memory.rs`：条目/作用域/状态/来源、`MemoryIndex` / `MemoryStore` / `MemoryPort` trait、`scan_secret()`、`MemoryConfig`；`lib.rs` 补 `pub use` | 不放文件 IO、不引 regex 等新依赖 |
| `agent-store` | `src/memory_store.rs`：jsonl append-only、加载回放、去重与版本、LRU、压缩；`KeywordIndex` | 不反向依赖 runtime/tools/tui |
| `agent-runtime` | `TaskSpec.memory_context`（在 core）、`ContextManager` 拼接、子任务 allowlist 排除记忆工具 | 不加 `RuntimeDeps` 字段、不改执行循环状态机 |
| `agent-tools` | `memory_search` 工具；`default_tools()` / `EXPECTED_TOOLS` 同步 | 不依赖 `agent-runtime` |
| `agent-tui` | `/remember`、`/memory` 面板 + 候选角标、`/improve` 提示 | 不直接读 jsonl 文件，走 `MemoryPort` |
| `apps/*` | 组装 store/port、任务提交时 `compose()` 并登记注入 id、启动提炼 worker | 不做业务逻辑 |

---

## 3. 数据模型（`agent-core/src/memory.rs`，设计级签名）

```rust
pub struct MemoryId(pub String);                 // uuid，与 TaskId 风格一致

pub enum MemoryScope {
    User,
    Project { root: String },                   // 归一化后的 project_root 绝对路径
}

pub enum MemoryState { Candidate, Active, Archived }

pub enum MemorySource {
    Explicit { task_id: String, at: i64 },      // /remember
    Distilled { task_id: String, at: i64 },    // 收尾自动提炼（待确认）
    Imported { task_id: Option<String>, at: i64 },
}

pub struct MemoryEntry {
    pub id: MemoryId,
    pub scope: MemoryScope,
    /// 可选稳定键。同 scope + 同 key 视为同一条 → 新覆盖旧并留历史版本。
    pub key: Option<String>,
    pub content: String,                        // 限长，见 MemoryConfig
    pub tags: Vec<String>,
    pub state: MemoryState,
    pub source: MemorySource,                  // O1 兜底：来源/时间戳强制
    pub version: u32,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_used_at: i64,                      // 注入命中 + 检索命中共同推进（R4）
}

/// 任务提交时算好的注入快照：进 TaskSpec，随事件/日志可复现。
pub struct MemoryContext {
    pub text: String,                          // 已格式化、已截断、带来源标注
    pub injected_ids: Vec<MemoryId>,
    pub truncated: usize,                       // 被动态截断丢弃的条数，供 TUI 提示
}

pub struct MemoryConfig {
    pub max_entry_chars: usize,                 // O1 兜底：默认 500
    pub inject_ratio: f64,                     // O3：占 max_context 的比例，默认 0.03
    pub user_active_limit: usize,              // 默认 200
    pub project_active_limit: usize,           // 默认 500
    pub candidate_limit: usize,                // 默认 100
    pub compact_over_records: usize,           // jsonl 压缩阈值，默认 2000
}
```

### 3.1 边界 trait

```rust
/// 持久化 + 管理（实现：agent-store）。零 IO 定义放 core。
#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn upsert(&self, e: MemoryEntry) -> Result<(), MemoryError>;
    async fn get(&self, id: &MemoryId) -> Result<Option<MemoryEntry>, MemoryError>;
    async fn list(&self, q: MemoryQuery) -> Result<Vec<MemoryEntry>, MemoryError>;
    async fn delete(&self, id: &MemoryId) -> Result<(), MemoryError>;   // 逻辑删除 → Archived
    async fn touch(&self, ids: &[MemoryId], at: i64) -> Result<(), MemoryError>;
}

/// 检索（实现：一期 KeywordIndex，二期可换向量实现）。
pub trait MemoryIndex: Send + Sync {
    fn search(&self, q: MemoryQuery) -> Vec<MemoryEntry>;
}

/// TUI 侧门面（组合 store + index + 确认动作），实现放 agent-store。
#[async_trait]
pub trait MemoryPort: Send + Sync {
    async fn remember(&self, scope: MemoryScope, content: String, tags: Vec<String>)
        -> Result<MemoryId, MemoryError>;
    async fn confirm(&self, ids: &[MemoryId]) -> Result<usize, MemoryError>;
    async fn discard(&self, ids: &[MemoryId]) -> Result<usize, MemoryError>;
    async fn panel(&self, q: MemoryQuery) -> Result<Vec<MemoryEntry>, MemoryError>;
    async fn compose(&self, scope: &MemoryScope, max_context: u32)
        -> Result<MemoryContext, MemoryError>;
}
```

> `MemoryPort::compose` 同时被装配层（算快照）与 `memory_search`（排除已注入 id）使用；`MemoryError` 复用 `AgentError` 体系或作为 core 自有枚举，实施阶段定。

### 3.2 存储格式（O4 裁决）

| 作用域 | 路径 | git 策略 |
|---|---|---|
| 用户级 | `~/.config/zero/memory.jsonl` | 不入仓库 |
| 项目级 | `<project_root>/.zero/memory.jsonl` | `.zero/` 加入 `.gitignore` |

append-only 操作日志，每行一个 `MemoryRecord`：

```json
{"op":"upsert","entry":{"id":"...","scope":{"Project":{"root":"/x"}},"state":"Active", ...}}
{"op":"touch","id":"...","at":1732}
{"op":"delete","id":"...","at":1732}
{"op":"state","id":"...","state":"Active","at":1732}
```

- 启动时**回放**成内存索引；无 schema 版本字段时按 v1 兼容（全部新字段 `#[serde(default)]`）。
- 行数超过 `compact_over_records` 时写一个只含最新状态的快照文件并原子替换；压缩期间写入排队。
- 逻辑删除即 `state=Archived`，`list` 默认不含；`get` 仍可查（可追溯，O1 兜底）。

---

## 4. 写入流程（双通道 + 准入管道）

```
/remember 显式 ─┐
                ├─► 准入管道 ─► state=Active ─► upsert
收尾自动提炼 ───┘（v1.1：通过准入管道的产出**直接 Active**，见 §4.4）
```

### 4.1 准入管道（顺序不可调换）

1. **限长**：`content.chars().count() <= max_entry_chars`，超出直接拒绝（O1 兜底）。
2. **密钥扫描**：`scan_secret(&content)` 命中即拒绝，**错误文案不回显原文**。
   - 纯字符串匹配：常见凭据前缀（`ghp_`/`xoxb-`/`sk-`/`AKIA`/`-----BEGIN ... PRIVATE KEY-----`）与 `http://user:pass@` 形态；零新依赖。
   - 参考教训：测试里写假样例时用 `EXAMPLE_NOT_A_REAL_KEY_000`，避免 Push Protection 拒收（见 `docs/interface-hardening.md` §8.3）。
3. **元信息黑名单（v1.1，P1-3）**：关于记忆系统自身的条目不入库——tags 命中黑名单
   （记忆机制/记忆存储/记忆系统/记忆库/长期记忆/记忆功能）或 content 命中关键词
   （记忆系统/记忆库/记忆机制/memory.jsonl/长期记忆//memory）即拒绝。
4. **来源与时间戳**：由写入端强制填充，调用方不能伪造（`MemoryEntry` 构造器统一生成）。
5. **去重 / 冲突**：同 `scope` + 同 `key` 且状态为 `Active | Candidate` → `version+1`、旧条目
   `Archived` 留版本（v1.1：候选之间也去重，P0-2）；无 `key` 时用归一化 content 哈希兜底。
6. **落盘**：`upsert` 追加记录并更新内存索引；失败不影响任务结束（O5）。

### 4.2 自动提炼（O5）

- **触发点**：装配层的**提炼 worker 订阅 `EventBus`**，只在 `TaskFinished` 且 `Outcome::Success` / `Failed` 时触发；取消/暂停不触发。
- **输入**：由该任务的**增量** `ContextSnapshot`（`added` / `reset`）序列重建的完整上下文（见 `JsonlStore::resume_context()`）+ outcome 摘要，**外加已有生效记忆清单**（v1.1：供模型避免重复、复用同主题 key）；**不读全量会话日志**，控制成本。
- **裁剪（v1.2）**：跳过固定 system、按角色分配配额（user 最长）、超 6000 字符时**保留尾部**并标注省略条数——固定模板与早期工具输出不再把最后一轮追问与结论挤掉。
- **跳过子任务（v1.2）**：worker 依据 `Event::SubtaskLinked` 记录 child id，子任务的 `TaskFinished` 直接跳过，避免内部探查污染项目级记忆库。
- **输出**：0~N 条 `state=Active` 条目（v1.1 自动转正，每条仍过准入管道），并要求模型
  为每条产出**稳定显式 key**（如 `user:commit_language`），同主题更新复用同 key。
- **失败策略**：provider 调用失败 / 解析失败 / 超时 → **静默跳过**，绝不阻塞任务结束、不写 `Event::Error`。
- **不进执行循环**：worker 是旁路任务，因此 `RuntimeDeps` 零改动。

### 4.3 候选批量确认（O6，v1.1 保留为兜底）

`/memory` 面板的 Candidate 页：逐条 Enter 确认 / **Ctrl+A 全部确认（v1.1 新增）** / Del 丢弃；`Ctrl+K` 清空当前页。候选态仍保留：`confirm`/`discard` 接口与面板入口不变，作为自动转正的兜底与审计路径。

### 4.4 自动转正（v1.1，P0-1A）

v1.0 实测：提炼产出全部停在 `Candidate` 且唯一 `confirm` 调用点在 TUI 逐条按键，导致
「记忆只写不读」。修订为：**通过准入管道的提炼产出直接 `state=Active`**，立即参与注入
与检索；用户仍可在 `/memory` 生效页删除/清理（逻辑删除可追溯）。

---

## 5. 读取流程（双通道）

### 5.1 通道 A：随首轮 user 注入（v1.2）

任务提交时，装配层：

1. 计算作用域：`User` 恒取；`Project { root }` 仅当 `TaskSpec.project_root` 非空时取，为空则只注入用户级。
2. 合并 `Active` 条目，**冲突条目两条都保留并在文本里标注来源**（O2）：

   ```
   【记忆 · 用户级】推送 GitHub 经常需要重试 3~8 次
   【记忆 · 项目级】推送 GitHub 经常需要重试 3~8 次（与上方冲突，请自行判断）
   ```

3. **动态截断（O3）**：预算 = `max_context * inject_ratio`（`max_context` v1.2 起取 `provider.capabilities().max_context`，不再是硬编码 128K；估算口径见 `ContextAssembler` / `ContextManager::estimate_tokens`，中文按 1 字符≈1 token、ASCII≈0.34）。按「`last_used_at` 降序 → `updated_at` 降序 → id 稳定排序」丢弃尾部，直到估算 token 落入预算；`MemoryContext.truncated` 记丢弃数。
4. 写入 `TaskSpec.memory_context: Option<MemoryContext>`（`#[serde(default)]`，旧日志可反序列化）。
5. **注入位置（v1.2 变更）**：`ContextAssembler::first_user()` 把任务与记忆文本一起放进**首轮 user 消息**（`【当前任务】` + `【长期记忆】`）。固定 system 恒定、不含任务与记忆——记忆变化不再打断请求前缀缓存。续跑时若本次 compose 与上轮注入内容不同，装配层填 `TaskSpec.memory_update`，runner 在历史**尾部**追加一条 `[背景更新]` 增量消息；相同则完全不动前缀。
6. `/resume` 时由 `resume_context()` 把增量快照序列重建成 `resume_messages`，`normalize_history()` 保证 `Role::System` 恒 1 条后接着用；历史里已注入的记忆继续生效，不重新 compose。

**去重约定**：装配层同时 `register(task_id, injected_ids)`；`memory_search` 返回结果时过滤掉这些 id，避免同一条同时出现在首轮注入区和工具结果里（§10.3 强调）。

### 5.2 通道 B：按需检索工具

`memory_search` 工具（构造时注入 `Arc<dyn MemoryIndex>` + 注入 id 注册表，不改 `ToolCtx`）：

| 参数 | 说明 |
|---|---|
| `query` | 关键词，按 content/tags 匹配 |
| `scope` | 可选 `user` / `project`，默认两者 |
| `limit` | 默认 5，钳制 1..=20 |

返回条目带**来源标注**与 `state=Active` 过滤；Candidate / Archived 不返回。工具走 `TaskSpec::allows_tool`，受 C3 的统一可见性/可执行性规则约束。

### 5.3 容量与淘汰（R4 / O11）

- `Active` 条数上限：用户级 200 / 项目级 500；`Candidate` 上限 100。
- 超限或定时整理时按 `last_used_at` LRU 淘汰 → `Archived`（保留记录，不物理删）。
- `last_used_at` 由**注入命中 + 检索命中**共同推进，不能只看写入时间。
- 注入截断只影响本次上下文，**不删除库里条目**；两者互不干扰。

---

## 6. 子任务隔离（验收第 6 条）

`run_child` 现有逻辑：

```rust
child.tool_allowlist = Some(父 tool_defs 去 "delegate");
```

需改为**同时排除记忆类工具**（`memory_search`，以及 `propose_patch` 若实现为工具）：

```rust
.filter(|n| n != "delegate" && !is_memory_tool(n))
```

并且子任务的 `TaskSpec.memory_context = None`（`TaskSpec::new` 默认已是 None，不要从父 spec 复制）。这样子任务既无注入也无工具，验收第 6 条可用 `tool_allowlist` 断言覆盖。

---

## 7. 记忆驱动的代码改进建议（O7）

流程：TUI `/improve <目标>` → 装配层组装（相关记忆 + 当前项目结构摘要）→ 现有 provider 生成 unified diff → 校验（不含绝对路径、不含凭据、不越出 workspace）→ 写 `<workspace>/patches/<timestamp>.patch` → TUI 提示路径。

**硬边界**：不建分支、不 `git apply`、不 commit、不 push、不自动跑测试；产物只是给人看的文本。`agent-tools` 若提供 `propose_patch`，同样必须只对 `workspace` 写文件，且受工具沙箱 `allowed_roots` 约束。

---

## 8. 命令、事件与影响面

| 交互 | 落点 | 是否新增枚举变体 |
|---|---|---|
| `/remember` | TUI 本地经 `MemoryPort` 写入 | 否（尽量复用现有 `UiAction`/本地处理） |
| `/memory` 面板 / 候选角标 | TUI 本地 | 否 |
| 自动提炼 | EventBus 旁路 worker，订阅现有 `TaskFinished` | 否 |
| 注入快照 | `TaskSpec.memory_context` 新字段（`serde(default)`） | 否（`Command` 已有 `large_enum_variant` 豁免，见接口加固 C3） |
| 检索 | 新工具 `memory_search` | 否 |

> 若实施中发现必须新增 `Event` / `Command` 变体：按 `docs/subagent-implementation.md` 的教训补齐 **3 处穷尽 match（`agent-tui/render.rs`、`xtask/log_view.rs` 等）** 与 **9 个 TUI 存量测试前置**。`Event` 三域拆分仍属二期，本期不碰。
>
> 审计：记忆的 upsert/delete/state 记录本身就是审计日志，不额外发事件，避免同一事实写两遍（与 `recorder` 单点裁决原则一致）。

---

## 9. 关键设计决策记录

| 编号 | 决策 | 理由 | 被否方案 |
|---|---|---|---|
| D1 | 注入快照放 `TaskSpec`，不放 `RuntimeDeps` | `RuntimeDeps` 有 14 个构造点；`TaskSpec` 随任务走，且审计可复现当时注入了什么 | 给 deps 加 `Arc<dyn MemoryStore>` |
| D2 | 提炼走 EventBus 旁路 worker | 不阻塞任务结束、不动执行循环状态机、失败静默 | 在 `run_inner` 收尾处直接调 provider |
| D3 | 记忆工具构造时注入 port | 与 `DelegateTool` 一致，`ToolCtx` 零改动 | 给 `ToolCtx` 加字段（11 个构造点） |
| D4 | 冲突两条都注入并标注来源（O2） | 用户裁决；模型自行判断 | 项目级覆盖用户级 |
| D5 | 按 token 比例动态截断（O3） | 用户裁决；不同模型窗口自适应 | 硬条数/字符上限 |
| D6 | 逻辑删除 + 历史版本 | 可追溯、可回滚误写；O1 兜底 | 物理删除 |
| D7 | 一期 `KeywordIndex` + `MemoryIndex` trait | R1 裁决：验收不得以「没有语义召回」判未完成 | 一期直接接 embedding |
| D8 | `.patch` 只落 workspace | O7 裁决收窄，外部动作风险最低 | 临时分支 / PR 草稿 / 自动跑测试 |

---

## 10. 验收映射（需求 §7 八条）

| # | 判据 | 由哪部分设计保证 |
|---|---|---|
| 1 | 关进程后新任务仍可见 | §3.2 持久化 + §5.1 任务提交 compose |
| 2 | 项目 X 的记忆在 Y 不可见 | §3 作用域 `Project { root }` + §5.1 第 1 步 |
| 3 | 凭据被拒且落盘不含该串 | §4.1 第 2 步；错误文案不回显 |
| 4 | 候选确认前不注入/不检索 | §4.3 + §5.2 过滤 state |
| 5 | 上限淘汰、冲突留历史版本 | §4.1 第 4 步 + §5.3 |
| 6 | 子任务无注入、无工具 | §6 |
| 7 | TUI 可查看/搜索/编辑/删除/清空 | §8 + `MemoryPort` 门面 |
| 8 | 全量测试/clippy/fmt 绿，测试无真实模型调用 | worker 与工具均可用测试替身；`--ignored` 用例不动 |

> 当前基线：`cargo test --workspace` **416 通过**、clippy 0 告警。实施后通过数应 ≥ 416 + 新增测试数。

---

## 11. 实施分期建议（供实施说明书参考）

| 阶段 | 内容 |
|---|---|
| M0 | `agent-core/src/memory.rs` 类型/trait/`scan_secret` + 单测；`lib.rs` 补导出 |
| M1 | `agent-store` jsonl store + `KeywordIndex` + 去重/版本/LRU/压缩 + 单测 |
| M2 | `TaskSpec.memory_context` + 装配层 compose/register + `ContextManager` 拼接 + 旧日志兼容测试 |
| M3 | `memory_search` 工具 + `default_tools` / `EXPECTED_TOOLS` 同步 + 子任务 allowlist 排除 |
| M4 | TUI `/remember` / `/memory` 面板 + 候选角标 + 批量确认 |
| M5 | 提炼 worker + `/improve` 生成 `.patch` + 全量回归 |

每阶段独立可 review、可单独跑 `cargo test --workspace`，与 `docs/interface-hardening.md` 的 C0~C4 组织方式一致。

---

## 12. 明确不做

1. 真实 embedding / 向量库 / `SyncEndpoint` 跨设备同步（二期）。
2. 子任务继承记忆、自动合并或推送代码、补丁自动跑测试。
3. `Event` 三域拆分、`Tool::execute` 签名变更、`ToolCtx` / `RuntimeDeps` 加字段。
4. PII / 源码原文的强制拦截（需求 O1 裁决为不纳入红线，仅 TUI 明示限制）。
5. 调用真实模型跑测试；新增用例一律测试替身。
