//! 上下文三层管理（对应方案 4.2）。
//!
//! 1. 全量层：最近 N 轮原始 messages；
//! 2. 摘要层：更早轮次滚动摘要（一期为摘录式，有硬上限）；
//! 3. 文件外置层：超大工具输出写入 ctx/，上下文只留摘要 + 路径。
//!
//! v1.2 关键约束（见 `docs/context-audit.md`）：
//! - **messages[0] 是逐字节恒定的固定 system 区**，压缩也不改写它；
//!   变化量（任务、记忆、摘要、内部提醒）一律以消息形式追加在尾部，
//!   这样追问时请求前缀完全一致，前缀缓存才能真正命中。
//! - 每轮只把「新增消息」交给审计层（增量协议），不再全量落盘。

use agent_core::provider::{Message, Role, ToolCall};

/// 上下文配置。
#[derive(Debug, Clone)]
pub struct ContextConfig {
    /// 保留的最近消息条数。
    pub keep_recent: usize,
    /// 估算触发压缩的 token 阈值比例（相对 max_context）。
    pub compress_at_ratio: f64,
    /// 单个工具输出超过此字节数则外置。
    pub externalize_over_bytes: usize,
    /// 模型上下文窗口。
    pub max_context: u32,
    /// 摘要区最大字符数：防止多次压缩后摘要自身把窗口撑爆。
    pub summary_max_chars: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            keep_recent: 12,
            compress_at_ratio: 0.7,
            externalize_over_bytes: 8 * 1024,
            max_context: 128_000,
            summary_max_chars: 2_000,
        }
    }
}

impl ContextConfig {
    /// 按实际模型的上下文窗口构造（其余项取默认）。
    ///
    /// 旧实现无论接什么模型都硬编码 128K，32K 模型下压缩阈值永远够不着，
    /// 请求直接超窗口。
    pub fn for_model(max_context: u32) -> Self {
        Self {
            max_context: max_context.max(1),
            ..Default::default()
        }
    }
}

/// 上下文管理器。
pub struct ContextManager {
    cfg: ContextConfig,
    /// 系统固定区（永不丢弃、永不改写）。
    #[allow(dead_code)]
    pinned_system: String,
    /// 已完成轮次的累积摘要（有上限）。
    summary: String,
    /// 活动消息（system + 对话）。
    messages: Vec<Message>,
    /// 自上次 flush 以来新增的消息（增量审计协议）。
    pending: Vec<Message>,
    /// 压缩重写了历史：下次 flush 必须发全量基线而非增量。
    reset_pending: bool,
}

impl ContextManager {
    pub fn new(cfg: ContextConfig, pinned_system: impl Into<String>) -> Self {
        let sys = Message::system(pinned_system.into());
        Self {
            cfg,
            pinned_system: sys.text(),
            messages: vec![sys.clone()],
            pending: vec![sys],
            summary: String::new(),
            reset_pending: false,
        }
    }

    /// 从一份既有上下文续跑（v1.2）：不再重建 system，直接沿用快照里的
    /// 固定区，保证「同一对话无论追问几次，Role::System 恒 1 条」。
    pub fn from_messages(cfg: ContextConfig, messages: Vec<Message>) -> Self {
        let pinned = messages
            .first()
            .filter(|m| m.role == Role::System)
            .map(Message::text)
            .unwrap_or_default();
        Self {
            cfg,
            pinned_system: pinned,
            messages,
            // 基线已在此前会话的增量快照里；本轮只记之后的新增。
            pending: Vec::new(),
            summary: String::new(),
            reset_pending: false,
        }
    }

    pub fn push(&mut self, m: Message) {
        self.messages.push(m.clone());
        self.pending.push(m);
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// 取走本轮增量：`(added, reset)`。
    ///
    /// `reset = true` 时 `added` 是压缩后的**完整基线**，重建方需清空累积
    /// 再整份替换；否则只是追加到既有序列之后。
    pub fn flush_delta(&mut self) -> (Vec<Message>, bool) {
        if self.reset_pending {
            self.reset_pending = false;
            self.pending.clear();
            return (self.messages.clone(), true);
        }
        (std::mem::take(&mut self.pending), false)
    }

    /// token 估算：中文按 1 字符≈1 token，ASCII 按 ≈1/3，
    /// 并计入 `tool_calls.arguments` 的 JSON 与 reasoning 文本。
    ///
    /// 旧实现只算 `m.text().len()/3`，把工具参数（往往是最大的一块）和
    /// 中文一并漏掉，压缩阈值形同虚设。
    pub fn estimate_tokens(&self) -> usize {
        agent_core::provider::estimate_messages_tokens(&self.messages)
    }

    pub fn should_compress(&self) -> bool {
        let threshold = (self.cfg.max_context as f64 * self.cfg.compress_at_ratio) as usize;
        self.estimate_tokens() > threshold
    }

    /// 压缩：把超出保留窗口的旧消息折叠进一条**追加在历史中的摘要消息**。
    ///
    /// 与旧实现的差别：不再重建 `messages[0]`（那会让固定 system 每次压缩
    /// 都变，前缀缓存全灭），而是把摘要作为 `Role::User` 插在保留窗口前。
    pub fn compress(&mut self) {
        let dialog_count = self.messages.len().saturating_sub(1);
        if dialog_count <= self.cfg.keep_recent {
            return;
        }
        let mut to_drop = dialog_count - self.cfg.keep_recent;
        // 裁剪点不能落在 assistant(tool_calls) 与其 tool 结果之间：孤儿 tool
        // 消息会让下一轮请求被 OpenAI 兼容端点直接 400。
        while to_drop > 0
            && self
                .messages
                .get(1 + to_drop)
                .is_some_and(|m| m.role == Role::Tool)
        {
            to_drop -= 1;
        }
        if to_drop == 0 {
            return;
        }
        let drained: Vec<Message> = self.messages.drain(1..1 + to_drop).collect();
        let mut digest = String::new();
        for m in &drained {
            let t = m.text();
            if !t.is_empty() {
                let snippet: String = t.chars().take(120).collect();
                digest.push_str(&format!("- {:?}: {}\n", m.role, snippet));
            }
        }
        if !digest.is_empty() {
            self.summary.push_str(&digest);
        }
        // 摘要硬上限：只保留最新部分，避免无限累积。
        if self.summary.chars().count() > self.cfg.summary_max_chars {
            let skip = self.summary.chars().count() - self.cfg.summary_max_chars;
            self.summary = self.summary.chars().skip(skip).collect();
        }
        if !self.summary.is_empty() {
            self.messages.insert(
                1,
                Message::user(format!(
                    "[历史摘要] 此前 {} 轮已被压缩，以下为摘录（仅作背景）：\n{}",
                    drained.len(),
                    self.summary
                )),
            );
        }
        // 历史被重写：下一次 flush 发全量基线。
        self.reset_pending = true;
        self.pending.clear();
    }

    /// 需要时自动压缩。
    pub fn maybe_compress(&mut self) {
        if self.should_compress() {
            self.compress();
        }
    }

    /// 判断工具输出是否应外置。
    pub fn should_externalize(&self, output_len: usize) -> bool {
        output_len > self.cfg.externalize_over_bytes
    }
}

/// 上下文的**唯一构造点**（v1.2）。
///
/// 此前 system_prompt / run_inner / resume_context / compress / compose /
/// distiller 各拼一份上下文，互不知情，于是出现双 system、prompt 重复三遍、
/// 记忆注入打断前缀缓存等问题。所有「什么进上下文、以什么角色、放在哪」
/// 的裁决集中到这里。
pub struct ContextAssembler;

pub const MEMORY_MARK: &str = "【长期记忆】\n";
pub const MEMORY_UPDATE_MARK: &str = "【长期记忆·更新】\n";

impl ContextAssembler {
    /// 固定 system 区：**不含本次任务、不含记忆**——它必须逐字节恒定。
    pub fn fixed_system(workspace: &str) -> String {
        format!(
            "你是一个个人助理 Agent，通过工具在用户电脑上完成真实任务。\n\
             【工作目录】{ws}（相对路径基于它；所有文件读写必须在此目录内）\n\
             \n\
             规则：\n\
             1. 先思考再行动；**独立的探查操作（读文件、grep、ls 等）必须在一轮里并行批量调用**，\n\
                不要一条一条慢慢试——每轮只发一条命令会迅速耗尽迭代预算。\n\
             2. 产出文件时写入工作目录（推荐放到 artifacts/ 下），并在 finish 的 artifacts 里登记交付物路径。\n\
             3. 任务完成时调用 finish 工具结束：在 summary 里给用户一段简洁总结。\n\
             4. 如果某个操作被拒绝，换一种策略，不要重复同样的调用。\n\
             5. 目标文件/目录不存在或目录为空时，**不要继续猜路径**；\n\
                直接说明缺什么，向用户索要材料或改用其他思路。\n\
             6. 迭代预算有限：拿到足以回答的信息就**立即收敛作答**，不要无限扩大检索范围。\n\
             \n\
             【审批】当你发起的工具调用需要用户确认时，用户批准后该工具的返回会以\n\
             [user-approved] 开头；被拒绝会以 [rejected by user] 或 [denied] 开头。\n\
             除此之外，你**无法得知**某次操作是否经过人工审批，也不得依据「操作成功了」\n\
             推断权限系统是否设防。",
            ws = workspace,
        )
    }

    /// 首轮 user 消息：本次任务 + 首次记忆快照。
    ///
    /// 任务只在这里出现一次（旧实现 system 里还有一份【用户任务】，
    /// 加上续跑尾部再补一条，同一句话最多出现三遍）。
    pub fn first_user(prompt: &str, memory: Option<&str>) -> Message {
        let mut s = format!("【当前任务】{prompt}");
        if let Some(text) = memory.map(str::trim).filter(|t| !t.is_empty()) {
            s.push_str("\n\n");
            s.push_str(MEMORY_MARK);
            s.push_str(text);
        }
        Message::user(s)
    }

    /// 从历史消息里提取**最近一次**注入的记忆文本（用于判断是否需要发增量）。
    ///
    /// 必须倒序遍历所有 user 消息：尾部通常是本次追问（普通 user），
    /// 只检查最后一条会漏掉历史里的记忆，导致每次追问都重发一遍记忆增量。
    pub fn extract_memory(messages: &[Message]) -> Option<String> {
        messages
            .iter()
            .rev()
            .filter(|m| m.role == Role::User)
            .find_map(|m| {
                let t = m.text();
                for mark in [MEMORY_UPDATE_MARK, MEMORY_MARK] {
                    if let Some(pos) = t.find(mark) {
                        return Some(t[pos + mark.len()..].trim().to_string());
                    }
                }
                None
            })
    }

    /// 记忆变化时的尾部增量消息：不改动历史，只在末尾追加。
    pub fn background_update(memory: &str) -> Message {
        Message::user(format!(
            "[背景更新] 长期记忆已变化，以下为最新内容，请以此为准：\n{}{}",
            MEMORY_UPDATE_MARK,
            memory.trim()
        ))
    }

    /// 续跑前的历史规范化：把「累积的旧 system」「内部伪用户提醒」
    /// 「孤儿 tool 消息 / 无配对 tool_calls」全部清掉。
    pub fn normalize_history(messages: Vec<Message>) -> Vec<Message> {
        let mut out: Vec<Message> = Vec::with_capacity(messages.len());
        let mut kept_system = false;
        for m in messages {
            if m.role == Role::System {
                // 只保留第一条 system（固定区）；后续追问叠出来的旧 system 全丢。
                if kept_system {
                    continue;
                }
                kept_system = true;
                out.push(m);
                continue;
            }
            // 兼容旧数据：R1 曾用 Role::User 注入内部提醒并永久留在历史里。
            if m.role == Role::User && m.text().starts_with("[系统提示]") {
                continue;
            }
            out.push(m);
        }
        if !kept_system {
            // 没有固定区时补一条空 system，保证 messages[0] 语义不变。
            out.insert(0, Message::system(""));
        }

        // 工具配对一致性：assistant.tool_calls 必须有对应的 tool 消息，反之亦然。
        let call_ids: std::collections::HashSet<String> = out
            .iter()
            .flat_map(|m| m.tool_calls.iter().map(|c: &ToolCall| c.id.0.clone()))
            .collect();
        let tool_ids: std::collections::HashSet<String> = out
            .iter()
            .filter(|m| m.role == Role::Tool)
            .filter_map(|m| m.tool_call_id.as_ref().map(|i| i.0.clone()))
            .collect();
        out.retain(|m| {
            if m.role == Role::Tool {
                return m
                    .tool_call_id
                    .as_ref()
                    .is_some_and(|i| call_ids.contains(&i.0));
            }
            true
        });
        for m in &mut out {
            if !m.tool_calls.is_empty() {
                m.tool_calls.retain(|c| tool_ids.contains(&c.id.0));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::ids::ToolCallId;
    use agent_core::provider::ContentPart;

    #[test]
    fn compresses_without_rewriting_system() {
        let cfg = ContextConfig {
            keep_recent: 2,
            ..Default::default()
        };
        let sys = ContextAssembler::fixed_system("/ws");
        let mut cm = ContextManager::new(cfg, sys.clone());
        cm.push(Message::user("u1"));
        cm.push(Message::assistant("a1"));
        cm.push(Message::user("u2"));
        cm.push(Message::assistant("a2"));
        cm.push(Message::user("u3"));
        cm.compress();
        assert_eq!(cm.messages()[0].text(), sys, "固定 system 必须逐字节不变");
        assert!(cm.messages()[1].text().contains("[历史摘要]"));
        assert!(cm.messages().len() <= 4);
        let (added, reset) = cm.flush_delta();
        assert!(reset, "压缩后应发全量基线");
        assert_eq!(added.len(), cm.messages().len());
    }

    /// 裁剪点落在 assistant(tool_calls) 与其 tool 结果之间时，必须整组保留。
    #[test]
    fn compress_never_orphans_tool_messages() {
        let cfg = ContextConfig {
            keep_recent: 1,
            ..Default::default()
        };
        let mut cm = ContextManager::new(cfg, "SYS");
        cm.push(Message::user("u1"));
        let call_a = ToolCallId::new();
        let call_b = ToolCallId::new();
        cm.push(Message {
            role: Role::Assistant,
            content: vec![],
            tool_calls: vec![
                ToolCall {
                    id: call_a.clone(),
                    name: "t".into(),
                    arguments: serde_json::json!({}),
                },
                ToolCall {
                    id: call_b.clone(),
                    name: "t".into(),
                    arguments: serde_json::json!({}),
                },
            ],
            tool_call_id: None,
            reasoning: None,
        });
        for id in [&call_a, &call_b] {
            cm.push(Message {
                role: Role::Tool,
                content: vec![ContentPart::text("ok")],
                tool_calls: vec![],
                tool_call_id: Some(id.clone()),
                reasoning: None,
            });
        }
        cm.push(Message::user("u2"));
        cm.push(Message::assistant("a2"));
        cm.compress();
        let msgs = cm.messages();
        assert_ne!(msgs[1].role, Role::Tool, "裁剪点切在了 tool 消息之前");
        let kept_ids: Vec<_> = msgs
            .iter()
            .flat_map(|m| m.tool_calls.iter().map(|c| c.id.clone()))
            .collect();
        for m in msgs.iter().filter(|m| m.role == Role::Tool) {
            let id = m.tool_call_id.as_ref().expect("tool 消息应有 tool_call_id");
            assert!(kept_ids.contains(id), "孤儿 tool 消息: {id:?}");
        }
    }

    #[test]
    fn estimate_tokens_counts_tool_args_and_chinese() {
        let mut cm = ContextManager::new(ContextConfig::default(), "SYS");
        cm.push(Message {
            role: Role::Assistant,
            content: vec![ContentPart::text("执行工具")],
            tool_calls: vec![ToolCall {
                id: ToolCallId::new(),
                name: "file_write".into(),
                arguments: serde_json::json!({"content": "x".repeat(3000)}),
            }],
            tool_call_id: None,
            reasoning: None,
        });
        assert!(
            cm.estimate_tokens() > 1000,
            "工具参数必须计入 token 估算：{}",
            cm.estimate_tokens()
        );
    }

    #[test]
    fn normalize_history_keeps_one_system_and_drops_fake_prompts() {
        let history = vec![
            Message::system("FIXED"),
            Message::user("第一问"),
            Message::system("旧 system"),
            Message::user("[系统提示] 迭代预算只剩 2 轮"),
            Message::assistant("答"),
        ];
        let out = ContextAssembler::normalize_history(history);
        assert_eq!(out.len(), 3);
        assert_eq!(out.iter().filter(|m| m.role == Role::System).count(), 1);
        assert_eq!(out[0].text(), "FIXED");
        assert!(out.iter().all(|m| !m.text().starts_with("[系统提示]")));
    }

    #[test]
    fn normalize_history_drops_orphan_tool_pairs() {
        let orphan_id = ToolCallId::new();
        let paired_id = ToolCallId::new();
        let history = vec![
            Message::system("FIXED"),
            Message {
                role: Role::Assistant,
                content: vec![],
                tool_calls: vec![ToolCall {
                    id: paired_id.clone(),
                    name: "ok".into(),
                    arguments: serde_json::json!({}),
                }],
                tool_call_id: None,
                reasoning: None,
            },
            Message {
                role: Role::Tool,
                content: vec![ContentPart::text("ok")],
                tool_calls: vec![],
                tool_call_id: Some(paired_id),
                reasoning: None,
            },
            Message {
                role: Role::Tool,
                content: vec![ContentPart::text("orphan")],
                tool_calls: vec![],
                tool_call_id: Some(orphan_id),
                reasoning: None,
            },
        ];
        let out = ContextAssembler::normalize_history(history);
        assert_eq!(out.len(), 3, "孤儿 tool 消息应被移除");
        assert_eq!(out[1].tool_calls.len(), 1);
    }

    #[test]
    fn first_user_contains_task_once_and_memory_is_extractable() {
        let m = ContextAssembler::first_user("帮我改 README", Some("提交信息用中文"));
        let t = m.text();
        assert_eq!(t.matches("帮我改 README").count(), 1);
        assert_eq!(
            ContextAssembler::extract_memory(std::slice::from_ref(&m)).as_deref(),
            Some("提交信息用中文")
        );
        let upd = ContextAssembler::background_update("端口改成 7890");
        assert_eq!(
            ContextAssembler::extract_memory(&[m, upd]).as_deref(),
            Some("端口改成 7890")
        );
    }

    #[test]
    fn fixed_system_has_no_task_or_memory() {
        let s = ContextAssembler::fixed_system("/ws");
        assert!(!s.contains("【用户任务】"));
        assert!(!s.contains("【长期记忆】"));
        assert!(s.contains("/ws"));
    }
}
