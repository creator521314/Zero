//! OpenAI 兼容适配器：一套代码覆盖 DeepSeek / 通义（兼容模式）/ OpenAI / Ollama。
//!
//! 关键差异抹平：
//! - OpenAI 的 `tool_calls[].function.arguments` 是 JSON **字符串**，统一层要求已解析 JSON → 此处 parse；
//! - tool 结果以独立 `role:"tool"` 消息回填；
//! - system 作为 messages 中的一条；
//! - 流式工具参数以增量字符串下发，累积到 Done 后整体 parse。

use std::collections::BTreeMap;

use agent_core::ids::ToolCallId;
use agent_core::provider::{
    BoxStream, Capabilities, ChatRequest, ChatResponse, ContentPart, FinishReason, LlmError,
    LlmProvider, Message, ModelPrice, Role, StreamEvent, ToolCall, Usage,
};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{json, Value};

use crate::config::ProviderConfig;

pub struct OpenAiCompatProvider {
    name: String,
    cfg: ProviderConfig,
    client: reqwest::Client,
    api_key: Option<String>,
}

impl OpenAiCompatProvider {
    pub fn new(name: impl Into<String>, cfg: ProviderConfig, client: reqwest::Client) -> Self {
        let api_key = cfg
            .api_key_env
            .as_ref()
            .and_then(|env| std::env::var(env).ok());
        Self {
            name: name.into(),
            cfg,
            client,
            api_key,
        }
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/chat/completions",
            self.cfg.base_url.trim_end_matches('/')
        )
    }

    fn build_body(&self, req: &ChatRequest) -> Value {
        let messages: Vec<Value> = req.messages.iter().map(to_openai_message).collect();

        let mut body = json!({
            "model": if req.model.is_empty() { self.cfg.model.clone() } else { req.model.clone() },
            "messages": messages,
            "stream": req.stream,
        });

        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(m) = req.max_tokens {
            body["max_tokens"] = json!(m);
        }
        if !req.tools.is_empty() {
            body["tools"] = json!(req
                .tools
                .iter()
                .map(|t| json!({
                    "type": "function",
                    "function": { "name": t.name, "description": t.description, "parameters": t.parameters }
                }))
                .collect::<Vec<_>>());
        }
        if req.stream {
            // 让流式响应末尾带回 usage
            body["stream_options"] = json!({ "include_usage": true });
        }
        body
    }

    async fn send(&self, req: &ChatRequest) -> Result<reqwest::Response, LlmError> {
        let body = self.build_body(req);
        let mut rb = self.client.post(self.endpoint()).json(&body);
        if let Some(key) = &self.api_key {
            rb = rb.bearer_auth(key);
        }
        let resp = rb
            .send()
            .await
            .map_err(|e| LlmError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(match status {
                401 | 403 => LlmError::Auth(body),
                429 => LlmError::RateLimited { retry_after: None },
                400 | 422 => LlmError::InvalidRequest(body),
                _ => LlmError::ServerError { status, body },
            });
        }
        Ok(resp)
    }
}

/// 统一 Message -> OpenAI 消息格式。
fn to_openai_message(m: &Message) -> Value {
    let role = match m.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };

    // tool 结果回填
    if m.role == Role::Tool {
        let mut v = json!({ "role": "tool", "content": m.text() });
        if let Some(id) = &m.tool_call_id {
            v["tool_call_id"] = json!(id.0);
        }
        return v;
    }

    let text = parts_to_text(&m.content);

    // assistant 发起工具调用
    if !m.tool_calls.is_empty() {
        let calls: Vec<Value> = m
            .tool_calls
            .iter()
            .map(|c| {
                json!({
                    "id": c.id.0,
                    "type": "function",
                    "function": {
                        "name": c.name,
                        "arguments": serde_json::to_string(&c.arguments).unwrap_or_else(|_| "{}".into())
                    }
                })
            })
            .collect();
        let mut v = json!({ "role": role, "tool_calls": calls });
        if !text.is_empty() {
            v["content"] = json!(text);
        }
        return v;
    }

    json!({ "role": role, "content": text })
}

fn parts_to_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None, // 多模态一期不发送
        })
        .collect::<Vec<_>>()
        .join("")
}

#[async_trait]
impl LlmProvider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming: true,
            tools: self.cfg.tools,
            parallel_tools: true,
            vision: self.cfg.vision,
            reasoning: self.cfg.reasoning,
            json_mode: true,
            max_context: self.cfg.max_context,
        }
    }

    async fn chat(&self, mut req: ChatRequest) -> Result<ChatResponse, LlmError> {
        req.stream = false;
        let resp = self.send(&req).await?;
        let v: Value = resp
            .json()
            .await
            .map_err(|e| LlmError::StreamBroken(format!("bad json: {e}")))?;

        let choice = v
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .ok_or_else(|| LlmError::StreamBroken("no choices".into()))?;

        let message = from_openai_message(choice.get("message").cloned().unwrap_or(Value::Null));
        let finish_reason = map_finish(choice.get("finish_reason").and_then(|f| f.as_str()));
        let usage = parse_usage(v.get("usage"));

        Ok(ChatResponse {
            message,
            finish_reason,
            usage,
            provider: self.name.clone(),
            model_returned: v
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or(&self.cfg.model)
                .to_string(),
        })
    }

    async fn chat_stream(
        &self,
        req: ChatRequest,
    ) -> Result<BoxStream<Result<StreamEvent, LlmError>>, LlmError> {
        let resp = self.send(&req).await?;
        let name = self.name.clone();
        let model = self.cfg.model.clone();

        let stream = async_stream::stream! {
            yield Ok(StreamEvent::Start { provider: name.clone(), model: model.clone() });

            let mut es = resp.bytes_stream().eventsource();
            // 累积工具调用参数：index -> (id, name, args_string)
            let mut tool_acc: BTreeMap<usize, (Option<ToolCallId>, Option<String>, String)> = BTreeMap::new();
            let mut finish = FinishReason::Stop;
            let mut last_usage: Option<Usage> = None;

            while let Some(ev) = es.next().await {
                match ev {
                    Ok(ev) => {
                        if ev.data == "[DONE]" {
                            break;
                        }
                        let v: Value = match serde_json::from_str(&ev.data) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                            last_usage = Some(parse_usage(Some(u)));
                        }
                        let Some(choice) = v.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first()) else {
                            continue;
                        };
                        if let Some(delta) = choice.get("delta") {
                            if let Some(t) = delta.get("content").and_then(|c| c.as_str()) {
                                if !t.is_empty() {
                                    yield Ok(StreamEvent::TextDelta(t.to_string()));
                                }
                            }
                            // 思考链（部分厂商用 reasoning_content）
                            if let Some(r) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
                                if !r.is_empty() {
                                    yield Ok(StreamEvent::ReasoningDelta(r.to_string()));
                                }
                            }
                            if let Some(tcs) = delta.get("tool_calls").and_then(|c| c.as_array()) {
                                for tc in tcs {
                                    let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                                    let entry = tool_acc.entry(index).or_insert((None, None, String::new()));
                                    if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                                        entry.0 = Some(ToolCallId(id.to_string()));
                                    }
                                    if let Some(f) = tc.get("function") {
                                        if let Some(n) = f.get("name").and_then(|n| n.as_str()) {
                                            if entry.1.is_none() { entry.1 = Some(n.to_string()); }
                                        }
                                        if let Some(a) = f.get("arguments").and_then(|a| a.as_str()) {
                                            entry.2.push_str(a);
                                        }
                                    }
                                    yield Ok(StreamEvent::ToolCallDelta {
                                        index,
                                        id: entry.0.clone(),
                                        name: entry.1.clone(),
                                        args_delta: tc.get("function")
                                            .and_then(|f| f.get("arguments"))
                                            .and_then(|a| a.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                    });
                                }
                            }
                        }
                        if let Some(f) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                            finish = map_finish(Some(f));
                        }
                    }
                    Err(e) => {
                        yield Err(LlmError::StreamBroken(e.to_string()));
                        return;
                    }
                }
            }

            if let Some(u) = last_usage {
                yield Ok(StreamEvent::Usage(u));
            }
            yield Ok(StreamEvent::Done(finish));
        };

        Ok(Box::pin(stream))
    }

    fn price(&self, _model: &str) -> Option<ModelPrice> {
        Some(ModelPrice {
            input_per_1k: self.cfg.input_per_1k,
            output_per_1k: self.cfg.output_per_1k,
        })
    }
}

fn from_openai_message(v: Value) -> Message {
    let role = match v.get("role").and_then(|r| r.as_str()) {
        Some("assistant") => Role::Assistant,
        Some("tool") => Role::Tool,
        Some("system") => Role::System,
        _ => Role::User,
    };
    let content_text = v.get("content").and_then(|c| c.as_str()).unwrap_or("");
    let mut msg = Message {
        role,
        content: if content_text.is_empty() {
            vec![]
        } else {
            vec![ContentPart::text(content_text)]
        },
        tool_calls: Vec::new(),
        tool_call_id: v
            .get("tool_call_id")
            .and_then(|i| i.as_str())
            .map(|s| ToolCallId(s.to_string())),
        reasoning: v
            .get("reasoning_content")
            .and_then(|r| r.as_str())
            .map(|s| s.to_string()),
    };

    if let Some(tcs) = v.get("tool_calls").and_then(|c| c.as_array()) {
        for tc in tcs {
            let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or_default();
            let f = tc.get("function");
            let name = f
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or_default();
            let args_raw = f
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .unwrap_or("{}");
            let arguments =
                serde_json::from_str(args_raw).unwrap_or(Value::Object(Default::default()));
            msg.tool_calls.push(ToolCall {
                id: ToolCallId(id.to_string()),
                name: name.to_string(),
                arguments,
            });
        }
    }
    msg
}

fn map_finish(s: Option<&str>) -> FinishReason {
    match s {
        Some("stop") => FinishReason::Stop,
        Some("tool_calls") => FinishReason::ToolCalls,
        Some("length") => FinishReason::Length,
        Some("content_filter") => FinishReason::ContentFilter,
        _ => FinishReason::Other,
    }
}

fn parse_usage(v: Option<&Value>) -> Usage {
    let Some(u) = v else {
        return Usage::default();
    };
    Usage {
        input_tokens: u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
        output_tokens: u
            .get("completion_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        reasoning_tokens: u
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|x| x.as_u64()),
        cached_tokens: u
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|x| x.as_u64()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_tool_message() {
        let m = Message {
            role: Role::Tool,
            content: vec![ContentPart::text("output")],
            tool_calls: vec![],
            tool_call_id: Some(ToolCallId("call_1".into())),
            reasoning: None,
        };
        let v = to_openai_message(&m);
        assert_eq!(v["role"], "tool");
        assert_eq!(v["tool_call_id"], "call_1");
    }

    #[test]
    fn converts_assistant_tool_call() {
        let m = Message {
            role: Role::Assistant,
            content: vec![],
            tool_calls: vec![ToolCall {
                id: ToolCallId("c1".into()),
                name: "shell_exec".into(),
                arguments: json!({"cmd":"ls"}),
            }],
            tool_call_id: None,
            reasoning: None,
        };
        let v = to_openai_message(&m);
        assert_eq!(v["tool_calls"][0]["function"]["name"], "shell_exec");
        // arguments 必须是字符串（OpenAI 协议要求）
        assert!(v["tool_calls"][0]["function"]["arguments"].is_string());
    }

    #[test]
    fn parses_finish_reason() {
        assert_eq!(map_finish(Some("tool_calls")), FinishReason::ToolCalls);
        assert_eq!(map_finish(Some("stop")), FinishReason::Stop);
    }
}
