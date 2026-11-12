//! Anthropic Messages API streaming (`/v1/messages`).
//!
//! Works against api.anthropic.com and against Anthropic-compatible relays
//! (DeepSeek `/anthropic`, Kimi, GLM, and public proxies), which share the wire
//! format but vary in how much of `thinking` / `cache_control` they accept.

use super::*;
use crate::config::ProviderConfig;
use crate::token::Usage;
use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;

pub struct AnthropicProvider {
    base_url: String,
    api_key: String,
    model: String,
    pub thinking_supported: bool,
}

impl AnthropicProvider {
    pub fn new(cfg: &ProviderConfig) -> Self {
        Self {
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: cfg.key.clone(),
            model: String::new(),
            thinking_supported: false,
        }
    }

    /// Bind a concrete model id for the next call.
    pub fn with_model(mut self, model: &str, thinking_supported: bool) -> Self {
        self.model = model.to_string();
        self.thinking_supported = thinking_supported;
        self
    }

    fn url(&self) -> String {
        if self.base_url.ends_with("/v1/messages") {
            self.base_url.clone()
        } else if self.base_url.ends_with("/v1") {
            format!("{}/messages", self.base_url)
        } else {
            format!("{}/v1/messages", self.base_url)
        }
    }

    fn headers(&self) -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            "x-api-key",
            self.api_key
                .parse()
                .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static("")),
        );
        h.insert("anthropic-version", reqwest::header::HeaderValue::from_static("2023-06-01"));
        h.insert("content-type", reqwest::header::HeaderValue::from_static("application/json"));
        h
    }
}

pub fn build_messages(messages: &[Msg]) -> Value {
    let mut out = Vec::new();
    for m in messages {
        match m {
            Msg::User(parts) => {
                let content: Vec<Value> = parts
                    .iter()
                    .map(|p| match p {
                        UserPart::Text(t) => json!({"type":"text","text": t}),
                        UserPart::Image { media_type, data } => json!({
                            "type":"image",
                            "source":{"type":"base64","media_type": media_type,"data": data}
                        }),
                        UserPart::ToolResult { id, content, is_error } => {
                            let mut v = json!({"type":"tool_result","tool_use_id": id,"content": content});
                            if *is_error {
                                v["is_error"] = json!(true);
                            }
                            v
                        }
                    })
                    .collect();
                out.push(json!({"role":"user","content": content}));
            }
            Msg::Assistant(parts) => {
                let mut content = Vec::new();
                for p in parts {
                    match p {
                        AsstPart::Text(t) => {
                            if !t.is_empty() {
                                content.push(json!({"type":"text","text": t}));
                            }
                        }
                        AsstPart::Thinking { text, signature } => {
                            // Thinking blocks must be replayed with their signature.
                            // If the relay never issued one we drop the block instead
                            // of sending an invalid payload.
                            if !signature.is_empty() {
                                content.push(json!({
                                    "type":"thinking","thinking": text,"signature": signature
                                }));
                            }
                        }
                        AsstPart::ToolUse { id, name, args } => {
                            content.push(json!({"type":"tool_use","id": id,"name": name,"input": args}));
                        }
                    }
                }
                if content.is_empty() {
                    content.push(json!({"type":"text","text":""}));
                }
                out.push(json!({"role":"assistant","content": content}));
            }
        }
    }
    Value::Array(out)
}

pub fn build_body(req: &ChatRequest, thinking_supported: bool) -> Value {
    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.input_schema,
            })
        })
        .collect();

    let mut max_tokens = req.max_tokens.max(1024);
    let mut body = json!({
        "model": req.model,
        "max_tokens": max_tokens,
        "messages": build_messages(&req.messages),
        "stream": true,
    });
    if !req.system.is_empty() {
        body["system"] = json!(req.system);
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    if req.thinking_level > 0 && thinking_supported {
        let budget = match req.thinking_level {
            1 => 1024u32,
            2 => 4096,
            _ => 8192,
        };
        if max_tokens <= budget {
            max_tokens = budget * 2;
            body["max_tokens"] = json!(max_tokens);
        }
        body["thinking"] = json!({"type":"enabled","budget_tokens": budget});
        // With thinking enabled the API requires temperature to be unset or 1.
    } else {
        body["temperature"] = json!(1.0);
    }
    body
}

/// One parsed SSE frame.
struct Frame {
    event: String,
    data: String,
}

fn parse_frames(buf: &str) -> Vec<Frame> {
    let mut out = Vec::new();
    let mut event = String::new();
    let mut data = String::new();
    for line in buf.lines() {
        if line.starts_with(':') {
            continue; // comment / keepalive
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event = rest.trim_start().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
    }
    if !data.is_empty() {
        out.push(Frame {
            event,
            data,
        });
    }
    out
}

struct BlockAcc {
    id: String,
    name: String,
    args: String,
    kind: String,
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn kind(&self) -> &'static str {
        "anthropic"
    }

    async fn stream_chat(&self, mut req: ChatRequest) -> Result<ChunkStream, HarnessError> {
        if self.api_key.is_empty() {
            return Err(HarnessError::Provider(
                "no API key configured for this provider".into(),
            ));
        }
        if req.model.is_empty() {
            req.model = self.model.clone();
        }
        if req.model.is_empty() {
            return Err(HarnessError::Provider("no model id set for this request".into()));
        }
        let body = build_body(&req, self.thinking_supported);
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(20))
            .build()?;
        let resp = client
            .post(self.url())
            .headers(self.headers())
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let text = truncate(&text, 2000);
            return Err(HarnessError::Provider(format!("HTTP {status}: {text}")));
        }

        let byte_stream = resp.bytes_stream();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamChunk, HarnessError>>(64);
        tokio::spawn(async move {
            let s = stream_chunks(byte_stream);
            futures::pin_mut!(s);
            while let Some(item) = s.next().await {
                if tx.send(item).await.is_err() {
                    break;
                }
            }
        });
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
        let body = json!({
            "model": self.model,
            "max_tokens": 1,
            "messages": [{"role":"user","content":"ping"}],
        });
        let resp = client
            .post(self.url())
            .headers(self.headers())
            .json(&body)
            .send()
            .await;
        let latency = t0.elapsed().as_millis() as u64;
        match resp {
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                if status.is_success() {
                    let models = serde_json::from_str::<Value>(&text)
                        .ok()
                        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(|s| s.to_string()))
                        .into_iter()
                        .collect();
                    Ok(TestReport {
                        ok: true,
                        latency_ms: latency,
                        message: format!("OK (model: {})", self.model),
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

fn stream_chunks<S>(byte_stream: S) -> impl Stream<Item = Result<StreamChunk, HarnessError>> + Send
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    async_stream::stream! {
        futures::pin_mut!(byte_stream);
        let mut buf = String::new();
        let mut usage = Usage::default();
        let mut blocks: HashMap<i64, BlockAcc> = HashMap::new();
        let mut done = false;

        'outer: while let Some(item) = byte_stream.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(e) => {
                    yield Err(HarnessError::Http(e));
                    break;
                }
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));

            // A frame is separated by a blank line.
            while let Some(pos) = find_frame_boundary(&buf) {
                let raw = buf[..pos.end].to_string();
                buf.drain(..pos.end);
                for frame in parse_frames(&raw) {
                    let data = frame.data.trim();
                    if data.is_empty() {
                        continue;
                    }
                    let v: Value = match serde_json::from_str(data) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let ev = frame.event.as_str();
                    match ev {
                        "message_start" => {
                            if let Some(u) = v.pointer("/message/usage") {
                                usage.input_tokens = u["input_tokens"].as_u64().unwrap_or(0);
                                usage.output_tokens = u["output_tokens"].as_u64().unwrap_or(0);
                                usage.cache_read_input_tokens =
                                    u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                                usage.cache_creation_input_tokens =
                                    u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                            }
                        }
                        "content_block_start" => {
                            let index = v["index"].as_i64().unwrap_or(0);
                            let cb = &v["content_block"];
                            match cb["type"].as_str() {
                                Some("tool_use") => {
                                    let id = cb["id"].as_str().unwrap_or_default().to_string();
                                    let name = cb["name"].as_str().unwrap_or_default().to_string();
                                    blocks.insert(index, BlockAcc {
                                        id: id.clone(),
                                        name: name.clone(),
                                        args: String::new(),
                                        kind: "tool".into(),
                                    });
                                    yield Ok(StreamChunk::ToolCallStart { id, name });
                                }
                                Some("thinking") => {}
                                _ => {}
                            }
                        }
                        "content_block_delta" => {
                            let index = v["index"].as_i64().unwrap_or(0);
                            let d = &v["delta"];
                            match d["type"].as_str() {
                                Some("text_delta") => {
                                    if let Some(t) = d["text"].as_str() {
                                        if !t.is_empty() {
                                            yield Ok(StreamChunk::Text(t.to_string()));
                                        }
                                    }
                                }
                                Some("thinking_delta") => {
                                    if let Some(t) = d["thinking"].as_str() {
                                        if !t.is_empty() {
                                            yield Ok(StreamChunk::Reasoning(t.to_string()));
                                        }
                                    }
                                }
                                Some("signature_delta") => {
                                    if let Some(sig) = d["signature"].as_str() {
                                        if let Some(b) = blocks.get_mut(&index) {
                                            b.args.push_str(sig); // reuse field for signature
                                            b.kind = "thinking".into();
                                        }
                                    }
                                }
                                Some("input_json_delta") => {
                                    if let Some(p) = d["partial_json"].as_str() {
                                        if let Some(b) = blocks.get_mut(&index) {
                                            b.args.push_str(p);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        "content_block_stop" => {
                            let index = v["index"].as_i64().unwrap_or(0);
                            if let Some(b) = blocks.remove(&index) {
                                if b.kind == "tool" {
                                    let args: Value = serde_json::from_str(&b.args)
                                        .unwrap_or_else(|_| json!({}));
                                    yield Ok(StreamChunk::ToolCallDone {
                                        id: b.id,
                                        name: b.name,
                                        args,
                                    });
                                } else {
                                    // thinking block finished: signature already folded in
                                    yield Ok(StreamChunk::Reasoning(String::new()));
                                }
                            }
                        }
                        "message_delta" => {
                            if let Some(u) = v.get("usage") {
                                if let Some(o) = u["output_tokens"].as_u64() {
                                    usage.output_tokens = o;
                                }
                            }
                            if let Some(sr) = v.pointer("/delta/stop_reason").and_then(|x| x.as_str()) {
                                let _ = sr;
                            }
                        }
                        "message_stop" => {
                            done = true;
                            break 'outer;
                        }
                        "error" => {
                            let msg = v
                                .pointer("/error/message")
                                .and_then(|x| x.as_str())
                                .unwrap_or("stream error")
                                .to_string();
                            yield Err(HarnessError::Provider(msg));
                            break 'outer;
                        }
                        _ => {}
                    }
                }
            }
        }

        if !done {
            yield Ok(StreamChunk::Done {
                usage,
                stop_reason: Some("end_turn".into()),
            });
        } else {
            yield Ok(StreamChunk::Done {
                usage,
                stop_reason: Some("end_turn".into()),
            });
        }
    }
}

/// Locate the end of the next SSE frame: a blank line (`\n\n` or `\r\n\r\n`).
fn find_frame_boundary(s: &str) -> Option<Boundary> {
    let b = s.as_bytes();
    for i in 0..b.len() {
        if b[i] == b'\n' {
            if i + 1 < b.len() && b[i + 1] == b'\n' {
                return Some(Boundary { end: i + 2 });
            }
            if i + 3 < b.len() && b[i + 1] == b'\r' && b[i + 2] == b'\n' && b[i + 3] == b'\n' {
                return Some(Boundary { end: i + 4 });
            }
            if i + 1 < b.len() && b[i + 1] == b'\r' {
                // `\n\r\n`
                if i + 2 < b.len() && b[i + 2] == b'\n' {
                    return Some(Boundary { end: i + 3 });
                }
            }
        }
    }
    None
}

struct Boundary {
    end: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frames() {
        let raw = "event: content_block_delta\ndata: {\"x\":1}\n\n";
        let frames = parse_frames(raw);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, "content_block_delta");
    }

    #[test]
    fn frame_boundary_crlf() {
        let s = "event: a\ndata: b\r\n\r\nrest";
        let b = find_frame_boundary(s).unwrap();
        assert_eq!(&s[..b.end], "event: a\ndata: b\r\n\r\n");
    }

    #[test]
    fn body_includes_tools_and_thinking() {
        let req = ChatRequest {
            model: "m".into(),
            system: "sys".into(),
            messages: vec![Msg::user("hi")],
            tools: vec![ToolSpec {
                name: "read_file".into(),
                description: "read".into(),
                input_schema: json!({"type":"object"}),
            }],
            max_tokens: 4096,
            thinking_level: 2,
        };
        let body = build_body(&req, true);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert!(body["max_tokens"].as_i64().unwrap() > 4096);
    }
}
