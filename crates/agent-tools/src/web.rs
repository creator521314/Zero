//! 网络工具：web_fetch（带 SSRF 防护）/ web_search。

use agent_core::perm::Risk;
use agent_core::tool::{Artifact, Tool, ToolCtx, ToolError, ToolOutput};
use async_trait::async_trait;
use serde_json::json;

use crate::paths::resolve_in_ctx;

/// 抓取网页正文（简化：取纯文本，去除标签）。
pub struct WebFetch {
    client: reqwest::Client,
}

impl WebFetch {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> &str {
        "抓取一个 http/https 网页并返回其文本内容。可选 save_as 将结果存入工作区文件。"
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string" },
                "save_as": { "type": "string", "description": "可选，相对工作区的保存路径" },
                "max_bytes": { "type": "integer", "description": "最大读取字节数，默认 200000" }
            },
            "required": ["url"]
        })
    }
    fn risk_level(&self) -> Risk {
        Risk::Medium
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'url'".into()))?;
        let max_bytes = args
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(200_000) as usize;

        let parsed = reqwest::Url::parse(url)
            .map_err(|e| ToolError::InvalidArgs(format!("bad url: {e}")))?;
        // SSRF 防护：禁内网/非 http(s)
        if agent_net::is_blocked_host(&parsed) {
            return Err(ToolError::Blocked(format!(
                "url blocked (private/localhost/non-http): {url}"
            )));
        }

        let resp = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            r = self.client.get(parsed).send() => r.map_err(|e| ToolError::Failed(format!("fetch failed: {e}")))?,
        };
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ToolError::Failed(format!("read body: {e}")))?;
        let truncated = &bytes[..bytes.len().min(max_bytes)];
        let text = String::from_utf8_lossy(truncated);
        let plain = strip_html(&text);

        let body = format!(
            "HTTP {status}\nURL: {url}\n\n{}",
            plain.chars().take(8000).collect::<String>()
        );

        let mut out = ToolOutput::ok(body);
        if let Some(save) = args.get("save_as").and_then(|v| v.as_str()) {
            let full = resolve_in_ctx(ctx, save)?;
            if let Some(parent) = full.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&full, plain.as_bytes()).await?;
            out = out.with_artifacts(vec![Artifact {
                path: full.display().to_string(),
                bytes: plain.len() as u64,
            }]);
        }
        Ok(out)
    }
}

/// 简易标签剥离（一期不引入完整 HTML 解析器）。
fn strip_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    let mut in_script = false;
    let lower = input.to_lowercase();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        // 跳过 script/style 内容
        if !in_tag && lower[i..].starts_with("<script") {
            in_script = true;
        }
        if in_script && lower[i..].starts_with("</script") {
            in_script = false;
            in_tag = true;
            i += 7;
            continue;
        }
        if c == '<' {
            in_tag = true;
        } else if c == '>' {
            in_tag = false;
            if !in_script {
                out.push('\n');
            }
        } else if !in_tag && !in_script {
            out.push(c);
        }
        i += 1;
    }
    // 压缩空白行
    out.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 搜索工具（一期为可插拔接口；未配置后端时返回明确提示，不静默失败）。
pub struct WebSearch {
    /// 预留：SearXNG 端点或搜索 API。
    endpoint: Option<String>,
    client: reqwest::Client,
}

impl WebSearch {
    pub fn new(client: reqwest::Client, endpoint: Option<String>) -> Self {
        Self { client, endpoint }
    }
}

#[async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "联网搜索并返回候选结果列表（标题/链接/摘要）。"
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "count": { "type": "integer", "description": "结果条数，默认 8" }
            },
            "required": ["query"]
        })
    }
    fn risk_level(&self) -> Risk {
        Risk::Low
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'query'".into()))?;
        let count = args.get("count").and_then(|v| v.as_u64()).unwrap_or(8);

        let Some(endpoint) = self.endpoint.as_ref() else {
            return Ok(ToolOutput::err(
                "web_search 未配置搜索后端。请在 config.toml 的 [search] 段设置 SearXNG 端点。\n\
                 提示：可直接使用 web_fetch 抓取已知 URL。"
                    .to_string(),
            ));
        };

        let url = format!("{endpoint}?q={}&format=json", urlencoding(query));
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| ToolError::Failed(format!("search failed: {e}")))?;
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ToolError::Failed(format!("bad search response: {e}")))?;

        let mut lines = Vec::new();
        if let Some(results) = json.get("results").and_then(|v| v.as_array()) {
            for r in results.iter().take(count as usize) {
                let title = r.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let link = r.get("url").and_then(|v| v.as_str()).unwrap_or("");
                let snippet = r.get("content").and_then(|v| v.as_str()).unwrap_or("");
                lines.push(format!("- {title}\n  {link}\n  {snippet}"));
            }
        }
        if lines.is_empty() {
            return Ok(ToolOutput::ok(format!("no results for: {query}")));
        }
        Ok(ToolOutput::ok(lines.join("\n\n")))
    }
}

/// 极简百分号编码（避免额外依赖）。
fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tags_and_scripts() {
        let html = "<html><head><script>var x=1;</script></head><body><h1>Hi</h1><p>Text</p></body></html>";
        let t = strip_html(html);
        assert!(t.contains("Hi"));
        assert!(t.contains("Text"));
        assert!(!t.contains("var x"));
    }
}
