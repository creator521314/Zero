//! agent-net —— reqwest 出站封装，工具与 provider 共用。
//!
//! 统一提供：连接池、超时、TLS（rustls）、User-Agent、SSRF 防护。

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
pub fn build_client(cfg: &HttpClientConfig) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(cfg.timeout)
        .connect_timeout(cfg.connect_timeout)
        .user_agent(&cfg.user_agent)
        .pool_max_idle_per_host(8)
        .build()
}

/// 判断 URL 是否为内网/危险地址（SSRF 防护，供 web_fetch 使用）。
pub fn is_blocked_host(url: &reqwest::Url) -> bool {
    if url.scheme() != "http" && url.scheme() != "https" {
        return true;
    }
    match url.host_str() {
        None => true,
        Some(host) => {
            let h = host.to_ascii_lowercase();
            if h == "localhost" || h == "metadata.google.internal" {
                return true;
            }
            if let Ok(ip) = h.parse::<std::net::IpAddr>() {
                return is_private_ip(ip);
            }
            false
        }
    }
}

fn is_private_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                // 100.64.0.0/10 CGNAT
                || (o[0] == 100 && (64..128).contains(&o[1]))
        }
        std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
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
    fn allows_public() {
        let u = reqwest::Url::parse("https://example.com/news").unwrap();
        assert!(!is_blocked_host(&u));
    }
}
