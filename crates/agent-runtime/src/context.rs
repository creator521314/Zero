//! 上下文三层管理（对应方案 4.2）。
//!
//! 1. 全量层：最近 N 轮原始 messages；
//! 2. 摘要层：更早轮次滚动摘要（本期实现为按轮次裁剪 + 系统固定区）；
//! 3. 文件外置层：超大工具输出写入 ctx/，上下文只留摘要 + 路径。

use agent_core::provider::Message;

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
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            keep_recent: 12,
            compress_at_ratio: 0.7,
            externalize_over_bytes: 8 * 1024,
            max_context: 128_000,
        }
    }
}

/// 上下文管理器。
pub struct ContextManager {
    cfg: ContextConfig,
    /// 系统固定区（永不丢弃）：原始任务、工作目录、产物路径。
    pinned_system: String,
    /// 已完成轮次的累积摘要。
    summary: String,
    /// 活动消息（system + 对话）。
    messages: Vec<Message>,
}

impl ContextManager {
    pub fn new(cfg: ContextConfig, pinned_system: impl Into<String>) -> Self {
        let pinned = pinned_system.into();
        Self {
            cfg,
            messages: vec![Message::system(pinned.clone())],
            pinned_system: pinned,
            summary: String::new(),
        }
    }

    pub fn push(&mut self, m: Message) {
        self.messages.push(m);
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// 粗略 token 估算（字符数 / 3，中英混合的保守近似）。
    pub fn estimate_tokens(&self) -> usize {
        let total: usize = self.messages.iter().map(|m| m.text().len()).sum();
        total / 3
    }

    pub fn should_compress(&self) -> bool {
        let threshold = (self.cfg.max_context as f64 * self.cfg.compress_at_ratio) as usize;
        self.estimate_tokens() > threshold
    }

    /// 压缩：把超出保留窗口的旧消息折叠进摘要区（一期为轻量摘录，二期可接 LLM 摘要）。
    pub fn compress(&mut self) {
        // messages[0] 是 system；对话消息条数 = len - 1
        let dialog_count = self.messages.len().saturating_sub(1);
        if dialog_count <= self.cfg.keep_recent {
            return;
        }
        // 需要裁掉的最旧对话消息条数
        let to_drop = dialog_count - self.cfg.keep_recent;
        // 从索引 1 开始裁剪，保留最后 keep_recent 条
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
        // 重建 system：固定区 + 历史摘要
        let sys = if self.summary.is_empty() {
            self.pinned_system.clone()
        } else {
            format!(
                "{}\n\n[已完成的历史摘要]\n{}",
                self.pinned_system, self.summary
            )
        };
        self.messages[0] = Message::system(sys);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compresses_old_messages() {
        let cfg = ContextConfig {
            keep_recent: 2,
            ..Default::default()
        };
        let mut cm = ContextManager::new(cfg, "SYSTEM PINNED");
        cm.push(Message::user("u1"));
        cm.push(Message::assistant("a1"));
        cm.push(Message::user("u2"));
        cm.push(Message::assistant("a2"));
        cm.push(Message::user("u3"));
        cm.compress();
        // system 仍在，且带有历史摘要
        let sys = cm.messages()[0].text();
        assert!(sys.contains("SYSTEM PINNED"));
        assert!(sys.contains("历史摘要"));
        // 保留下来的最近消息数
        assert!(cm.messages().len() <= 3);
    }
}
