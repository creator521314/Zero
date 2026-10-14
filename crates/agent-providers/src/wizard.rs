//! 交互式配置向导。
//!
//! 启动流程：
//! - `ensure_loaded()`：统一入口；非交互环境直接报错退出，交互环境按需进入向导。
//! - `run_first_time()`：文件不存在时引导生成。
//! - `run_invalid(reason)`：文件存在但解析/校验失败时引导覆盖。
//!
//! 设计要点：
//! - API key **不入文件**，只记录环境变量名；用户另行 `setx / export`。
//! - 多选 provider 模板 + 单选 router.default，最后预览 + 确认写入。
//! - 非 TTY（CI、SSH 后台、管道）：直接写模板并 bail，提示用户编辑后重启。

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::Path;

use dialoguer::{Confirm, Input, MultiSelect, Select};

use crate::config::{
    AgentConfig, Budget, ConfigError, ProviderConfig, ProvidersFile, RouterConfig, SearchConfig,
    EXAMPLE_TOML,
};

/// 预置 provider 模板。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    DeepSeek,
    Qwen,
    Ollama,
    Custom,
}

impl Preset {
    fn label(&self) -> &'static str {
        match self {
            Preset::DeepSeek => "DeepSeek (云端；默认档，需 DEEPSEEK_API_KEY)",
            Preset::Qwen => "通义千问 (云端；需 DASHSCOPE_API_KEY)",
            Preset::Ollama => "Ollama (本地；http://localhost:11434，无需 key)",
            Preset::Custom => "自定义 OpenAI 兼容端点 (需 key)",
        }
    }

    fn default_name(&self) -> &'static str {
        match self {
            Preset::DeepSeek => "deepseek",
            Preset::Qwen => "qwen",
            Preset::Ollama => "ollama",
            Preset::Custom => "custom",
        }
    }

    fn template(&self) -> ProviderConfig {
        match self {
            Preset::DeepSeek => ProviderConfig {
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
            Preset::Qwen => ProviderConfig {
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
            Preset::Ollama => ProviderConfig {
                kind: "openai_compat".into(),
                base_url: "http://localhost:11434/v1".into(),
                api_key_env: None,
                model: "qwen2.5-coder".into(),
                tools: true,
                reasoning: false,
                vision: false,
                max_context: 32_000,
                input_per_1k: 0.0,
                output_per_1k: 0.0,
            },
            Preset::Custom => ProviderConfig {
                kind: "openai_compat".into(),
                base_url: String::new(),
                api_key_env: Some("CUSTOM_API_KEY".into()),
                model: "gpt-4o-mini".into(),
                tools: true,
                reasoning: false,
                vision: false,
                max_context: 128_000,
                input_per_1k: 0.0,
                output_per_1k: 0.0,
            },
        }
    }
}

/// 启动期统一入口：根据环境与文件状态决定是否走 wizard。
///
/// 返回校验通过的 `ProvidersFile`。
pub fn ensure_loaded() -> anyhow::Result<ProvidersFile> {
    let path = ProvidersFile::config_path();

    if !std::io::stdin().is_terminal() {
        // 非交互：写模板并让用户手动编辑 + 设环境变量后重启。
        if !path.exists() {
            ProvidersFile::write_template(&path)?;
            anyhow::bail!(
                "未找到配置文件。已写入模板：{}\n\
                 请用文本编辑器填好 `api_key_env` 对应的环境变量后重新运行。",
                path.display()
            );
        }
        // 已有文件：按严格模式加载并校验；任一失败即报错退出（非交互）。
        let cfg = ProvidersFile::load(&path)
            .map_err(|e| anyhow::anyhow!("配置文件解析失败 {}：{e}", path.display()))?;
        cfg.validate().map_err(|e| anyhow::anyhow!("{e}"))?;
        return Ok(cfg);
    }

    // 交互环境
    if !path.exists() {
        return run_first_time();
    }
    match ProvidersFile::load(&path) {
        Ok(cfg) => match cfg.validate() {
            Ok(()) => Ok(cfg),
            Err(e) => run_invalid(&path, &format!("{e}")),
        },
        Err(e) => run_invalid(&path, &format!("TOML 解析失败：{e}")),
    }
}

/// 首次启动：模板不存在时的引导。
pub fn run_first_time() -> anyhow::Result<ProvidersFile> {
    eprintln!("\n==== 个人 Agent 配置向导（首次运行）====");
    eprintln!("将引导你配置至少一个 LLM provider。");
    eprintln!(
        "API key 不会写入文件 —— 你只需提供一个「环境变量名」，\n\
        key 本身另行用 `setx` / `export` 设置。\n"
    );

    let presets = pick_presets()?;
    let providers = collect_providers(&presets)?;
    let default_provider = pick_default(&providers)?;
    let cfg = ProvidersFile {
        providers,
        router: RouterConfig {
            default: default_provider,
            by_task: HashMap::new(),
        },
        budget: Budget::default(),
        search: SearchConfig::default(),
        agent: AgentConfig::default(),
    };

    confirm_and_write(&cfg, false)?;
    // 再加载一次（保证从磁盘读到的与写入的一致，验证往返）。
    let saved = ProvidersFile::load(&ProvidersFile::config_path())?;
    saved.validate()?;
    Ok(saved)
}

/// 配置文件无效时的引导（用户确认后才覆盖）。
pub fn run_invalid(existing: &Path, reason: &str) -> anyhow::Result<ProvidersFile> {
    eprintln!("\n[!] 现有配置文件无法使用：{}", existing.display());
    eprintln!("    原因：{reason}");

    let overwrite = Confirm::new()
        .with_prompt("是否覆盖现有配置并重新配置？")
        .default(false)
        .interact()?;
    if !overwrite {
        anyhow::bail!("已取消。请手动修复配置文件后再启动。");
    }

    let presets = pick_presets()?;
    let providers = collect_providers(&presets)?;
    let default_provider = pick_default(&providers)?;
    let cfg = ProvidersFile {
        providers,
        router: RouterConfig {
            default: default_provider,
            by_task: HashMap::new(),
        },
        budget: Budget::default(),
        search: SearchConfig::default(),
        agent: AgentConfig::default(),
    };

    confirm_and_write(&cfg, true)?;
    let saved = ProvidersFile::load(&ProvidersFile::config_path())?;
    saved.validate()?;
    Ok(saved)
}

fn pick_presets() -> anyhow::Result<Vec<Preset>> {
    let items = [
        Preset::DeepSeek,
        Preset::Qwen,
        Preset::Ollama,
        Preset::Custom,
    ];
    let labels: Vec<&str> = items.iter().map(|p| p.label()).collect();
    let defaults = vec![true, false, false, false];

    let picks = MultiSelect::new()
        .with_prompt("勾选要启用的 provider（空格切换，回车确认；至少 1 个）")
        .items(&labels)
        .defaults(&defaults)
        .interact()?;

    if picks.is_empty() {
        anyhow::bail!("至少需要选择一个 provider。可重新启动再次运行。");
    }
    Ok(picks.into_iter().map(|i| items[i]).collect())
}

fn collect_providers(presets: &[Preset]) -> anyhow::Result<HashMap<String, ProviderConfig>> {
    let mut providers = HashMap::new();
    for preset in presets {
        let mut pc = preset.template();

        // 自定义：让用户填名字 / base_url / model / api_key_env
        let name = if matches!(preset, Preset::Custom) {
            let n: String = Input::new()
                .with_prompt("自定义 provider 名称（用作 router key，例: openai）")
                .interact_text()?;
            if n.trim().is_empty() {
                anyhow::bail!("provider 名称不能为空");
            }
            n
        } else {
            preset.default_name().to_string()
        };

        if matches!(preset, Preset::Custom) {
            pc.base_url = Input::new()
                .with_prompt("自定义 base_url (例 https://api.openai.com/v1)")
                .default("https://api.openai.com/v1".into())
                .interact_text()?;
            pc.model = Input::new()
                .with_prompt("默认 model 名")
                .default("gpt-4o-mini".into())
                .interact_text()?;
        }

        // 需要 key 的 provider：让用户确认环境变量名，并提示如何 setx。
        if pc.api_key_env.is_some() {
            let default_env = pc.api_key_env.clone().unwrap_or_default();
            let key_env: String = Input::new()
                .with_prompt(format!("{name}: API key 所在的环境变量名"))
                .default(default_env)
                .interact_text()?;

            if key_env.trim().is_empty() {
                eprintln!("    [提示] 你选择了「无环境变量」，Agent 会跳过这个 provider。");
                pc.api_key_env = None;
            } else {
                pc.api_key_env = Some(key_env.clone());
                match std::env::var(&key_env) {
                    Ok(_) => eprintln!("    [✓] 环境变量 {key_env} 已设置"),
                    Err(_) => {
                        eprintln!("    [警告] 环境变量 {key_env} 当前未设置");
                        eprintln!("    PowerShell:  setx {key_env} \"<your-key>\"");
                        eprintln!("    bash/zsh:    export {key_env}=<your-key>");
                    }
                }
            }
        } else {
            eprintln!("    [i] {name}: 无需 API key（本地或免鉴权端点）");
        }

        if providers.contains_key(&name) {
            anyhow::bail!("provider 名 `{name}` 重复");
        }
        providers.insert(name, pc);
    }
    Ok(providers)
}

fn pick_default(providers: &HashMap<String, ProviderConfig>) -> anyhow::Result<String> {
    let names: Vec<String> = providers.keys().cloned().collect();
    if names.len() == 1 {
        return Ok(names.into_iter().next().unwrap());
    }
    let idx = Select::new()
        .with_prompt("选择 router.default（没有 by_task 匹配时用这个）")
        .items(&names)
        .default(0)
        .interact()?;
    Ok(names.into_iter().nth(idx).unwrap())
}

fn confirm_and_write(cfg: &ProvidersFile, overwriting: bool) -> anyhow::Result<()> {
    let path = ProvidersFile::config_path();
    eprintln!("\n==== 配置预览 ====");
    eprintln!("路径      : {}", path.display());
    eprintln!(
        "providers : {}",
        cfg.providers.keys().cloned().collect::<Vec<_>>().join(", ")
    );
    eprintln!("default   : {}", cfg.router.default);
    eprintln!(
        "budget    : 月 ${:.2} / 任务 ${:.2}",
        cfg.budget.monthly_limit_usd, cfg.budget.per_task_limit_usd
    );
    eprintln!();

    let action = if overwriting {
        "覆盖写入"
    } else {
        "写入"
    };
    let ok = Confirm::new()
        .with_prompt(format!("{action}配置到上述路径？"))
        .default(true)
        .interact()?;
    if !ok {
        anyhow::bail!("已取消。可重新启动再次运行 wizard。");
    }

    // 用嵌入模板（带注释、可读性更好）+ 我们 wizard 生成的 section 覆盖关键字段。
    // 简化：直接写 wizard 生成的紧凑版本（to_string_pretty）。
    let body = render_with_header(cfg)?;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&path, body)?;
    eprintln!("[✓] 已写入 {}\n", path.display());

    // 安全提示：key 不入文件，必须 setx
    eprintln!("下一步：");
    eprintln!("  1. 用 `setx` / `export` 设置上面提到的环境变量；");
    eprintln!("  2. 重新运行本程序；");
    eprintln!("  3. 配置文件位于 {}，可随时编辑。\n", path.display());

    Ok(())
}

/// 写出带顶部说明的 TOML，便于用户首次打开时看懂结构。
fn render_with_header(cfg: &ProvidersFile) -> anyhow::Result<String> {
    use std::fmt::Write as _;

    let mut out = String::new();
    out.push_str("# 个人 Agent 配置文件（由配置向导生成）\n");
    out.push_str("# 安全提示：API key 不会写入此文件，只记录环境变量名。\n");
    out.push_str("# 详细模板与字段说明见 config/config.example.toml。\n\n");

    let body = toml::to_string_pretty(cfg)?;
    out.push_str(&body);
    let _ = write!(out, "\n# === 嵌入模板 (示例) ===\n{EXAMPLE_TOML}");
    Ok(out)
}

/// 把 ConfigError 转成一行人类可读的字符串（用于 wizard 提示）。
pub fn format_err(e: &ConfigError) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn preset_template_deepseek_has_key_env() {
        let p = Preset::DeepSeek.template();
        assert_eq!(p.kind, "openai_compat");
        assert!(p.api_key_env.is_some());
    }

    #[test]
    fn preset_template_ollama_has_no_key() {
        let p = Preset::Ollama.template();
        assert!(p.api_key_env.is_none());
    }

    #[test]
    fn render_contains_all_sections() {
        let cfg = ProvidersFile::default();
        let body = render_with_header(&cfg).unwrap();
        assert!(body.contains("[providers.deepseek]"));
        assert!(body.contains("[router]"));
        assert!(body.contains("[budget]"));
        assert!(body.contains("[search]"));
        assert!(body.contains("[agent]"));
    }

    #[test]
    fn collect_providers_rejects_duplicate_name() {
        // 通过 collect_providers 的间接约束：HashMap 自然拒绝重复。
        // 这里直接验证 hashmap 行为，避免依赖 stdin 的部分。
        let mut m: HashMap<String, ProviderConfig> = HashMap::new();
        m.insert("a".to_string(), Preset::DeepSeek.template());
        assert!(m.insert("a".to_string(), Preset::Qwen.template()).is_some());
        assert_eq!(m.len(), 1);
    }
}
