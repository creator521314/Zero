# Zero 记忆系统 · 实测问题报告（已修复）

> 状态：**已修复**（2026-11-27，修复见本文 §9）
> 上游：`docs/memory-design.md` v1.1、`docs/memory-requirements.md` v1.0
> 本文性质：**实测缺陷报告**，不是设计文档。所有结论均来自对运行中系统的实际观察 + 源码核对，每条都附可复现证据。

---

## 0. 一句话结论

**记忆系统「只写不读」：文件在持续膨胀，但生效记忆恒为空。**

实测 5 轮对话后，`.zero/memory.jsonl` 从 2 条涨到 11 条，**全部停留在 `candidate` 态**，因此 `memory_search` 每次都返回空、上下文注入也永远为空。用户级记忆文件从未被创建。

---

## 1. 实测环境与数据

| 项 | 值 |
|---|---|
| 项目根 | `/home/creator/git_rc/Zero` |
| 项目级记忆文件 | `.zero/memory.jsonl`（已在 `.gitignore`） |
| 用户级记忆文件 | `~/.config/zero/memory.jsonl` —— **不存在，目录 `~/.config/zero/` 也不存在** |
| 状态分布（实测时） | `candidate: 11~14`，`active: 0`，`archived: 0` |
| scope 分布 | `Project{root:"/home/creator/git_rc/Zero"}`，`User: 0` |
| `key` 字段 | **全部为 `null`**（提炼 worker 从不填充显式 key） |

**关键观察**：`memory_search` 工具在每一轮都返回「没有找到相关记忆」，与文件里已有 11 条记录的事实直接矛盾。

---

## 2. 缺陷清单

### P0-1：候选态永不转正，导致记忆完全不可见

**现象**：全部记录停留在 `state: "candidate"`，`MemoryIndex::search` 默认只返回 `Active`，故检索恒为空。

**根因**：转正接口 `confirm()` 存在且工作正常，但**全仓库只有一个调用点**（TUI 记忆面板逐条确认键）。没有任何自动转正路径，也没有「批量确认全部候选」的入口。提炼 worker 写完后不会触发确认，于是候选无限堆积。

**影响**：记忆功能对 Agent 行为零影响。这是最严重的问题——其余问题都建立在「写入至少能被读到」的前提上。

### P0-2：去重逻辑被候选态绕过，导致无限重复

**现象**：同一事实被反复写入（实测「工作目录是 Rust 项目」重复 4 次、「两层 JSONL 存储路径」重复 4 次）。

**根因（两层）**：

1. **状态条件过窄**：upsert 去重只匹配 `state == Active`，候选态之间互不去重——既然 P0-1 导致所有条目都是候选，去重逻辑实际上**从未生效过**。
2. **dedup_key 对自然语言几乎无效**：`implicit_key()` 是「去空白 + 转小写 + 哈希」，只能识别逐字相同的内容；实测重复条目措辞各不相同，哈希完全不同 → 全部判为不重复。

**影响**：文件无界增长；注入上下文被同义重复条目挤爆（这是**重复**而非设计已接受的**冲突**，属实现缺陷）。

### P1-3：提炼质量差，产出自我指涉的元信息

**现象**：全部记录都是「环境事实」，且大量是**关于记忆系统自身**的描述（记忆库存储路径 ×3、设计文档位置 ×2、甚至有一条断言「长期记忆默认没有用户偏好内容」——而它本身就是一条被写入的记录，写入瞬间即不成立）。

**根因**：提炼 worker 把「对话中讨论过的话题」直接当作「值得长期记住的事实」，缺少准入过滤。用户问「你的记忆库在哪」，讨论的是元信息，不该沉淀为记忆。

**影响**：信噪比极低。真正有价值的用户偏好（如「提交信息用中文」「测试跑 `cargo xtask test`」）从未被捕获。

### P1-4：用户级记忆从未初始化

**现象**：`~/.config/zero/memory.jsonl` 不存在，`~/.config/zero/` 目录也不存在。

**根因**：`ensure_loaded` 在文件缺失时走「空缓存」分支（不崩溃，但也不创建目录/文件）。

**影响**：跨项目通用的用户偏好无处存放，两层存储实际只有一层在工作。

---

## 3. 复现步骤

```bash
cd <project_root>

# 1. 观察文件规模与状态分布
wc -l .zero/memory.jsonl
python3 -c "
import json
from collections import Counter
rows=[json.loads(l) for l in open('.zero/memory.jsonl') if l.strip()]
print('state:', Counter(r['entry']['state'] for r in rows))
"

# 2. 确认用户级文件不存在
ls -la ~/.config/zero/          # 修复前：No such file or directory

# 3. 在 Agent 会话中调用 memory_search（任意关键词）
#    修复前预期：返回「没有找到相关记忆」，与步骤 1 中非空文件矛盾
```

---

## 4. 与设计文档的偏差（修复前）

| 设计文档承诺 | 实测结果 | 偏差 |
|---|---|---|
| 两层存储（用户级 + 项目级） | 仅项目级存在 | 用户级从未初始化 |
| 候选 → 生效 → 归档 状态机 | 全部卡在候选 | 状态机后半段不可达 |
| 写入流程「双通道 + 准入管道」 | 无准入过滤，元信息入库 | 准入管道缺失或未生效 |
| 「冲突记忆会同时注入」（已接受限制） | 实际是**重复**记忆堆积 | 超出已接受范围，属实现缺陷 |
| upsert 去重逻辑 | 从未触发 | 条件过窄 + 键失效 |

---

## 5. 修复方案（已实施：A + B 组合）

### 第一步：让候选能转正（P0-1）

- **A. 自动转正（已实施）**：提炼 worker 对通过准入管道的条目直接置 `Active`；
- **B. 批量确认入口（已实施）**：TUI 记忆面板候选页加 `Ctrl+A` 全部确认快捷键；候选态机制与逐条确认保留为兜底。

### 第二步：修去重（P0-2）

- **放宽状态条件（已实施）**：upsert 去重条件 `Active` → `Active | Candidate`，候选之间也去重；
- **显式 key（已实施）**：提炼 worker 先把已有生效记忆清单喂给模型，要求不重复已记内容、同主题更新复用其 key、新主题产出稳定结构化 key（如 `user:commit_language`）；同 key 新写入覆盖旧条目并归档留版本。

### 第三步：加准入过滤（P1-3）

- **元信息黑名单（已实施）**：tags 命中黑名单（`记忆机制`/`记忆存储`/`记忆系统`/`记忆库`/`长期记忆`/`记忆功能`）或 content 命中关键词（`记忆系统`/`记忆库`/`记忆机制`/`memory.jsonl`/`长期记忆`/`/memory`）→ 直接拒绝；
- **提示词收紧（已实施）**：提炼提示词明确「优先捕获用户显式表达的偏好与约定，忽略关于 Agent 自身机制的元话题，不要重复已有记忆清单中的内容」。

### 第四步：初始化用户级存储（P1-4）

- **已实施**：`MemoryService::new` 启动时 `create_dir_all` 用户级目录（幂等，不创建/不覆盖文件本身）；首次写入用户级条目时由 append 落盘创建文件。

---

## 6. 修复验收标准

1. `memory_search` 能检索到已确认的条目；
2. 连续 5 轮同类对话后，`.zero/memory.jsonl` 的**有效条目数不增长**（去重生效）；
3. 文件中不存在 tags 含 `记忆机制`/`记忆存储`/`记忆系统` 的条目（准入生效）；
4. `~/.config/zero/memory.jsonl` 在首次写入用户级记忆后被创建；
5. 现有冗余候选数据被清理或归档（`state: "archived"`，保留可追溯性）。

**对应回归测试**：`candidate_entries_dedup_against_each_other`（验收 2）、`parse_candidates_rejects_meta_memory` / `is_meta_memory_matches_tags_and_content`（验收 3）、`new_creates_user_dir_without_touching_file`（验收 4）、`distilled_entries_are_active_and_dedup_by_key`（验收 1）。验收 5 需在真实环境用 `/memory` 候选页 `Ctrl+K` 清空或逐条处理存量数据。

---

## 7. 附：实测原始记录（修复前）

实测 11 条记录全部 `op: "upsert"`、`state: "candidate"`、`version: 1`、`key: null`、scope 均为项目级。典型样例：

| 内容性质 | 重复次数 |
|---|---|
| 「工作目录是 Rust 项目」（措辞各异的变体） | 4 |
| 「两层 JSONL 存储路径」（措辞各异的变体） | 4 |
| 「记忆系统设计文档位置」 | 2 |
| 「长期记忆默认没有用户偏好内容」（自我指涉） | 1 |

---

## 8. 关键源码位置索引（修复后）

| 位置 | 作用 | 相关问题 |
|---|---|---|
| `crates/agent-store/src/memory_store.rs` | upsert 去重条件（`Active \| Candidate`）+ 用户级目录初始化 | P0-2 / P1-4 |
| `crates/agent-store/src/distiller.rs` | 自动转正（`Active`）、显式 key、`is_meta_memory` 黑名单、已有记忆清单注入 prompt | P0-1A / P0-2 / P1-3 |
| `crates/agent-tui/src/app.rs` | `MemoryPending::ConfirmAll` 批量确认 | P0-1B |
| `crates/agent-tui/src/render.rs` | 候选页 `Ctrl+A` 键位 + 面板提示行 | P0-1B |
