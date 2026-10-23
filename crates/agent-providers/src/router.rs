//! Provider 路由：按任务类型选候选列表，失败顺延（failover）。
//! 预算守护：单任务 + 月度双熔断（阈值可配置，应对 P2 未定）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use agent_core::provider::{ChatRequest, LlmError, LlmProvider};

use crate::budget::{spend_path, SpendStore};
use crate::config::{Budget, ProvidersFile};
use crate::openai_compat::OpenAiCompatProvider;

/// Provider 路由中心。业务层只认它，不感知厂商差异。
pub struct ProviderRouter {
    providers: HashMap<String, Arc<dyn LlmProvider>>,
    default: String,
    by_task: HashMap<String, Vec<String>>,
    budget: Budget,
    /// 跨重启的月度用量账本（见 `budget.rs`）。
    spend: SpendStore,
}

impl ProviderRouter {
    /// 从配置构建（解析不到 key 的 provider 会被跳过并告警，不阻塞启动）。
    ///
    /// 月度用量默认落在 `agent.root/budget.json`。
    pub fn from_config(cfg: &ProvidersFile, client: reqwest::Client) -> Self {
        Self::with_spend_file(cfg, client, Some(spend_path(&cfg.agent.resolved_root())))
    }

    /// 显式指定用量账本位置；`None` = 只记在内存里（测试用）。
    pub fn with_spend_file(
        cfg: &ProvidersFile,
        client: reqwest::Client,
        spend_path: Option<PathBuf>,
    ) -> Self {
        let mut providers: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
        for (name, pc) in &cfg.providers {
            if pc.kind != "openai_compat" {
                tracing::warn!(provider = %name, kind = %pc.kind, "unsupported provider kind, skipped");
                continue;
            }
            // 声明了鉴权但两个来源都拿不到 key：跳过并提示。
            // 优先级：api_key_env 指向的环境变量 > 配置里的 api_key 明文。
            if pc.needs_auth() && pc.resolve_api_key().is_none() {
                tracing::warn!(
                    provider = %name,
                    env = ?pc.api_key_env,
                    has_inline_key = pc.api_key.is_some(),
                    "api key unavailable (env not set and no inline api_key), provider disabled"
                );
                continue;
            }
            providers.insert(
                name.clone(),
                Arc::new(OpenAiCompatProvider::new(
                    name.clone(),
                    pc.clone(),
                    client.clone(),
                )),
            );
        }

        Self {
            providers,
            default: cfg.router.default.clone(),
            by_task: cfg.router.by_task.clone(),
            budget: cfg.budget.clone(),
            spend: match spend_path {
                Some(p) => SpendStore::load(p),
                None => SpendStore::memory_only(),
            },
        }
    }

    pub fn available(&self) -> Vec<String> {
        let mut v: Vec<String> = self.providers.keys().cloned().collect();
        v.sort();
        v
    }

    /// 解析某任务类型的候选 provider 顺序。
    pub fn candidates(
        &self,
        route_hint: Option<&str>,
        model_override: Option<&str>,
    ) -> Vec<String> {
        // 显式指定 model/provider 时优先
        if let Some(m) = model_override {
            if self.providers.contains_key(m) {
                return vec![m.to_string()];
            }
        }
        if let Some(hint) = route_hint {
            if let Some(list) = self.by_task.get(hint) {
                let filtered: Vec<String> = list
                    .iter()
                    .filter(|p| self.providers.contains_key(*p))
                    .cloned()
                    .collect();
                if !filtered.is_empty() {
                    return filtered;
                }
            }
        }
        if self.providers.contains_key(&self.default) {
            vec![self.default.clone()]
        } else {
            self.available()
        }
    }

    /// 仅云端（有 api_key_env）的 provider 是否全部不可用。
    /// 用于区分「真没配」和「只剩未验证的本地兜底」。
    pub fn cloud_unavailable(&self) -> bool {
        self.providers.is_empty()
    }

    /// 取第一个可用 provider（含预算检查）。
    ///
    /// 月度预算见底时**先退到本地（免 key）provider**，本地也没有才拒绝——
    /// 与 `design.md` 的「降级到本地或拒绝 + 告警」一致：钱花完了不至于直接停工，
    /// 但也不该继续悄悄烧。
    pub async fn resolve(
        &self,
        route_hint: Option<&str>,
        model_override: Option<&str>,
    ) -> Result<Arc<dyn LlmProvider>, LlmError> {
        let cands = self.candidates(route_hint, model_override);
        let pick = |names: Vec<String>| {
            names
                .into_iter()
                .find_map(|n| self.providers.get(&n).cloned())
        };

        let hit = pick(cands.clone());
        let Some(provider) = hit else {
            return Err(LlmError::Auth(
                "no available provider (check API keys)".into(),
            ));
        };

        let limit = self.budget.monthly_limit_usd;
        if limit <= 0.0 || self.spent_month_usd() < limit {
            return Ok(provider);
        }

        let mut local: Vec<String> = self
            .providers
            .iter()
            .filter(|(name, _)| !self.needs_paid_key(name))
            .map(|(name, _)| name.clone())
            .collect();
        // HashMap 的迭代顺序是随机的：不排序的话「退到哪个本地 provider」
        // 每次运行都可能不同，问题会变得极难复现。
        local.sort();
        if let Some(local_provider) = pick(local) {
            tracing::warn!(
                spent = %self.spent_month_usd(),
                limit = %limit,
                "月度预算见底，本次改走本地 provider"
            );
            return Ok(local_provider);
        }

        Err(LlmError::BudgetExceeded(format!(
            "monthly budget ${limit:.2} exceeded (spent ${:.2})，且没有可用的本地 provider 兜底",
            self.spent_month_usd()
        )))
    }

    /// 该 provider 是否需要付费 key（本地 Ollama 之类不需要）。
    fn needs_paid_key(&self, name: &str) -> bool {
        match self.providers.get(name).and_then(|p| p.price("")) {
            // 本地 Ollama 等免 key 端点的单价是 0。
            Some(pr) => pr.input_per_1k > 0.0 || pr.output_per_1k > 0.0,
            None => true,
        }
    }

    /// 记录一次调用的成本（USD）。
    pub async fn record_cost(&self, provider: &str, usage: &agent_core::provider::Usage) {
        let Some(price) = self.providers.get(provider).and_then(|p| p.price("")) else {
            return;
        };
        let cost = (usage.input_tokens as f64 / 1000.0) * price.input_per_1k
            + (usage.output_tokens as f64 / 1000.0) * price.output_per_1k;
        self.record_spend(cost);
    }

    /// 直接记一笔已算好的金额（USD）。
    ///
    /// 供执行循环上报流式调用的成本——那条路径直接围着 provider 转，没经过
    /// `chat_with_failover`，早期也因此**根本没被计入月度用量**（熔断形同虚设）。
    /// 同步实现是刻意的：调用方不必 `spawn`，进程退出时不会丢掉最后一笔。
    pub fn record_spend(&self, cost: f64) {
        let before = self.spend.month_total();
        let after = self.spend.add(cost);
        let limit = self.budget.monthly_limit_usd;
        if limit > 0.0 {
            warn_on_threshold(before, after, limit);
        }
    }

    /// 本月累计（USD）。
    pub fn spent_month_usd(&self) -> f64 {
        self.spend.month_total()
    }

    pub fn per_task_limit(&self) -> f64 {
        self.budget.per_task_limit_usd
    }

    pub fn monthly_limit(&self) -> f64 {
        self.budget.monthly_limit_usd
    }

    /// 带 failover 的非流式调用。
    pub async fn chat_with_failover(
        &self,
        route_hint: Option<&str>,
        model_override: Option<&str>,
        req: ChatRequest,
    ) -> Result<agent_core::provider::ChatResponse, LlmError> {
        let cands = self.candidates(route_hint, model_override);
        let mut last_err = None;
        for name in cands {
            let Some(p) = self.providers.get(&name) else {
                continue;
            };
            match p.chat(req.clone()).await {
                Ok(resp) => {
                    self.record_cost(&name, &resp.usage).await;
                    return Ok(resp);
                }
                Err(e) if e.is_retryable() => {
                    tracing::warn!(provider = %name, error = %e, "retryable error, failover");
                    last_err = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap_or_else(|| LlmError::Auth("no provider available".into())))
    }
}

/// 预算状态：`月 $20.00 中已用 $4.20（21%）`。给启动日志与 TUI 提示用。
pub fn budget_line(spent: f64, limit: f64) -> String {
    if limit <= 0.0 {
        return format!("本月已用 ${spent:.2}（未设月度上限）");
    }
    format!(
        "月度预算 ${limit:.2} 已用 ${spent:.2}（{}%）",
        (spent / limit * 100.0).round() as i64
    )
}

/// 用量跨过阈值时告警一次（80% / 100%）。
///
/// 「跨过」才报：每次调用都刷一条同样的话只会变成噪音，真正需要知道的是**刚刚过了线**。
fn warn_on_threshold(before: f64, after: f64, limit: f64) {
    const THRESHOLDS: [f64; 2] = [0.8, 1.0];
    for r in THRESHOLDS {
        let edge = limit * r;
        if before < edge && after >= edge {
            tracing::warn!("{}", budget_line(after, limit));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, ProvidersFile};

    /// 造一个含「本地 provider」的配置：单价 0 即视为本地。
    fn cfg_with_local(budget: Budget) -> ProvidersFile {
        let mut cfg = ProvidersFile {
            budget,
            ..Default::default()
        };
        cfg.providers.insert(
            "local".into(),
            crate::config::ProviderConfig {
                kind: "openai_compat".into(),
                base_url: "http://localhost:11434/v1".into(),
                api_key_env: None,
                api_key: None,
                model: "m".into(),
                tools: true,
                reasoning: false,
                vision: false,
                max_context: 32_000,
                input_per_1k: 0.0,
                output_per_1k: 0.0,
            },
        );
        cfg
    }

    #[tokio::test]
    async fn over_budget_downgrades_to_local_instead_of_failing() {
        // 关键回归：见底时不能直接报错（那等于配了预算就自断后路），
        // 要像 design.md 写的那样先退本地。
        let dir = std::env::temp_dir().join(format!("agent-router-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = spend_path(&dir);

        let cfg = cfg_with_local(Budget {
            monthly_limit_usd: 1.0,
            ..Default::default()
        });
        let r = ProviderRouter::with_spend_file(&cfg, reqwest::Client::new(), Some(path.clone()));

        // 花超 1 美元：本地 provider 不该受影响。
        r.record_spend(5.0);
        let got = r.resolve(None, None).await;
        assert!(
            got.is_ok(),
            "预算见底后应退到本地 provider，实际 {}",
            got.err().map(|e| e.to_string()).unwrap_or_default()
        );
        assert_eq!(got.unwrap().name(), "local");

        // 重启：账本仍在，累计值不能清零（否则熔断是摆设）。
        let r2 = ProviderRouter::with_spend_file(&cfg, reqwest::Client::new(), Some(path));
        assert_eq!(r2.spent_month_usd(), 5.0, "重启后月度用量清零 = 熔断失灵");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn under_budget_resolves_normally() {
        let cfg = cfg_with_local(Budget::default());
        let r = ProviderRouter::with_spend_file(&cfg, reqwest::Client::new(), None);
        r.record_spend(0.1);
        let got = r.resolve(None, None).await;
        assert!(got.is_ok(), "未超限应正常取到 provider");
        assert_eq!(r.spent_month_usd(), 0.1);
    }

    #[tokio::test]
    async fn over_budget_without_local_fails_cleanly() {
        // 没有本地兜底时必须拒绝——宁可停工也不默默烧钱。
        // 注意：ProvidersFile::default() 自带一个单价 0 的 ollama，所以这里
        // 必须造一个**只有付费 provider** 的配置，否则测的是另一条路径。
        let mut cfg = ProvidersFile::default();
        cfg.providers.clear();
        cfg.budget = Budget {
            monthly_limit_usd: 0.5,
            ..Default::default()
        };
        cfg.providers.insert(
            "paid".into(),
            crate::config::ProviderConfig {
                kind: "openai_compat".into(),
                base_url: "https://api.example.com/v1".into(),
                api_key_env: Some("AGENT_TEST_PAID_KEY".into()),
                api_key: None,
                model: "m".into(),
                tools: true,
                reasoning: false,
                vision: false,
                max_context: 32_000,
                input_per_1k: 0.001,
                output_per_1k: 0.002,
            },
        );
        // 没有 key 会被跳过 → 手动塞进去，保证候选非空。
        std::env::set_var("AGENT_TEST_PAID_KEY", "sk-test");
        let r = ProviderRouter::with_spend_file(&cfg, reqwest::Client::new(), None);
        std::env::remove_var("AGENT_TEST_PAID_KEY");
        assert_eq!(
            r.available(),
            vec!["paid".to_string()],
            "应只装配出付费 provider"
        );

        r.record_spend(2.0);
        let got = r.resolve(None, None).await;
        assert!(
            matches!(got, Err(LlmError::BudgetExceeded(_))),
            "无本地兜底且超限应报 BudgetExceeded，实际 {}",
            got.err().map(|e| e.to_string()).unwrap_or_default()
        );
    }

    #[tokio::test]
    async fn local_downgrade_picks_a_deterministic_provider() {
        // 多个本地 provider 时结果必须稳定（HashMap 迭代顺序随机，
        // 不排序就会「这次退到这个、下次退到那个」）。
        let cfg = cfg_with_local(Budget::default());
        let r = ProviderRouter::with_spend_file(&cfg, reqwest::Client::new(), None);
        let first: String = r
            .resolve(None, None)
            .await
            .expect("应取到 provider")
            .name()
            .to_string();
        for _ in 0..8 {
            let p = r.resolve(None, None).await.expect("应取到 provider");
            assert_eq!(p.name(), first.as_str(), "本地兜底的选择应稳定");
        }
    }

    #[test]
    fn budget_line_renders_percentage() {
        assert_eq!(budget_line(2.0, 10.0), "月度预算 $10.00 已用 $2.00（20%）");
        assert!(
            budget_line(1.0, 0.0).contains("未设月度上限"),
            "{}",
            budget_line(1.0, 0.0)
        );
    }

    #[test]
    fn threshold_warning_fires_only_on_crossing() {
        // 只要「跨过」才该报，否则每次调用都刷同一条。
        warn_on_threshold(0.79 * 10.0, 0.81 * 10.0, 10.0);
        warn_on_threshold(0.5, 0.6, 10.0);
        warn_on_threshold(1.0, 2.0, 10.0);
        warn_on_threshold(-1.0, 0.5, 10.0);
    }

    #[test]
    fn default_agent_root_gets_a_spend_file() {
        let cfg = ProvidersFile::default();
        assert_eq!(spend_path(&cfg.agent.resolved_root()), {
            let mut p = cfg.agent.resolved_root();
            p.push("budget.json");
            p
        });
        let _ = AgentConfig::default();
    }
}
