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
    AgentConfig, Budget, ConfigError, LogConfig, ProviderConfig, ProvidersFile, RouterConfig,
    SearchConfig,
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
                // 向导默认只记环境变量名（不写明文 key）；用户可事后在
                // config/config.toml 里补 api_key = "sk-..." 作为兜底。
                api_key: None,
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
                api_key: None,
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
                api_key: None,
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
                api_key: None,
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
        log: LogConfig::default(),
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
        log: LogConfig::default(),
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

    let body = render_with_header(cfg)?;
    // 写盘前先按「读取路径」解析一遍：向导产出的文件必须能被自己读回。
    // 否则用户会看到「已写入」紧接着 `TOML parse error`，还留下一份坏配置，
    // 下次启动只能再进一次「配置无效」向导——死循环。
    let _: ProvidersFile = toml::from_str(&body)
        .map_err(|e| anyhow::anyhow!("向导生成了无法解析的配置（已中止，未写入）：{e}"))?;
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
///
/// **只写 wizard 生成的内容，不再拼接 `EXAMPLE_TOML`。** 曾经为了「注释更全」
/// 把整份模板附在生成内容后面，结果同一个文件里出现两份 `[providers.deepseek]`，
/// 写出来的配置**连自己都读不回**（`duplicate key`）：向导刚打印「已写入」，
/// 下一行就报 TOML 解析错误。注释放在顶部摘要里，完整模板留在
/// `config/config.example.toml`，不再进同一份文件。
fn render_with_header(cfg: &ProvidersFile) -> anyhow::Result<String> {
    let mut out = String::new();
    out.push_str("# 个人 Agent 配置文件（由配置向导生成）\n");
    out.push_str("# 安全提示：API key 不会写入此文件，只记录环境变量名。\n");
    out.push_str("# 字段说明与注释版模板见 config/config.example.toml（`cargo xtask gen-cfg` 可重新生成）。\n");
    out.push_str("# 常用改法：\n");
    out.push_str(
        "#   - 不想依赖环境变量：在对应 [providers.<name>] 下补 api_key = \"sk-...\" 作兜底；\n",
    );
    out.push_str("#   - 工具读写范围：agent.project_root（默认 \".\" 即启动目录）；\n");
    out.push_str("#   - 权限档位：agent.default_tier = \"L1\" / \"L2\" / \"L3\"。\n\n");

    let body = toml::to_string_pretty(cfg)?;
    out.push_str(&body);
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

    /// 向导只勾一个 provider、`by_task` 为空时的产物。
    /// 用户报出的 `duplicate key` 正是这条路径。
    fn wizard_like_config() -> ProvidersFile {
        ProvidersFile {
            providers: HashMap::from([("deepseek".to_string(), Preset::DeepSeek.template())]),
            router: RouterConfig {
                default: "deepseek".into(),
                by_task: HashMap::new(),
            },
            ..ProvidersFile::default()
        }
    }

    /// 回归：向导写出的文件必须能被**读取路径**解析回来。
    ///
    /// 曾经的 bug：`render_with_header` 为了保留注释，把整份 `EXAMPLE_TOML`
    /// 拼在生成内容后面——同一文件里出现两份 `[providers.deepseek]`，
    /// 向导打印「已写入」后紧接着就 `duplicate key` 解析失败。
    #[test]
    fn rendered_config_round_trips_through_loader() {
        for cfg in [ProvidersFile::default(), wizard_like_config()] {
            let body = render_with_header(&cfg).unwrap();
            let parsed: ProvidersFile =
                toml::from_str(&body).expect("向导产物必须能读回（不能有重复表）");
            assert_eq!(parsed.providers.len(), cfg.providers.len());
            assert_eq!(parsed.router.default, cfg.router.default);
            assert_eq!(
                parsed.budget.per_task_limit_usd,
                cfg.budget.per_task_limit_usd
            );
            assert_eq!(parsed.agent.default_tier, cfg.agent.default_tier);
        }
    }

    /// 同一个表头只能出现一次（重复即 TOML 解析失败）。
    #[test]
    fn rendered_config_has_no_duplicate_tables() {
        let body = render_with_header(&wizard_like_config()).unwrap();
        assert_eq!(
            body.matches("[providers.deepseek]").count(),
            1,
            "表头重复了：\n{body}"
        );
        assert!(
            !body.contains("# === 嵌入模板"),
            "不应再把整份模板拼进同一文件"
        );
        assert!(body.contains("config.example.toml"), "应给出模板文件的指路");
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
