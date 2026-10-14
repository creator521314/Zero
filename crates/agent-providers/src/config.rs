//! 配置模型（对应方案 5.3）。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 顶层配置文件：`~/.config/agent/config.toml`。
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
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
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
    /// 从路径加载；不存在则返回默认配置。
    pub fn load_or_default(path: &std::path::Path) -> anyhow::Result<Self> {
        if path.exists() {
            let s = std::fs::read_to_string(path)?;
            Ok(toml::from_str(&s)?)
        } else {
            Ok(Self::default())
        }
    }

    /// 写出默认配置（用于首次运行生成模板）。
    pub fn write_default(path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(path, toml::to_string_pretty(&Self::default())?)?;
        Ok(())
    }
}
