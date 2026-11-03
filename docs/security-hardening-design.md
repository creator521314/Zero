# 权限系统加固设计文档（详细设计）

> 状态：**已实现（2026-11-03）**，实现记录见文末「七、实现记录」。
> 上游：一次真实权限探测（`artifacts/permission_probe_report.md`、`artifacts/agent_permission_handoff.md`）
> 与对应审计日志 `~/agent/logs/62f3b379-*.jsonl` 的复盘结论。
>
> 结论先放在前面：那两份 artifacts 的核心结论是**错的**。审批弹窗其实弹了 7 次，
> 真正的问题不在「权限系统没有防御」，而在三点：
> 1. **密钥卫生**——完整 API key 进了工具输出、模型上下文和 jsonl 日志；
> 2. **审批不可观测**——agent 看不见「被人工批准过」，于是把自己臆测成「没设防」；
> 3. **若干语义漏洞**——`shell_exec` 旁路文件边界、审视器不知道项目根、批量批准粒度太粗。
>
> 本文把这些修成分项，每项给出「现状 → 改动 → 验收」。

---

## 0. 范围与优先级

| 编号 | 分项 | 优先级 | 改动面 |
|---|---|---|---|
| P0-1 | `shell_exec` 子进程剥离敏感环境变量 | P0 | agent-tools |
| P0-2 | 工具输出 / 审计日志值级脱敏 | P0 | agent-core + agent-runtime + agent-providers + 两个入口 |
| P1-1 | `Decision::AskUser` 携带原因与来源 | P1 | agent-core + agent-perm |
| P1-2 | 审批请求填真实 reason/source/matched_rule | P1 | agent-runtime |
| P1-3 | 新增 `Event::ApprovalDecided`（记录批准/拒绝） | P1 | agent-core + agent-runtime + agent-store |
| P1-4 | 工具结果标注 `[user-approved]` + 系统提示词说明 | P1 | agent-runtime |
| P1-5 | 审视器提示词传入 `project_root` | P1 | agent-providers |
| P1-6 | 实现 transient 事件过期清理（`transient_retention_days` 现在是死配置） | P1 | agent-store + 两个入口 |
| P1-7 | 审批弹层显示真实原因 | P1 | agent-tui |
| P2-1 | 批量批准按「操作类」而非工具名 | P2 | agent-perm + agent-runtime |
| P2-2 | 修正 README/design 的沙箱表述 | P2 | 文档 |
| P2-3 | 任务标题随实质指令更新 | P2 | agent-cli / agent-runtime |
| P2-4 | `xtask log` 支持 id 前缀 | P2 | xtask |

附录 A：密钥泄露处置手册（运维动作，非代码）。

**实现铁律**：所有改动只许新增字段时带 `#[serde(default)]`，保证旧 jsonl 仍可反序列化；
任何事件枚举新增变体必须同步补上 `agent-store/src/recorder.rs::classify` 与
`xtask/src/log_view.rs::render_events` 两个 match，否则编译报错（两个 match 均无通配分支）。

---

## P0-1 `shell_exec` 子进程剥离敏感环境变量

**现状**：[`crates/agent-tools/src/shell.rs:138`](../crates/agent-tools/src/shell.rs#L138)
直接 `std::process::Command::new(prog)`，子进程**继承主进程全部环境变量**。
因此 `sh -c 'env'` 能看到 `DEEPSEEK_API_KEY`（本次事故的直接根因之一）。

**方案**：黑名单剥离（不要 `env_clear` + 白名单——那会丢掉 `PATH`/`HOME`/`LANG` 等，
大量命令立即失效）。在 `spawn` 之前遍历当前进程环境，按**变量名后缀**判断是否敏感，
命中则 `env_remove`。

新增常量（放 `shell.rs` 顶部）：

```rust
/// 命中即从子进程环境中移除的变量名后缀。
const DENIED_ENV_SUFFIXES: &[&str] = &[
    "_API_KEY", "_API_TOKEN", "_TOKEN", "_SECRET", "_SECRET_KEY",
    "_ACCESS_KEY", "_PASSWORD", "_PASSWD", "_CREDENTIALS",
];
```

加一个纯函数（可单测）：

```rust
fn sensitive_env_names() -> Vec<String> {
    std::env::vars()
        .filter(|(k, _)| DENIED_ENV_SUFFIXES.iter().any(|s| k.ends_with(s)))
        .map(|(k, _)| k)
        .collect()
}
```

在 `execute` 里构造命令后调用：

```rust
for name in sensitive_env_names() {
    std_cmd.env_remove(name);
}
```

**边界**：
- 只作用于 `shell_exec` 子进程；主进程环境不动（provider 取 key 走主进程，不受影响）。
- 后缀匹配不会误伤 `FOO_TOKENIZER`（`_TOKEN` 要求前有下划线且结尾）。
- Windows 与 Unix 同用 `std::env` / `Command::env_remove`，无需平台分支。

**验收**：
- 单测：`set_var("AGENT_TEST_API_KEY","sk-x")` 后跑 `sh -c 'env'`（`#[cfg(unix)]`），
  断言输出不含 `AGENT_TEST_API_KEY`，含 `HOME`（白名单未被误删）。

---

## P0-2 工具输出 / 审计日志值级脱敏

**现状**：工具返回的原文原样进两处——
1. 回填给模型的 `tool_messages`（[`runner.rs:571`](../crates/agent-runtime/src/runner.rs#L571-L577)）；
2. 落盘的 `Event::ToolCallEnd.output`（[`runner.rs:578`](../crates/agent-runtime/src/runner.rs#L578-L591)）。

本次事故里，`env` 的输出（含完整 key）同时进了模型上下文和 jsonl，共 18 处完整 key。
仅靠 P0-1 防不住「`cat config/config.toml`（明文兜底 key）」这类来源，所以要做**值级**脱敏。

**方案**：引入一个脱敏器，在 runner 这**单一汇点**统一替换；runner 不知道 key 值，
由装配层注入（复用 `CostSinkFn` 的注入套路）。

1. `agent-runtime/src/runner.rs` 新增类型与依赖字段：

```rust
pub type RedactFn = Arc<dyn Fn(&str) -> String + Send + Sync>;

pub struct RuntimeDeps {
    // ...现有字段...
    /// 把工具输出里的敏感值替换为 `<redacted>`。装配层从配置/env 收集密钥值后注入。
    /// 测试/无密钥场景注入恒等函数。
    pub redact: RedactFn,
}
```

2. `agent-providers/src/redact.rs`（新增）实现收集与替换：

```rust
pub struct SecretRedactor { secrets: Vec<String> }

impl SecretRedactor {
    pub fn from_config(cfg: &ProvidersFile) -> Self {
        let mut set = std::collections::BTreeSet::new();
        // 来源一：进程环境里的敏感名对应的值（覆盖所有 *_API_KEY 等）
        for (k, v) in std::env::vars() {
            if is_sensitive_name(&k) && !v.trim().is_empty() { set.insert(v); }
        }
        // 来源二：配置文件里每个 provider 解析出的实际 key（含明文兜底）
        for p in cfg.providers.values() {
            if let Some(k) = p.resolve_api_key() { set.insert(k); }
        }
        let mut secrets: Vec<String> = set.into_iter()
            .filter(|s| s.chars().count() >= 8)   // 过滤假 key（"sk-test" 之类）
            .collect();
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));  // 长值优先，避免子串覆盖
        Self { secrets }
    }

    pub fn redact(&self, s: &str) -> String {
        let mut out = s.to_string();
        for sec in &self.secrets {
            out = out.replace(sec, "<redacted>");
        }
        out
    }
}
```

`is_sensitive_name` 复用 P0-1 的同款后缀表（抽到共享位置或各自定义，避免 agent-tools↔agent-providers 互相依赖）。

3. 两个入口注入：
   - `apps/agent-cli/src/main.rs`、`apps/agent-server/src/main.rs` 组装 `RuntimeDeps` 处，
     用 `SecretRedactor::from_config(&cfg)` 构造 `Arc::new(move |s| r.redact(s))`。
     注意：`RuntimeDeps` 的所有构造点都要补（含 runner 单测里的构造，注入恒等函数）。

4. 应用点（全部在 runner，先 redact 再 push/send，确保模型上下文与日志都干净）：
   - 成功分支 `tool_messages.push(...)` 的 `r.content` → `(self.deps.redact)(&r.content)`；
   - `Event::ToolCallEnd { output: Some(...), detail: ... }`（成功与失败两处）→ 同样 redact；
   - `finish_summary` 取自 `finish` 工具输出，也 redact。
   - denied / rejected 分支不含工具输出，跳过。

**为什么在这几处做**：`ContextSnapshot` 由 `cm.messages` 生成，工具消息先 redact，
快照自然干净；`ToolCallEnd` 先 redact，jsonl 干净。两处都归结到同一个 `r.content`。

**已知限制（文档注明，不实现）**：不做 URL 编码/反转义变体；`AssistantTurn.text`
（模型自己的回复）不回改——模型可能自己复述 key，但那已是模型输出，脱敏器只
覆盖「工具原文」这一最大且可控的泄漏面。

**验收**：
- `agent-providers`：`SecretRedactor` 单测——env 里一个假 key + 配置一个明文 key，
  `redact("x sk-a... y sk-b...")` 两段都被替换；空值、短值不参与。
- `agent-runtime`：构造带 `redact` 的 `RuntimeDeps` + 一个返回含 key 的 mock 工具，
  断言 tool message 与 `ToolCallEnd.output` 都是 `<redacted>`。

---

## P1-1 `Decision::AskUser` 携带原因与来源

**现状**：[`crates/agent-core/src/perm.rs`](../crates/agent-core/src/perm.rs#L37-L41)
`Decision::AskUser` 是无负载的单元变体；`ApprovalResult` 里的 `reason`/`source`
在 [`agent-perm/src/lib.rs:130`](../crates/agent-perm/src/lib.rs#L126-L135) 被 `Ok(_) => AskUser` 丢弃。

**方案**：把 `Decision::AskUser` 改成带数据：

```rust
pub enum Decision {
    Allow,
    Deny(String),
    AskUser {
        /// 为什么需要用户确认（进弹窗、进日志）。
        reason: String,
        /// 裁决来源：Static=静态规则，Ai=AI 审视/审视通道异常，User=策略（如 L3）。
        source: ApprovalSource,
        /// 静态规则命中时的规则描述；其余为 None。
        matched_rule: Option<String>,
    },
}
```

`agent-perm/src/lib.rs::check` 里现有 5 个返回点逐一填值：

| 分支 | 现返回值 | 改为 |
|---|---|---|
| 静态规则命中，L2/L3（约 line 104） | `AskUser` | `AskUser { reason: format!("高危操作被静态规则拦截：{rule}"), source: Static, matched_rule: Some(rule) }` |
| L3 未批量批准（约 line 113） | `AskUser` | `AskUser { reason: "L3 策略：逐项确认".into(), source: User, matched_rule: None }` |
| L2 无审视器降级（约 line 117） | `AskUser` | `AskUser { reason: "无可用 AI 审视器，降级询问".into(), source: Ai, matched_rule: None }` |
| AI 审视 `Ok(_)` 判 ask（约 line 130） | `AskUser` | `AskUser { reason: r.reason, source: r.source, matched_rule: None }`（注意保留 `r`，不再丢弃） |
| AI 审视 `Err` 降级（约 line 133） | `AskUser` | `AskUser { reason: format!("AI 审视失败，降级询问：{e}"), source: Ai, matched_rule: None }` |

L1 命中静态规则仍返回 `Deny`，不变。

**验收**：`agent-perm` 既有单测更新匹配 `AskUser { .. }`；新增「静态命中 source=Static 且
matched_rule 非空」「L3 命中 source=User」。

---

## P1-2 审批请求填真实 reason/source/matched_rule

**现状**：[`runner.rs:484`](../crates/agent-runtime/src/runner.rs#L484-L496) 硬编码
`reason: "需要用户确认"`、`matched_rule: None`。本次日志里连 `sudo`（静态命中）都是 `null`。

**方案**：
1. `agent-core/src/perm.rs` 的 `ApprovalRequest` 增加字段（带默认值，兼容旧日志）：

```rust
pub struct ApprovalRequest {
    // ...现有字段不变...
    /// 裁决来源。旧日志无此字段 → None。
    #[serde(default)]
    pub source: Option<ApprovalSource>,
}
```

2. `runner.rs` 的 `Decision::AskUser { reason, source, matched_rule }` 分支改成解构并透传：

```rust
Decision::AskUser { reason, source, matched_rule } => {
    ...
    req: ApprovalRequest {
        id: approval_id.clone(),
        task_id: task_id.clone(),
        tool: call.name.clone(),
        args: call.arguments.clone(),
        risk: self.tool_risk(&call.name),   // 保持现状：弹窗红/橙着色用
        reason,
        matched_rule,
        source: Some(source),
    },
    ...
}
```

**说明**：`risk` 保持「工具静态风险」语义（`shell_exec` 恒 High），它只驱动弹窗配色；
真正的拦截原因由 `reason`/`matched_rule`/`source` 表达，二者不混。

**验收**：runner 单测——`AskUser { source: Static, matched_rule: Some("提权执行") }` 时，
捕获的 `Event::AwaitingApproval.req` 里三个字段一致、`matched_rule` 非空。

---

## P1-3 新增 `Event::ApprovalDecided`

**现状**：只有 `Event::AwaitingApproval`，没有「批准/拒绝」的落盘记录，事后只能靠
「有请求 + 有输出」推断是否批准。这正是 agent 误判的温床。

**方案**：
1. `agent-core/src/event.rs` 增加事件与判定枚举：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approved { remember: bool },
    Rejected { reason: String },
}

// Event 枚举新增：
ApprovalDecided {
    id: TaskId,
    approval_id: ApprovalId,
    decision: ApprovalDecision,
},
```

2. `runner.rs`：`wait_approval` 返回后**立即**发事件（在 `ToolCallEnd` 之前）：
   - `Ok(Approve { remember })` 分支内 `bus.send(Event::ApprovalDecided { .. Approved { remember } })`；
   - `Ok(Reject { reason })` 分支内发 `Rejected { reason: reason.clone() }`。
   注意 reject 分支目前把 `reason` move 进消息；先 clone 再发。

3. `agent-store/src/recorder.rs::classify` 增加分支：`ApprovalDecided { id, .. } => Some((id.clone(), Durability::Transient))`。

4. `xtask/src/log_view.rs::render_events` 增加渲染：
   `⏸ 等待审批` 之后输出 `✔ 已批准`（remember 时加 `（本任务同类一并批准）`）或 `⊘ 已拒绝：<reason>`。

**验收**：
- event.rs 往返序列化测试；
- recorder classify 测试（ApprovalDecided → Transient）；
- 系统级：runner 单测里 ask→approve，断言总线同时收到 AwaitingApproval 与 ApprovalDecided，
  且顺序为 AwaitingApproval 在前。

---

## P1-4 工具结果标注 `[user-approved]` + 系统提示词说明

**现状**：批准后执行，模型拿到的工具返回与「自动放行」完全一样，无法区分。
本次 agent 因此把「用户批准过」当成「没设防」。

**方案**：
1. `runner.rs`：在 `Decision::AskUser` 分支用局部标记：

```rust
let mut user_approved = false;
...
Ok(ApprovalReply::Approve { remember }) => { user_approved = true; ... }
```

执行成功回填时（约 line 571）：

```rust
let content = if user_approved {
    format!("[user-approved] {}", r.content)
} else {
    r.content.clone()
};
```

只加给**模型**（`tool_messages` 的 content）；`ToolCallEnd.output` 保持脱敏后的原文，不加前缀——
日志忠实于工具返回，模型侧才需要「被批准过」这一信号。

2. `runner::system_prompt`（约 line 89）追加一段，明确模型的自知边界：

```text
【审批】当你发起的工具调用需要审批时，用户批准后该工具的返回会以 [user-approved] 开头；
被拒绝会以 [rejected by user] 或 [denied] 开头。除此之外，你无法得知某次操作是否经过
人工审批，也不得依据「操作成功了」推断权限系统是否设防。
```

**验收**：runner 单测——走 ask→approve 的工具，其 tool message 文本以 `[user-approved]` 开头；
ask→reject 的仍是 `[rejected by user]`；自动放行的无前缀。

---

## P1-5 审视器提示词传入 `project_root`

**现状**：[`review.rs:147`](../crates/agent-providers/src/review.rs#L147-L151) 只给
`task.workspace`（私有工作区 `~/agent/workspace/<id>`），不是项目根，审视器**根本不知道**
工具能读写到哪里，无法对「越界」做判断。

**方案**：
1. `ctx` 增加一行，口径与 `runner::system_prompt` 的 `ws` 一致：

```rust
let root = task.project_root.clone().unwrap_or_else(|| task.workspace.clone());
let ctx = format!(
    "任务目标: {}\n工作区: {}\n项目根: {}\n静态风险: {:?}\n当前操作: 工具={} 参数={}",
    task.prompt, task.workspace, root, static_risk, call.name, call.arguments
);
```

2. `SYSTEM_PROMPT` 追加一句（保守，不推翻既有的「默认放行、只判危害」调优结论）：

```text
读写项目根之外的路径（尤其 /etc、~/.ssh 等系统/敏感目录）作为 risk 参考，
但只在可能造成不可逆危害、访问敏感凭据、外传数据到不可信地址时才 deny。
```

**验收**：`review.rs` 单测——构造带 `project_root` 的 `TaskSpec`，断言传给 provider 的
user 消息里含该项目根字符串；`project_root` 为 None 时回退到 workspace。

---

## P1-6 实现 transient 事件过期清理

**现状**：`transient_retention_days`（[`config.rs:293`](../crates/agent-providers/src/config.rs#L293-L294)）
只定义、**没有任何代码消费**；README 却承诺「临时信息保留 7 天」。因此本次 key 会永久留在 jsonl。

**方案**：落点 `agent-store`。
1. 在 `JsonlStore` 上加一个公开方法（签名示例）：

```rust
/// 清理每个会话文件里早于 `now - retain_days` 的 Transient 事件。
/// Persistent 事件全部保留。失败只记 warning，不阻塞启动。
pub async fn prune_transient(&self, retain_days: u64) -> anyhow::Result<usize>;
```

实现：遍历 `logs/*.jsonl`，逐行解析 `StoredEvent`，用 `clock.wall_ms` 与
`durability == Durability::Transient` 判定是否删除；把保留行写到同目录临时文件再
`rename`（原子替换）。返回本次删除的事件总数。

2. 两个入口在装配完 store、进入主循环前调用一次：

```rust
let days = cfg.agent.transient_retention_days.max(1);
let _ = store.prune_transient(days).await;  // 记 warning，不 fatal
```

**边界**：`tasks/*.json`（任务摘要）不清理；`save_stream_deltas=true` 时的 `TextDelta`
也是 Transient，一并按墙钟清理；损坏行跳过（沿用 `log_view` 的「跳过损坏行」）。

**验收**：`agent-store` 单测——写两条墙钟差异超过阈值的事件（一条 Persistent、一条
Transient），清理后 Persistent 在、过期 Transient 没了、未过期 Transient 在；重写后
文件仍可被 `load_session` 读回。

---

## P1-7 审批弹层显示真实原因

**现状**：[`render.rs:1560`](../crates/agent-tui/src/render.rs#L1560-L1565) 只显示
工具 / 风险 / 参数；`reason` 与 `matched_rule` 从未上屏。

**方案**：`draw_approval` 的 body 增加字段（在「参数」之后）：

```rust
approval_field("原因", &p.req.reason),          // 已有 reason 字段，直接渲染
// matched_rule 非空时再追加一行：
if let Some(r) = &p.req.matched_rule {
    approval_field("规则", r),
}
```

弹层高度从 `11` 视行数放宽到 `13`（或 `min` 上限加 2），并同步 `approval_button_areas`
的按钮坐标（它按「倒数第 2 行」对齐，需随高度重算，参考 [`render.rs:514`](../crates/agent-tui/src/render.rs#L514)）。

**验收**：`agent-tui` 渲染快照单测——`reason`/`matched_rule` 非空时，`TestBackend` 输出
含这两段文本；不撑破布局（按钮仍在弹层内）。

---

## P2-1 批量批准按「操作类」而非工具名

**现状**：[`agent-perm/src/lib.rs:68`](../crates/agent-perm/src/lib.rs#L67-L73)
`approve_for_task(task_id, tool)` 以**工具名**为键。对 `shell_exec` 而言，批准一次 `ls`
即放行本任务后续所有 shell 命令（静态高危仍会被静态规则拦，但非静态的 `cat ~/.ssh/id_rsa`
之类会被一并放行）。

**方案**：把键从「工具名」升级为「操作类」。接口不变名可加新参数，建议：

```rust
/// 计算一次调用的「类键」。
/// file_* → 工具名本身；shell_exec → "shell:<首 token>"；
/// 解析不出或含 && / ; / | 等复合结构 → "shell:<整条 cmd 的稳定 hash>"（不批量放行）。
pub fn class_key(call: &ToolCall) -> String;
```

放 `agent-perm`（策略层）。`check` 的 L3 分支把 `is_batch_approved(task_id, &call.name)`
改为 `is_batch_approved(task_id, &class_key(call))`；`approve_for_task` 的入参也改传类键，
`runner.rs` 的 `remember` 分支同步传 `class_key(call)`。

首 token 解析（启发式，写死在文档里）：去空白 → 若以 `sudo`/`doas` 开头则剥掉再取 →
按空白切出第一个词 → 去掉首尾引号。含 `&&`/`;`/`|`/`>` 的命令**不**进批量白名单。

**验收**：`agent-perm` 单测——批准 `ls` 后：`ls -la` 免问、`rm -f /tmp/x`（非静态高危）
仍问、`ls && rm x` 仍问。

---

## P2-2 修正沙箱表述

[`README.md:195`](../README.md#L195) 与 `docs/design.md` 相关段落，把「所有文件操作
限制在项目目录内」改为明确两层：

- `file_read/file_write/file_edit/list_dir`：路径 realpath 后必须落在 `project_root`/私有工作区内，`..` 拒绝；
- `shell_exec`：`cwd` 锁定项目根，但**不强制**路径边界，靠权限层（静态规则 + AI 审视）把关。

---

## P2-3 任务标题随实质指令更新

**现状**：任务标题固定为第一条消息（本次全程是「你好？」）。`/tasks` 与任务面板因此失去回溯价值。

**方案**：`agent-runtime` 在任务开始时/每轮收尾时，若当前 `TaskSpec.title` 与最新一条
用户指令差异明显（或标题为空/过短），用首条**非寒暄**的用户消息更新 `TaskSummary.title`
（沿用现有 `store` 的 `touch_task`/`upsert_task` 落盘路径，只改写入值）。
判据：标题长度 > 2 且包含实质内容时覆盖；`你好？`/`/exit` 这类不覆盖。

**验收**：单测——连续两轮「你好？」→「做一个权限探测」后，任务标题更新为后者。

---

## P2-4 `xtask log` 支持 id 前缀

**现状**：[`log_view.rs:35`](../xtask/src/log_view.rs#L35-L43) 直接拼 `{arg}.jsonl`，前缀会报「找不到会话文件」。

**方案**：`resolve(arg)` 在 arg 不含 `.jsonl` 时，列出 `logs/` 目录，按前缀匹配：
0 个 → 报错；1 个 → 用之；>1 个 → 报「匹配到 N 个：…」。逻辑与 `apps/agent-cli/src/main.rs::resolve_task` 一致。

**验收**：单测——唯一前缀命中、歧义报数、无匹配报错。

---

## 附录 A：密钥泄露处置手册（运维动作，非代码）

本次 key 已在 `~/agent/logs/62f3b379-*.jsonl` 落盘 18 处，并已随上下文发给模型。
**实现代码之前**先执行：

1. **轮换** `DEEPSEEK_API_KEY`（旧 key 视为已泄露，立即作废）；
2. 删除或整体覆写 `~/agent/logs/62f3b379-303c-45a2-a472-842c1dec472b.jsonl`
   （连同任何备份 / `ctx/` 外置文件里出现的副本）；
3. 检查 `~/agent/workspace/62f3b379-*/` 下是否有快照/外置输出残留；
4. 确认 `config/config.toml` 未写入明文 key（向导只记环境变量名，正常应没有）；
5. 两处 artifacts（`artifacts/permission_probe_report.md`、`agent_permission_handoff.md`）
   结论有误且复现清单会再泄一次，**不要原样交给其他 agent**；如需移交，先按本文修复后重测。

---

## 全局验收（全部完成后）

- `cargo xtask check` 通过（fmt + clippy `-D warnings` 零警告 + 全量测试）；
- `cargo test --workspace` 全绿（新增用例数见各分项）；
- 端到端复核（可复现本次事故）：L2 档下让 agent 跑 `env | grep key`——
  ① 输出含 `<redacted>` 而非真实 key；② jsonl 里无真实 key；③ 审批请求在日志里带
  真实 `reason/matched_rule`；④ 批准后有 `ApprovalDecided`；⑤ 模型能看到 `[user-approved]`。
- 旧 jsonl（含本次事故日志）仍能被 `xtask log` 正常读取（兼容性）。

---

## 七、实现记录（2026-11-03）

全部 13 项已落地，`cargo xtask check` 通过（fmt + clippy `-D warnings` 零警告；
`cargo test --workspace` **369 passed / 0 failed / 4 ignored**，原 354）。

| 分项 | 落点 | 与设计的差异 / 说明 |
|---|---|---|
| P0-1 | `agent-tools/src/shell.rs` | 按变量名后缀剥离；`PATH` 等保留（有回归测试） |
| P0-2 | `agent-providers/src/redact.rs`（新增）、`agent-runtime` 的 `RuntimeDeps.redact`、两个入口 | 脱敏器在装配层构造，runner 在「回填模型前 + 落盘前」各用一次 |
| P1-1 | `agent-core/src/perm.rs`、`agent-perm` | `AskUser { reason, source, matched_rule }` |
| P1-2 | `agent-runtime/src/runner.rs` | `ApprovalRequest.source: Option<ApprovalSource>`（旧日志兼容） |
| P1-3 | `agent-core` 事件 + runner + recorder + log_view | 事件名 `ApprovalDecided`；日志渲染「✔ 已批准 / ⊘ 已拒绝」 |
| P1-4 | `agent-runtime/src/runner.rs` | 只给模型加 `[user-approved]`；日志保留原文 |
| P1-5 | `agent-providers/src/review.rs` | 提示词增加「项目根」与越界参考 |
| P1-6 | `agent-store/src/lib.rs::prune_transient` + 两个入口 | 重写 jsonl（临时文件 + rename）；损坏行保留 |
| P1-7 | `agent-tui/src/render.rs` | 弹层高度 11→13，显示「原因 / 规则」 |
| P2-1 | `agent-core/src/perm.rs::class_key` | **落在 core 而非 agent-perm**：runner 不能依赖 agent-perm（依赖铁律）；复合命令退化为一次性键 |
| P2-2 | `README.md` | 明确 `file_*` 有路径边界、`shell_exec` 没有 |
| P2-3 | `apps/agent-cli/src/main.rs` | 寒暄标题遇到首条实质指令时替换（仍沿用原任务 id） |
| P2-4 | `xtask/src/log_view.rs` | id 前缀解析；歧义报「匹配到 N 个」 |

**未做（有意）**：真实 key 的轮换与旧日志清理属运维动作（附录 A），代码不会替用户执行；
`AssistantTurn.text` 的回改也不做——模型可能自己复述密钥，但那是模型输出，脱敏器
只覆盖「工具原文」这一最大且可控的泄漏面。
