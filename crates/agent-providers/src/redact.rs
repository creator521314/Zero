//! 工具输出脱敏：把已知密钥值替换成 `<redacted>`。
//!
//! 为什么需要这一层：仅靠「不给子进程传密钥」（见 `agent-tools/src/shell.rs`）
//! 防不住 `cat config/config.toml` 这类把明文 key 读出来的路径。工具返回的原文
//! 会同时进入两处——回填给模型的上下文、以及 jsonl 审计日志——所以要在**回填之前**
//! 做值级替换。
//!
//! 设计取舍：
//! - 只替换「已知的密钥值」而不是猜模式，避免把模型正常讨论的 `sk-...` 文本也改掉；
//! - 值来自两处：进程环境里名字像密钥的变量，以及配置里各 provider 解析出的实际 key；
//! - 过短的值（< 8 字符）不参与，否则 `sk-1` 这种会误伤大量正常文本。

use std::collections::BTreeSet;

use crate::config::ProvidersFile;

/// 与 `agent-tools/src/shell.rs::DENIED_ENV_SUFFIXES` 保持同一套口径。
///
/// 两处分别定义而不是共享，是为了守住「实现 crate 只依赖 agent-core」的依赖铁律
/// （agent-tools 与 agent-providers 之间不该有依赖）。改动时两处一起改。
const SENSITIVE_ENV_SUFFIXES: &[&str] = &[
    "_API_KEY",
    "_API_TOKEN",
    "_TOKEN",
    "_SECRET",
    "_SECRET_KEY",
    "_ACCESS_KEY",
    "_PASSWORD",
    "_PASSWD",
    "_CREDENTIALS",
];

/// 变量名是否像密钥。
fn is_sensitive_name(name: &str) -> bool {
    SENSITIVE_ENV_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// 已知密钥值的集合；`redact` 把它们替换成 `<redacted>`。
pub struct SecretRedactor {
    secrets: Vec<String>,
}

impl SecretRedactor {
    /// 从配置 + 当前进程环境收集密钥值。
    ///
    /// 环境变量在**构造时**读取一次：agent 启动后再改环境不影响已注入的脱敏器
    /// （正常用法也不会中途换 key）。
    pub fn from_config(cfg: &ProvidersFile) -> Self {
        let mut set = BTreeSet::new();

        for (name, value) in std::env::vars() {
            if is_sensitive_name(&name) && !value.trim().is_empty() {
                set.insert(value);
            }
        }
        for provider in cfg.providers.values() {
            if let Some(key) = provider.resolve_api_key() {
                set.insert(key);
            }
        }

        let mut secrets: Vec<String> = set.into_iter().filter(|s| s.chars().count() >= 8).collect();
        // 长值优先：避免某段子串先被替换、导致更长的值匹配不上（语义上无差别，
        // 但输出更稳定，也便于排查）。
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        Self { secrets }
    }

    /// 空脱敏器（测试 / 无密钥场景）。
    pub fn empty() -> Self {
        Self {
            secrets: Vec::new(),
        }
    }

    /// 替换文本里出现的所有已知密钥值。
    pub fn redact(&self, text: &str) -> String {
        if self.secrets.is_empty() {
            return text.to_string();
        }
        let mut out = text.to_string();
        for secret in &self.secrets {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), "<redacted>");
            }
        }
        out
    }

    /// 收集到的密钥值数量（仅测试与诊断用，不暴露值）。
    pub fn secret_count(&self) -> usize {
        self.secrets.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProvidersFile;

    /// 环境变量与配置明文两条来源都要覆盖；过短的值不参与。
    #[test]
    fn redacts_env_and_inline_secrets() {
        const ENV_NAME: &str = "AGENT_REDACT_TEST_API_KEY";
        std::env::set_var(ENV_NAME, "sk-env-value-123456");

        let mut cfg = ProvidersFile::default();
        let deepseek = cfg.providers.get_mut("deepseek").unwrap();
        deepseek.api_key_env = None; // 避免测试机上的真实环境变量干扰
        deepseek.api_key = Some("sk-inline-value-abcdef".into());

        let r = SecretRedactor::from_config(&cfg);
        std::env::remove_var(ENV_NAME);

        let out = r.redact("x sk-env-value-123456 y sk-inline-value-abcdef z");
        assert!(
            !out.contains("sk-env-value-123456"),
            "env 值必须被替换: {out}"
        );
        assert!(
            !out.contains("sk-inline-value-abcdef"),
            "配置里的明文 key 必须被替换: {out}"
        );
        assert!(out.contains("<redacted>"), "应留下可读的占位: {out}");
        assert!(out.contains(" z"), "无关文本必须保持原样: {out}");
    }

    #[test]
    fn short_values_are_ignored() {
        const ENV_NAME: &str = "AGENT_REDACT_SHORT_API_KEY";
        std::env::set_var(ENV_NAME, "sk-1");

        let mut cfg = ProvidersFile::default();
        cfg.providers.get_mut("deepseek").unwrap().api_key_env = None;
        cfg.providers.get_mut("deepseek").unwrap().api_key = None;

        let r = SecretRedactor::from_config(&cfg);
        std::env::remove_var(ENV_NAME);

        assert_eq!(r.redact("sk-1"), "sk-1", "过短的值不该参与替换");
    }

    #[test]
    fn empty_redactor_is_identity() {
        let r = SecretRedactor::empty();
        assert_eq!(r.redact("nothing"), "nothing");
        assert_eq!(r.secret_count(), 0);
    }
}
