//! Provider 路由：按任务类型选候选列表，失败顺延（failover）。
//! 预算守护：单任务 + 月度双熔断（阈值可配置，应对 P2 未定）。

use std::collections::HashMap;
use std::sync::Arc;

use agent_core::provider::{ChatRequest, LlmError, LlmProvider};
use tokio::sync::Mutex;

use crate::config::{Budget, ProvidersFile};
use crate::openai_compat::OpenAiCompatProvider;

/// Provider 路由中心。业务层只认它，不感知厂商差异。
pub struct ProviderRouter {
    providers: HashMap<String, Arc<dyn LlmProvider>>,
    default: String,
    by_task: HashMap<String, Vec<String>>,
    budget: Budget,
    /// 累计月度成本（USD），可持久化。
    spent_month_usd: Arc<Mutex<f64>>,
}

impl ProviderRouter {
    /// 从配置构建（未配置 key 的 provider 会被跳过并告警，不阻塞启动）。
    pub fn from_config(cfg: &ProvidersFile, client: reqwest::Client) -> Self {
        let mut providers: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
        for (name, pc) in &cfg.providers {
            if pc.kind != "openai_compat" {
                tracing::warn!(provider = %name, kind = %pc.kind, "unsupported provider kind, skipped");
                continue;
            }
            // 有 api_key_env 但环境变量未设置的云端 provider：跳过并提示
            if let Some(env) = &pc.api_key_env {
                if std::env::var(env).is_err() {
                    tracing::warn!(provider = %name, env = %env, "api key env not set, provider disabled");
                    continue;
                }
            }
            providers.insert(
                name.clone(),
                Arc::new(OpenAiCompatProvider::new(name.clone(), pc.clone(), client.clone())),
            );
        }

        Self {
            providers,
            default: cfg.router.default.clone(),
            by_task: cfg.router.by_task.clone(),
            budget: cfg.budget.clone(),
            spent_month_usd: Arc::new(Mutex::new(0.0)),
        }
    }

    pub fn available(&self) -> Vec<String> {
        let mut v: Vec<String> = self.providers.keys().cloned().collect();
        v.sort();
        v
    }

    /// 解析某任务类型的候选 provider 顺序。
    pub fn candidates(&self, route_hint: Option<&str>, model_override: Option<&str>) -> Vec<String> {
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
    pub async fn resolve(
        &self,
        route_hint: Option<&str>,
        model_override: Option<&str>,
    ) -> Result<Arc<dyn LlmProvider>, LlmError> {
        let spent = *self.spent_month_usd.lock().await;
        if spent >= self.budget.monthly_limit_usd {
            return Err(LlmError::BudgetExceeded(format!(
                "monthly budget ${:.2} exceeded (spent ${:.2})",
                self.budget.monthly_limit_usd, spent
            )));
        }
        let cands = self.candidates(route_hint, model_override);
        cands
            .into_iter()
            .find_map(|n| self.providers.get(&n).cloned())
            .ok_or_else(|| LlmError::Auth("no available provider (check API keys)".into()))
    }

    /// 记录一次调用的成本。
    pub async fn record_cost(&self, provider: &str, usage: &agent_core::provider::Usage) {
        let price = self
            .providers
            .get(provider)
            .and_then(|p| p.price(""));
        if let Some(price) = price {
            let cost = (usage.input_tokens as f64 / 1000.0) * price.input_per_1k
                + (usage.output_tokens as f64 / 1000.0) * price.output_per_1k;
            *self.spent_month_usd.lock().await += cost;
        }
    }

    pub async fn spent_month_usd(&self) -> f64 {
        *self.spent_month_usd.lock().await
    }

    pub fn per_task_limit(&self) -> f64 {
        self.budget.per_task_limit_usd
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
            let Some(p) = self.providers.get(&name) else { continue };
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
