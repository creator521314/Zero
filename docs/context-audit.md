# Zero 上下文构造 · 全面审视与 v1.2 修复

> 状态：**v1.2 已实施**（2026-11-27）
> 触发：用户实测反馈三连——「我发送的内容没有显示」「我的提问有没有被完整记录，会不会导致缓存命中率很低」「全面审视上下文构造存在的问题」。
> 本文是审视报告 + 目标协议 + 落地清单，供后续维护时对照；不是逐行实施说明书。

---

## 1. 三个实测问题

| # | 现象 | 直接原因 |
|---|---|---|
| 1 | 追问后对话区反复显示同一个「任务开始」标题，用户输入始终不出现 | `TaskStarted` 只带冻结的 `title`；用户 prompt 从不落盘；回放时对话区被任务标题替换 |
| 2 | 追问场景前缀缓存命中率约等于零 | 每轮重建 system，把本次任务、记忆快照、动态提醒都拼进 `messages[0]`；system 一变，请求前缀从第 0 字节断开 |
| 3 | 用户提问「有但组织很乱」：续跑出现双 system、同一句 prompt 最多出现三遍 | 上下文在装配层、runner、`resume_context()`、压缩、记忆 compose、提炼 worker 里各拼一份，互不知情 |

> DeepSeek / OpenAI 的 implicit prompt cache 依赖**请求前缀逐字节一致**。任何进入 `messages[0]` 的变化量都会让后续所有 token 无法复用。

---

## 2. 系统性问题清单

### A. 用户输入可见性
- `TaskStarted` 的 `title` 是任务开始时冻结的标题，追问不更新；
- 用户 prompt 只存在于 `TaskSpec`，没有任何 `Persistent` 事件记录；
- `/resume`、切任务、重启后回放，只能看到标题，看不到「用户到底问了什么」。

### B. 前缀缓存与请求结构
- system 每轮重建，内含 `【用户任务】`、`【长期记忆】`、工作目录、预算提醒等变化量；
- 预算提醒以 `Role::User` 永久 push 进历史，于是提醒进了快照、进了提炼输入、跨续跑被当成真实用户输入，过期的轮数还持续误导模型；
- provider 适配层只解析 OpenAI `prompt_tokens_details.cached_tokens`，DeepSeek 的 `prompt_cache_hit_tokens` 读不到，缓存命中不可观测。

### C. 上下文完整性与生命周期
- `ContextSnapshot` 每轮落**全量** messages，日志按 O(N²) 膨胀；
- `resume_context()` 取最后一份全量快照，只补 `AssistantTurn.text`，丢掉 `tool_calls` 与工具结果；`assistant.tool_calls` 找不到配对 `Tool` 消息时，OpenAI 兼容端点直接 400；
- `prune_transient()` 清理瞬态事件时可能把最后一份上下文基线一起删掉，`/resume` 直接失效；
- token 估算只算 `m.text().len()/3`，漏掉 `tool_calls.arguments`（往往是最大的一块）与中文，压缩阈值形同虚设；
- `ContextConfig.max_context` 无论接什么模型都硬编码 128K，32K 模型下永远触发不了压缩；
- 压缩会改写 `messages[0]`，每次压缩都摧毁前缀缓存；裁剪点可能切在 `assistant(tool_calls)` 与 `tool` 结果之间。

### D. 记忆 / 上下文耦合
- 记忆 compose 结果直接拼进 system；记忆一变前缀就变，缓存全灭；
- 提炼 worker 从「最后一次全量快照」取输入，从头截断 6000 字符，固定 system 与早期工具输出会把尾部真正的追问与结论挤掉；
- 子任务复用同一条 EventBus，其内部探查过程也被提炼成项目级记忆，污染记忆库；
- `ctx/` 外置输出目录未忽略；`ContentPart::Image` 被 `parts_to_text()` 静默丢弃，而 `Capabilities.vision` 可能为 true。

---

## 3. 三条根因

1. **没有单一权威构造点**：system / 首轮 user / 续跑历史 / 压缩摘要 / 记忆注入 / 提炼输入由六个地方各拼一份，语义互相漂移。
2. **变化量进入前缀**：任务、记忆、内部提醒都被塞进 `messages[0]`，与「前缀恒定」这一缓存前提直接冲突。
3. **一份全量快照被四种语义复用**：审计、续跑、提炼、UI 回放共用 `ContextSnapshot.messages`，谁改一点都会波及其余三方。

---

## 4. 裁决（用户 2026-11-27）

| 决策点 | 裁决 |
|---|---|
| 记忆注入位置 | **按推荐**：首轮 user 带一份；记忆变化时尾部追加 `[背景更新]` 增量；记忆不变则完全不动前缀 |
| system 去掉 `【用户任务】` | **接受** |
| 快照协议 | **改增量协议**，`ContextSnapshot` 不再带全量 `messages`；**旧日志格式不保留**，无需兼容旧格式读取 |

---

## 5. 目标上下文形状（v1.2）

```
messages[0]  Role::System  固定区：角色 / 工作目录 / 规则 / 审批约定（逐字节恒定）
messages[1]  Role::User    【当前任务】<prompt>\n\n【长期记忆】\n<compose 文本>   ← 仅首轮
...          历史原样保留，永不改写
messages[n]  Role::User    [背景更新] 长期记忆已变化…【长期记忆·更新】\n<最新文本>   ← 仅当记忆变化
messages[n+1] Role::User   <本次追问>                                        ← 每次追问
（请求尾部，不写历史）   [系统提示] 迭代预算只剩 N 轮 / 强制收尾指令        ← ephemeral
```

规则：

- `Role::System` 恒 **1** 条，压缩也不改写；
- 任务只在首轮 user 出现 **1** 次；
- 续跑沿用快照里的固定 system，不再重建；
- 变化量一律尾部追加；
- 压缩通过插入 `[历史摘要]` user 消息完成，并发送 `reset=true` 全量基线。

---

## 6. 增量上下文协议

```rust
Event::ContextSnapshot {
    id, iteration, model,
    added: Vec<Message>,   // 自上次 flush 以来的新增消息
    reset: bool,           // true = 压缩重写后的全量基线，重建方需清空再替换
    tools: Vec<String>,
}
```

- `ContextManager` 维护 `pending` / `reset_pending`，`flush_delta()` 取走增量；
- `JsonlStore::resume_context()` 按序列重建：`reset=true` 清空，否则 `extend`；最后一份快照之后再补 `AssistantTurn.calls` 与 `ToolCallEnd.output/detail`，保证工具调用配对完整；
- `prune_transient()` 保留每个 session 最后一份上下文基线；
- 新任务首份快照天然是「system + 首轮 user」的全量基线，收尾路径也必须 flush，否则最终答复只存在于事件里。

配套：新增 `Event::UserPrompt`（`Persistent`），TUI 回放显示为 `Line::User`，`xtask log_view` 显示「✎ 用户」；`Event::TaskFinished` / `Error` / `TaskStarted` 同为 `Persistent`。

---

## 7. 单一构造点

`agent-runtime::context::ContextAssembler` 统一裁决「什么进上下文、以什么角色、放在哪」：

| 方法 | 职责 |
|---|---|
| `fixed_system(workspace)` | 恒定 system，不含任务 / 记忆 |
| `first_user(prompt, memory)` | 首轮 user：`【当前任务】` + 可选 `【长期记忆】` |
| `extract_memory(messages)` | 倒序遍历所有 user，取最近一次注入的记忆文本 |
| `background_update(memory)` | 记忆变化时的尾部增量消息 |
| `normalize_history(messages)` | 只留 1 条 system、丢弃 `[系统提示]` 伪用户消息、清理孤儿 tool / 无配对 tool_calls |

`ContextConfig::for_model(provider.capabilities().max_context)` 由装配层按实际模型窗口构造；`estimate_tokens()` 计入工具参数 JSON、reasoning，中文按 1 字符≈1 token、ASCII≈0.34；摘要有 `summary_max_chars` 硬上限。

---

## 8. 落地清单

| 阶段 | 内容 | 主要文件 |
|---|---|---|
| P0 | 增量协议、固定 system、用户输入落盘、首轮 user 承载任务+记忆、记忆尾部增量、收尾 flush | `agent-core/{event,task}.rs`、`agent-runtime/{context,runner}.rs`、`agent-store/{recorder,lib}.rs` |
| P1 | 单一构造点、历史规范化、token 估算 / 压缩修正、按模型窗口构造配置、TUI 缓存率显示 | `agent-runtime/context.rs`、`apps/agent-{cli,server}/main.rs`、`agent-tui/{app,render}.rs`、`xtask/log_view.rs` |
| P2 | DeepSeek 缓存字段解析、多模态图片真正发送、提炼输入尾部优先与跳过子任务、`ctx/` 忽略、日志视图 / 文档回写 | `agent-providers/openai_compat.rs`、`agent-store/distiller.rs`、`.gitignore`、`docs/*` |

---

## 9. 验收

- `cargo fmt --all`、`cargo clippy --all-targets`（0 告警）、`cargo test --all` 全绿；
- 新增回归：`memory_is_injected_into_first_user_not_system`（固定 system 逐字节恒定、追问后仍能回溯记忆）、`resume_sends_history_and_then_the_new_instruction`（system 恒 1 条、新指令在历史末尾、提醒不累积）、`logs_full_context_and_tool_output`（增量重建 + 收尾快照）、`budget_warning_is_injected_once_near_iteration_limit`（提醒不进历史）、`clip_transcript_keeps_tail_and_drops_fixed_system`、`usage_reads_both_openai_and_deepseek_cache_fields`、`user_image_is_sent_as_multimodal_content`；
- 真实续跑端到端测试 `apps/agent-cli/tests/resume_live.rs` 改为按 `added/reset` 重建序列后跑通。

---

## 10. 已知限制 / 后续

- 提炼写入 scope 由装配层固定为 `Project`；「用户级偏好自动写 User scope」需要模型输出 scope 字段，属需求变更，本期未做。
- `memory_search` 通道 B 的注入去重仍基于 `InjectedRegistry`，与首轮 user 注入共用一套 id 登记。
- 旧 `ContextSnapshot.messages` 日志按裁决**不做兼容读取**；测试期可用 `scripts/memory-clear.sh` 清理记忆文件，历史 session 日志需自行清理。
- 缓存命中率是「本次响应」的显示值，TUI header 显示最近一次 `Usage.cached_tokens / input_tokens`，不是跨轮累计。
