# 个人 Agent（personal-agent）

一个由个人独占使用的、运行在自有设备上的、能实际动手干活的通用 AI Agent。

> 一期（MVP）：Rust 单机 Agent，TUI 交互，接入多云端 LLM（预留本地接口），
> 能读写文件 / 执行命令 / 联网，完成任务级三档权限管控，
> 并支持「写文件 → 执行脚本 → 报告结果」的端到端任务闭环。

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
│   ├── agent-tools/     # file/shell/web/finish 工具 + 写前快照
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

## TUI 操作

界面四区布局：header（档位徽章 + 运行状态）、对话区（Markdown 渲染）、
输入区、footer（快捷键提示，可 `Ctrl+H` 隐藏）。右上角 Toast 负责瞬态反馈
（切档、无历史、footer 显隐等，2 秒自动消失），叠在审批弹层之上。

| 快捷键 | 功能 |
|---|---|
| `Enter` | 提交输入 |
| `Ctrl+Enter` | 输入区换行（多行输入，最多 3 行可视） |
| `Ctrl+R` | 循环回溯输入历史（连续重复提交只记一条） |
| `Ctrl+H` | 显示 / 隐藏底部 footer |
| `Ctrl+L` | 整屏重绘（屏幕被外部改动弄花时救回） |
| `PageUp` / `PageDown` / 鼠标滚轮 | 对话区滚动 |
| `Ctrl+A` / `Home`、`Ctrl+E` / `End` | 光标行首 / 行尾 |
| `/` | 触发命令补全（`/clear` `/help` `/tier` `/tasks` `/resume` `/news`） |
| `Ctrl+G` | 任务面板：`↑/↓` 选、`Enter` 切到该任务、`Esc` 关闭 |
| `Tab` / `↑` `↓` / `Esc` | 补全菜单：补全选中 / 选候选 / 关闭 |
| `Ctrl+C` | 退出（审批弹层期间也可退出） |
| 点击 header 档位徽章 | 循环切换权限档 L1 → L2 → L3 |

审批弹层：`y` 或 `Enter` 批准本次；`a` 批准本任务同类操作；`n` 或 `Esc` 拒绝；
`Ctrl+C` 退出。弹层按钮与补全候选也支持鼠标点击。档位切换（`/tier` 或点徽章）
只影响**之后**提交的任务。

### 多轮对话、历史任务与续跑

- **对话是连续的**：同一个话题里连续追问，沿用同一任务 id，并把上一轮的上下文
  （最后一次 `context_snapshot` + 快照之后的答复）带给模型——模型记得刚才做过什么。
  `/clear` 才是「开新话题」。
- **`/tasks`**：把历史任务打在对话区（`id 前 8 位 · 状态 · 最近更新 · 标题`）。
- **`Ctrl+G`**：同一份数据的面板版，选中即切过去，对话区还原成当时的样子；
  之后输入的话会接着那条任务跑。
- **`/resume <id前几位>`**：面板的命令版，支持唯一前缀（前缀有歧义会报「匹配到 N 个」）。
- 历史任务列表来自 `agent.root/tasks/<id>.json`，由执行循环在开始与收尾时更新。

### `/news`：新闻收集模板

`/news <主题>`（如 `/news Rust`）把 `templates/news.md` 渲染进输入框——**不自动发送**，
先看一眼、改一改再回车。模板要求模型「先搜再抓、每条带标题/日期/链接、不许拼 URL、
交付到 `artifacts/news-<slug>.md` 并登记」；`templates/news.md` 优先于内置副本，
改完重启即生效。

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

### API key 的两种写法

```toml
[providers.deepseek]
api_key_env = "DEEPSEEK_API_KEY"   # 优先：从环境变量取
api_key      = "sk-真实key"         # 兜底：环境变量未设时用这个
```

解析顺序：**环境变量有值 → 用环境变量；否则 → 用配置文件里的明文 `api_key`**。
环境变量存在但为空白串时视为未设置，继续走明文兜底。

`config/config.toml` 已被 `.gitignore` 排除，不会提交，因此允许明文写入；
但共享/备份这台机器时仍需注意——文件里就是真实 key。两个字段都省略才视为
「无需鉴权」（本地 Ollama 场景）。

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

# 工具读写的沙箱：**先 cd 到你的项目再启动 agent**，默认值就能命中它
[agent]
project_root = "."     # 也支持绝对路径与 ~；留空则退回私有工作区

# 审计日志（默认即可，见下方「排障」一节）
[log]
save_context       = true    # 每轮记录完整上下文（system + 全部历史消息）
save_stream_deltas = false   # 逐字增量（开着日志会大一数量级）
```

> 注意：TOML **没有 `null` 字面量**。「无需 API key」的表达方式是省略 `api_key_env` 这一行。

- **已适配**：DeepSeek / 通义（兼容模式）/ OpenAI / Ollama（本地）—— 均走同一个 `openai_compat` 适配器。
- **本地模型**：配置 `base_url = "http://localhost:11434/v1"` 即接入 Ollama（无需 key）。

## 工具

内置 8 个工具：

| 工具 | 用途 |
|---|---|
| `file_read` / `file_write` / `file_edit` | 读写/编辑文件；覆写前自动快照 |
| `list_dir` | 列目录（读目录时提示改用此工具） |
| `shell_exec` | 执行命令 |
| `web_fetch` / `web_search` | 联网抓取 / 搜索 |
| `finish` | 显式终止信号：总结 + 登记交付物路径，触发任务收尾 |

- 所有文件操作限制在**项目目录**内（防 `../` 逃逸）。项目目录默认就是
  你启动 agent 时的当前目录（`agent.project_root`），这样「介绍当前项目」
  才有东西可看；每任务的私有数据仍单独存放，不污染项目。
- `file_write` / `file_edit` 覆写前把旧内容备份到私有工作区 `snapshots/`
  （`<文件名>.<毫秒>`），误写可回滚；新建文件不产生空快照。
- `finish` 是显式终止信号：模型在 `summary` 里给总结、在 `artifacts` 里登记
  交付物路径。它无副作用，调用后跳过权限裁决直接执行，并立即结束本轮任务
  —— 任务完成不会被 L3 的审批弹窗打断。
- `shell_exec` 用平台原生解释器：Windows 上 `cmd /C`、Unix 上 `/bin/sh -c`。
  子进程带 `CREATE_NO_WINDOW` 起在自己的控制台里，它改代码页 / 清屏都不会
  碰到 TUI 的屏幕。输出先按 UTF-8 严格解，失败再按系统 OEM 代码页解
  （`dir` 这类是 GBK，`git` 这类是 UTF-8，两者都要活下来）。
- `web_fetch` 有 SSRF 防护（禁内网/非 http(s)）；
- `web_search` **免配置即可用**（内置后端）。想走自建 SearXNG 就在 `[search]`
  设 `searxng_endpoint`，缺省策略是「SearXNG 优先、失败自动回退」；
  只想用自建实例时设 `backend = "searxng"`，`backend = "off"` 则彻底关闭搜索。
  全部后端都失败时会逐个列出失败原因，不会只留一句「search failed」。
- 超大工具输出自动外置到 `ctx/`。

## 数据布局

```
~/agent/
├── workspace/<task-id>/
│   ├── artifacts/   # 交付物（持久化）
│   ├── ctx/         # 外置大输出（临时，超期清理）
│   └── snapshots/   # 写前备份
├── logs/
│   ├── agent.log          # 运行期 tracing 日志（TUI 下 stdout 被占用，只能写文件）
│   └── <session>.jsonl    # 事件流（唯一真相源，带 schema_version + 逻辑时钟）
└── tasks/<task-id>.json
```

> 日志目录由 `agent.root` 推导（`<root>/logs`），可用 `AGENT_LOG_DIR` 覆盖。
> **不放在 `config/` 下**：那是代码仓库里的配置目录，运行期产物混进去既脏又
> 容易被误提交。

> 两个目录要分清：
>
> - **项目目录**（`agent.project_root`，默认 = 启动目录）—— 工具读写的沙箱，
>   相对路径的基准。删掉它，工具就没东西可看。
> - **私有工作区**（`agent.root/workspace/<task-id>`）—— 只放 `ctx/` 等系统
>   数据，绝不写到项目里。
>
> jsonl 事件流已是二期同步（分级同步 / AI 合并 / 回滚）的数据基础：
> 每行带 `schema_version`、逻辑时钟 `Clock`、以及 `durability`（持久化/临时）字段。

## 排障：读会话日志

日志里保存的是**完整对话上下文**，而不只是工具调用记录：

| 事件 | 内容 | 用途 |
|---|---|---|
| `context_snapshot` | 每轮送往模型的系统提示词 + 全部历史消息 + 可用工具 | 模型当时看到了什么 |
| `assistant_turn` | 本轮完整回复（正文 / 工具调用 / token 用量） | 模型回了什么 |
| `tool_call_start` | 工具名 + 参数 | 准备做什么 |
| `tool_call_end` | 四态结局 + 耗时 + 原因 + **工具返回原文** | 工具实际返回了什么 |
| `task_started` / `task_finished` / `error` | 任务生命周期 | 是否跑完 |

```bash
cargo xtask log <task-id>            # 按时间线读（也可直接给 .jsonl 路径）
cargo xtask log <task-id> --errors   # 只看失败项：工具失败 / 被拦 / 报错
cargo xtask log <task-id> --full     # 展开完整上下文与工具返回原文
```

落盘规则集中在 `crates/agent-store/src/recorder.rs`：逐字增量默认不落
（内容已由 `assistant_turn` 完整覆盖，开着会让日志大一数量级——同一段
对话实测从 687 条事件降到约 20 条）。

### 屏幕花了 / 大面积变黑

按 **`Ctrl+L`** 整屏重绘即可恢复。

TUI 只发送差分（ratatui 的机制），物理屏幕一旦被外部改动，没变化的区域
就不会被重画。整屏重绘必须**擦屏**（`Terminal::clear()`），而擦屏会闪，
所以只在 `Ctrl+L` 与终端缩放时做——工具执行完不自动重绘，因为子进程已经
起在自己的控制台里（`CREATE_NO_WINDOW`），碰不到我们的屏幕。

## 测试

```bash
cargo test           # 全量测试
cargo xtask check    # fmt + clippy + test 一站式检查
cargo xtask smoke    # 工具端到端冒烟：真实跑一遍 8 个工具

# 真实模型用例（--ignored，需要配置 provider，会花少量 token）
cargo test -p agent-cli --test resume_live -- --ignored --nocapture   # 续跑：模型记得上一轮
cargo test -p agent-providers --test review_live -- --ignored --nocapture  # L2 审视判定
```

### 工具冒烟

单元测试全绿不等于工具能用——`web_search` 曾 100% 失败而当时 257 个单测全过：
单测直接构造工具、注入替身参数，既不过 `default_tools()` 这条生产装配路径，
也不比对「给模型看的 JSON Schema」和「`execute` 真正读的键」。

`crates/agent-tools/tests/smoke.rs` 只补单元测试看不见的部分：

| 检查 | 拦截的问题 |
|---|---|
| 注册表内容 | 工具被漏掉/改名没人发现 |
| `defs()` 完整性 | 描述为空、`parameters` 不合 schema → 模型根本不会用 |
| 声明参数 ↔ 实际入参 | 只改一侧 → 模型永远拿到 `InvalidArgs`（web_search 那类） |
| 缺必填参数报 `InvalidArgs` | 校验被顺手删掉后静默按空值跑 |
| 真实读写/执行/联网 | 「不报错」但输出不对（中文乱码、标签没剥、搜出零条） |

联网用例在**网络不可达或被限流时跳过**（并打印原因），不会让离网环境误红；
但只要连得上，输出不成立就算失败。冒烟是集成测试，已被 `cargo xtask check`
带回 CI 门禁。

## 路线图

| 阶段 | 内容 |
|---|---|
| **一期（✅ 已完成，M4 收尾 2026-10-24）** | 单机通用 Agent：Rust + TUI + 多模型 + 三档权限；三类真实任务（写程序 / 写脚本 / 收集新闻）均跑通 = MVP 达成 |
| 二期 | 跨设备同步：手机权威节点、四级频率分级同步、AI 辅助合并 + 回滚、设备注册中心 |
| 三期 | 手机端 App、单片机哑终端接入 |

细分里程碑与验收方式见 [docs/milestones.md](docs/milestones.md)。
