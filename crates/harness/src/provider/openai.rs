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
use std::collections::BTreeMap;

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
    // 文本/图像延迟缓冲:tool 消息必须紧跟 assistant 的 tool_calls 且保持
    // 连续,图像插进 tool 序列中间会被 API 拒绝。缓冲在遇到非 User 消息或
    // 消息结束时统一落成一条 user 消息,位置一定在所有 tool 消息之后。
    let mut pending: Vec<Value> = Vec::new();
    for m in messages {
        match m {
            Msg::User(parts) => {
                for p in parts {
                    match p {
                        UserPart::Text(t) => pending.push(json!({"type":"text","text": t})),
                        UserPart::Image { media_type, data } => pending.push(json!({
                            "type":"image_url",
                            "image_url":{"url": format!("data:{media_type};base64,{data}")}
                        })),
                        UserPart::ToolResult { id, content, .. } => {
                            out.push(json!({"role":"tool","tool_call_id": id,"content": content}));
                        }
                    }
                }
            }
            Msg::Assistant(parts) => {
                flush_pending(&mut out, &mut pending);
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
    flush_pending(&mut out, &mut pending);
    Value::Array(out)
}

/// 把延迟缓冲的文本/图像落成一条 user 消息:全文本时合并成字符串形态以
/// 兼容中转站,含图像时用 content 数组形态。
fn flush_pending(out: &mut Vec<Value>, pending: &mut Vec<Value>) {
    if pending.is_empty() {
        return;
    }
    let content = if pending.iter().all(|b| b["type"] == "text") {
        json!(pending
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join(""))
    } else {
        json!(pending)
    };
    out.push(json!({"role":"user","content": content}));
    pending.clear();
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
    });
    // o 系列 / gpt-5 只认 max_completion_tokens,老参数会被它们直接拒绝;
    // 其余模型保持 max_tokens。
    if is_reasoning_model(&req.model) {
        body["max_completion_tokens"] = json!(req.max_tokens);
    } else {
        body["max_tokens"] = json!(req.max_tokens);
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    // thinking_level → reasoning_effort;0 = 显式关闭,不发参数。
    if let Some(effort) = reasoning_effort(req.thinking_level) {
        body["reasoning_effort"] = json!(effort);
    }
    body
}

/// thinking_level → OpenAI 的 reasoning effort 档位;0 表示显式关闭。
fn reasoning_effort(level: u32) -> Option<&'static str> {
    match level {
        1 => Some("low"),
        2 => Some("medium"),
        3 => Some("high"),
        _ => None,
    }
}

/// o 系列(`^o\d`,如 o1/o3/o4-mini)与 gpt-5 一族是推理模型,token 上限
/// 参数名与旧模型不同。
fn is_reasoning_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    if m.starts_with("gpt-5") {
        return true;
    }
    let mut chars = m.chars();
    chars.next() == Some('o') && chars.next().is_some_and(|c| c.is_ascii_digit())
}

fn sse_data_lines(buf: &str) -> Vec<String> {
    buf.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|s| s.trim_start().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 在字节缓冲里定位下一个 SSE 帧的结束位置。SSE 以空行分帧:标准 `\n\n` /
/// `\r\n\r\n`,个别中转只有 `\r\r`。返回帧总字节数(含分隔符);None 表示
/// 还没有完整帧。只在字节层扫描,跨 chunk 切断的 UTF-8 字符不会丢帧。
fn find_frame_boundary(b: &[u8]) -> Option<usize> {
    let n = b.len();
    for i in 0..n {
        match b[i] {
            b'\n' => {
                if i + 1 < n && b[i + 1] == b'\n' {
                    return Some(i + 2);
                }
                if i + 1 < n && b[i + 1] == b'\r' && i + 2 < n && b[i + 2] == b'\n' {
                    return Some(i + 3);
                }
            }
            b'\r' => {
                if i + 1 < n && b[i + 1] == b'\r' {
                    return Some(i + 2);
                }
                if i + 1 < n && b[i + 1] == b'\n' {
                    if i + 2 < n && b[i + 2] == b'\n' {
                        return Some(i + 3);
                    }
                    if i + 3 < n && b[i + 2] == b'\r' && b[i + 3] == b'\n' {
                        return Some(i + 4);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// 从字节缓冲取出所有完整帧(整体解码为字符串);不完整的字节留在缓冲,
/// 等下一个 chunk 拼上再解析。chat 与 responses 两条路径共用。
fn take_frames(buf: &mut Vec<u8>) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(end) = find_frame_boundary(buf) {
        let raw = String::from_utf8_lossy(&buf[..end]).into_owned();
        buf.drain(..end);
        out.push(raw);
    }
    out
}

/// 边界安全截断(错误信息用),与 anthropic.rs 的写法一致。
fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        let mut end = n;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…(truncated)", &s[..end])
    }
}

/// 解析工具调用参数;坏 JSON 不再静默成空对象,带回原因与原始串前 200 字节,
/// 让模型看到真实问题并自行修正。
fn parse_args(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| {
        json!({
            "__args_error": "arguments were not valid JSON",
            "raw": truncate(raw, 200),
        })
    })
}

/// 按 index 顺序取出累积的工具调用并解析参数(BTreeMap 保证升序,多调用
/// 不会乱序);缺 id 的合成稳定的 `call_{index}`。
fn flush_calls(
    calls: &mut BTreeMap<i64, (String, String, String)>,
) -> Vec<(String, String, Value)> {
    // BTreeMap 没有 drain();先摘走再清空,保序输出。
    let taken: Vec<(i64, (String, String, String))> =
        calls.iter().map(|(k, v)| (*k, v.clone())).collect();
    calls.clear();
    taken
        .into_iter()
        .map(|(idx, (mut id, name, args))| {
            if id.is_empty() {
                id = format!("call_{idx}");
            }
            (id, name, parse_args(&args))
        })
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
        let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
        let mut usage = Usage::default();
        // index -> (id, name, args);BTreeMap 让多个调用按 index 顺序收尾
        let mut calls: BTreeMap<i64, (String, String, String)> = BTreeMap::new();

        while let Some(item) = byte_stream.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(e) => { yield Err(HarnessError::Http(e)); return; }
            };
            buf.extend_from_slice(&chunk);
            for raw in take_frames(&mut buf) {
                for data in sse_data_lines(&raw) {
                    if data == "[DONE]" {
                        // 收尾:把还没 flush 的工具调用合成完成事件再结束
                        for (id, name, args) in flush_calls(&mut calls) {
                            yield Ok(StreamChunk::ToolCallDone { id, name, args });
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
                                let args_delta = tc.pointer("/function/arguments")
                                    .and_then(|x| x.as_str()).unwrap_or("");
                                let entry = calls.entry(idx).or_insert_with(|| (String::new(), String::new(), String::new()));
                                if let Some(n) = tc.pointer("/function/name").and_then(|x| x.as_str()) {
                                    if !n.is_empty() && entry.1.is_empty() {
                                        entry.1 = n.to_string();
                                    }
                                }
                                // 还没"开始"过的调用:确定 id(显式优先;缺 id 时
                                // 合成稳定的 call_{index},同轮内一致)并发开始事件。
                                // 判据是 entry.0 仍为空——写入后即不再触发。
                                let mut start_name: Option<String> = None;
                                if entry.0.is_empty() {
                                    let explicit = tc["id"].as_str().filter(|s| !s.is_empty()).map(String::from);
                                    let ready = explicit.is_some() || !entry.1.is_empty() || !args_delta.is_empty();
                                    if ready {
                                        entry.0 = explicit.unwrap_or_else(|| format!("call_{idx}"));
                                        start_name = Some(entry.1.clone());
                                    }
                                }
                                if let Some(name) = start_name {
                                    yield Ok(StreamChunk::ToolCallStart { id: entry.0.clone(), name });
                                }
                                if !args_delta.is_empty() {
                                    entry.2.push_str(args_delta);
                                    if !entry.0.is_empty() {
                                        yield Ok(StreamChunk::ToolCallArgsDelta { id: entry.0.clone(), delta: args_delta.to_string() });
                                    }
                                }
                            }
                        }
                    }
                    if let Some(finish) = choice.get("finish_reason").and_then(|x| x.as_str()) {
                        if finish == "stop" || finish == "tool_calls" {
                            for (id, name, args) in flush_calls(&mut calls) {
                                yield Ok(StreamChunk::ToolCallDone { id, name, args });
                            }
                        }
                    }
                }
            }
        }
        // 流自然结束(没有 [DONE] / finish_reason 的中转):同样补齐工具调用
        for (id, name, args) in flush_calls(&mut calls) {
            yield Ok(StreamChunk::ToolCallDone { id, name, args });
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
    // thinking_level → reasoning.effort;0 = 显式关闭,不发参数。
    if let Some(effort) = reasoning_effort(req.thinking_level) {
        body["reasoning"] = json!({"effort": effort});
    }
    body
}

fn stream_responses<S>(byte_stream: S) -> impl Stream<Item = Result<StreamChunk, HarnessError>> + Send
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    async_stream::stream! {
        futures::pin_mut!(byte_stream);
        let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
        let mut usage = Usage::default();
        // 是否已经通过 .delta 事件流出过正文:决定收尾时是否用 .done 兜底
        let mut text_seen = false;
        let mut done_texts: Vec<String> = Vec::new();
        while let Some(item) = byte_stream.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(e) => { yield Err(HarnessError::Http(e)); return; }
            };
            buf.extend_from_slice(&chunk);
            for raw in take_frames(&mut buf) {
                for data in sse_data_lines(&raw) {
                    if data == "[DONE]" {
                        if !text_seen {
                            for t in done_texts.drain(..) {
                                yield Ok(StreamChunk::Text(t));
                            }
                        }
                        yield Ok(StreamChunk::Done { usage, stop_reason: Some("stop".into()) });
                        return;
                    }
                    let v: Value = match serde_json::from_str(&data) { Ok(v) => v, Err(_) => continue };
                    let typ = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
                    match typ {
                        "response.output_text.delta" => {
                            if let Some(t) = v["delta"].as_str() {
                                if !t.is_empty() {
                                    text_seen = true;
                                    yield Ok(StreamChunk::Text(t.to_string()));
                                }
                            }
                        }
                        "response.reasoning_summary_text.delta" => {
                            if let Some(t) = v["delta"].as_str() {
                                if !t.is_empty() { yield Ok(StreamChunk::Reasoning(t.to_string())); }
                            }
                        }
                        // 兜底:不发 .delta 的中转只在 done 事件里带全文;
                        // 收尾时若全程没有 delta 再放行,避免正文重复。
                        "response.output_text.done" => {
                            if let Some(t) = v["text"].as_str() {
                                if !t.is_empty() {
                                    done_texts.push(t.to_string());
                                }
                            }
                        }
                        "response.output_item.done" => {
                            let item = &v["item"];
                            if item["type"] == "function_call" {
                                let id = item["call_id"].as_str().unwrap_or_default().to_string();
                                let name = item["name"].as_str().unwrap_or_default().to_string();
                                let args = parse_args(item["arguments"].as_str().unwrap_or("{}"));
                                yield Ok(StreamChunk::ToolCallDone { id, name, args });
                            }
                        }
                        "response.completed" => {
                            if let Some(u) = v.pointer("/response/usage") {
                                usage.input_tokens = u["input_tokens"].as_u64().unwrap_or(0);
                                usage.output_tokens = u["output_tokens"].as_u64().unwrap_or(0);
                            }
                            if !text_seen {
                                for t in done_texts.drain(..) {
                                    yield Ok(StreamChunk::Text(t));
                                }
                            }
                            yield Ok(StreamChunk::Done { usage, stop_reason: Some("stop".into()) });
                            return;
                        }
                        "response.failed" | "error" => {
                            // response.failed 把错误挂在 response.error 上,
                            // 顶层 error 事件挂在 /error 上,两处都找一遍。
                            let msg = v.pointer("/error/message").and_then(|x| x.as_str())
                                .or_else(|| v.pointer("/response/error/message").and_then(|x| x.as_str()))
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
        // 流自然结束(没有 [DONE] / response.completed):同样兜底补全文
        if !text_seen {
            for t in done_texts.drain(..) {
                yield Ok(StreamChunk::Text(t));
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
        // 代理感知共享 client（设置 → 网络与隐私 → HTTP 代理）
        let client = crate::config::http_client();
        let resp = client.post(&url).headers(self.headers()).json(&body).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            // 直接按字节切片会在 UTF-8 边界上 panic,用边界安全截断
            return Err(HarnessError::Provider(format!(
                "HTTP {status}: {}",
                truncate(&text, 1500)
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
        // 代理感知共享 client
        let client = crate::config::http_client();
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
                        message: format!("HTTP {status}: {}", truncate(&text, 400)),
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
