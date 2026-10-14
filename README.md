# 个人 Agent（personal-agent）

一个由个人独占使用的、运行在自有设备上的、能实际动手干活的通用 AI Agent。

> 一期（M1 骨架）：Rust 单机 Agent，TUI 交互，接入多云端 LLM（预留本地接口），
> 能读写文件 / 执行命令 / 联网，完成任务级三档权限管控。

## 快速开始

```bash
# 1. 设置 API key（以 DeepSeek 为例）
export DEEPSEEK_API_KEY=sk-xxxx

# 2. 运行（首次会自动进入配置向导；也可 cargo xtask gen-cfg 先生成模板）
cargo run -p agent-cli

# 或 headless 模式
cargo run -p agent-server -- "写一个备份脚本并执行"
```

## 工程结构

```
agent-workspace/
├── apps/
│   ├── agent-cli/       # TUI 入口
│   └── agent-server/    # 服务器端（一期 headless）
├── crates/
│   ├── agent-core/      # 领域核心：Event + 7 大 trait，零 IO 依赖（枢纽）
│   ├── agent-runtime/   # 执行循环 + 上下文管理
│   ├── agent-tools/     # file/shell/web 工具
│   ├── agent-perm/      # 权限引擎（静态规则 + AI 审视）
│   ├── agent-providers/ # LLM 适配（OpenAI 兼容 + 路由）
│   ├── agent-tui/       # ratatui 界面
│   ├── agent-store/     # jsonl 持久化
│   ├── agent-net/       # reqwest 封装 + SSRF 防护
│   └── agent-sync/      # 二期占位
└── xtask/
```

**依赖铁律**：所有实现 crate 只依赖 `agent-core`；`agent-core` 永不依赖 ratatui/reqwest/DB。
二期加同步、三期换手机端时，核心零改动。

## 权限模型（任务级三档）

| 档位 | 行为 |
|---|---|
| **L1 全自动** | 全部放行，仅记审计（**高危命令仍被静态规则拦截**） |
| **L2 AI 审视** | AI 判断是否放行；任何故障（超时/解析失败）**降级为询问用户** |
| **L3 逐步确认** | 每个操作都需确认（支持 `a` 本任务此类全批准） |

安全第一性原则：无沙箱下「误放行」的不可逆代价 > 「多问一次」的骚扰成本。

静态高危规则（不可绕过，命中即人工确认）包括：`rm -rf /`、提权、磁盘写入、
fork bomb、`curl | sh`、写 `/etc`、`~/.ssh` 等。

## 配置

配置文件默认位于**项目根下的 `config/config.toml`**（相对当前工作目录）。
可用环境变量 `AGENT_CONFIG` 覆盖为任意路径。

### 首次运行 = 自动配置向导

Agent 检测到以下情况时会**自动进入配置向导**（仅 stdin 是 TTY 时）：

| 情况 | 行为 |
|---|---|
| `config/config.toml` 不存在 | 进入向导，引导选择 provider / 环境变量名 |
| 配置文件 TOML 语法错误 | 进入向导，提示是否覆盖 |
| 配置语义无效（如 `router.default` 指向不存在的 provider） | 进入向导，提示是否覆盖 |
| 非交互环境（CI / 管道 / SSH 后台） | 写模板并退出，提示编辑后重启 |

向导只记录「环境变量名」，**API key 不写入文件**。

### 手动生成模板

```bash
cargo xtask gen-cfg     # 把注释版模板写到 config/config.toml
```

模板源文件在仓库 `config/config.example.toml`（被 `include_str!` 嵌入二进制）。
`config/config.toml` 已被 `.gitignore` 忽略，不会提交个人配置。

`config/config.toml`（首次运行自动生成 / 向导引导填写）：

```toml
[providers.deepseek]
kind = "openai_compat"
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"

[router]
default = "deepseek"
[router.by_task]
code = ["deepseek", "qwen"]

[budget]
monthly_limit_usd = 20.0
per_task_limit_usd = 0.5
```

> 注意：TOML **没有 `null` 字面量**。「无需 API key」的表达方式是省略 `api_key_env` 这一行。

- **已适配**：DeepSeek / 通义（兼容模式）/ OpenAI / Ollama（本地）—— 均走同一个 `openai_compat` 适配器。
- **本地模型**：配置 `base_url = "http://localhost:11434/v1"` 即接入 Ollama（无需 key）。

## 工具

`file_read` / `file_write` / `file_edit` / `list_dir` / `shell_exec` / `web_fetch` / `web_search`

- 所有文件操作限制在工作区内（防 `../` 逃逸）；
- `web_fetch` 有 SSRF 防护（禁内网/非 http(s)）；
- 超大工具输出自动外置到 `ctx/`。

## 数据布局

```
~/agent/
├── workspace/<task-id>/
│   ├── artifacts/   # 交付物（持久化）
│   ├── ctx/         # 外置大输出（临时，超期清理）
│   └── snapshots/   # 写前备份
├── logs/<session>.jsonl   # 事件流（唯一真相源，带 schema_version + 逻辑时钟）
└── tasks/<task-id>.json
```

> jsonl 事件流已是二期同步（分级同步 / AI 合并 / 回滚）的数据基础：
> 每行带 `schema_version`、逻辑时钟 `Clock`、以及 `durability`（持久化/临时）字段。

## 测试

```bash
cargo test
```

## 路线图

| 阶段 | 内容 |
|---|---|
| **一期（当前）** | 单机通用 Agent：Rust + TUI + 多模型 + 三档权限 |
| 二期 | 跨设备同步：手机权威节点、四级频率分级同步、AI 辅助合并 + 回滚、设备注册中心 |
| 三期 | 手机端 App、单片机哑终端接入 |
