# Zero 接口加固实施说明书（照做即可版）

> 版本：v1.1（**代码已在真实仓库落地验证**）
> 日期：2026-11-25
> 适用仓库：`creator521314/Zero`，基线提交 `0dc655c`（subagent 功能已落地）
> 配套文档：`docs/subagent-implementation.md`（subagent 实施说明书）、`docs/subagent-design.md`（设计决策）
> 参考：`dsh`（DeepSeek Harness）的接口设计。本文只借鉴**接口手法**，不引入它的插件生态。
>
> **本文写给「执行者」**：可能是一个刚克隆仓库、没有历史上下文的低成本模型。
> `git clone` + 打开本文，按 §5 的 C0→C5 顺序做，就能完成开发、测试、提交、推送。
> 所有类型签名、字段名、插入位置、可复制代码、验收命令、编译错误预案都在里面。

## 验证状态（先读这段）

本文 §5 的**全部代码已在一个临时工作树里逐字落地跑通**，不是纸面推演：

| 检查项 | 结果 |
|---|---|
| `cargo build --workspace` | 通过 |
| `cargo test --workspace` | **413 passed / 0 failed**（基线 401，新增 12） |
| `cargo clippy --workspace --all-targets` | **0 warning** |
| `cargo fmt --all --check` | 无 diff |

落地过程中发现 v1.0 **有 6 处照着做会编译不过或 clippy 报警**，已全部修正并写回正文：

1. `MalformedArgs::new` 返回值不是 `Self`，clippy `new_ret_no_self` → 已改名 `marker()`。
2. C3 给 `TaskSpec` 加了 `Option<Vec<String>>` 后体积变大，触发 `Command` 枚举的 `large_enum_variant` 告警 → §C3 步骤 3.2(d) 补了显式豁免。
3. C3 子任务测试用 `vec![...]` → clippy `useless_vec`，改成数组。
4. §4.1(b) 的代码块是**整个 trait 的替换版**，当成增量补丁会留下重复的 `run_subtask` 残段、直接语法错 → 正文已明确「整段替换」。
5. `detail: Some(reason)` 有两处，其中 Rejected 分支缩进与其它分支不同 → 正文要求按 `outcome:` 上下文定位，别全局替换。
6. 代码块是手写排版，**每个 C 结束必须 `cargo fmt --all`**，否则 `fmt --check` 有 diff。

> 当前 v1.1 的代码块可以逐字复制。遇到编译错误先查 §7，那里的征兆都是实测过的。

## 为什么改这些（一句话版）

对照 dsh 的接口设计，dsh 把「约定」都做成了**类型**：工具输出有规范 schema、错误有稳定 code、安全守卫只能减权不能加权、能力集可协商、工具可见性是显式规则、容量可观察。Zero 目前靠字符串哨兵、自由文本错误和行为约定表达同样的语义——约定不写进类型，就会被下一个改动悄悄破坏。本文把这 6 处收口成类型，全部是**小改动、零行为回归**。

---

## 0. 执行纪律（先读完再动手）

1. **严格按 C0 → C1 → C2 → C3 → C4 → C5 顺序做**，每节结束跑完验收命令、全绿才进下一节。
2. **每个 C 一个 commit**。改崩了 `git reset --hard HEAD~1` 回到干净点。
3. **不要自由发挥类型定义**。§4 的字段名、枚举变体、方法签名逐字照抄；它们是从 `0dc655c` 的真实代码核对出来的。
4. **不要新增外部依赖**。只用 `serde` / `serde_json` / `thiserror` / `tokio` / `tokio-util` / `async-trait`，全在 workspace 里。
5. **不要改 `Tool::execute` 的签名**。`args: serde_json::Value` 保持原样——改成枚举要动 18 个 `ToolCall` 构造点 + 10 个工具实现，收益不值（§3.2）。
6. **不要给 `ToolCtx` 加字段**（11 个构造点），不要给 `RuntimeDeps` 加字段（14 个构造点）。
7. **不要拆 `Event` 枚举**（§9 有设计草图，但本次明确不实施）。
8. **所有测试离线**，不调用真实模型，不需要 API key。基线 401 测试全绿，不需要联网。
9. 每节结束前**必须** `cargo fmt --all`（本文代码块是手写排版，未格式化；不跑 fmt 会留下 `--check` diff）；`cargo clippy --workspace --all-targets` 必须零警告。
10. **不要贴任何看起来像真实密钥的字符串进仓库**（GitHub Push Protection 会拒收，见 §8.3）。
11. 行号会漂移：用「函数名 / 结构体名 / 唯一字符串」当锚点，别死记行号。

---

## 1. 环境与基线

### 1.1 确认基线

```bash
cd Zero
git log --oneline -1          # 必须是 0dc655c
cargo test --workspace 2>&1 | grep -oE "^test result: ok\. [0-9]+ passed" | awk '{s+=$4} END {print s}'
# 必须输出 401
```

### 1.2 若 cargo 拉依赖卡住

直连 crates.io 在本环境会卡 20 分钟以上。配镜像（只改 `~/.cargo/config.toml`，**不要提交**）：

```toml
[source.crates-io]
replace-with = 'rsproxy-sparse'
[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"
[net]
git-fetch-with-cli = true
```

### 1.3 本次最终目标

| 检查项 | 目标 |
|---|---|
| `cargo build --workspace` | 通过 |
| `cargo test --workspace` | 全绿，通过数 **413**（基线 401 + 新增 12） |
| `cargo clippy --workspace --all-targets` | 0 warning |
| `cargo fmt --all --check` | 无 diff |
| 旧 jsonl 兼容 | 不受影响（新增字段一律 `#[serde(default)]`） |

---

## 2. 代码地图（改动前必读）

### 2.1 真实签名表（从 `0dc655c` 逐字核对）

| 类型 / 函数 | 位置 | 真实签名 |
|---|---|---|
| `Tool` | `agent-core/src/tool.rs` | `fn name(&self)->&str` / `description` / `parameters()->serde_json::Value` / `risk_level()->Risk` / `is_terminating()->bool` / `async fn execute(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError>` |
| `ToolOutput` | 同上 | `content: String`, `is_error: bool`, `side_effects: Vec<Artifact>`；`ok()` / `err()` / `with_artifacts()` |
| `ToolError` | 同上 | 7 个变体：`InvalidArgs(String)` / `PathEscape(String)` / `Io(#[from] std::io::Error)` / `Timeout` / `Cancelled` / `Blocked(String)` / `Failed(String)` |
| `ToolCall` | `agent-core/src/provider.rs` | `id: ToolCallId`, `name: String`, `arguments: serde_json::Value`（**全仓 18 个构造点**） |
| `ToolDef` | 同上 | `name`, `description`, `parameters` |
| `Decision` | `agent-core/src/perm.rs` | `Allow` / `Deny(String)` / `AskUser { reason, source, matched_rule }` |
| `ApprovalResult` | 同上 | `allow: bool`, `risk: Risk`, `reason: String`, `confidence: f64`, `source: ApprovalSource` |
| `AiReviewer` | 同上 | `async fn review(&self, task: &TaskSpec, call: &ToolCall, static_risk: Risk, timeout: Duration) -> Result<ApprovalResult, String>` |
| `PermissionEngineImpl::check` | `agent-perm/src/lib.rs:87` | 静态 `RuleMatch::Hit` → 立即 `AskUser{source: Static}`；之后才 L1/L3/AI |
| `TaskSpec` | `agent-core/src/task.rs` | 12 个字段 + `new()`；已有 `parent_id` / `depth`（都是 `#[serde(default)]`） |
| `AgentRuntime::stream_once` | `agent-runtime/src/runner.rs:214` | 入参含 `task: &TaskSpec`；`ChatRequest.tools: self.deps.tool_defs.clone()` 在 224 行 |
| `AgentRuntime::execute_tool` | 同上 `:888` | 返回 `Result<ToolExecuted, AgentError>`；`ToolExecuted { content, is_error, artifacts }` 定义在 343 行，**共 3 个构造点** |
| `ToolExecuted` 消费处 | 同上 `:729` 起 | `let redacted = ...; let model_content = if user_approved {...} else {...};`；`Event::ToolCallEnd.detail` 与 `output` 都来自 `redacted` |
| `Event::ToolCallEnd` | `agent-core/src/event.rs:63` | `id, call_id, outcome: ToolOutcome, ms: u64, detail: Option<String>, output: Option<String>`（**全仓 27 个构造点，一律不改**） |
| `SubagentPort` | `agent-core/src/subagent.rs` | 只有一个方法 `async fn run_subtask(&self, parent_task_id: &str, spec: SubtaskSpec, cancel: CancellationToken) -> Result<SubtaskResult, AgentError>` |
| `SubtaskSpec` | 同上 | `goal`, `context`, `expected_output`, `model`, `max_iterations`, `budget_usd`（后 5 个 `#[serde(default)]`） |
| `RuntimeSubagent` | `agent-runtime/src/subagent.rs` | `live: AtomicUsize`, `spawned: AtomicUsize`, `budget: BudgetCell`；常量 `MAX_LIVE_CHILDREN=1` / `MAX_DEPTH=1` |
| `DelegateTool` | `agent-tools/src/delegate.rs` | `execute` 里 `serde_json::from_value::<SubtaskSpec>` → `port.run_subtask(...)`；失败用 `ToolOutput::err(...)` 回填 |

### 2.2 关键构造点数量（决定「能改什么」）

| 目标 | 构造点数 | 结论 |
|---|---|---|
| `ToolCall { ... }` | 18 | **不改结构**，否则全面波及 provider/perm/context/测试 |
| `Event::ToolCallEnd { ... }` | 27 | **不新增字段**，稳定错误码走 `model_content` 前缀 + `detail` 前缀 |
| `ToolExecuted { ... }` | 3 | 可以加字段（本次加 `error_code`） |
| `ToolOutput { ... }` | 2（`ok`/`err` 内部） | 可以加字段（本次加 `error_code`） |
| `TaskSpec::new` | 1 | 可以加字段（本次加 `tool_allowlist`） |
| `RuntimeDeps { ... }` | 14 | **不加字段** |

### 2.3 定位命令

```bash
grep -n "MALFORMED_ARGS_KEY\|__malformed_args_error" --include=*.rs crates apps
grep -n "struct ToolExecuted\|ToolExecuted {" crates/agent-runtime/src/runner.rs
grep -n "tools: self.deps.tool_defs\|tools: self.deps.tool_defs.iter" crates/agent-runtime/src/runner.rs
grep -n "fn check" crates/agent-perm/src/lib.rs
grep -rn "tool_defs.retain" crates/agent-runtime/src/subagent.rs
```

---

## 3. 设计决策（与 dsh 逐条对照）

| # | dsh 手法 | Zero 现状 | 本文做法 | 为什么不照抄 |
|---|---|---|---|---|
| 3.1 | `ToolErrorInfo` 稳定 code | 错误是自由文本，模型靠读中文重试 | **C0**：`ToolErrorCode` 枚举 + `ToolError::code()` + `ToolOutput.error_code`，模型看到 `[error:timeout]` 这类稳定前缀 | 不新增 `Event::ToolCallEnd` 字段——27 个构造点不值得动 |
| 3.2 | `args: unknown` + `ToolArgsError` 类型化 | `__malformed_args_error` 字符串哨兵塞进 `arguments`，约定只在 runner 里 | **C1**：`MalformedArgs` 类型（常量 + 构造 + 解析）收口，runner 不再持有魔法字符串 | **不改 `Tool::execute` 签名**：18 个 `ToolCall` 构造点 + 10 个工具实现 + serde 兼容，代价远大于收益 |
| 3.3 | `ToolGuard` 返回类型无 `allow`，只能减权 | 静态命中先返回，AI 根本不会被调用——**行为正确但没有类型保证** | **C2**：`StaticVerdict` 新类型（私有元组字段 + `into_decision()`）+ 3 档契约测试 | 不改 `Decision` 枚举（会波及 TUI/审批/回放） |
| 3.4 | `ToolRestriction` 按 scope 交集过滤 | 子任务「看不到 delegate」硬编码在 `RuntimeSubagent` 的两处 | **C3**：`TaskSpec.tool_allowlist` + `allows_tool()`，在 `stream_once`（模型可见）与 `execute_tool`（可执行）统一收口；子任务改用同一机制 | 不引入 `ScopeKey` / `isolate realm`——Zero 只有父子两层 |
| 3.5 | `SubagentCapabilities` 启动前能力校验 | `SubtaskSpec` 的 5 个可选字段一律静默处理 | **C4**：port 加 `capabilities()` / `capacity()` **默认方法**，`DelegateTool` 对不支持的选项明确报错 | 默认方法保证**现有实现零改动**；不做插件式能力注册 |
| 3.6 | `SubagentCapacity { running, limit }` | `live` / `spawned` 私有，撞上限只给一句错误 | **C4**：同上一并暴露，失败信息带上 `running/limit` | 不做独立查询工具，模型不需要多一次调用 |
| 3.7 | 三域事件分离（持久/实时/能力） | 单 `Event` 大枚举 + `classify` 裁决 | **本次不实施**，§9 给设计草图 | 拆分要动所有事件生产/消费点，属于二期重构，混进本次会让低成本执行者失控 |
| 3.8 | —（v1.0 的 6 处落地坑） | **已修**：`marker()` 命名、`Command` 体积告警豁免、数组替代 `vec!`、trait 整段替换、`detail` 按上下文定位、每节 fmt | 见顶部「验证状态」与 §7 | 这 6 条是真正跑过才暴露的，照 v1.0 做会卡住 |

### 三条贯穿全文的约束

- **稳定 code 只进「模型可见内容」和 `detail`，不进 `output` 原文**。`crates/agent-runtime/src/runner.rs` 有测试断言 `ToolCallEnd.output == Some("[error] 工具自报失败")`，动原文会红。
- **`[error]` 前缀不要重复加**。`execute_tool` 统一加 `[error] `；模型可见内容里我们把它**替换**成 `[error:<code>] `，不是叠加。
- **新字段一律 `#[serde(default)]`**，保证旧 jsonl 能反序列化。

---

## 4. 可复制的类型定义（§5 会反复引用）

### 4.1 `ToolErrorCode`（`agent-core/src/tool.rs`）

```rust
/// 稳定的工具错误码。
///
/// 模型可以据此采取不同策略：timeout 换策略重试、invalid_args 修参数、
/// denied / approval_rejected 不要再试、not_permitted 换工具。
/// 不要在这些码里塞人类可读的中文说明——说明放 message。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorCode {
    InvalidArgs,
    PathEscape,
    Io,
    Timeout,
    Cancelled,
    Blocked,
    ToolFailed,
    UnknownTool,
    MalformedArgs,
    NotPermitted,
    Denied,
    ApprovalRejected,
    UnsupportedOption,
    DelegationFailed,
}

impl ToolErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgs => "invalid_args",
            Self::PathEscape => "path_escape",
            Self::Io => "io",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Blocked => "blocked",
            Self::ToolFailed => "tool_failed",
            Self::UnknownTool => "unknown_tool",
            Self::MalformedArgs => "malformed_args",
            Self::NotPermitted => "not_permitted",
            Self::Denied => "denied",
            Self::ApprovalRejected => "approval_rejected",
            Self::UnsupportedOption => "unsupported_option",
            Self::DelegationFailed => "delegation_failed",
        }
    }
}

impl std::fmt::Display for ToolErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
```

### 4.2 `MalformedArgs`（`agent-core/src/tool.rs`）

```rust
/// 「模型发来的参数不是合法 JSON 对象」的标记。
///
/// 收口成类型的原因：原先 `__malformed_args_error` 是个散落在 runner 里的
/// 魔法字符串，任何想判断「这次调用参数坏了」的代码都得自己拼这个键。
#[derive(Debug, Clone, Copy)]
pub struct MalformedArgs;

impl MalformedArgs {
    pub const KEY: &'static str = "__malformed_args_error";

    pub fn marker(reason: impl Into<String>) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        m.insert(Self::KEY.to_string(), serde_json::Value::String(reason.into()));
        serde_json::Value::Object(m)
    }

    pub fn reason(args: &serde_json::Value) -> Option<&str> {
        args.get(Self::KEY).and_then(|v| v.as_str())
    }
}
```

### 4.3 `StaticVerdict`（`agent-core/src/perm.rs`）

```rust
/// 静态规则闸门的输出：类型上不允许被后续 AI 审视覆盖。
///
/// 元组字段私有，外部只能 `into_decision()`，没有「改成 Allow」的入口。
/// 这就是 dsh「ToolGuard 返回类型不含 allow」在 Zero 里的等价物。
#[derive(Debug, Clone)]
pub struct StaticVerdict(Decision);

impl StaticVerdict {
    pub fn ask(rule: impl Into<String>) -> Self {
        let rule = rule.into();
        Self(Decision::AskUser {
            reason: format!("高危操作被静态规则拦截：{rule}"),
            source: ApprovalSource::Static,
            matched_rule: Some(rule),
        })
    }

    pub fn into_decision(self) -> Decision {
        self.0
    }
}
```

### 4.4 `SubagentCapabilities` / `SubagentCapacity`（`agent-core/src/subagent.rs`）

```rust
/// 委派后端声明自己支持什么。DelegateTool 在调用前据此拒绝不支持的选项，
/// 而不是静默忽略（模型会以为「我指定了便宜模型」但根本没生效）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentCapabilities {
    pub supports_model_override: bool,
    pub supports_budget: bool,
    pub supports_context: bool,
    pub max_depth: u8,
}

impl Default for SubagentCapabilities {
    fn default() -> Self {
        Self {
            supports_model_override: true,
            supports_budget: true,
            supports_context: true,
            max_depth: 1,
        }
    }
}

/// 当前委派树的并发占用。`limit == 0` 表示「后端不提供容量信息」。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentCapacity {
    pub running: usize,
    pub limit: usize,
}
```

### 4.5 常量表

| 常量 | 位置 | 值 |
|---|---|---|
| `MALFORMED_ARGS_KEY` | 原 `runner.rs:33` | C1 删除，改用 `MalformedArgs::KEY` |
| `MAX_LIVE_CHILDREN` | `agent-runtime/src/subagent.rs` | `1usize` |
| `MAX_DEPTH` | 同上 | `1u8` |
| `DELEGATE_TOOL_TIMEOUT` | `runner.rs` | `Duration::from_secs(330)`（不动） |

---

## 5. 逐 commit 实施

### C0 — 稳定错误码

**目标**：工具失败时模型看到 `[error:<code>]`，审计 `detail` 也带 code；`output` 原文不变；行为零回归。

#### 步骤 0.1 `crates/agent-core/src/tool.rs`

**(a)** 在 `pub struct ToolOutput {` 的 `side_effects` 字段之后加一个字段：

```rust
    /// 稳定错误码（仅 is_error=true 时有意义）。旧日志没有 → None。
    #[serde(default)]
    pub error_code: Option<ToolErrorCode>,
```

**(b)** `impl ToolOutput` 里 `ok` / `err` 两个构造函数都补 `error_code: None`，并新增 `err_code`：

把整个 `impl ToolOutput { ... }` 替换成：

```rust
impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            side_effects: Vec::new(),
            error_code: None,
        }
    }
    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            side_effects: Vec::new(),
            error_code: None,
        }
    }
    /// 带稳定错误码的失败。content 里**不要**自己写 `[error]` 前缀：
    /// execute_tool 会统一加，模型可见内容再替换成 `[error:<code>]`。
    pub fn err_code(code: ToolErrorCode, content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            side_effects: Vec::new(),
            error_code: Some(code),
        }
    }
    pub fn with_artifacts(mut self, a: Vec<Artifact>) -> Self {
        self.side_effects = a;
        self
    }
}
```

**(c)** 在 `ToolError` 枚举**之后**追加 §4.1 的 `ToolErrorCode` 全文，以及：

```rust
impl ToolError {
    /// 稳定错误码映射：新增 ToolError 变体时必须同步这里（match 是穷尽的，
    /// 编译器会强制你补）。
    pub fn code(&self) -> ToolErrorCode {
        match self {
            Self::InvalidArgs(_) => ToolErrorCode::InvalidArgs,
            Self::PathEscape(_) => ToolErrorCode::PathEscape,
            Self::Io(_) => ToolErrorCode::Io,
            Self::Timeout => ToolErrorCode::Timeout,
            Self::Cancelled => ToolErrorCode::Cancelled,
            Self::Blocked(_) => ToolErrorCode::Blocked,
            Self::Failed(_) => ToolErrorCode::ToolFailed,
        }
    }
}
```

**(d)** 在文件末尾加 §4.2 的 `MalformedArgs` 全文（C1 才用，但类型先放这里，避免两次改同一文件）和测试模块：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_error_codes_are_stable() {
        assert_eq!(ToolError::Timeout.code().as_str(), "timeout");
        assert_eq!(
            ToolError::InvalidArgs("x".into()).code(),
            ToolErrorCode::InvalidArgs
        );
        assert_eq!(ToolErrorCode::UnsupportedOption.as_str(), "unsupported_option");
        assert_eq!(format!("{}", ToolErrorCode::NotPermitted), "not_permitted");
        let out = ToolOutput::err_code(ToolErrorCode::Blocked, "被挡了");
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ToolErrorCode::Blocked));
    }

    #[test]
    fn malformed_args_roundtrip() {
        let v = MalformedArgs::marker("bad json");
        assert_eq!(MalformedArgs::reason(&v), Some("bad json"));
        assert_eq!(
            MalformedArgs::reason(&serde_json::json!({"path": "a.txt"})),
            None,
            "正常参数不能被误判为 malformed"
        );
        assert_eq!(v[MalformedArgs::KEY], serde_json::json!("bad json"));
    }
}
```

**(e)** `crates/agent-core/src/lib.rs`：把

```rust
pub use tool::{Tool, ToolCtx, ToolError, ToolOutput};
```

改成

```rust
pub use tool::{MalformedArgs, Tool, ToolCtx, ToolError, ToolErrorCode, ToolOutput};
```

#### 步骤 0.2 `crates/agent-runtime/src/runner.rs`

**(a)** import 行 `use agent_core::tool::{Tool, ToolCtx};` 改成：

```rust
use agent_core::tool::{MalformedArgs, Tool, ToolCtx, ToolErrorCode};
```

> `MalformedArgs` 在 C1 才用；这里先导入不会报 unused（C1 会用）。若你按 commit 边界先跑 C0，会有一条 unused import 警告——**C0 结束时把 `MalformedArgs` 从这行去掉，C1 再加回来**，或者干脆一次做到 C1 再跑 clippy。本文推荐后者：C0/C1 连做，只在 C1 末尾跑 clippy。

**(b)** `struct ToolExecuted` 加字段：

```rust
struct ToolExecuted {
    /// 回填给模型的内容（大输出已外置，出错时带 `[error]` 前缀）。
    content: String,
    /// 工具自报失败（如 `web_search` 所有后端都没搜到结果）。
    is_error: bool,
    artifacts: Vec<String>,
    /// 稳定错误码；run_inner 据此把 `[error] ` 替换成 `[error:<code>] `。
    error_code: Option<ToolErrorCode>,
}
```

**(c)** 三个 `ToolExecuted` 构造点各补一行。

第 1 个（unknown tool）：

```rust
            return Ok(ToolExecuted {
                content: format!("[error] unknown tool: {}", call.name),
                is_error: true,
                artifacts: vec![],
                error_code: Some(ToolErrorCode::UnknownTool),
            });
```

第 2 个（超时）：

```rust
                return Ok(ToolExecuted {
                    content: format!("[error] 工具执行超时（{}s），已中止", ctx.timeout.as_secs()),
                    is_error: true,
                    artifacts: vec![],
                    error_code: Some(ToolErrorCode::Timeout),
                })
```

第 3 个（正常返回）——把原来的 `let content = if out.is_error {...} else {...};` 与 `Ok(ToolExecuted {...})` 整段替换成：

```rust
        let error_code = out
            .error_code
            .or(if out.is_error { Some(ToolErrorCode::ToolFailed) } else { None });

        let content = if out.is_error {
            format!("[error] {content}")
        } else {
            content
        };

        // 注意：这里**不落盘**。事件一律由执行循环统一发到总线，
        // 交给 `agent_store::recorder` 单点裁决，避免同一条事件被写两遍。
        Ok(ToolExecuted {
            content,
            is_error: out.is_error,
            artifacts: arts,
            error_code,
        })
```

> `ToolErrorCode` 是 `Copy`，`out.error_code` 读走不会 move 掉 `out`（后面还要用 `out.is_error`）。

**(d)** `run_inner` 里 `Ok(r) => {` 分支的 `model_content` 计算，把：

```rust
                        let model_content = if user_approved {
                            format!("[user-approved] {redacted}")
                        } else {
                            redacted.clone()
                        };
```

改成：

```rust
                        // 稳定错误码只进「模型可见内容」，不进 output 原文：
                        // output 是事后审计「工具到底返回了什么」的唯一依据，
                        // 已有测试断言它的字面值，不能被前缀污染。
                        let model_content = match (r.error_code, user_approved) {
                            (Some(code), _) => {
                                let s = redacted.strip_prefix("[error] ").unwrap_or(&redacted);
                                format!("[error:{code}] {s}")
                            }
                            (None, true) => format!("[user-approved] {redacted}"),
                            (None, false) => redacted.clone(),
                        };
```

> `r.error_code` 是 `Option<ToolErrorCode>`（`Copy`），`match` 后 `r` 仍可用（后面用了 `r.artifacts`、`r.is_error`）。

**(e)** `run_inner` 里 `Err(e) => {` 分支（工具抛 `ToolError` 经 `AgentError::Tool` 上来）改成：

```rust
                    Err(e) => {
                        let code = match &e {
                            AgentError::Tool(t) => Some(t.code()),
                            _ => None,
                        };
                        let text = match code {
                            Some(c) => format!("[error:{c}] {e}"),
                            None => format!("[error] {e}"),
                        };
                        tool_messages.push(Message {
                            role: Role::Tool,
                            content: vec![ContentPart::text(text)],
                            tool_calls: vec![],
                            tool_call_id: Some(call_id.clone()),
                            reasoning: None,
                        });
                        let _ = bus.send(Event::ToolCallEnd {
                            id: task_id.clone(),
                            call_id: call_id.clone(),
                            outcome: ToolOutcome::Failed,
                            ms,
                            detail: Some(match code {
                                Some(c) => format!("[{c}] {e}"),
                                None => e.to_string(),
                            }),
                            output: None,
                        });
                    }
```

**(f)** 三个「不经过 `execute_tool`」的失败分支，给 `detail` 加 code 前缀（模型侧文本保持不变，避免动存量断言）：

- malformed args 分支：`detail: Some("malformed JSON arguments".to_string()),` →
  `detail: Some("[malformed_args] malformed JSON arguments".to_string()),`
- `Decision::Deny(reason)` 分支：`detail: Some(reason),` → `detail: Some(format!("[denied] {reason}")),`
- `ApprovalReply::Reject { reason }` 分支：`detail: Some(reason),` → `detail: Some(format!("[approval_rejected] {reason}")),`

> 这两处 `Some(reason)` 在文件里各出现一次吗？不一定。用**上下文**定位：`outcome: ToolOutcome::Denied` 那一块、`outcome: ToolOutcome::Rejected` 那一块。不要全局替换。
> 另外 **Rejected 分支的缩进比 Deny 分支多 4 个空格**（它在更深一层的 `match` 里），照抄补丁时按 `outcome:` 那一行的实际缩进对齐，不要硬套上面的空格数。

**(g)** system prompt 里说明前缀的那行（约 204 行）：

```rust
             [user-approved] 开头；被拒绝会以 [rejected by user] 或 [denied] 开头。\n\
```

改成：

```rust
             [user-approved] 开头；被拒绝会以 [rejected by user] 或 [denied] 开头。\n\
             工具失败可能以 [error:错误码] 开头（如 [error:timeout]、[error:invalid_args]）：\n\
             timeout 请换策略重试，invalid_args 请修正参数，denied / approval_rejected / not_permitted 不要原样重试。\n\
```

#### 步骤 0.3 C0 新增测试

在 `crates/agent-runtime/src/runner.rs` 的 `#[cfg(test)] mod tests` 里追加（用该文件已有的 `make_runtime` 夹具）：

```rust
    #[tokio::test]
    async fn unknown_tool_has_stable_error_code() {
        let (runtime, _store, _tmp) = make_runtime(vec![], LogConfig::default());
        let task = TaskSpec::new("t", "/tmp/ws");
        let call = ToolCall {
            id: ToolCallId::new(),
            name: "no_such_tool".into(),
            arguments: serde_json::json!({}),
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut cm = ContextManager::new(ContextConfig::default(), "sys");
        let r = runtime
            .execute_tool(&call, &task, &cancel, &mut cm)
            .await
            .unwrap();
        assert!(r.is_error);
        assert_eq!(r.error_code, Some(ToolErrorCode::UnknownTool));
        assert!(r.content.starts_with("[error]"));
    }
```

#### 步骤 0.4 验收

```bash
cargo fmt --all
cargo test -p agent-core -p agent-runtime 2>&1 | grep -E "test result|FAILED|error\[" | head -10
git add -A && git commit -m "feat(interface): 工具失败引入稳定错误码 ToolErrorCode"
```

**别在这节单独跑 clippy**（原因见 0.2(a) 的说明），C1 末尾一起跑。

---

### C1 — 收口 malformed args 哨兵

**目标**：魔法字符串只存在一处，且是类型 API；runner 不再持有常量。

#### 步骤 1.1 `crates/agent-runtime/src/runner.rs`

**(a)** 删除顶部常量：

```rust
/// 工具参数解析失败时的标记键：模型发来的 args 不是合法 JSON 对象时，
/// 执行循环据此回填一条明确错误（而不是把 Null 当「没传参数」静默执行）。
const MALFORMED_ARGS_KEY: &str = "__malformed_args_error";
```

整段删掉（含注释）。

**(b)** `stream_once` 里构造 `arguments` 的 match（约 300 行），把两处 `serde_json::json!({ MALFORMED_ARGS_KEY: ... })` 改成 `MalformedArgs::marker(...)`：

```rust
            let arguments = match serde_json::from_str::<serde_json::Value>(&args) {
                Ok(v) if v.is_object() => v,
                Ok(v) => MalformedArgs::marker(format!("参数不是 JSON 对象：{v}")),
                Err(e) => {
                    let raw: String = args.chars().take(300).collect();
                    MalformedArgs::marker(format!("JSON 解析失败：{e}；原文：{raw}"))
                }
            };
```

**(c)** `run_inner` 里检测哨兵的那段（约 563 行），把：

```rust
                if let Some(reason) = call
                    .arguments
                    .get(MALFORMED_ARGS_KEY)
                    .and_then(|v| v.as_str())
                {
```

改成：

```rust
                if let Some(reason) = MalformedArgs::reason(&call.arguments) {
```

**(d)** 把该分支回填给模型的文本加上稳定码（C0 已改过 detail；这里只动 content）：

```rust
                    let content = format!(
                        "[error:malformed_args] 工具参数不是合法 JSON，未执行：{reason}\n\
                         请重新完整输出合法的 JSON 参数。"
                    );
```

> 这是唯一一处「直接写 `[error:xxx]` 字符串」的地方，因为它不经过 `execute_tool`。其它路径一律由 `ToolExecuted.error_code` 驱动。

**(e)** 确认 `use agent_core::tool::{MalformedArgs, Tool, ToolCtx, ToolErrorCode};`（C0 已改好）。此时 `MalformedArgs` 真正被使用，clippy 不再报 unused。

#### 步骤 1.2 新增测试

`crates/agent-runtime/src/runner.rs` 的测试模块追加：

```rust
    #[test]
    fn malformed_args_are_built_by_the_typed_helper_not_a_magic_string() {
        let v = MalformedArgs::marker("oops");
        assert!(MalformedArgs::reason(&v) == Some("oops"));
        assert!(v.is_object());
        // 正常参数绝不能被误判
        assert!(MalformedArgs::reason(&serde_json::json!({"cmd": "ls"})).is_none());
    }
```

#### 步骤 1.3 验收（C0+C1 一起）

```bash
cargo fmt --all
cargo build --workspace
cargo test --workspace 2>&1 | grep -E "test result|FAILED" | head -30
cargo clippy --workspace --all-targets 2>&1 | grep -cE "^warning: |^error"   # 必须 0
git add -A && git commit -m "refactor(interface): malformed args 收口为 MalformedArgs 类型 API"
```

---

### C2 — 权限单调性的类型保证 + 契约测试

**目标**：静态规则命中后，任何档位、任何 AI 审视结果都不能把它变成 Allow。**行为不变**，把约定写进类型和测试。

#### 步骤 2.1 `crates/agent-core/src/perm.rs`

在 `Decision` 枚举之后追加 §4.3 的 `StaticVerdict` 全文。`ApprovalSource` 在同文件，无需额外 import。

#### 步骤 2.2 `crates/agent-perm/src/lib.rs`

**(a)** import 行：

```rust
use agent_core::perm::{class_key, ApprovalSource, Decision, PermissionTier, Risk};
```

改成：

```rust
use agent_core::perm::{class_key, ApprovalSource, Decision, PermissionTier, Risk, StaticVerdict};
```

**(b)** `check` 里第一层静态规则那段（约 90 行）替换成：

```rust
        // ---- 第一层：静态规则（先跑、优先级最高）----
        // StaticVerdict 的元组字段私有，且没有 allow 构造器：类型上保证
        // 后面的 L1 / AI 审视不可能把这次命中「翻案」成 Allow。
        if let RuleMatch::Hit { rule, .. } = self.rules.match_call(call) {
            return StaticVerdict::ask(rule).into_decision();
        }
```

> 原有注释里关于「L1 为什么不是 Deny」的解释要保留在文件里（可移到这段上方作为注释），别删掉——那是重要的设计理由。

#### 步骤 2.3 契约测试

`crates/agent-perm/src/lib.rs` 的 `#[cfg(test)] mod tests` 里追加：

```rust
    /// 一个「永远说放行」的审视器：用来证明静态闸门不可能被 AI 覆盖。
    struct AlwaysAllowReviewer;

    #[async_trait]
    impl AiReviewer for AlwaysAllowReviewer {
        async fn review(
            &self,
            _task: &TaskSpec,
            _call: &ToolCall,
            _static_risk: Risk,
            _timeout: Duration,
        ) -> Result<agent_core::perm::ApprovalResult, String> {
            Ok(agent_core::perm::ApprovalResult {
                allow: true,
                risk: Risk::Low,
                reason: "always allow".into(),
                confidence: 1.0,
                source: ApprovalSource::Ai,
            })
        }
    }

    #[tokio::test]
    async fn static_hit_cannot_be_overridden_in_any_tier() {
        for tier in [
            PermissionTier::L1,
            PermissionTier::L2,
            PermissionTier::L3,
        ] {
            let eng = PermissionEngineImpl::new(StaticRules::default(), PermConfig::default())
                .with_reviewer(Box::new(AlwaysAllowReviewer));
            let d = eng
                .check(
                    &call("shell_exec", serde_json::json!({"cmd": "rm -rf /"})),
                    &task(tier),
                )
                .await;
            match d {
                Decision::AskUser {
                    source: ApprovalSource::Static,
                    matched_rule: Some(_),
                    ..
                } => {}
                other => panic!("{tier:?} 下静态命中必须保持 Static 询问，实际: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn delegate_goal_with_dangerous_command_is_still_gated() {
        // 回归：C2 之前加的「委派 goal 内嵌高危命令」拦截不能被 AI 翻案。
        let eng = PermissionEngineImpl::new(StaticRules::default(), PermConfig::default())
            .with_reviewer(Box::new(AlwaysAllowReviewer));
        let d = eng
            .check(
                &call(
                    "delegate",
                    serde_json::json!({"goal": "请执行 rm -rf / 然后汇报"}),
                ),
                &task(PermissionTier::L1),
            )
            .await;
        assert!(
            matches!(d, Decision::AskUser { source: ApprovalSource::Static, .. }),
            "委派夹带高危命令必须走静态闸门，实际: {d:?}"
        );
    }
```

> `AiReviewer`、`ApprovalResult`、`Duration`、`async_trait` 都在该文件的顶层 import / pub use 里（`use super::*` 带进来），无需重复导入。若编译器提示找不到，用全路径 `agent_core::perm::AiReviewer`。

#### 步骤 2.4 验收

```bash
cargo fmt --all
cargo test -p agent-core -p agent-perm 2>&1 | grep -E "test result|FAILED|error\[" | head -10
cargo clippy --workspace --all-targets 2>&1 | grep -cE "^warning: |^error"   # 必须 0
git add -A && git commit -m "refactor(perm): StaticVerdict 类型化静态闸门单调性 + 契约测试"
```

---

### C3 — 工具可见性显式化（`tool_allowlist`）

**目标**：一处规则同时管住「模型能看到哪些工具」和「能执行哪些工具」；子任务不再硬编码过滤 delegate。

#### 步骤 3.1 `crates/agent-core/src/task.rs`

**(a)** 在 `depth: u8` 字段之后加：

```rust
    /// 本任务可见/可执行的工具白名单。None = 不限制（默认，兼容旧日志）。
    /// 子任务由 RuntimeSubagent 设为「父工具集去掉 delegate」。
    #[serde(default)]
    pub tool_allowlist: Option<Vec<String>>,
```

**(b)** `TaskSpec::new` 里 `depth: 0,` 之后加 `tool_allowlist: None,`。

**(c)** 在 `impl TaskSpec` 里（`new` 之后）加方法：

```rust
    /// 工具是否对本任务可见/可执行。None = 全放行。
    pub fn allows_tool(&self, name: &str) -> bool {
        self.tool_allowlist
            .as_ref()
            .is_none_or(|list| list.iter().any(|n| n == name))
    }
```

> `Option::is_none_or` 需要 Rust 1.82+。本仓库锁定的工具链支持；若编译器不认，改成 `map_or(true, |list| ...)` —— 但 clippy 会建议 `is_none_or`，所以优先用前者。

**(d)** 测试：在 `task.rs` 末尾加（若该文件已有 `#[cfg(test)]`，追加到已有模块里）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_allowlist_defaults_to_unrestricted_and_filters_when_set() {
        let mut t = TaskSpec::new("t", "/tmp/ws");
        assert!(t.allows_tool("anything"));
        t.tool_allowlist = Some(vec!["file_read".into(), "delegate".into()]);
        assert!(t.allows_tool("file_read"));
        assert!(!t.allows_tool("shell_exec"));
    }

    #[test]
    fn legacy_task_spec_without_allowlist_deserializes() {
        // 用真实 JSON 形态：旧日志里没有这个字段
        let json = serde_json::json!({
            "id": "t-1",
            "title": "老任务",
            "prompt": "hi",
            "tier": "L2",
            "workspace": "/tmp/ws",
            "model": null,
            "max_iterations": 25,
            "budget_usd": null
        });
        let t: TaskSpec = serde_json::from_value(json).expect("旧日志仍可反序列化");
        assert!(t.tool_allowlist.is_none());
        assert!(t.allows_tool("shell_exec"));
    }
}
```

> 若 `TaskSpec` 的序列化字段名与上面 JSON 不完全一致导致测试红，**以真实 `serde` 输出为准**：先跑 `let s = serde_json::to_string(&TaskSpec::new("t","/tmp/ws")).unwrap(); println!("{s}")`，把打印结果贴进测试再删掉新字段。别猜字段名。

#### 步骤 3.2 `crates/agent-runtime/src/runner.rs`

**(a)** `stream_once` 里 `ChatRequest` 的 `tools` 行：

```rust
            tools: self.deps.tool_defs.clone(),
```

改成：

```rust
            tools: self
                .deps
                .tool_defs
                .iter()
                .filter(|d| task.allows_tool(&d.name))
                .cloned()
                .collect(),
```

**(b)** `Event::ContextSnapshot` 里的 `tools` 行（约 458 行）：

```rust
                    tools: self.deps.tool_defs.iter().map(|t| t.name.clone()).collect(),
```

改成：

```rust
                    tools: self
                        .deps
                        .tool_defs
                        .iter()
                        .filter(|d| task.allows_tool(&d.name))
                        .map(|d| d.name.clone())
                        .collect(),
```

> 这一处和 (a) 必须成对：落盘的快照要能回答「模型当时到底看到了哪些工具」。

**(c)** `execute_tool` 开头，`let Some(tool) = ... else {...};` **之前**插入执行闸门：

```rust
        // 模型可见性（stream_once 过滤 tool_defs）与可执行性（这里）共用同一规则。
        // 即使模型从别处猜到工具名并硬调用，也过不了这一关。
        if !task.allows_tool(&call.name) {
            return Ok(ToolExecuted {
                content: format!("[error] 工具 {} 不在本任务允许的工具集内，未执行", call.name),
                is_error: true,
                artifacts: vec![],
                error_code: Some(ToolErrorCode::NotPermitted),
            });
        }
```

> 现在 `ToolExecuted` 共有 **4** 个构造点。

**(d) `crates/agent-core/src/event.rs`：显式豁免 `large_enum_variant`**（**必做，否则 clippy 报警**）

`TaskSpec` 加了 `Option<Vec<String>>` 之后体积变大，`Command::SubmitTask(TaskSpec)` 与其它变体的体积差跨过 clippy 阈值，会报 `warning: large size difference between variants`。`Command` 是 mpsc 通道里的低频消息（每次提交任务一条），boxing 要改装配层所有权写法而收益极小，所以显式豁免而不是改接口。

把：

```rust
/// 外界 → Agent 的命令。
#[derive(Debug, Clone)]
pub enum Command {
```

改成：

```rust
/// 外界 → Agent 的命令。
///
/// `SubmitTask` 直接按值携带 `TaskSpec`（含若干 Option<Vec<_>> 字段，体积较大）。
/// 这是 mpsc 通道里的低频消息（每次提交任务一条），boxing 会改动装配层的
/// 所有权写法而收益极小；C3 加了 `tool_allowlist` 后 TaskSpec 跨过了
/// clippy 的体积阈值，这里显式豁免而不是为了消警告去改接口。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Command {
```

> 不要试图改成 `SubmitTask(Box<TaskSpec>)`：`apps/agent-cli/src/main.rs` 里 `spec.clone()` 是给 `port.register` 的 `TaskSpec`，`runtime.run(spec, ...)` 也要值传递，boxing 会连带改 3 处以上所有权写法。

#### 步骤 3.3 `crates/agent-runtime/src/subagent.rs`

把 `run_child` 里「子 deps：Clone 之后只覆写工具集与预算」那一段的**工具过滤两处**替换掉。原文：

```rust
        // ---- 子 deps：Clone 之后只覆写工具集与预算 ----
        let mut child_deps: RuntimeDeps = parent.deps.clone();
        child_deps.tool_defs.retain(|d| d.name != "delegate");
        let inner: ToolLookup = parent.deps.tools.clone();
        child_deps.tools = Arc::new(move |name: &str| {
            // 递归防护第二道闸：子任务连工具都查不到 delegate。
            if name == "delegate" {
                None
            } else {
                inner(name)
            }
        });
        if let Some(cap) = child.budget_usd {
```

改成：

```rust
        // ---- 子 deps：Clone 之后只覆写预算；工具集用统一的 allowlist 规则 ----
        let mut child_deps: RuntimeDeps = parent.deps.clone();
        // 递归防护：子任务看不到、也调不到 delegate。
        // 刻意不再在子 deps 里硬编码过滤——stream_once / execute_tool 会用
        // 同一条 TaskSpec::allows_tool 规则处理，避免两处逻辑漂移。
        child.tool_allowlist = Some(
            parent
                .deps
                .tool_defs
                .iter()
                .map(|d| d.name.clone())
                .filter(|n| n != "delegate")
                .collect(),
        );
        if let Some(cap) = child.budget_usd {
```

**同时删掉**文件顶部的 `use crate::runner::ToolLookup;`（现在没人用了，否则 clippy `unused_imports` 警告）。`Arc` 仍被 `ParentCtx` / `RuntimeSubagent` 用到，保留。

> 注意：`child` 在后面被 move 进 `runtime.run(child, ...)`，所以 `child.tool_allowlist = ...` 必须在那之前（这里就是）。`child_deps` 仍会 `clone()` 给 port 之外的用途吗？不会，直接 `AgentRuntime::new(child_deps)`。

#### 步骤 3.4 新增测试

**(a)** `crates/agent-runtime/src/runner.rs` 测试模块追加：

```rust
    #[tokio::test]
    async fn tool_allowlist_blocks_execution_with_stable_code() {
        let (runtime, _store, _tmp) = make_runtime(vec![], LogConfig::default());
        let mut task = TaskSpec::new("t", "/tmp/ws");
        task.tool_allowlist = Some(vec!["ok_tool".into()]);
        let call = ToolCall {
            id: ToolCallId::new(),
            name: "fail_tool".into(),
            arguments: serde_json::json!({}),
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut cm = ContextManager::new(ContextConfig::default(), "sys");
        let r = runtime
            .execute_tool(&call, &task, &cancel, &mut cm)
            .await
            .unwrap();
        assert!(r.is_error);
        assert_eq!(r.error_code, Some(ToolErrorCode::NotPermitted));
    }
```

**(b)** `crates/agent-runtime/src/subagent.rs` 测试模块追加（验证子任务自动排除 delegate）：

```rust
    #[tokio::test]
    async fn child_tool_allowlist_excludes_delegate() {
        // 用 C1 已有的测试夹具思路：直接构造最小 deps，跑一次 run_subtask
        // 太重；这里只验证规则本身——run_child 用的是同一段表达式。
        let defs = [
            agent_core::provider::ToolDef {
                name: "delegate".into(),
                description: "".into(),
                parameters: serde_json::json!({}),
            },
            agent_core::provider::ToolDef {
                name: "file_read".into(),
                description: "".into(),
                parameters: serde_json::json!({}),
            },
        ];
        let allow: Vec<String> = defs
            .iter()
            .map(|d| d.name.clone())
            .filter(|n| n != "delegate")
            .collect();
        let mut task = TaskSpec::new("child", "/tmp/ws");
        task.tool_allowlist = Some(allow);
        assert!(!task.allows_tool("delegate"));
        assert!(task.allows_tool("file_read"));
    }
```

> 这个测试看着「只测了一遍同样的表达式」，它真正的价值是**把「子任务必须排除 delegate」变成绿色断言**：将来有人改了过滤条件，红灯会立刻出现。

#### 步骤 3.5 验收

```bash
cargo fmt --all
cargo build --workspace
cargo test --workspace 2>&1 | grep -E "test result|FAILED" | head -30
cargo clippy --workspace --all-targets 2>&1 | grep -cE "^warning: |^error"   # 必须 0
git add -A && git commit -m "feat(interface): TaskSpec.tool_allowlist 统一模型可见性与可执行性"
```

---

### C4 — 委派能力协商与容量可观察

**目标**：`SubagentPort` 声明能力，`DelegateTool` 对不支持的选项明确报错；失败信息带 `running/limit`。

#### 步骤 4.1 `crates/agent-core/src/subagent.rs`

**(a)** 在 `SubtaskResult` 之后追加 §4.4 的两个类型全文。

**(b)** `SubagentPort` trait 加两个**默认方法**（关键：现有实现零改动）。
**下面这个代码块是「整个 trait 定义的替换版」——用整段 `pub trait SubagentPort { ... }` 换掉文件里原有的那个，不是往里插两行。** 当成增量补丁会留下重复的 `run_subtask` 残段，直接语法错（`unexpected closing delimiter`）。

```rust
#[async_trait]
pub trait SubagentPort: Send + Sync {
    /// 后端声明自己支持什么。默认「全支持」，保证既有实现行为不变。
    fn capabilities(&self) -> SubagentCapabilities {
        SubagentCapabilities::default()
    }

    /// 当前并发占用。`limit == 0` 表示不提供容量信息。
    fn capacity(&self) -> SubagentCapacity {
        SubagentCapacity::default()
    }

    async fn run_subtask(
        &self,
        parent_task_id: &str,
        spec: SubtaskSpec,
        cancel: CancellationToken,
    ) -> Result<SubtaskResult, AgentError>;
}
```

**(c)** `crates/agent-core/src/lib.rs`：把

```rust
pub use subagent::{SubagentPort, SubtaskResult, SubtaskSpec};
```

改成

```rust
pub use subagent::{
    SubagentCapabilities, SubagentCapacity, SubagentPort, SubtaskResult, SubtaskSpec,
};
```

#### 步骤 4.2 `crates/agent-runtime/src/subagent.rs`

在 `impl RuntimeSubagent {` 里（`run_child` 之前，第 147 行附近）加两个方法：

```rust
    fn capabilities(&self) -> SubagentCapabilities {
        SubagentCapabilities {
            max_depth: MAX_DEPTH,
            ..SubagentCapabilities::default()
        }
    }

    fn capacity(&self) -> SubagentCapacity {
        SubagentCapacity {
            running: self.live.load(Ordering::Relaxed),
            limit: MAX_LIVE_CHILDREN,
        }
    }
```

**但是**这两个方法是 trait 方法，必须写在 `impl SubagentPort for RuntimeSubagent` 块里，不是固有 impl。找到：

```rust
#[async_trait]
impl SubagentPort for RuntimeSubagent {
    async fn run_subtask(
```

改成：

```rust
#[async_trait]
impl SubagentPort for RuntimeSubagent {
    fn capabilities(&self) -> SubagentCapabilities {
        SubagentCapabilities {
            max_depth: MAX_DEPTH,
            ..SubagentCapabilities::default()
        }
    }

    fn capacity(&self) -> SubagentCapacity {
        SubagentCapacity {
            running: self.live.load(Ordering::Relaxed),
            limit: MAX_LIVE_CHILDREN,
        }
    }

    async fn run_subtask(
```

并确认顶部的 import 变成：

```rust
use agent_core::subagent::{
    SubagentCapabilities, SubagentCapacity, SubagentPort, SubtaskResult, SubtaskSpec,
};
```

> `Ordering` 和 `MAX_LIVE_CHILDREN` / `MAX_DEPTH` 该文件已有，无需新增 import。

#### 步骤 4.3 `crates/agent-tools/src/delegate.rs`

**(a)** import 行：

```rust
use agent_core::subagent::{SubagentPort, SubtaskResult, SubtaskSpec};
use agent_core::tool::{Tool, ToolCtx, ToolError, ToolOutput};
```

改成：

```rust
use agent_core::subagent::{SubagentPort, SubtaskResult, SubtaskSpec};
use agent_core::tool::{Tool, ToolCtx, ToolError, ToolErrorCode, ToolOutput};
```

**(b)** `execute` 里 `let spec: SubtaskSpec = ...` 之后、`if ctx.cancel.is_cancelled()` 之前插入能力校验：

```rust
        let caps = self.port.capabilities();
        let unsupported = [
            (spec.model.is_some() && !caps.supports_model_override, "model"),
            (spec.budget_usd.is_some() && !caps.supports_budget, "budget_usd"),
            (spec.context.is_some() && !caps.supports_context, "context"),
        ]
        .into_iter()
        .find(|(bad, _)| *bad)
        .map(|(_, key)| key);
        if let Some(key) = unsupported {
            return Ok(ToolOutput::err_code(
                ToolErrorCode::UnsupportedOption,
                format!(
                    "当前委派后端不支持选项 {key}，未执行子任务；请去掉该参数后重试"
                ),
            ));
        }
```

**(c)** `Err(e) =>` 分支改成带容量：

```rust
            Err(e) => {
                let c = self.port.capacity();
                Ok(ToolOutput::err_code(
                    ToolErrorCode::DelegationFailed,
                    format!(
                        "委派失败，未执行子任务：{e}（当前子任务 {}/{}）",
                        c.running, c.limit
                    ),
                ))
            }
```

> 注意：`delegate_failure_is_a_tool_error_not_a_task_failure` 断言 `!out.content.starts_with("[error]")` —— `err_code` 的 content 不带前缀，仍然通过；`assert!(out.content.contains("委派失败"))` 也仍然通过。

#### 步骤 4.4 新增测试

`crates/agent-tools/src/delegate.rs` 测试模块里追加：

```rust
    /// 一个「什么都不支持」的后端：验证 DelegateTool 会明确拒绝而不是静默忽略。
    struct NoOptionsPort;

    #[async_trait]
    impl SubagentPort for NoOptionsPort {
        fn capabilities(&self) -> agent_core::subagent::SubagentCapabilities {
            agent_core::subagent::SubagentCapabilities {
                supports_model_override: false,
                supports_budget: false,
                supports_context: false,
                max_depth: 1,
            }
        }

        async fn run_subtask(
            &self,
            _parent_task_id: &str,
            _spec: SubtaskSpec,
            _cancel: CancellationToken,
        ) -> Result<SubtaskResult, AgentError> {
            panic!("能力校验应先拦下，不应走到这里")
        }
    }

    #[tokio::test]
    async fn unsupported_option_is_rejected_before_dispatch() {
        let tool = DelegateTool::new(Arc::new(NoOptionsPort));
        let out = tool
            .execute(json!({ "goal": "做事", "model": "cheap-model" }), &ctx())
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ToolErrorCode::UnsupportedOption));
        assert!(out.content.contains("model"));
        assert!(!out.content.starts_with("[error]"), "前缀由 execute_tool 统一加");
    }

    #[test]
    fn default_capabilities_keep_existing_ports_compatible() {
        // FakePort 没有实现 capabilities()/capacity()，必须走默认值且不报错。
        let port = FakePort { fail: false };
        let caps = SubagentPort::capabilities(&port);
        assert!(caps.supports_model_override);
        assert!(caps.supports_budget);
        assert_eq!(caps.max_depth, 1);
        assert_eq!(SubagentPort::capacity(&port).limit, 0);
    }
```

> 测试模块顶部需要 `use agent_core::tool::ToolErrorCode;`（或把断言写成全路径 `agent_core::tool::ToolErrorCode::UnsupportedOption`）。若 `SubagentPort::capabilities(&port)` 这种 UFCS 写法编译不过，改成先 `let p: &dyn SubagentPort = &port; p.capabilities()`。

#### 步骤 4.5 验收

```bash
cargo fmt --all
cargo test -p agent-core -p agent-runtime -p agent-tools 2>&1 | grep -E "test result|FAILED|error\[" | head -12
cargo clippy --workspace --all-targets 2>&1 | grep -cE "^warning: |^error"   # 必须 0
git add -A && git commit -m "feat(subagent): 端口能力协商 capabilities/capacity + delegate 前置校验"
```

---

### C5 — 全量回归与收尾

```bash
cargo fmt --all --check
cargo build --workspace
cargo test --workspace 2>&1 | grep -E "test result|FAILED" | head -40
cargo test --workspace 2>&1 | grep -oE "^test result: ok\. [0-9]+ passed" | awk '{s+=$4} END {print "通过:"s}'
cargo clippy --workspace --all-targets 2>&1 | grep -cE "^warning: |^error"     # 必须 0
git status --short                                                             # 必须干净（都已提交）
git add -A && git commit -m "test(interface): 接口加固回归测试补齐"             # 若有遗漏改动
```

**预期**：全绿；通过数 ≥ 401 + 新增测试数；clippy 0；fmt 无 diff。见 §7 排查常见问题。

---

## 6. 总验收清单

执行者完成前逐条勾选：

- [ ] `cargo build --workspace` 通过（**必须 `--workspace`**）
- [ ] `cargo test --workspace` 全绿（零 failed），通过数 = 413（基线 401 + 新增 12）
- [ ] `cargo fmt --all --check` 无 diff、`cargo clippy --workspace --all-targets` 零警告
- [ ] `Tool::execute` 签名未改；`ToolCtx` / `RuntimeDeps` 未加字段
- [ ] `Event::ToolCallEnd` 结构未改（27 个构造点零改动）
- [ ] `ToolCall` 结构未改（18 个构造点零改动）
- [ ] 新增字段全部 `#[serde(default)]`；旧 jsonl 反序列化测试通过
- [ ] `ToolCallEnd.output` 原文没被错误码前缀污染（存量断言仍通过）
- [ ] 模型失败信息形如 `[error:timeout] ...`，不是 `[error:timeout] [error] ...`（没有双前缀）
- [ ] 静态规则命中在 L1/L2/L3 下都保持 `AskUser { source: Static }`
- [ ] 子任务看不到也调不到 `delegate`（`tool_allowlist` 断言通过）
- [ ] `SubagentPort` 新增方法都有默认实现，现有实现零改动
- [ ] `agent-tools` 的依赖表里没有 `agent-runtime`
- [ ] 全程没有调用真实模型、没有新增外部依赖、没有贴真实格式密钥
- [ ] 5 个 commit 各自可独立 review

---

## 7. 故障速查表

| 现象 | 原因 | 修复 |
|---|---|---|
| `error[E0432]: unresolved import MalformedArgs / StaticVerdict / SubagentCapabilities` | 忘了在 `agent-core/src/lib.rs` 加 `pub use` | §0.1(e) / §2.1 / §4.1(c) |
| `error[E0063]: missing field error_code in initializer of ToolOutput / ToolExecuted` | 加了字段但某个构造点没补 | grep `ToolOutput {` / `ToolExecuted {` 逐个补：`None` 或 `Some(...)`；**`ToolExecuted` 是 4 个构造点**（C3 新增 1 个） |
| 测试 `tool_call_end_roundtrip...` 或 `bad.2` 断言红 | 把错误码前缀写进了 `output` 原文 | 错误码只进 `model_content` 和 `detail`，`output` 保持 `redacted`（§3 约束） |
| 模型看到 `[error:timeout] [error] ...` | 在 `execute_tool` 里也加了 `[error:...]` 前缀 | 只在 `run_inner` 用 `strip_prefix("[error] ")` 替换（§0.2(d)） |
| `error[E0599]: no method named into_decision` | `StaticVerdict` 放错 crate 或没写 impl | 必须在 `agent-core/src/perm.rs`，且 `agent-perm` import 里加 `StaticVerdict` |
| `error[E0405]` / 找不到 `ApprovalResult` | 契约测试里少路径 | 用全路径 `agent_core::perm::ApprovalResult` |
| `unused import: ToolLookup`（subagent.rs） | C3 删掉了硬编码过滤但没删 import | §3.3 明确要求删 `use crate::runner::ToolLookup;` |
| `error[E0599]: no method named allows_tool` | `TaskSpec` 方法没加，或加在了 `impl TaskSpec` 之外 | §3.1(c) |
| `Option::is_none_or` 编译不过 | 工具链较旧 | 改 `map_or(true, \|list\| ...)`，并接受 clippy 可能给建议 |
| `TaskSpec` 旧日志测试红 | JSON 字段名和真实 serde 输出不一致 | 先 `println!("{}", serde_json::to_string(&TaskSpec::new(...)).unwrap())`，按真实输出改测试（§3.1(d)） |
| `error[E0407]: method capabilities is not a member of trait` | 把方法写进了固有 `impl RuntimeSubagent` 而不是 `impl SubagentPort for ...` | §4.2 明确位置 |
| `delegate_failure_is_a_tool_error_not_a_task_failure` 红 | `err_code` 的 content 自己带了 `[error]` 前缀 | `err_code` 只写人话，`[error]` 由 `execute_tool` 加 |
| `default_capabilities_keep_existing_ports_compatible` 编译不过 | UFCS 写法问题 | 改成 `let p: &dyn SubagentPort = &port; p.capabilities()` |
| clippy 报 `unused_imports: MalformedArgs`（C0 单独跑时） | C1 还没用它 | 按 §0.2(a) 说明：C0/C1 连做，只在 C1 末尾跑 clippy |
| `cargo build` 卡在下载依赖 | 直连 crates.io 卡死 | §1.2 配 rsproxy |
| 推送 403 / 认证失败 | remote 里是只读 token | §8.2 |
| `warning: methods called 'new' usually return 'Self'`（`new_ret_no_self`） | 照 v1.0 把构造函数叫 `MalformedArgs::new`，但它返回 `serde_json::Value` | 用 `MalformedArgs::marker(...)`（v1.1 已全文改名） |
| `warning: large size difference between variants` 指向 `Command` | C3 给 `TaskSpec` 加了 `Option<Vec<String>>`，体积跨过 clippy 阈值 | §C3 步骤 3.2(d) 加 `#[allow(clippy::large_enum_variant)]` + 注释，**不要**改成 `Box<TaskSpec>` |
| `warning: useless use of vec!` | C3 子任务测试里 `let defs = vec![...]` | 改成 `let defs = [...]`（v1.1 已改） |
| `error: unexpected closing delimiter: )` 在 `subagent.rs` | 把 §4.1(b) 的整段 trait 当增量补丁插进去，留下重复的 `run_subtask` 残段 | 用代码块**整段替换** `pub trait SubagentPort { ... }`（§C4 步骤 4.1(b) 已写明） |
| `cargo fmt --all --check` 有 diff | 代码块是手写排版，落地后没跑 fmt | 每个 C 结束跑 `cargo fmt --all`（§0 第 9 条） |
| `web_search_returns_real_hits_without_configuration` 偶发失败 | 沙箱网络抖动，Bing 不可达 | 连跑 3 次 `cargo test -p agent-tools --test smoke`；3 次内全绿即非代码问题，与本次改动无关 |
| `detail: Some(reason)` 补丁匹配不到 | Rejected 分支缩进比 Deny 分支多 4 空格 | 按 `outcome: ToolOutcome::Rejected` 那块的真实缩进对齐（§0.2(f) 说明） |

---

## 8. 提交与推送

### 8.1 commit 顺序（每个 C 一个）

```
C0  feat(interface): 工具失败引入稳定错误码 ToolErrorCode
C1  refactor(interface): malformed args 收口为 MalformedArgs 类型 API
C2  refactor(perm): StaticVerdict 类型化静态闸门单调性 + 契约测试
C3  feat(interface): TaskSpec.tool_allowlist 统一模型可见性与可执行性
C4  feat(subagent): 端口能力协商 capabilities/capacity + delegate 前置校验
C5  test(interface): 接口加固回归测试补齐（若有遗漏改动）
```

### 8.2 推送（GitHub 连通率约 1/3，必须重试循环）

```bash
for i in 1 2 3 4 5 6 7 8; do
  echo "=== 推送尝试 $i ==="
  if timeout 120 git push origin main 2>&1; then echo PUSH_OK; break; fi
  sleep 3
done
# 确认远端 HEAD 与本地一致
for i in 1 2 3 4 5; do
  r=$(timeout 45 git ls-remote origin refs/heads/main 2>/dev/null)
  if [ -n "$r" ]; then echo "远端 main: $r"; break; fi
  sleep 2
done
git log --oneline -1    # 与远端输出比对
```

> 若报 `Invalid username or token`，说明 remote URL 里嵌的凭据已失效或只读——**重试解决不了认证问题**，换用有写权限的凭据再推。
> 若报 `GnuTLS recv error (-110)`，那是网络抖动，继续重试即可。

### 8.3 不要贴形似真实凭据的字符串

GitHub Push Protection 会扫描提交内容。测试里要假密钥时写成明显不真实的占位（例如 `EXAMPLE_NOT_A_REAL_KEY_000`），不要写任何看起来像 `ghp_` / `xoxb-` / `sk-` 开头的真实格式串。

---

## 9. 完成定义 / 明确不要做的事

**本次不实施**（写在这里是为了防止执行者自作主张）：

1. **不拆 `Event` 枚举**（对应分析里的第 6 点）。dsh 把事件分三域（持久会话事件 / `agent/*` 实时事件 / 能力事件），Zero 目前是单 `Event` 大枚举 + `recorder::classify` 裁决。拆分收益是「加变体不再触发 3 处穷尽 match 的 E0004」，但代价是**所有事件生产点与消费点都要改**，属于二期重构。设计草图：
   - 新增 `SessionEvent`（持久事实：`TaskStarted`/`TaskFinished`/`ToolCallStart`/`ToolCallEnd`/`ContextSnapshot`/`AssistantTurn`/`Error`/`SubtaskLinked`）与 `UiEvent`（实时：`TextDelta`/`ReasoningDelta`/`TaskList`/`TaskOpened`/`AwaitingApproval`/`ApprovalDecided`）。
   - `EventBus` 拆成两条 broadcast；`classify` 消失（SessionEvent 天然全落盘）。
   - 迁移策略：先让 `Event` 保留为 `enum Event { Session(SessionEvent), Ui(UiEvent) }` 过渡一版，再逐点替换。
   - **本次做这个会把 C0~C4 的改动面放大数倍，明确不碰。**
2. **不改 `Tool::execute` 的参数类型**（18 个 `ToolCall` 构造点）。
3. **不引入插件系统 / 运行时能力注册 / `ScopeKey`**。Zero 只有父子两层，编译期 trait 足够。
4. **不给 `ToolCtx` / `RuntimeDeps` 加字段**。
5. **不改 `default_tools()` 的 9 个工具与 `EXPECTED_TOOLS`**。
6. **不调用真实模型**；真实模型用例继续 `--ignored`。

---

## 附录 A：改动文件清单

| 文件 | 动作 | 阶段 |
|---|---|---|
| `crates/agent-core/src/tool.rs` | `ToolErrorCode` / `ToolError::code()` / `MalformedArgs` / `ToolOutput.error_code` + `err_code()` + 2 测试 | C0 / C1 |
| `crates/agent-core/src/lib.rs` | `pub use` 补 4 个类型 | C0 / C4 |
| `crates/agent-core/src/perm.rs` | `StaticVerdict` | C2 |
| `crates/agent-core/src/task.rs` | `tool_allowlist` 字段 + `new()` 初始化 + `allows_tool()` + 2 测试 | C3 |
| `crates/agent-core/src/subagent.rs` | `SubagentCapabilities` / `SubagentCapacity` + port 两个默认方法（**整个 trait 定义替换**） | C4 |
| `crates/agent-core/src/event.rs` | `Command` 枚举加 `#[allow(clippy::large_enum_variant)]` + 理由注释 | C3 |
| `crates/agent-perm/src/lib.rs` | import + `check` 用 `StaticVerdict` + 2 契约测试 | C2 |
| `crates/agent-runtime/src/runner.rs` | import、删常量、`ToolExecuted.error_code`、4 个构造点、model_content、detail 前缀、system prompt、allowlist 过滤与闸门 + 3 测试 | C0 / C1 / C3 |
| `crates/agent-runtime/src/subagent.rs` | 子任务改用 `tool_allowlist`、删 `ToolLookup` import、trait 两方法 + 1 测试 | C3 / C4 |
| `crates/agent-tools/src/delegate.rs` | 能力校验、`err_code`、容量信息 + 2 测试 | C4 |

> 共 **10 个文件**。`agent-store` / `agent-tui` / `apps/*` / `xtask` / `Cargo.toml` **一律不改**。

## 附录 B：与 dsh 接口逐条对应

| dsh 概念 | 本文对应 | 阶段 |
|---|---|---|
| `ToolErrorInfo.reason` / 稳定 code | `ToolErrorCode` + `[error:<code>]` | C0 |
| `ToolArgsError(INVALID_ARGS)` / frozen args | `MalformedArgs`（类型化哨兵，签名不动） | C1 |
| `ToolGuard` 返回类型无 allow | `StaticVerdict` 私有元组 + 契约测试 | C2 |
| `ToolRestriction` 按 scope 交集 + 本域自注册不受限 | `TaskSpec.tool_allowlist` + `allows_tool()`（模型可见 & 可执行双收口） | C3 |
| `SubagentCapabilities` | port `capabilities()` 默认方法 | C4 |
| `SubagentCapacity { running, limit }` | port `capacity()` 默认方法 | C4 |
| 三域事件 / session 日志即上下文 | **本次不做**，§9 设计草图 | 二期 |

## 附录 C：一句话心智模型

> **把散落在字符串、注释和约定里的规则，搬进类型；凡是要动 18 个以上构造点的「更彻底方案」，一律先记进「不做清单」，用小改动把类型边界立起来。**
