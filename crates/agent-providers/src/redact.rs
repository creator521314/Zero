//! 工具输出脱敏：把已知密钥值替换成 `<redacted>`。
//!
//! 为什么需要这一层：仅靠「不给子进程传密钥」（见 `agent-tools/src/shell.rs`）
//! 防不住 `cat config/config.toml` 这类把明文 key 读出来的路径。工具返回的原文
//! 会同时进入两处——回填给模型的上下文、以及 jsonl 审计日志——所以要在**回填之前**
//! 做值级替换。
//!
//! 两层防线（先精确后模式）：
//! 1. **已知值替换**：进程环境里名字像密钥的变量 + 配置里各 provider 的实际 key，
//!    逐值替换。零误报，但只认见过的值；
//! 2. **形态识别**：对从未见过的密钥（比如 `env` 输出里第三方服务的 key），
//!    按常见密钥格式做模式匹配（`sk-`、`ghp_`、AWS AKIA、JWT、`key = "..."` 等）。
//!    模式刻意保守——要求长度下限 + 字符集约束，把误伤压到最低；代价只是
//!    一段正常文本被替换成 `<redacted>`，而漏掉一个真 key 的代价是泄漏，
//!    所以这里宁可偏保守地多拦。

use std::collections::BTreeSet;
use std::sync::OnceLock;

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

/// 形态识别模式（第二层）。刻意保守：全部带长度下限，避免误伤普通文本。
///
/// 排查新误报时往这里加白名单逻辑，而不是删模式。
const PATTERNS: &[&str] = &[
    // OpenAI / Anthropic 风格：sk-xxx（含 sk-proj-、sk-ant-），20+ 载荷
    r"\bsk-[A-Za-z0-9_-]{20,}\b",
    // GitHub classic token：ghp_/gho_/ghu_/ghs_/ghr_ + 36 字符
    r"\bgh[pousr]_[A-Za-z0-9]{36,}\b",
    // GitHub fine-grained PAT
    r"\bgithub_pat_[A-Za-z0-9_]{22,}\b",
    // AWS Access Key ID
    r"\bAKIA[0-9A-Z]{16}\b",
    // Slack token
    r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b",
    // JWT（三段 base64url）
    r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b",
    // Bearer 头里的裸 token
    r"(?i)\bbearer\s+[A-Za-z0-9._-]{24,}",
    // key = "value" / api_key: value —— 名字像密钥 + 值 16+ 且有数字
    // （要求含数字是关键：纯字母词如 "ConfigurationSettings" 不误伤）
    r#"(?i)\b(api[_-]?key|apikey|secret[_-]?key|access[_-]?key|auth[_-]?token|client[_-]?secret|password)\b["']?\s*[:=]\s*["']?([A-Za-z0-9_.+-]{16,})["']?"#,
];

fn patterns() -> &'static Vec<regex::Regex> {
    static RE: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        PATTERNS
            .iter()
            .filter_map(|p| regex::Regex::new(p).ok())
            .collect()
    })
}

/// 形态识别下的单个替换：普通模式整段替换；key=value 只换值（保留名字可读）。
fn apply_patterns(text: &str) -> String {
    let mut out = text.to_string();
    for re in patterns() {
        out = re
            .replace_all(&out, |caps: &regex::Captures| match caps.get(2) {
                // key = "value" 形态：只把值打码，保留 "key = " 可读
                Some(val) => caps
                    .get(0)
                    .unwrap()
                    .as_str()
                    .replace(val.as_str(), "<redacted>"),
                None => "<redacted>".to_string(),
            })
            .into_owned();
    }
    out
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

    /// 替换文本里出现的所有已知密钥值，再过一遍形态识别。
    pub fn redact(&self, text: &str) -> String {
        if self.secrets.is_empty() {
            return apply_patterns(text);
        }
        let mut out = text.to_string();
        for secret in &self.secrets {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), "<redacted>");
            }
        }
        apply_patterns(&out)
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
        // SAFETY: 测试专用唯一变量名，写入后立即读取并删除，无并发读写。
        unsafe { std::env::set_var(ENV_NAME, "sk-env-value-123456") };

        let mut cfg = ProvidersFile::default();
        let deepseek = cfg.providers.get_mut("deepseek").unwrap();
        deepseek.api_key_env = None; // 避免测试机上的真实环境变量干扰
        deepseek.api_key = Some("sk-inline-value-abcdef".into());

        let r = SecretRedactor::from_config(&cfg);
        unsafe { std::env::remove_var(ENV_NAME) };

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
        // SAFETY: 测试专用唯一变量名，写入后立即读取并删除，无并发读写。
        unsafe { std::env::set_var(ENV_NAME, "sk-1") };

        let mut cfg = ProvidersFile::default();
        cfg.providers.get_mut("deepseek").unwrap().api_key_env = None;
        cfg.providers.get_mut("deepseek").unwrap().api_key = None;

        let r = SecretRedactor::from_config(&cfg);
        unsafe { std::env::remove_var(ENV_NAME) };

        assert_eq!(r.redact("sk-1"), "sk-1", "过短的值不该参与替换");
    }

    #[test]
    fn empty_redactor_is_identity() {
        let r = SecretRedactor::empty();
        assert_eq!(r.redact("nothing"), "nothing");
        assert_eq!(r.secret_count(), 0);
    }

    /// 第二层：从未见过的密钥也要按形态拦下（根因：脱敏只认已知明文值，
    /// `env` 输出里第三方服务的 key 会原样落进上下文与日志）。
    #[test]
    fn pattern_layer_catches_unseen_secret_shapes() {
        let r = SecretRedactor::empty();
        for (name, text, needle) in [
            (
                "openai",
                "key is sk-proj-4fA9x2Bc8De7Fg1Hj3Kl5Mn9Op2Qr4St here",
                "sk-proj-4fA9x2Bc8De7Fg1Hj3Kl5Mn9Op2Qr4St",
            ),
            (
                "anthropic",
                "sk-ant-api03-XXXXXXXXXXXXXX1234567890abcdef",
                "sk-ant-api03-XXXXXXXXXXXXXX1234567890abcdef",
            ),
            (
                "github",
                "token ghp_0123456789abcdefghijklmnopqrstuvwxyzAB",
                "ghp_0123456789abcdefghijklmnopqrstuvwxyzAB",
            ),
            (
                "gh_pat",
                "github_pat_11AAAAAAA0abcdefghij_0123456789ABCDEFG",
                "github_pat_11AAAAAAA0abcdefghij_0123456789ABCDEFG",
            ),
            (
                "aws",
                "AKIAIOSFODNN7EXAMPLE is the access key",
                "AKIAIOSFODNN7EXAMPLE",
            ),
            // 注意：样例必须是 GitHub secret 扫描器不认定为真实 token 的假串，
            // 否则 push protection 会拒收整个提交。
            (
                "slack",
                "xoxb-EXAMPLE-not-a-real-secret",
                "xoxb-EXAMPLE-not-a-real-secret",
            ),
            (
                "jwt",
                "token eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U end",
                "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0",
            ),
            (
                "bearer",
                "Authorization: Bearer abc123def456ghi789jkl012mno345",
                "abc123def456ghi789jkl012mno345",
            ),
            (
                "kv",
                "api_key = \"supersecretvalue123456\" in config.toml",
                "supersecretvalue123456",
            ),
        ] {
            let out = r.redact(text);
            assert!(!out.contains(needle), "{name} 未被脱敏: {out}");
            assert!(out.contains("<redacted>"), "{name} 缺占位符: {out}");
        }
    }

    /// 模式必须保守：普通文本、变量名、短词不能被打码。
    #[test]
    fn pattern_layer_does_not_mangle_normal_text() {
        let r = SecretRedactor::empty();
        for text in [
            "cargo build --release",
            "let api_key = load_config();",            // 值太短
            "openai_api_key = config.api_key.clone()", // 值太短
            "sk-1 and sk-short",                       // 过短的 sk-
            "Bearer token authentication is used",     // bearer 后是普通词组
            "THE_PASSWORD_IS_SECRET",                  // 无分隔符的普通句子
        ] {
            assert_eq!(r.redact(text), text, "误伤了正常文本: {text}");
        }
    }

    /// 两层叠加：已知值先替换，形态层兜底。
    #[test]
    fn known_values_win_over_patterns() {
        let mut cfg = ProvidersFile::default();
        cfg.providers.get_mut("deepseek").unwrap().api_key_env = None;
        cfg.providers.get_mut("deepseek").unwrap().api_key =
            Some("sk-proj-4fA9x2Bc8De7Fg1Hj3Kl5Mn9Op2Qr4St".into());
        let r = SecretRedactor::from_config(&cfg);
        let out = r.redact("use sk-proj-4fA9x2Bc8De7Fg1Hj3Kl5Mn9Op2Qr4St now");
        assert_eq!(out.matches("<redacted>").count(), 1, "只替换一次: {out}");
    }
}
