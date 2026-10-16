//! 配置模型（对应方案 5.3）。

// 审计落盘策略定义在 agent-core（落盘规则与事件模型同处一个 crate），
// 此处再导出，让配置的使用方统一从 `config` 取。
pub use agent_core::store::LogConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 配置文件名。
pub const CONFIG_FILE: &str = "config.toml";
/// 默认配置文件（相对当前工作目录，即项目根下的 `config/config.toml`）。
///
/// 一期为单机单人使用，配置跟着项目走；`AGENT_CONFIG` 环境变量可覆盖为任意路径。
pub const DEFAULT_CONFIG_REL: &str = "config/config.toml";

/// 嵌入的注释版 TOML 模板（首次启动写入磁盘的就是它）。
pub const EXAMPLE_TOML: &str = include_str!("../../../config/config.example.toml");

/// 顶层配置文件，默认 `<项目根>/config/config.toml`。
///
/// 路径优先级：
/// 1. `AGENT_CONFIG` 环境变量（显式覆盖，测试 / 多环境用）；
/// 2. 当前工作目录下的 `config/config.toml`（默认）。
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
    /// 审计日志落盘策略。
    #[serde(default)]
    pub log: LogConfig,
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
                api_key: None, // 默认不写明文 key；由环境变量提供
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
                api_key: None,
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
                api_key: None,
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
            log: LogConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// 适配器类型：目前支持 "openai_compat"。
    pub kind: String,
    pub base_url: String,
    /// 从哪个环境变量取 key（**优先级高于** `api_key`）；None 表示无需鉴权（如本地 Ollama）。
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// 明文 API key，环境变量未设置时作为兜底。
    ///
    /// `config/config.toml` 已被 `.gitignore` 排除，因此允许明文写入；
    /// 两个字段都为空才视为「无需鉴权」（本地端点）。
    #[serde(default)]
    pub api_key: Option<String>,
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

impl ProviderConfig {
    /// 该 provider 是否需要鉴权（两个 key 字段任一被声明即需要）。
    pub fn needs_auth(&self) -> bool {
        self.api_key_env.is_some() || self.api_key.is_some()
    }

    /// 解析实际使用的 API key。
    ///
    /// 优先级：`api_key_env` 指向的环境变量 > 配置文件里的 `api_key` 明文。
    /// 环境变量存在但为空白串时视为未设置（继续走 fallback）。
    pub fn resolve_api_key(&self) -> Option<String> {
        if let Some(env) = &self.api_key_env {
            if let Ok(v) = std::env::var(env) {
                let v = v.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
        if let Some(k) = &self.api_key {
            let k = k.trim();
            if !k.is_empty() {
                return Some(k.to_string());
            }
        }
        None
    }
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
    /// 私有数据根目录（日志、每任务工作区）。
    #[serde(default = "default_root")]
    pub root: String,
    /// 项目根目录：工具读写的沙箱，也是相对路径的基准。
    ///
    /// 默认为 `.`，即**启动 agent 时的当前目录**——Agent 只有在真实项目
    /// 目录里才能对「介绍当前项目」这类任务给出有效回答；每任务的私有
    /// 数据仍放在 `root/workspace/<task-id>`，不会污染项目。
    /// 也支持绝对路径与 `~`；留空则退回私有工作区（旧行为）。
    #[serde(default = "default_project_root")]
    pub project_root: String,
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
    "~/agent".into()
}
fn default_project_root() -> String {
    ".".into()
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

impl AgentConfig {
    /// 解析工作区根目录：展开 `~` 前缀为真实 home 目录。
    ///
    /// 模板默认值是 `~/agent`（用户常直接沿用）。若不展开，会在**当前工作
    /// 目录**下创建字面名为 `~` 的目录（Windows 上表现为项目里出现 `~/agent/`）。
    pub fn resolved_root(&self) -> PathBuf {
        expand_home(&self.root)
    }

    /// 解析项目根目录：`.` 取启动目录，其余展开 `~`。
    ///
    /// 取不到当前目录时退回私有根目录，绝不静默变成空路径。
    pub fn resolved_project_root(&self) -> PathBuf {
        let raw = self.project_root.trim();
        if raw.is_empty() || raw == "." || raw == "./" {
            return std::env::current_dir()
                .unwrap_or_else(|_| self.resolved_root().join("workspace"));
        }
        expand_home(raw)
    }
}

/// 展开路径中的 `~` 前缀（`~` 或 `~/...`）。
///
/// 找不到 home 目录时原样返回，让后续 IO 报错给出明确信息。
pub fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        if let Some(h) = home_dir() {
            return h;
        }
    } else if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        if let Some(h) = home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(path)
}

/// 取 home 目录：Windows 用 `USERPROFILE`，其余用 `HOME`，最后回退 `directories`。
pub fn home_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("USERPROFILE") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    if let Ok(p) = std::env::var("HOME") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf())
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            root: default_root(),
            project_root: default_project_root(),
            default_tier: default_tier(),
            max_iterations: default_max_iter(),
            transient_retention_days: default_retention(),
        }
    }
}

impl ProvidersFile {
    /// 决定配置文件路径。
    ///
    /// 优先级：`AGENT_CONFIG` 环境变量 > 项目下 `config/config.toml`（默认）。
    pub fn config_path() -> PathBuf {
        if let Ok(p) = std::env::var("AGENT_CONFIG") {
            let pb = PathBuf::from(p);
            if !pb.as_os_str().is_empty() {
                return pb;
            }
        }
        PathBuf::from(DEFAULT_CONFIG_REL)
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

    /// 非致命配置警告（不阻塞启动，仅提示常见踩坑）。
    ///
    /// 最典型的一类：已知厂商的 `base_url` 缺少 API 路径段（如 DeepSeek
    /// 写成 `https://api.deepseek.com` 而漏掉 `/v1`）。这种错误下厂商仍会
    /// 先做认证，返回的却是「api key invalid」——极易误判为 key 问题。
    pub fn warnings(&self) -> Vec<String> {
        /// (域名关键字, 必需的路径段)
        const KNOWN: &[(&str, &str)] = &[
            ("api.deepseek.com", "/v1"),
            ("dashscope.aliyuncs.com", "/compatible-mode/v1"),
            ("api.openai.com", "/v1"),
            ("api.anthropic.com", "/v1"),
            ("open.bigmodel.cn", "/api/paas/v4"),
            ("api.moonshot.cn", "/v1"),
        ];
        let mut out = Vec::new();
        for (name, p) in &self.providers {
            for (host, required) in KNOWN {
                if p.base_url.contains(host) && !p.base_url.contains(required) {
                    out.push(format!(
                        "provider `{name}`: base_url `{}` 可能缺少 `{required}` 路径段\
                         （{host} 的 API 路径）；厂商对此仍会先认证，报错会是\
                         「api key invalid」从而误导排查",
                        p.base_url
                    ));
                }
            }
        }
        // 项目根不存在会让所有文件类工具集体失败，且症状看起来像「工具坏了」。
        let proj = self.agent.resolved_project_root();
        if !proj.is_dir() {
            out.push(format!(
                "agent.project_root 指向的目录不存在：{}（工具会找不到任何文件；\
                 改为 `.` 表示启动目录）",
                proj.display()
            ));
        }
        out
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
    fn config_path_defaults_to_project_config() {
        // 默认（AGENT_CONFIG 未设时）应为项目下 config/config.toml。
        // 注意：不在测试里设 AGENT_CONFIG，避免并行测试互相污染。
        let p = ProvidersFile::config_path();
        assert_eq!(p, PathBuf::from(DEFAULT_CONFIG_REL));
        assert!(p.ends_with(CONFIG_FILE));
    }

    #[test]
    fn template_parses_and_validates() {
        let parsed: ProvidersFile = toml::from_str(EXAMPLE_TOML).expect("template should parse");
        parsed.validate().expect("template should validate");
    }

    #[test]
    fn expand_home_handles_tilde_prefix() {
        let home = home_dir().expect("home dir should resolve");
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/agent"), home.join("agent"));
        assert_eq!(expand_home("~\\agent"), home.join("agent"));
    }

    #[test]
    fn expand_home_leaves_absolute_untouched() {
        let abs = if cfg!(windows) {
            "D:\\data\\agent"
        } else {
            "/data/agent"
        };
        assert_eq!(expand_home(abs), PathBuf::from(abs));
    }

    #[test]
    fn expand_home_leaves_relative_untouched() {
        assert_eq!(expand_home("./agent"), PathBuf::from("./agent"));
        assert_eq!(expand_home("agent"), PathBuf::from("agent"));
    }

    #[test]
    fn resolved_root_expands_tilde() {
        let cfg = AgentConfig {
            root: "~/agent".into(),
            ..Default::default()
        };
        let resolved = cfg.resolved_root();
        let home = home_dir().expect("home dir should resolve");
        assert_eq!(resolved, home.join("agent"));
        // 关键回归：不得残留字面 `~` 段（否则会在 CWD 下创建 `~/agent/`）。
        assert!(!resolved.components().any(|c| c.as_os_str() == "~"));
    }

    #[test]
    fn default_root_is_tilde_form() {
        // 模板与 default 一致：都是 `~/agent`，由 resolved_root() 展开。
        assert_eq!(AgentConfig::default().root, "~/agent");
    }

    // ---- api_key 解析（环境变量优先，明文兜底） ----

    fn provider_with(env: Option<&str>, inline: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            kind: "openai_compat".into(),
            base_url: "https://x/v1".into(),
            api_key_env: env.map(Into::into),
            api_key: inline.map(Into::into),
            model: "m".into(),
            tools: true,
            reasoning: false,
            vision: false,
            max_context: 1000,
            input_per_1k: 0.0,
            output_per_1k: 0.0,
        }
    }

    #[test]
    fn needs_auth_true_when_either_field_set() {
        assert!(provider_with(Some("E"), None).needs_auth());
        assert!(provider_with(None, Some("k")).needs_auth());
        assert!(provider_with(Some("E"), Some("k")).needs_auth());
        // 本地 Ollama：两者皆无
        assert!(!provider_with(None, None).needs_auth());
    }

    #[test]
    fn inline_key_used_when_env_absent() {
        // 用唯一 env 名避免并行测试互相污染（确保它一定不存在）。
        let pc = provider_with(Some("AGENT_TEST_ABSENT_ENV_XYZ"), Some("sk-inline-123"));
        assert_eq!(pc.resolve_api_key().as_deref(), Some("sk-inline-123"));
    }

    #[test]
    fn env_key_wins_over_inline() {
        let env_name = "AGENT_TEST_PRIORITY_ENV";
        std::env::set_var(env_name, "sk-from-env");
        let pc = provider_with(Some(env_name), Some("sk-inline-should-lose"));
        assert_eq!(pc.resolve_api_key().as_deref(), Some("sk-from-env"));
        std::env::remove_var(env_name);
    }

    #[test]
    fn blank_env_falls_back_to_inline() {
        let env_name = "AGENT_TEST_BLANK_ENV";
        std::env::set_var(env_name, "   ");
        let pc = provider_with(Some(env_name), Some("sk-inline-456"));
        assert_eq!(pc.resolve_api_key().as_deref(), Some("sk-inline-456"));
        std::env::remove_var(env_name);
    }

    #[test]
    fn no_key_anywhere_returns_none() {
        let pc = provider_with(Some("AGENT_TEST_ABSENT_ENV_ABC"), None);
        assert!(pc.resolve_api_key().is_none());
        // 空串明文也视为无 key
        let pc2 = provider_with(None, Some("  "));
        assert!(pc2.resolve_api_key().is_none());
    }

    #[test]
    fn no_auth_provider_resolves_none() {
        let pc = provider_with(None, None);
        assert!(!pc.needs_auth());
        assert!(pc.resolve_api_key().is_none());
    }

    // ---- base_url 常见踩坑警告 ----

    #[test]
    fn warns_when_deepseek_base_url_missing_v1() {
        let mut cfg = ProvidersFile::default();
        cfg.providers.get_mut("deepseek").unwrap().base_url = "https://api.deepseek.com".into();
        let w = cfg.warnings();
        assert_eq!(w.len(), 1, "应恰好一条警告，实际: {w:?}");
        assert!(w[0].contains("deepseek"));
        assert!(w[0].contains("/v1"));
    }

    #[test]
    fn no_warning_when_base_url_correct() {
        let cfg = ProvidersFile::default();
        assert!(cfg.warnings().is_empty(), "默认配置不应有警告");
    }

    #[test]
    fn no_warning_for_unknown_vendor() {
        let mut cfg = ProvidersFile::default();
        cfg.providers.get_mut("deepseek").unwrap().base_url = "https://my-proxy.example.com".into();
        assert!(cfg.warnings().is_empty(), "未知域名不应误报");
    }

    #[test]
    fn warns_for_multiple_vendors() {
        let mut cfg = ProvidersFile::default();
        cfg.providers.get_mut("deepseek").unwrap().base_url = "https://api.deepseek.com".into();
        cfg.providers.get_mut("qwen").unwrap().base_url = "https://dashscope.aliyuncs.com".into();
        assert_eq!(cfg.warnings().len(), 2);
    }
}
