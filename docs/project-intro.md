# 项目介绍：个人 Agent（personal-agent）

> 本文面向第一次接触本仓库的读者，5 分钟了解「这是什么、做到哪了、怎么跑、看什么」。
> 权威细节以 `README.md`、`docs/requirements.md`、`docs/design.md`、`PROGRESS.md` 为准。

---

## 一、一句话定位

**一个由个人独占使用、运行在自有设备上、能实际动手干活的通用 AI Agent。**

它不是问答型聊天助手，而是「编码型/执行型 Agent」：能读写文件、执行命令、联网抓取，
并把结果产出为文件。这一判断来自需求阶段的洞察——用户的三个真实场景
（写程序、写脚本、收集新闻）本质是**同一类任务**：
「在用户电脑上执行操作、访问网络、并产出文件」。

## 二、为什么这样分期

长期愿景是「一个框架装进用户拥有的任意设备（PC / 手机 / 服务器 / 单片机），
手机为权威数据源，设备间按重要性分级同步」。

但终局形态是一个**分布式系统**，工程风险高。因此采用收缩策略：

| 阶段 | 范围 | 状态 |
|---|---|---|
| **一期（MVP）** | **Rust 单机通用 Agent**：TUI 交互 + 多云端 LLM + 文件/命令/联网 + 三档权限 | 骨架已完成并跑通真实对话 |
| 二期 | 跨设备同步：手机权威节点、四级频率分级同步、AI 辅助合并与回滚、设备注册中心 | 仅留有 trait 占位 |
| 三期 | 手机端 App、单片机哑终端接入 | 未开始 |

> 决策依据：先证明「Agent 有用」，再证明「到处都能用」。

## 三、技术栈与工程结构

语言 Rust（stable）+ Tokio 异步；TUI 用 ratatui/crossterm；LLM 走 OpenAI 兼容 HTTP+SSE；
持久化用 jsonl（append-only）；配置用 TOML；辅助脚本用 xtask。

```
Zero/
├── apps/
│   ├── agent-cli/       # TUI 入口
│   └── agent-server/    # headless 入口（一期）
├── crates/
│   ├── agent-core/      # 领域核心：Event 总线 + 7 大 trait，零 IO 依赖（枢纽）
│   ├── agent-runtime/   # 执行循环 + 上下文三层管理 + 上下文压缩
│   ├── agent-tools/     # file_read/file_write/file_edit/list_dir/shell_exec/web_fetch/web_search
│   ├── agent-perm/      # 权限引擎：静态高危规则 + 三档决策流
│   ├── agent-providers/ # LLM 适配（openai_compat）+ 路由 + 预算 + 配置向导
│   ├── agent-tui/       # ratatui 界面：四区布局、六类消息样式、审批弹层
│   ├── agent-store/     # jsonl 事件流持久化 + 逻辑时钟 + durability 分层
│   ├── agent-net/       # reqwest 封装 + SSRF 防护
│   └── agent-sync/      # 二期占位（trait 冻结）
├── docs/                # 需求 / 技术方案 / UI 设计 / 本介绍
├── config/              # config.example.toml 模板（config.toml 被 gitignore）
├── scripts/ xtask/      # 构建与日志辅助
└── PROGRESS.md          # 里程碑与测试覆盖台账
```

**依赖铁律**：所有实现 crate 只依赖 `agent-core`；`agent-core` 永不依赖
ratatui / reqwest / DB。二期加同步、三期换手机端时，核心零改动。

## 四、三个关键设计

### 1. 权限模型（任务级三档）

| 档位 | 行为 |
|---|---|
| **L1 全自动** | 全部放行，仅记审计；但**高危命令仍被静态规则拦截** |
| **L2 AI 审视** | 由 AI 判断；任何故障（超时/解析失败）**降级为询问用户** |
| **L3 逐步确认** | 每个操作都需确认，支持 `a` 本任务此类全批准 |

安全第一性原则：无沙箱下「误放行」的不可逆代价 > 「多问一次」的骚扰成本。
静态高危规则命中即人工确认，含 `rm -rf /`、提权、磁盘写入、fork bomb、
`curl | sh`、写 `/etc` 与 `~/.ssh` 等。

### 2. 项目目录 / 私有工作区分治

- **项目目录**（`agent.project_root`，默认 = 启动目录）：工具读写的沙箱，
  相对路径基准，防 `../` 逃逸。所以「介绍当前项目」这类任务才有东西可看。
- **私有工作区**（`agent.root/workspace/<task-id>`）：只放 `ctx/`（大输出外置）
  等系统数据，绝不写进项目。

### 3. 事件流是唯一真相源

运行期落 `<root>/logs/<session>.jsonl`，正式事件带 `schema_version` + 逻辑时钟
`Clock` + `durability`，并区分「持久/临时/不落」。日志保存**完整对话上下文**
（每轮 `context_snapshot` = 系统提示词 + 全部历史消息 + 可用工具），
因此模型当时看到什么、回了什么、工具返回什么都可复现——这既是排障手段，
也是二期同步与回滚的数据基础。读日志：`cargo xtask log <task-id> [--errors|--full]`。

## 五、怎么跑

```bash
# 1. 设置 API key（以 DeepSeek 为例；Ollama 本地模型无需 key）
export DEEPSEEK_API_KEY=sk-xxxx

# 2. 运行（首次自动进入配置向导，也可 cargo xtask gen-cfg 先生成模板）
cargo run -p agent-cli

# headless 模式
cargo run -p agent-server -- "写一个备份脚本并执行"

# 测试
cargo test
```

已适配 DeepSeek / 通义（兼容模式）/ OpenAI / Ollama，均走同一个 `openai_compat`
适配器。API key 只在环境变量或（被 gitignore 的）`config/config.toml` 中，
向导本身只记录环境变量名。

## 六、当前进度（M1 骨架）

- 9 个库 crate + 2 个二进制 + xtask 全部落地；核心 trait 签名冻结。
- `cargo build` / `cargo clippy --all-targets` 均 **0 warning**。
- `cargo test`：P0 后 **92 passed**，P1（工具四态）后 **121 passed**，
  连同后续几批 TUI 修复，现为 **167 passed / 0 failed**。
- 端到端真实对话已验证（DeepSeek 真实端点）。
- TUI 已完成 P0 主题化（Dracula 系配色、四区布局、六类消息符号、审批弹层、
  鼠标滚轮、`Ctrl+L` 整屏重绘恢复花屏）；P1 完成「工具调用四态 + 耗时 + 失败原因」，
  并补齐输入框右侧的发送指示器（工作中 / 可发送）与显眼的「任务完成」行。
  P1 剩余项（Toast、`Ctrl+R` 历史、`/` 补全）与 P2（侧边栏、Markdown、diff 视图）待做。
- 已修复的典型案例：审批自锁（命令循环与任务互相等待造成死锁，改为
  `tokio::spawn` 独立执行任务）；任务收尾把最后一段回答重复输出一遍（收尾不再
  整段塞 `summary`，它本身就是已流式渲染过的原文）；滚到底看不到最后几行
  （滚动上限不再靠估算，改为自己软换行）。

## 七、已知限制与后续重点

- 一期只有单机；`agent-sync` 是占位，无真实同步能力。
- 无系统级沙箱，安全依赖静态规则 + 权限档位 + 路径沙箱三层约束。
- 下一步：补齐 P1/P2 交互体验，再进入二期同步设计（依赖一期沉淀的真实数据形态）。
