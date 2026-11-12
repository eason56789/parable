//! OpenAI-compatible providers: `/v1/chat/completions` and `/v1/responses`.
//!
//! Both are implemented against the *streaming* protocol and tolerate the small
//! variations third-party relays add (`reasoning_content`, missing `id`s, etc.).

use super::*;
use crate::config::ProviderConfig;
use crate::token::Usage;
use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;

pub struct OpenAiBase {
    base_url: String,
    api_key: String,
    model: String,
    kind: &'static str,
}

impl OpenAiBase {
    pub fn new(cfg: &ProviderConfig, kind: &'static str) -> Self {
        Self {
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: cfg.key.clone(),
            model: String::new(),
            kind,
        }
    }

    pub fn with_model(mut self, model: &str) -> Self {
        self.model = model.to_string();
        self
    }

    fn headers(&self) -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            "authorization",
            format!("Bearer {}", self.api_key)
                .parse()
                .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static("")),
        );
        h.insert("content-type", reqwest::header::HeaderValue::from_static("application/json"));
        h
    }

    fn endpoint(&self, path: &str) -> String {
        if self.base_url.ends_with(&format!("/v1/{path}")) {
            self.base_url.clone()
        } else if self.base_url.ends_with("/v1") {
            format!("{}/{path}", self.base_url)
        } else {
            format!("{}/v1/{path}", self.base_url)
        }
    }
}

/// Build an OpenAI *Chat Completions* provider.
pub fn chat(cfg: &ProviderConfig) -> OpenAiBase {
    OpenAiBase::new(cfg, "openai")
}

/// Build an OpenAI *Responses* provider.
pub fn responses(cfg: &ProviderConfig) -> OpenAiBase {
    OpenAiBase::new(cfg, "responses")
}

fn build_chat_messages(system: &str, messages: &[Msg]) -> Value {
    let mut out = Vec::new();
    if !system.is_empty() {
        out.push(json!({"role":"system","content": system}));
    }
    for m in messages {
        match m {
            Msg::User(parts) => {
                // 文本 + 视觉图合并为一条多模态消息（chat completions 的
                // content 数组形态）；纯文本退回字符串形态以兼容中转站。
                let mut blocks: Vec<Value> = Vec::new();
                for p in parts {
                    match p {
                        UserPart::Text(t) => blocks.push(json!({"type":"text","text": t})),
                        UserPart::Image { media_type, data } => blocks.push(json!({
                            "type":"image_url",
                            "image_url":{"url": format!("data:{media_type};base64,{data}")}
                        })),
                        UserPart::ToolResult { id, content, .. } => {
                            out.push(json!({"role":"tool","tool_call_id": id,"content": content}));
                        }
                    }
                }
                if !blocks.is_empty() {
                    let content = if blocks.iter().all(|b| b["type"] == "text") {
                        json!(blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join(""))
                    } else {
                        json!(blocks)
                    };
                    out.push(json!({"role":"user","content": content}));
                }
            }
            Msg::Assistant(parts) => {
                let mut text = String::new();
                let mut calls = Vec::new();
                for p in parts {
                    match p {
                        AsstPart::Text(t) => {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(t);
                        }
                        AsstPart::Thinking { .. } => {} // not replayable on chat completions
                        AsstPart::ToolUse { id, name, args } => {
                            calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {"name": name, "arguments": args.to_string()}
                            }));
                        }
                    }
                }
                let mut v = json!({"role":"assistant","content": if text.is_empty() { Value::Null } else { Value::String(text) }});
                if !calls.is_empty() {
                    v["tool_calls"] = Value::Array(calls);
                }
                out.push(v);
            }
        }
    }
    Value::Array(out)
}

fn build_chat_body(req: &ChatRequest) -> Value {
    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            json!({
                "type":"function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                }
            })
        })
        .collect();
    let mut body = json!({
        "model": req.model,
        "messages": build_chat_messages(&req.system, &req.messages),
        "stream": true,
        "stream_options": {"include_usage": true},
        "max_tokens": req.max_tokens,
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    body
}

fn sse_data_lines(buf: &str) -> Vec<String> {
    buf.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|s| s.trim_start().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn stream_chat_completions<S>(
    byte_stream: S,
) -> impl Stream<Item = Result<StreamChunk, HarnessError>> + Send
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    async_stream::stream! {
        futures::pin_mut!(byte_stream);
        let mut buf = String::new();
        let mut usage = Usage::default();
        // index -> (id, name, args)
        let mut calls: HashMap<i64, (String, String, String)> = HashMap::new();

        while let Some(item) = byte_stream.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(e) => { yield Err(HarnessError::Http(e)); break; }
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find("\n\n") {
                let raw = buf[..pos].to_string();
                buf.drain(..pos + 2);
                for data in sse_data_lines(&raw) {
                    if data == "[DONE]" {
                        // flush any pending tool calls
                        for (idx, (id, name, args)) in calls.drain() {
                            let _ = idx;
                            let v: Value = serde_json::from_str(&args).unwrap_or_else(|_| json!({}));
                            yield Ok(StreamChunk::ToolCallDone { id, name, args: v });
                        }
                        yield Ok(StreamChunk::Done { usage, stop_reason: Some("stop".into()) });
                        return;
                    }
                    let v: Value = match serde_json::from_str(&data) { Ok(v) => v, Err(_) => continue };
                    if let Some(u) = v.get("usage") {
                        usage.input_tokens = u["prompt_tokens"].as_u64().unwrap_or(0);
                        usage.output_tokens = u["completion_tokens"].as_u64().unwrap_or(0);
                        usage.cache_read_input_tokens =
                            u.pointer("/prompt_tokens_details/cached_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                    }
                    let Some(choice) = v.pointer("/choices/0") else { continue };
                    if let Some(d) = choice.get("delta") {
                        if let Some(c) = d["content"].as_str() {
                            if !c.is_empty() { yield Ok(StreamChunk::Text(c.to_string())); }
                        }
                        // DeepSeek / some relays surface reasoning here.
                        if let Some(r) = d["reasoning_content"].as_str().or_else(|| d["reasoning"].as_str()) {
                            if !r.is_empty() { yield Ok(StreamChunk::Reasoning(r.to_string())); }
                        }
                        if let Some(tcs) = d.get("tool_calls").and_then(|x| x.as_array()) {
                            for tc in tcs {
                                let idx = tc["index"].as_i64().unwrap_or(0);
                                let entry = calls.entry(idx).or_insert_with(|| (String::new(), String::new(), String::new()));
                                if let Some(id) = tc["id"].as_str() {
                                    if !id.is_empty() {
                                        if entry.0.is_empty() {
                                            entry.0 = id.to_string();
                                            let name = tc.pointer("/function/name").and_then(|x| x.as_str()).unwrap_or_default().to_string();
                                            entry.1 = name.clone();
                                            yield Ok(StreamChunk::ToolCallStart { id: entry.0.clone(), name });
                                        }
                                    }
                                }
                                if let Some(n) = tc.pointer("/function/name").and_then(|x| x.as_str()) {
                                    if entry.1.is_empty() {
                                        entry.1 = n.to_string();
                                    }
                                }
                                if let Some(a) = tc.pointer("/function/arguments").and_then(|x| x.as_str()) {
                                    entry.2.push_str(a);
                                    if !entry.0.is_empty() {
                                        yield Ok(StreamChunk::ToolCallArgsDelta { id: entry.0.clone(), delta: a.to_string() });
                                    }
                                }
                            }
                        }
                    }
                    if let Some(finish) = choice.get("finish_reason").and_then(|x| x.as_str()) {
                        if finish == "stop" || finish == "tool_calls" {
                            for (idx, (id, name, args)) in calls.drain() {
                                let _ = idx;
                                let v: Value = serde_json::from_str(&args).unwrap_or_else(|_| json!({}));
                                yield Ok(StreamChunk::ToolCallDone { id, name, args: v });
                            }
                        }
                    }
                }
            }
        }
        yield Ok(StreamChunk::Done { usage, stop_reason: Some("stop".into()) });
    }
}

fn build_responses_body(req: &ChatRequest) -> Value {
    let mut input = Vec::new();
    if !req.system.is_empty() {
        input.push(json!({"role":"system","content": req.system}));
    }
    for m in &req.messages {
        match m {
            Msg::User(parts) => {
                for p in parts {
                    match p {
                        UserPart::Text(t) => input.push(json!({"role":"user","content": t})),
                        UserPart::Image { media_type, data } => input.push(json!({
                            "role":"user",
                            "content":[{"type":"input_image","image_url": format!("data:{media_type};base64,{data}")}]
                        })),
                        UserPart::ToolResult { id, content, .. } => input.push(json!({
                            "type":"function_call_output","call_id": id,"output": content
                        })),
                    }
                }
            }
            Msg::Assistant(parts) => {
                for p in parts {
                    match p {
                        AsstPart::Text(t) => input.push(json!({"role":"assistant","content": t})),
                        AsstPart::Thinking { .. } => {}
                        AsstPart::ToolUse { id, name, args } => input.push(json!({
                            "type":"function_call","call_id": id,"name": name,"arguments": args.to_string()
                        })),
                    }
                }
            }
        }
    }
    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            json!({
                "type":"function","name": t.name,"description": t.description,"parameters": t.input_schema
            })
        })
        .collect();
    let mut body = json!({
        "model": req.model,
        "input": input,
        "stream": true,
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    body
}

fn stream_responses<S>(byte_stream: S) -> impl Stream<Item = Result<StreamChunk, HarnessError>> + Send
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    async_stream::stream! {
        futures::pin_mut!(byte_stream);
        let mut buf = String::new();
        let mut usage = Usage::default();
        while let Some(item) = byte_stream.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(e) => { yield Err(HarnessError::Http(e)); break; }
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find("\n\n") {
                let raw = buf[..pos].to_string();
                buf.drain(..pos + 2);
                for data in sse_data_lines(&raw) {
                    if data == "[DONE]" {
                        yield Ok(StreamChunk::Done { usage, stop_reason: Some("stop".into()) });
                        return;
                    }
                    let v: Value = match serde_json::from_str(&data) { Ok(v) => v, Err(_) => continue };
                    let typ = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
                    match typ {
                        "response.output_text.delta" => {
                            if let Some(t) = v["delta"].as_str() {
                                if !t.is_empty() { yield Ok(StreamChunk::Text(t.to_string())); }
                            }
                        }
                        "response.reasoning_summary_text.delta" => {
                            if let Some(t) = v["delta"].as_str() {
                                if !t.is_empty() { yield Ok(StreamChunk::Reasoning(t.to_string())); }
                            }
                        }
                        "response.output_item.done" => {
                            let item = &v["item"];
                            if item["type"] == "function_call" {
                                let id = item["call_id"].as_str().unwrap_or_default().to_string();
                                let name = item["name"].as_str().unwrap_or_default().to_string();
                                let args: Value = serde_json::from_str(item["arguments"].as_str().unwrap_or("{}"))
                                    .unwrap_or_else(|_| json!({}));
                                yield Ok(StreamChunk::ToolCallDone { id, name, args });
                            }
                        }
                        "response.completed" => {
                            if let Some(u) = v.pointer("/response/usage") {
                                usage.input_tokens = u["input_tokens"].as_u64().unwrap_or(0);
                                usage.output_tokens = u["output_tokens"].as_u64().unwrap_or(0);
                            }
                            yield Ok(StreamChunk::Done { usage, stop_reason: Some("stop".into()) });
                            return;
                        }
                        "response.failed" | "error" => {
                            let msg = v.pointer("/error/message").and_then(|x| x.as_str())
                                .or_else(|| v.get("message").and_then(|x| x.as_str()))
                                .unwrap_or("provider error").to_string();
                            yield Err(HarnessError::Provider(msg));
                            return;
                        }
                        _ => {}
                    }
                }
            }
        }
        yield Ok(StreamChunk::Done { usage, stop_reason: Some("stop".into()) });
    }
}

#[async_trait]
impl Provider for OpenAiBase {
    fn kind(&self) -> &'static str {
        self.kind
    }

    async fn stream_chat(&self, mut req: ChatRequest) -> Result<ChunkStream, HarnessError> {
        if self.api_key.is_empty() {
            return Err(HarnessError::Provider("no API key configured".into()));
        }
        // The bound model wins; an empty request model is a caller oversight.
        if req.model.is_empty() {
            req.model = self.model.clone();
        }
        if req.model.is_empty() {
            return Err(HarnessError::Provider("no model id set for this request".into()));
        }
        let (url, body) = if self.kind == "responses" {
            (self.endpoint("responses"), build_responses_body(&req))
        } else {
            (self.endpoint("chat/completions"), build_chat_body(&req))
        };
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(20))
            .build()?;
        let resp = client.post(&url).headers(self.headers()).json(&body).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(HarnessError::Provider(format!(
                "HTTP {status}: {}",
                if text.len() > 1500 { &text[..1500] } else { &text }
            )));
        }
        let self_kind = self.kind;
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamChunk, HarnessError>>(64);
        if self_kind == "responses" {
            tokio::spawn(async move {
                let s = stream_responses(resp.bytes_stream());
                futures::pin_mut!(s);
                while let Some(item) = s.next().await {
                    if tx.send(item).await.is_err() {
                        break;
                    }
                }
            });
        } else {
            tokio::spawn(async move {
                let s = stream_chat_completions(resp.bytes_stream());
                futures::pin_mut!(s);
                while let Some(item) = s.next().await {
                    if tx.send(item).await.is_err() {
                        break;
                    }
                }
            });
        }
        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    async fn test(&self) -> Result<TestReport, HarnessError> {
        let t0 = std::time::Instant::now();
        if self.api_key.is_empty() {
            return Ok(TestReport {
                ok: false,
                latency_ms: 0,
                message: "API Key is empty".into(),
                models: vec![],
            });
        }
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()?;
        // The models list validates the key without spending tokens.
        let url = self.endpoint("models");
        let resp = client.get(&url).headers(self.headers()).send().await;
        let latency = t0.elapsed().as_millis() as u64;
        match resp {
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                if status.is_success() {
                    let models = serde_json::from_str::<Value>(&text)
                        .ok()
                        .and_then(|v| {
                            v.get("data")
                                .and_then(|d| d.as_array())
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|m| m["id"].as_str().map(|s| s.to_string()))
                                        .take(30)
                                        .collect::<Vec<_>>()
                                })
                        })
                        .unwrap_or_default();
                    Ok(TestReport {
                        ok: true,
                        latency_ms: latency,
                        message: "OK (key accepted)".into(),
                        models,
                    })
                } else {
                    Ok(TestReport {
                        ok: false,
                        latency_ms: latency,
                        message: format!("HTTP {status}: {}", &text[..text.len().min(400)]),
                        models: vec![],
                    })
                }
            }
            Err(e) => Ok(TestReport {
                ok: false,
                latency_ms: latency,
                message: format!("connection failed: {e}"),
                models: vec![],
            }),
        }
    }
}
