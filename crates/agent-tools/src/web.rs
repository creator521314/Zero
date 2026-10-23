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
///
/// 按 **char** 迭代而非字节：网页正文大量含中文、emoji 等多字节字符，
/// 按字节推进 `i` 再对 `&str` 做 `[i..]` 切片会切在字符中间 panic
/// （`start byte index ... is not a char boundary`）；把字节当 Latin-1
/// 输出还会让非 ASCII 文本变成乱码。
fn strip_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    let mut in_script = false;
    let mut i = 0;

    while i < input.len() {
        // `i` 始终是 char 边界：推进只走 `len_utf8()` 与 ASCII 标签长度。
        let rest = &input[i..];

        // 标签名是 ASCII，用字节前缀做大小写不敏感匹配即可；
        // 剩余不足或开头是多字节字符时自然不匹配。
        if !in_tag && starts_with_ignore_ascii_case(rest, b"<script") {
            in_script = true;
        }
        if in_script && starts_with_ignore_ascii_case(rest, b"</script") {
            in_script = false;
            in_tag = true;
            i += b"</script".len();
            continue;
        }

        let c = rest.chars().next().expect("i < input.len() 保证有字符");
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
        i += c.len_utf8();
    }

    // 压缩空白行
    out.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// ASCII 大小写不敏感的前缀匹配；长度不足或不匹配返回 false。
fn starts_with_ignore_ascii_case(haystack: &str, needle: &[u8]) -> bool {
    let h = haystack.as_bytes();
    h.len() >= needle.len() && h[..needle.len()].eq_ignore_ascii_case(needle)
}

// ---------------------------------------------------------------------------
// web_search
// ---------------------------------------------------------------------------

/// 一条搜索结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// 内置搜索端点的上限条数（`count` 会被夹到这里，避免把整页塞回上下文）。
const MAX_HITS: usize = 10;
/// 内置免 key 后端：Bing 的 HTML 结果页。
const BING_ENDPOINT: &str = "https://www.bing.com/search";

/// 搜索后端。
///
/// 一期两个实现：
/// - [`SearchBackend::Searxng`]：用户自建实例（配置 `[search] searxng_endpoint`），走 JSON API；
/// - [`SearchBackend::Bing`]：**内置免 key 后端，无需任何配置即可用**。
///
/// 之所以必须有内置后端：早期版本把 SearXNG 当成唯一后端，而默认配置与配置
/// 向导都不会写 `[search]` 段，于是 `web_search` 每次调用都返回「未配置搜索
/// 后端」——工具成了纯摆设。内置后端让「搜索」这件事开箱可用，自建实例退化为
/// 可选的隐私增强。
///
/// 列表顺序即优先级：前一个后端失败（HTTP 错误 / 响应不可解析 / 零结果）
/// 就顺延下一个。
#[derive(Debug, Clone)]
pub enum SearchBackend {
    /// SearXNG JSON API，endpoint 形如 `http://localhost:8080/search`。
    Searxng(String),
    /// Bing HTML 结果页。
    Bing,
}

impl SearchBackend {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Searxng(_) => "searxng",
            Self::Bing => "bing",
        }
    }

    /// 按配置解析出有序后端列表。
    ///
    /// `mode` 对应 `config.toml` 的 `[search] backend`：
    /// - `auto`（缺省，含字段缺失）：配了 SearXNG 就先用它，失败再退回内置 Bing；
    /// - `searxng` / `bing`：只用指定后端，**不回退**（隐私敏感时用）；
    /// - `off`：关闭搜索。
    pub fn resolve(mode: Option<&str>, searxng_endpoint: Option<&str>) -> Vec<Self> {
        let endpoint = searxng_endpoint.map(str::trim).filter(|s| !s.is_empty());
        let mode = mode
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("auto")
            .to_ascii_lowercase();

        let searxng = || endpoint.map(|e| Self::Searxng(e.to_string()));
        match mode.as_str() {
            "off" | "none" | "disabled" => Vec::new(),
            "bing" => vec![Self::Bing],
            "searxng" => searxng().into_iter().collect(),
            _ => {
                let mut v = Vec::new();
                v.extend(searxng());
                v.push(Self::Bing);
                v
            }
        }
    }

    /// 构造请求 URL（查询串由 [`reqwest::Url`] 负责百分号编码）。
    fn build_url(&self, query: &str) -> Result<reqwest::Url, String> {
        let mut url = match self {
            Self::Searxng(endpoint) => reqwest::Url::parse(endpoint)
                .map_err(|e| format!("search.searxng_endpoint 非法（`{endpoint}`）：{e}"))?,
            Self::Bing => reqwest::Url::parse(BING_ENDPOINT)
                .map_err(|e| format!("内置 Bing 端点非法：{e}"))?,
        };
        match self {
            Self::Searxng(_) => {
                url.query_pairs_mut()
                    .append_pair("q", query)
                    .append_pair("format", "json");
            }
            Self::Bing => {
                url.query_pairs_mut().append_pair("q", query);
            }
        }
        Ok(url)
    }

    /// 期望的响应类型，用于 `Accept` 头。
    fn accept(&self) -> &'static str {
        match self {
            Self::Searxng(_) => "application/json",
            Self::Bing => "text/html",
        }
    }

    /// 把响应体解析成候选结果（空列表表示「无结果」，交由调用方顺延下一个后端）。
    fn parse(&self, body: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        match self {
            Self::Searxng(_) => parse_searxng(body, limit),
            Self::Bing => parse_bing(body, limit),
        }
    }
}

/// 搜索工具：按后端列表顺序尝试，全部失败才报错。
pub struct WebSearch {
    backends: Vec<SearchBackend>,
    client: reqwest::Client,
}

impl WebSearch {
    pub fn new(client: reqwest::Client, backends: Vec<SearchBackend>) -> Self {
        Self { backends, client }
    }

    /// 跑单个后端：发请求 → 校验状态码 → 解析。
    ///
    /// 返回 `Err` 时**必须**带上人类可读的原因（会直接进 ToolOutput 给模型看）：
    /// 早期版本直接 `resp.json()` 而不看状态码，SearXNG 返回 403 的 HTML 错误页时
    /// 报错是「bad search response: expected value at line 1 column 1」，完全无法排查。
    async fn run(
        &self,
        backend: &SearchBackend,
        query: &str,
        count: usize,
        ctx: &ToolCtx,
    ) -> Result<Vec<SearchHit>, String> {
        let url = backend.build_url(query)?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!("不支持的协议：{}", url.scheme()));
        }
        let req = self
            .client
            .get(url.clone())
            .header(reqwest::header::ACCEPT, backend.accept())
            // 依赖未开启 gzip/brotli 解压特性，索性显式要求服务端不压缩，
            // 否则抓回来的可能是二进制正文，解析出零结果却查不出原因。
            .header(reqwest::header::ACCEPT_ENCODING, "identity");

        let resp = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err("已取消".to_string()),
            r = req.send() => r.map_err(|e| format!("请求失败：{e}"))?,
        };
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("读取响应失败：{e}"))?;
        let body = String::from_utf8_lossy(&bytes);

        if !status.is_success() {
            return Err(http_error(status.as_u16(), &body));
        }
        backend.parse(&body, count)
    }
}

#[async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "联网搜索并返回候选结果列表（标题/链接/摘要）。无需配置即可使用；\
         也可在 config.toml 的 [search] 段改用自建 SearXNG 端点。"
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "count": { "type": "integer", "description": "结果条数，默认 8，最多 10" }
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
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'query'".into()))?;
        let count = args
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(8)
            .clamp(1, MAX_HITS as u64) as usize;

        if query.trim().is_empty() {
            return Err(ToolError::InvalidArgs("'query' 为空".into()));
        }
        if self.backends.is_empty() {
            return Ok(ToolOutput::err(
                "web_search 已被关闭（config.toml: [search] backend = \"off\"）。\n\
                 需要联网时改用 web_fetch 抓取已知 URL。"
                    .to_string(),
            ));
        }

        let mut failures = Vec::new();
        for backend in &self.backends {
            if ctx.cancel.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            match self.run(backend, query, count, ctx).await {
                Ok(hits) if !hits.is_empty() => return Ok(ToolOutput::ok(render_hits(&hits))),
                Ok(_) => failures.push(format!("- {}：无结果", backend.name())),
                Err(e) => failures.push(format!("- {}：{e}", backend.name())),
            }
        }

        let tried = self
            .backends
            .iter()
            .map(|b| b.name())
            .collect::<Vec<_>>()
            .join(" → ");
        Ok(ToolOutput::err(format!(
            "web_search 失败（已依次尝试 {tried}）：\n{}\n\
             提示：可直接用 web_fetch 抓取已知 URL。",
            failures.join("\n")
        )))
    }
}

fn render_hits(hits: &[SearchHit]) -> String {
    hits.iter()
        .enumerate()
        .map(|(i, h)| {
            let head = format!("{}. {}\n   {}", i + 1, h.title, h.url);
            if h.snippet.is_empty() {
                head
            } else {
                format!("{head}\n   {}", h.snippet)
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// 非 2xx 时的报错：带上状态码与正文开头，一眼能看出是鉴权、限流还是 JSON 输出没开。
fn http_error(status: u16, body: &str) -> String {
    let preview: String = text_of(body).chars().take(200).collect();
    let base = if preview.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}：{preview}")
    };
    // 最常见的自建踩坑：SearXNG 默认关闭 JSON 输出，返回 403 + 错误页。
    if status == 403 && body.to_ascii_lowercase().contains("format") {
        return format!(
            "{base}（该实例未启用 JSON 输出：settings.yml 的 search.formats 需加入 json）"
        );
    }
    base
}

/// 解析 SearXNG 的 JSON 响应。
fn parse_searxng(body: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
    // 最常见的自建踩坑：实例默认关闭 JSON 输出，返回的其实是 HTML 错误页。
    // 不单独识别的话，报错只会是「expected value at line 1 column 1」。
    if body.trim_start().starts_with('<') {
        return Err(
            "响应是 HTML 而非 JSON（该实例可能未启用 JSON 输出或被限流：\
             settings.yml 的 search.formats 需加入 json）"
                .to_string(),
        );
    }
    let value: serde_json::Value = serde_json::from_str(body).map_err(|e| {
        let head: String = text_of(body).chars().take(120).collect();
        format!("响应不是合法 JSON：{e}（开头：{head}）")
    })?;
    let Some(results) = value.get("results").and_then(|v| v.as_array()) else {
        return Err(
            "JSON 里没有 results 数组（多半是实例未启用 JSON 输出，返回了 HTML 错误页）"
                .to_string(),
        );
    };

    Ok(results
        .iter()
        .filter_map(|r| {
            let title = r.get("title").and_then(|v| v.as_str()).unwrap_or("").trim();
            let url = r.get("url").and_then(|v| v.as_str()).unwrap_or("").trim();
            if title.is_empty() && url.is_empty() {
                return None;
            }
            let snippet = r
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            Some(SearchHit {
                title: title.to_string(),
                url: url.to_string(),
                snippet: snippet.to_string(),
            })
        })
        .take(limit)
        .collect())
}

/// 解析 Bing 结果页：每条结果是一个 `class` 含 `b_algo` 的条目，标题在 `<h2><a>`，
/// 摘要在随后的 `<p>`。
///
/// 只认这两个锚点，其余标记（`<link>`/`<script>`/图标 div）一概跳过——没有 HTML
/// 依赖，也不想为一期引入一个解析器。
fn parse_bing(body: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
    let mut hits = Vec::new();
    let mut cursor = 0usize;

    while hits.len() < limit {
        let Some(rel) = body[cursor..].find("<li") else {
            break;
        };
        let tag_start = cursor + rel;
        let Some(rel_end) = body[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + rel_end + 1;

        let is_item = attrs(&body[tag_start..tag_end])
            .iter()
            .any(|(k, v)| k == "class" && v.split_ascii_whitespace().any(|c| c == "b_algo"));
        if !is_item {
            cursor = tag_end;
            continue;
        }

        // 条目边界：下一个 </li>（没有的话取到末尾）。
        let block_end = body[tag_end..]
            .find("</li")
            .map(|i| tag_end + i)
            .unwrap_or(body.len());
        cursor = block_end;

        if let Some(hit) = bing_hit(&body[tag_end..block_end]) {
            hits.push(hit);
        }
    }
    Ok(hits)
}

fn bing_hit(block: &str) -> Option<SearchHit> {
    let (head_start, head_end) = inner_between(block, "<h2", "</h2")?;
    let (url, title) = first_anchor(&block[head_start..head_end])?;
    let tail = &block[head_end..];
    let snippet = inner_between(tail, "<p", "</p")
        .map(|(s, e)| text_of(&tail[s..e]))
        .unwrap_or_default();
    if title.is_empty() && url.is_empty() {
        return None;
    }
    Some(SearchHit {
        title,
        url,
        snippet,
    })
}

/// 取 `open` 开标签之后、`close` 之前的内容，返回 `(起始, 结束)` 下标。
fn inner_between(src: &str, open: &str, close: &str) -> Option<(usize, usize)> {
    let i = src.find(open)?;
    let after_open = i + open.len();
    let body_start = src[after_open..].find('>').map(|j| after_open + j + 1)?;
    let end = src[body_start..].find(close).map(|j| body_start + j)?;
    if end < body_start {
        return None;
    }
    Some((body_start, end))
}

/// 取片段里第一个 `<a>` 的 `(href, 文本)`。
fn first_anchor(src: &str) -> Option<(String, String)> {
    let mut from = 0usize;
    loop {
        let i = src[from..].find("<a")? + from;
        let next = src.as_bytes().get(i + 2).copied();
        // `<article>` 之类也算 `<a` 开头，按分隔符排除。
        if !matches!(
            next,
            Some(b' ') | Some(b'>') | Some(b'\n') | Some(b'\t') | Some(b'\r')
        ) {
            from = i + 2;
            continue;
        }
        let tag_end = src[i..].find('>')? + i + 1;
        let url = attrs(&src[i..tag_end])
            .into_iter()
            .find(|(k, _)| k == "href")
            .map(|(_, v)| v)
            .unwrap_or_default();
        let rest = &src[tag_end..];
        let text_end = rest.find("</a").unwrap_or(rest.len());
        return Some((url, text_of(&rest[..text_end])));
    }
}

/// 解析 HTML 开标签的属性表：`(属性名小写, 已解码的值)`。
fn attrs(tag: &str) -> Vec<(String, String)> {
    let inner = tag.trim_start_matches('<').trim_end_matches('>');
    let bytes = inner.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;

    // 跳过标签名。
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let name_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' {
            i += 1;
        }
        let name = inner[name_start..i].to_ascii_lowercase();

        let mut value = String::new();
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            let (v_start, v_end) = if let Some(&quote) = bytes.get(i) {
                if quote == b'"' || quote == b'\'' {
                    i += 1;
                    let s = i;
                    while i < bytes.len() && bytes[i] != quote {
                        i += 1;
                    }
                    let e = i;
                    i += 1; // 跳过收尾引号
                    (s, e)
                } else {
                    let s = i;
                    while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    (s, i)
                }
            } else {
                (i, i)
            };
            value = decode_entities(&inner[v_start..v_end]);
        }
        out.push((name, value));
    }
    out
}

/// 去标签 + 解码实体 + 压缩空白（标题 / 摘要 / 报错预览都要用）。
fn text_of(src: &str) -> String {
    let mut stripped = String::with_capacity(src.len());
    let mut in_tag = false;
    for c in src.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => stripped.push(c),
            _ => {}
        }
    }
    let decoded = decode_entities(&stripped);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 解码常见 HTML 实体（数值实体含十进制与十六进制）。
fn decode_entities(src: &str) -> String {
    const NAMED: &[(&str, char)] = &[
        ("amp", '&'),
        ("lt", '<'),
        ("gt", '>'),
        ("quot", '"'),
        ("apos", '\''),
        ("nbsp", ' '),
        ("ensp", ' '),
        ("emsp", ' '),
        ("mdash", '—'),
        ("ndash", '–'),
        ("hellip", '…'),
    ];

    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 1..];
        let Some(semi) = after.find(';') else {
            out.push('&');
            rest = after;
            continue;
        };
        let code = &after[..semi];
        let mapped = NAMED.iter().find(|(k, _)| *k == code).map(|(_, v)| *v);
        let mapped = mapped.or_else(|| {
            if let Some(hex) = code.strip_prefix("#x").or_else(|| code.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
            } else {
                code.strip_prefix('#')
                    .and_then(|d| d.parse::<u32>().ok())
                    .and_then(char::from_u32)
            }
        });

        match mapped {
            Some(c) => {
                out.push(c);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn strips_tags_and_scripts() {
        let html = "<html><head><script>var x=1;</script></head><body><h1>Hi</h1><p>Text</p></body></html>";
        let t = strip_html(html);
        assert!(t.contains("Hi"));
        assert!(t.contains("Text"));
        assert!(!t.contains("var x"));
    }

    #[test]
    fn strips_tags_with_multibyte_text() {
        // 回归：按字节推进再对 &str 切片会切在「解」中间 panic
        // （start byte index ... is not a char boundary），
        // 按字节当 Latin-1 输出还会把中文变成乱码。
        let html = "<p>这是一段中文解析测试</p><p>emoji 😀</p>";
        let t = strip_html(html);
        assert!(t.contains("这是一段中文解析测试"), "中文不能丢/乱码: {t:?}");
        assert!(t.contains("😀"), "emoji 不能丢: {t:?}");
        assert!(!t.contains('<'), "标签应被剥离: {t:?}");
    }

    // ---- web_search：后端选择 ----

    #[test]
    fn default_backend_works_without_any_configuration() {
        // 回归：早期 web_search 只有 SearXNG 一条路，而默认配置与向导都不写
        // [search] 段，于是每次调用都是「未配置搜索后端」——工具 100% 不可用。
        let b = SearchBackend::resolve(None, None);
        assert_eq!(b.len(), 1, "必须有一个能用的内置后端: {b:?}");
        assert_eq!(b[0].name(), "bing");
    }

    #[test]
    fn searxng_is_preferred_when_configured() {
        let b = SearchBackend::resolve(None, Some("  http://localhost:8080/search  "));
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].name(), "searxng");
        assert_eq!(b[1].name(), "bing", "SearXNG 挂了还能退回内置后端");
    }

    #[test]
    fn explicit_backend_does_not_fall_back() {
        // 隐私诉求：只认 searxng 时，不允许偷偷把 query 发给第三方。
        assert!(SearchBackend::resolve(Some("searxng"), None).is_empty());
        // 端点为空/纯空白视为未配置。
        assert!(SearchBackend::resolve(Some("searxng"), Some("   ")).is_empty());

        let b = SearchBackend::resolve(Some("bing"), Some("http://x/search"));
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].name(), "bing");
    }

    #[test]
    fn off_disables_search() {
        for mode in ["off", "OFF", " none ", "disabled"] {
            assert!(
                SearchBackend::resolve(Some(mode), Some("http://x/search")).is_empty(),
                "{mode} 应关闭搜索"
            );
        }
    }

    #[test]
    fn unknown_mode_degrades_to_auto() {
        let b = SearchBackend::resolve(Some("wat"), None);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].name(), "bing");
    }

    // ---- web_search：请求构造 ----

    #[test]
    fn searxng_url_keeps_existing_params_and_appends_query() {
        let b = SearchBackend::Searxng("http://localhost:8080/search?lang=zh".into());
        let url = b.build_url("rust 语言").unwrap();
        assert_eq!(url.path(), "/search");
        let query = url.query().expect("应有查询串");
        assert!(query.contains("lang=zh"), "原参数不能被吃掉: {query}");
        assert!(query.contains("format=json"), "{query}");
        // 编码的关键在于往返一致（空格是 + 还是 %20 由 url crate 决定）。
        let sent = url
            .query_pairs()
            .find(|(k, _)| k == "q")
            .map(|(_, v)| v.to_string())
            .expect("q 必须存在");
        assert_eq!(sent, "rust 语言");
    }

    #[test]
    fn bing_url_encodes_query() {
        let url = SearchBackend::Bing.build_url("rust lang & co").unwrap();
        assert_eq!(url.domain(), Some("www.bing.com"));
        let sent = url
            .query_pairs()
            .find(|(k, _)| k == "q")
            .map(|(_, v)| v.to_string())
            .expect("q 必须存在");
        assert_eq!(sent, "rust lang & co");
    }

    #[test]
    fn malformed_endpoint_reports_endpoint_not_generic_error() {
        let b = SearchBackend::Searxng("不是 URL".into());
        let err = b.build_url("q").unwrap_err();
        assert!(err.contains("searxng_endpoint"), "应点名字段: {err}");
    }

    // ---- web_search：响应解析 ----

    /// 取自真实 Bing 结果页：`<li class="b_algo">` 条目，标题在 `<h2><a>`，
    /// 摘要在 `<p class="b_lineclamp2">`，正文里有实体（`&#0183;` `&amp;` `&ensp;`）。
    const BING_PAGE: &str = r#"
<li class="b_algo" data-id iid=SERP.1>
  <div class="b_tpcn"><a class="tilk" href="https://rust-lang.org/"><div class="tptt">rust-lang.org</div></a></div>
  <h2><a target="_blank" href="https://rust-lang.org/" h="ID=SERP,1.2"><strong>Rust</strong> Programming Language</a></h2>
  <div class="b_caption"><p class="b_lineclamp2">1 天前&#0183;&ensp;Rust is blazingly fast &amp; memory-efficient: with no runtime or garbage collector.</p></div>
</li>
<li class="b_algo" data-id iid=SERP.2>
  <h2><a href="https://doc.rust-lang.org/book/">The Rust Programming Language</a></h2>
  <div class="b_caption"><p>《Rust 程序设计语言》是官方入门书。</p></div>
</li>
"#;

    #[test]
    fn parses_bing_results_and_decodes_entities() {
        let hits = parse_bing(BING_PAGE, MAX_HITS).expect("应能解析");
        assert_eq!(hits.len(), 2, "实际: {hits:?}");
        assert_eq!(hits[0].title, "Rust Programming Language");
        assert_eq!(hits[0].url, "https://rust-lang.org/");
        assert!(
            hits[0].snippet.contains("memory-efficient"),
            "摘要丢了: {:?}",
            hits[0].snippet
        );
        assert!(
            hits[0].snippet.contains('&'),
            "&amp; 必须解码: {:?}",
            hits[0].snippet
        );
        assert_eq!(hits[1].url, "https://doc.rust-lang.org/book/");
    }

    #[test]
    fn bing_parsing_keeps_multibyte_text() {
        // 回归：按字节推进再对 &str 切片会 panic（not a char boundary）。
        let hits = parse_bing(BING_PAGE, MAX_HITS).unwrap();
        assert_eq!(
            hits[1].snippet, "《Rust 程序设计语言》是官方入门书。",
            "中文摘要不能丢/乱码: {:?}",
            hits[1].snippet
        );
    }

    #[test]
    fn bing_limit_is_respected() {
        let hits = parse_bing(BING_PAGE, 1).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn irrelevant_markup_yields_no_results() {
        // 无结果必须变成空列表（好让上层顺延下一个后端），不能报错。
        let hits = parse_bing(
            "<html><body><ol><li class=\"other\">x</li></ol></body></html>",
            8,
        )
        .expect("结构不对也不该 panic");
        assert!(hits.is_empty());
    }

    #[test]
    fn parses_searxng_json() {
        let body = r#"{"query":"rust","results":[
            {"title":"Rust Home","url":"https://rust-lang.org/","content":"A language empowering everyone."},
            {"title":"Docs","url":"https://doc.rust-lang.org/","content":""} ]}"#;
        let hits = SearchBackend::Searxng("http://x/search".into())
            .parse(body, MAX_HITS)
            .expect("应能解析");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://rust-lang.org/");
        assert_eq!(hits[0].snippet, "A language empowering everyone.");
        assert_eq!(hits[1].snippet, "");
    }

    #[test]
    fn searxng_html_error_page_is_reported_clearly() {
        // 回归：早期不看状态码，403 的 HTML 错误页会变成
        // 「bad search response: expected value at line 1 column 1」。
        let err = SearchBackend::Searxng("http://x/search".into())
            .parse("<html><body>403 Forbidden</body></html>", MAX_HITS)
            .unwrap_err();
        assert!(err.contains("json"), "应指向 JSON 输出未启用: {err}");
        assert!(
            !err.contains("expected value"),
            "不该是裸 serde 报错: {err}"
        );
    }

    #[test]
    fn http_error_mentions_status_and_body() {
        let err = http_error(503, "<html>Service Unavailable</html>");
        assert!(err.contains("503"), "{err}");
        assert!(err.contains("Service Unavailable"), "{err}");
    }

    #[test]
    fn http_error_hints_when_searxng_json_is_disabled() {
        let err = http_error(403, "Format is not supported by this instance");
        assert!(err.contains("403"), "{err}");
        assert!(err.contains("json"), "应给出启用 JSON 的具体办法: {err}");
    }

    #[test]
    fn renders_hits_as_readable_list() {
        let out = render_hits(&[
            SearchHit {
                title: "A".into(),
                url: "https://a/".into(),
                snippet: "sa".into(),
            },
            SearchHit {
                title: "B".into(),
                url: "https://b/".into(),
                snippet: String::new(),
            },
        ]);
        assert_eq!(out, "1. A\n   https://a/\n   sa\n\n2. B\n   https://b/");
    }

    #[tokio::test]
    async fn empty_backend_list_reports_disabled() {
        let out = WebSearch::new(noop_client(), vec![])
            .execute(json!({ "query": "rust" }), &search_ctx())
            .await
            .expect("不应返回 Err");
        assert!(out.is_error);
        assert!(out.content.contains("off"), "实际: {}", out.content);
    }

    #[tokio::test]
    async fn blank_query_is_invalid_args() {
        let out = WebSearch::new(noop_client(), SearchBackend::resolve(None, None))
            .execute(json!({ "query": "  " }), &search_ctx())
            .await;
        assert!(matches!(out, Err(ToolError::InvalidArgs(_))), "{out:?}");
    }

    /// 真实网络冒烟：验证「零配置也能搜」这条路径确实通。
    ///
    /// 默认不跑（要联网）：`cargo test -p agent-tools -- --ignored web_search`。
    #[tokio::test]
    #[ignore]
    async fn searches_without_configuration_against_live_bing() {
        let client = agent_net::build_client(&Default::default()).expect("客户端应能构建");
        let out = WebSearch::new(client, SearchBackend::resolve(None, None))
            .execute(
                json!({ "query": "rust programming language", "count": 5 }),
                &search_ctx(),
            )
            .await
            .expect("不应返回 Err");
        assert!(!out.is_error, "搜索应成功，实际: {}", out.content);
        assert!(
            out.content.contains("https://"),
            "应有候选链接: {}",
            out.content
        );
        println!(
            "--- live web_search ---\n{}\n-----------------------",
            out.content
        );
    }

    fn noop_client() -> reqwest::Client {
        reqwest::Client::builder().build().expect("客户端应能构建")
    }

    fn search_ctx() -> ToolCtx {
        ToolCtx {
            workspace: std::env::temp_dir().join("agent-tools-search-test"),
            project_root: None,
            task_id: "test".into(),
            timeout: Duration::from_secs(30),
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }
}
