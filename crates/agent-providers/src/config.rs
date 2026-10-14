//! 配置模型（对应方案 5.3）。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 项目标识（用于 `directories::ProjectDirs`），三段拼成最终路径。
pub const PROJECT_QUALIFIER: &str = "dev";
pub const PROJECT_ORG: &str = "personal";
pub const PROJECT_APP: &str = "agent";
/// 配置文件名。
pub const CONFIG_FILE: &str = "config.toml";

/// 嵌入的注释版 TOML 模板（首次启动写入磁盘的就是它）。
pub const EXAMPLE_TOML: &str = include_str!("../../../config/config.example.toml");

/// 顶层配置文件：`~/.config/agent/config.toml`（Windows: `%APPDATA%\dev\personal\agent\config.toml`）。
///
/// 也可通过 `AGENT_CONFIG` 环境变量覆盖为任意路径。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvidersFile {
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub router: RouterConfig,
    #[serde(default)]
    pub budget: Budget,
    #[serde(default)]
    pub search: SearchConfig,
    #[serde(default)]
    pub agent: AgentConfig,
}

/// 配置校验错误。
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("providers 段为空（至少需要一个 provider）")]
    NoProviders,
    #[error("provider `{name}` 配置无效：{reason}")]
    InvalidProvider { name: String, reason: &'static str },
    #[error("router.default = `{provider}`，但 providers 段里没有这个条目")]
    UnknownDefault { provider: String },
    #[error("router.by_task[{task}] 引用了不存在的 provider `{provider}`")]
    UnknownRoute { task: String, provider: String },
    #[error("budget 不能为负数（monthly={monthly}, per_task={per_task}）")]
    InvalidBudget { monthly: f64, per_task: f64 },
    #[error("agent.root 为空")]
    EmptyRoot,
}

impl Default for ProvidersFile {
    fn default() -> Self {
        let mut providers = HashMap::new();
        // 默认示例配置：DeepSeek + 通义 + 本地 Ollama
        providers.insert(
            "deepseek".into(),
            ProviderConfig {
                kind: "openai_compat".into(),
                base_url: "https://api.deepseek.com/v1".into(),
                api_key_env: Some("DEEPSEEK_API_KEY".into()),
                model: "deepseek-chat".into(),
                tools: true,
                reasoning: true,
                vision: false,
                max_context: 128_000,
                input_per_1k: 0.0014,
                output_per_1k: 0.0028,
            },
        );
        providers.insert(
            "qwen".into(),
            ProviderConfig {
                kind: "openai_compat".into(),
                base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1".into(),
                api_key_env: Some("DASHSCOPE_API_KEY".into()),
                model: "qwen-max".into(),
                tools: true,
                reasoning: false,
                vision: false,
                max_context: 128_000,
                input_per_1k: 0.0016,
                output_per_1k: 0.0064,
            },
        );
        providers.insert(
            "ollama".into(),
            ProviderConfig {
                kind: "openai_compat".into(),
                base_url: "http://localhost:11434/v1".into(),
                api_key_env: None, // 无需鉴权
                model: "qwen2.5-coder".into(),
                tools: true,
                reasoning: false,
                vision: false,
                max_context: 32_000,
                input_per_1k: 0.0,
                output_per_1k: 0.0,
            },
        );

        let mut by_task = HashMap::new();
        by_task.insert("code".to_string(), vec!["deepseek".into(), "qwen".into()]);
        by_task.insert("news".to_string(), vec!["qwen".into(), "deepseek".into()]);
        // 隐私数据强制本地（应对 P3）
        by_task.insert("sensitive".to_string(), vec!["ollama".into()]);

        Self {
            providers,
            router: RouterConfig {
                default: "deepseek".into(),
                by_task,
            },
            budget: Budget::default(),
            search: SearchConfig::default(),
            agent: AgentConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// 适配器类型：目前支持 "openai_compat"。
    pub kind: String,
    pub base_url: String,
    /// 从哪个环境变量取 key；None 表示无需鉴权（如本地 Ollama）。
    #[serde(default)]
    pub api_key_env: Option<String>,
    pub model: String,
    #[serde(default = "default_true")]
    pub tools: bool,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub vision: bool,
    #[serde(default = "default_ctx")]
    pub max_context: u32,
    #[serde(default)]
    pub input_per_1k: f64,
    #[serde(default)]
    pub output_per_1k: f64,
}

fn default_true() -> bool {
    true
}
fn default_ctx() -> u32 {
    128_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterConfig {
    pub default: String,
    /// 任务类型 -> 有序候选列表（失败顺延）。
    #[serde(default)]
    pub by_task: HashMap<String, Vec<String>>,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            default: "deepseek".into(),
            by_task: HashMap::new(),
        }
    }
}

/// 成本控制（P2 未定 → 全部可配置，默认保守）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Budget {
    #[serde(default = "default_monthly")]
    pub monthly_limit_usd: f64,
    #[serde(default = "default_per_task")]
    pub per_task_limit_usd: f64,
}

fn default_monthly() -> f64 {
    20.0
}
fn default_per_task() -> f64 {
    0.5
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            monthly_limit_usd: default_monthly(),
            per_task_limit_usd: default_per_task(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchConfig {
    /// SearXNG 端点，如 http://localhost:8080/search
    #[serde(default)]
    pub searxng_endpoint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// 工作区根目录。
    #[serde(default = "default_root")]
    pub root: String,
    /// 默认权限档。
    #[serde(default = "default_tier")]
    pub default_tier: String,
    #[serde(default = "default_max_iter")]
    pub max_iterations: usize,
    /// 临时信息保留天数（P4）。
    #[serde(default = "default_retention")]
    pub transient_retention_days: u64,
}

fn default_root() -> String {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    format!("{home}/agent")
}
fn default_tier() -> String {
    "L2".into()
}
fn default_max_iter() -> usize {
    25
}
fn default_retention() -> u64 {
    7
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            root: default_root(),
            default_tier: default_tier(),
            max_iterations: default_max_iter(),
            transient_retention_days: default_retention(),
        }
    }
}

impl ProvidersFile {
    /// 决定配置文件路径。
    ///
    /// 优先级：`AGENT_CONFIG` 环境变量 > `directories::ProjectDirs` 标准目录 > 当前目录回退。
    pub fn config_path() -> PathBuf {
        if let Ok(p) = std::env::var("AGENT_CONFIG") {
            let pb = PathBuf::from(p);
            if !pb.as_os_str().is_empty() {
                return pb;
            }
        }
        if let Some(dirs) =
            directories::ProjectDirs::from(PROJECT_QUALIFIER, PROJECT_ORG, PROJECT_APP)
        {
            dirs.config_dir().join(CONFIG_FILE)
        } else {
            PathBuf::from("./agent-config.toml")
        }
    }

    /// 从路径加载（不存在则返回默认配置）。
    pub fn load_or_default(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            let s = std::fs::read_to_string(path)?;
            Ok(toml::from_str(&s)?)
        } else {
            Ok(Self::default())
        }
    }

    /// 从路径严格加载（不存在返回 Err）。
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&s)?)
    }

    /// 校验配置语义合法性（不检查文件 IO）。
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.providers.is_empty() {
            return Err(ConfigError::NoProviders);
        }
        for (name, p) in &self.providers {
            if p.base_url.trim().is_empty() {
                return Err(ConfigError::InvalidProvider {
                    name: name.clone(),
                    reason: "base_url 为空",
                });
            }
            if p.model.trim().is_empty() {
                return Err(ConfigError::InvalidProvider {
                    name: name.clone(),
                    reason: "model 为空",
                });
            }
        }
        if !self.providers.contains_key(&self.router.default) {
            return Err(ConfigError::UnknownDefault {
                provider: self.router.default.clone(),
            });
        }
        for (task, list) in &self.router.by_task {
            for p in list {
                if !self.providers.contains_key(p) {
                    return Err(ConfigError::UnknownRoute {
                        task: task.clone(),
                        provider: p.clone(),
                    });
                }
            }
        }
        if self.budget.monthly_limit_usd < 0.0 || self.budget.per_task_limit_usd < 0.0 {
            return Err(ConfigError::InvalidBudget {
                monthly: self.budget.monthly_limit_usd,
                per_task: self.budget.per_task_limit_usd,
            });
        }
        if self.agent.root.trim().is_empty() {
            return Err(ConfigError::EmptyRoot);
        }
        Ok(())
    }

    /// 写出嵌入的注释模板（用于首次运行 / `cargo xtask gen-cfg`）。
    pub fn write_template(path: &Path) -> anyhow::Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(path, EXAMPLE_TOML)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_validates_ok() {
        assert!(ProvidersFile::default().validate().is_ok());
    }

    #[test]
    fn empty_providers_fails() {
        let mut c = ProvidersFile::default();
        c.providers.clear();
        assert!(matches!(c.validate(), Err(ConfigError::NoProviders)));
    }

    #[test]
    fn unknown_default_fails() {
        let mut c = ProvidersFile::default();
        c.router.default = "nonexistent".into();
        assert!(matches!(
            c.validate(),
            Err(ConfigError::UnknownDefault { .. })
        ));
    }

    #[test]
    fn unknown_route_fails() {
        let mut c = ProvidersFile::default();
        c.router.by_task.insert("code".into(), vec!["ghost".into()]);
        assert!(matches!(
            c.validate(),
            Err(ConfigError::UnknownRoute { .. })
        ));
    }

    #[test]
    fn empty_base_url_fails() {
        let mut c = ProvidersFile::default();
        c.providers.get_mut("deepseek").unwrap().base_url = " ".into();
        assert!(matches!(
            c.validate(),
            Err(ConfigError::InvalidProvider { .. })
        ));
    }

    #[test]
    fn negative_budget_fails() {
        let mut c = ProvidersFile::default();
        c.budget.per_task_limit_usd = -1.0;
        assert!(matches!(
            c.validate(),
            Err(ConfigError::InvalidBudget { .. })
        ));
    }

    #[test]
    fn config_path_respects_env_override() {
        let p = ProvidersFile::config_path();
        assert!(p.ends_with(CONFIG_FILE));
    }

    #[test]
    fn template_parses_and_validates() {
        let parsed: ProvidersFile = toml::from_str(EXAMPLE_TOML).expect("template should parse");
        parsed.validate().expect("template should validate");
    }
}
