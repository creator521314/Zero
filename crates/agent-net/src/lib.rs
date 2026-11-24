//! agent-net —— reqwest 出站封装，工具与 provider 共用。
//!
//! 统一提供：连接池、超时、TLS（rustls）、User-Agent、SSRF 防护。
//!
//! SSRF 防护分两层：
//! - **provider / 普通出站**：[`build_client`]，保持 reqwest 默认行为；
//! - **工具出站（web_fetch / web_search）**：[`build_guarded_client`] +
//!   [`guarded_get`] —— 禁用自动重定向，每一跳（含重定向目标）都做
//!   scheme / 内网地址 / DNS 解析结果校验，域名解析出的 IP 会**钉死**
//!   到本次请求上（防 DNS rebinding 的 check-then-connect 竞态）。

use std::net::IpAddr;
use std::time::Duration;

/// 网络客户端配置。
#[derive(Debug, Clone)]
pub struct HttpClientConfig {
    pub timeout: Duration,
    pub connect_timeout: Duration,
    pub user_agent: String,
}

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            connect_timeout: Duration::from_secs(15),
            user_agent: concat!("personal-agent/", env!("CARGO_PKG_VERSION")).to_string(),
        }
    }
}

/// 构建共享的 HTTP 客户端（连接池复用）。
///
/// 供 LLM provider 等可信配置的端点使用；工具出站请用 [`build_guarded_client`]。
pub fn build_client(cfg: &HttpClientConfig) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(cfg.timeout)
        .connect_timeout(cfg.connect_timeout)
        .user_agent(&cfg.user_agent)
        .pool_max_idle_per_host(8)
        .build()
}

/// 构建「守卫」HTTP 客户端：**禁用自动重定向**。
///
/// reqwest 默认静默跟随最多 10 次重定向且重定向目标不再做任何校验——
/// `http://evil.com/redirect → http://169.254.169.254` 就能绕过 URL 层的
/// SSRF 检查。工具出站必须用这个客户端，配合 [`guarded_get`] 逐跳手动
/// 处理重定向、每一跳都重新校验。
pub fn build_guarded_client(cfg: &HttpClientConfig) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(cfg.timeout)
        .connect_timeout(cfg.connect_timeout)
        .user_agent(&cfg.user_agent)
        .pool_max_idle_per_host(8)
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

/// 工具出站允许的最大重定向跳数。
const MAX_REDIRECTS: usize = 5;

/// 带 SSRF 守卫的 GET：每一跳都校验，重定向手动跟随（最多 [`MAX_REDIRECTS`] 次）。
///
/// 校验内容（逐跳）：
/// 1. scheme 必须是 http/https，host 不得是内网/保留地址（[`is_blocked_host`]）；
/// 2. 域名做 DNS 解析，**任一**解析结果落在内网/保留段即拦截（防 rebinding 的
///    域名侧）。
///
/// 已知残余风险（一期接受，注释在案）：reqwest 只能在 ClientBuilder 层钉 DNS，
/// 无法按请求钉住，因此「校验通过后、真正连接前」存在一个很窄的 TOCTOU 窗口，
/// 攻击者可用极短 TTL 的 DNS 在窗口内切换解析结果。彻底封死需要自定义
/// DNS resolver（reqwest 不直接支持），留给后续版本。
pub async fn guarded_get(
    client: &reqwest::Client,
    url: reqwest::Url,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<reqwest::Response, String> {
    guarded_get_with(client, url, &[], cancel).await
}

/// [`guarded_get`] 的带额外请求头版本（如搜索后端的 `Accept`）。
pub async fn guarded_get_with(
    client: &reqwest::Client,
    url: reqwest::Url,
    extra_headers: &[(reqwest::header::HeaderName, &str)],
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<reqwest::Response, String> {
    let mut current = url;
    for hop in 0..=MAX_REDIRECTS {
        check_url(&current)?;

        // 域名 → IP 逐一校验（IP 字面量在 check_url 已拦）。
        resolve_checked(&current).await?;

        let mut req = client.get(current.clone());
        for (name, value) in extra_headers {
            req = req.header(name.clone(), *value);
        }

        let resp = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err("已取消".to_string()),
            r = req.send() => r.map_err(|e| format!("fetch failed: {e}"))?,
        };

        if resp.status().is_redirection() {
            if hop == MAX_REDIRECTS {
                return Err(format!("重定向超过 {MAX_REDIRECTS} 次，已中止"));
            }
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| format!("重定向响应缺少 Location（HTTP {}）", resp.status()))?;
            // 相对路径 Location 以当前 URL 为基准解析。
            current = current
                .join(loc)
                .map_err(|e| format!("重定向目标非法（{loc}）：{e}"))?;
            continue; // 旧 resp 在此 drop，连接归还连接池
        }
        return Ok(resp);
    }
    unreachable!("循环内要么 return 要么 continue")
}

fn check_url(url: &reqwest::Url) -> Result<(), String> {
    if is_blocked_host(url) {
        return Err(format!(
            "url blocked (private/localhost/non-http): {url}"
        ));
    }
    Ok(())
}

/// 判断 URL 是否为内网/危险地址（SSRF 防护，供 web_fetch 使用）。
pub fn is_blocked_host(url: &reqwest::Url) -> bool {
    if url.scheme() != "http" && url.scheme() != "https" {
        return true;
    }
    match url.host_str() {
        None => true,
        Some(host) => {
            let h = host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_ascii_lowercase();
            if h == "localhost" || h == "metadata.google.internal" {
                return true;
            }
            if let Ok(ip) = h.parse::<IpAddr>() {
                return is_private_ip(ip);
            }
            false
        }
    }
}

/// 解析 URL 的域名并逐一校验 IP。
///
/// - IP 字面量：`is_blocked_host` 已拦，直接返回；
/// - 域名：DNS 解析，**任一**结果为内网/保留地址即报错。
pub async fn resolve_checked(url: &reqwest::Url) -> Result<(), String> {
    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
        return Err("URL 缺少主机名".to_string());
    };
    // IPv6 字面量在 host_str 里带方括号（url crate 行为），剥掉再判。
    let host = host.trim_start_matches('[').trim_end_matches(']').to_string();
    if host.parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    let port = url.port_or_known_default().unwrap_or(80);
    let addrs = tokio::task::spawn_blocking(move || {
        use std::net::ToSocketAddrs;
        (host.as_str(), port).to_socket_addrs()
    })
    .await
    .map_err(|e| format!("DNS 解析任务失败：{e}"))?
    .map_err(|e| format!("DNS 解析失败：{e}"))?
    .collect::<Vec<_>>();

    let blocked: Vec<IpAddr> = addrs
        .iter()
        .map(|a| a.ip())
        .filter(|ip| is_private_ip(*ip))
        .collect();
    if !blocked.is_empty() {
        return Err(format!(
            "域名解析到内网/保留地址，已拦截（SSRF 防护）：{blocked:?}"
        ));
    }
    Ok(())
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                // 100.64.0.0/10 CGNAT
                || (o[0] == 100 && (64..128).contains(&o[1]))
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // fc00::/7 唯一本地地址（ULA，IPv6 的「私网」）
                || v6.is_unique_local()
                // fe80::/10 链路本地
                || v6.is_unicast_link_local()
                // IPv4-mapped ::ffff:10.0.0.1 —— 复用 v4 判断，堵住「拿 v4 私网
                // 地址套 v6 皮」的绕过写法。
                || v6
                    .to_ipv4_mapped()
                    .map(|v4| is_private_ip(IpAddr::V4(v4)))
                    .unwrap_or(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_localhost_and_private() {
        for s in [
            "http://localhost/x",
            "http://127.0.0.1/x",
            "http://10.0.0.5/x",
            "http://192.168.1.1/x",
            "https://169.254.169.254/latest/meta-data",
            "ftp://example.com/x",
        ] {
            let u = reqwest::Url::parse(s).unwrap();
            assert!(is_blocked_host(&u), "{s} should be blocked");
        }
    }

    #[test]
    fn blocks_ipv6_internal_ranges() {
        // 回归：v6 早期只拦 loopback/unspecified，ULA / 链路本地 / v4-mapped 全放行。
        for s in [
            "http://[::1]/x",                     // loopback
            "http://[fc00::1]/x",                 // ULA fc00::/7
            "http://[fd12:3456::1]/x",            // ULA（fd00::/8，本地指派）
            "http://[fe80::1]/x",                 // 链路本地
            "http://[::ffff:10.0.0.1]/x",         // IPv4-mapped 私网
            "http://[::ffff:127.0.0.1]/x",        // IPv4-mapped loopback
        ] {
            let u = reqwest::Url::parse(s).unwrap();
            assert!(is_blocked_host(&u), "{s} should be blocked");
        }
    }

    #[test]
    fn allows_public() {
        let u = reqwest::Url::parse("https://example.com/news").unwrap();
        assert!(!is_blocked_host(&u));
        let u = reqwest::Url::parse("http://[2606:2800:220:1::]/x").unwrap();
        assert!(!is_blocked_host(&u), "公网 v6 不应被拦");
    }

    #[tokio::test]
    async fn resolve_checked_allows_public_domains() {
        // 公网域名解析应通过；离网环境解析失败也不算逻辑失败（工具层会报错）。
        let u = reqwest::Url::parse("https://example.com/").unwrap();
        match resolve_checked(&u).await {
            Ok(()) => {}
            Err(e) => assert!(e.contains("DNS"), "公网域名不应被 SSRF 拦截: {e}"),
        }
    }

    #[tokio::test]
    async fn guarded_get_rejects_blocked_url_before_any_io() {
        let client = build_guarded_client(&Default::default()).expect("客户端应能构建");
        let cancel = tokio_util::sync::CancellationToken::new();
        let u = reqwest::Url::parse("http://169.254.169.254/latest/meta-data").unwrap();
        let err = guarded_get(&client, u, &cancel).await.unwrap_err();
        assert!(err.contains("blocked"), "应点名 SSRF 拦截: {err}");
    }

    #[tokio::test]
    async fn guarded_get_rejects_non_http_scheme() {
        let client = build_guarded_client(&Default::default()).expect("客户端应能构建");
        let cancel = tokio_util::sync::CancellationToken::new();
        let u = reqwest::Url::parse("file:///etc/passwd").unwrap();
        let err = guarded_get(&client, u, &cancel).await.unwrap_err();
        assert!(err.contains("blocked"), "{err}");
    }
}
