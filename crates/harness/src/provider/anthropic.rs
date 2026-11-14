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
        // 代理感知共享 client（设置 → 网络与隐私 → HTTP 代理）
        let client = crate::config::http_client();
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
        // 代理感知共享 client
        let client = crate::config::http_client();
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
        // 字节缓冲:SSE 帧边界只在字节层找,完整帧才整体解码——避免跨 chunk
        // 被切断的多字节 UTF-8 字符被 from_utf8_lossy 打碎或误判成帧尾。
        let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
        let mut usage = Usage::default();
        let mut blocks: HashMap<i64, BlockAcc> = HashMap::new();
        let mut stop_reason: Option<String> = None;

        'outer: while let Some(item) = byte_stream.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(e) => {
                    yield Err(HarnessError::Http(e));
                    return; // 出错后直接结束,不再补发 Done 掩盖错误
                }
            };
            buf.extend_from_slice(&chunk);

            // A frame is separated by a blank line.
            while let Some(end) = find_frame_boundary(&buf) {
                let raw = String::from_utf8_lossy(&buf[..end]).into_owned();
                buf.drain(..end);
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
                                Some("thinking") => {
                                    // thinking 块也要登记:后续 signature_delta
                                    // 才有落点,stop 时才知道要发 ReasoningDone。
                                    blocks.insert(index, BlockAcc {
                                        id: String::new(),
                                        name: String::new(),
                                        args: String::new(),
                                        kind: "thinking".into(),
                                    });
                                }
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
                                            // 签名写进该 thinking 块(args 字段对
                                            // thinking 块复用作签名暂存)
                                            b.args.push_str(sig);
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
                                    // 参数 JSON 解析失败不再静默成空对象:带回
                                    // 原因与原始串,让模型能看到真实问题。
                                    let args: Value = serde_json::from_str(&b.args).unwrap_or_else(|_| {
                                        json!({
                                            "__args_error": "arguments were not valid JSON",
                                            "raw": truncate(&b.args, 200),
                                        })
                                    });
                                    yield Ok(StreamChunk::ToolCallDone {
                                        id: b.id,
                                        name: b.name,
                                        args,
                                    });
                                } else {
                                    // thinking 块结束:把采集到的签名交给上层
                                    // 回填 AsstPart::Thinking.signature
                                    if !b.args.is_empty() {
                                        yield Ok(StreamChunk::ReasoningDone {
                                            signature: b.args.clone(),
                                        });
                                    }
                                }
                            }
                        }
                        "message_delta" => {
                            if let Some(u) = v.get("usage") {
                                if let Some(o) = u["output_tokens"].as_u64() {
                                    usage.output_tokens = o;
                                }
                            }
                            // stop_reason 透传给上层(end_turn / max_tokens / tool_use …)
                            if let Some(sr) = v.pointer("/delta/stop_reason").and_then(|x| x.as_str()) {
                                stop_reason = Some(sr.to_string());
                            }
                        }
                        "message_stop" => {
                            break 'outer;
                        }
                        "error" => {
                            let msg = v
                                .pointer("/error/message")
                                .and_then(|x| x.as_str())
                                .unwrap_or("stream error")
                                .to_string();
                            yield Err(HarnessError::Provider(msg));
                            // 出错后直接结束,不再补发 Done 掩盖错误
                            return;
                        }
                        _ => {}
                    }
                }
            }
        }

        // 正常收尾(只发一次 Done):透传 message_delta 的 stop_reason,
        // 缺省按 end_turn 处理。
        yield Ok(StreamChunk::Done {
            usage,
            stop_reason: Some(stop_reason.unwrap_or_else(|| "end_turn".into())),
        });
    }
}

/// 在字节缓冲里定位下一个 SSE 帧的结束位置(空行分隔:`\n\n` / `\r\n\r\n`,
/// 兼容混用行尾的 `\n\r\n`)。返回帧的总字节数(含分隔符);None 表示还没有
/// 完整帧。只在字节层扫描,跨 chunk 被切断的 UTF-8 多字节字符不会造成丢帧。
fn find_frame_boundary(b: &[u8]) -> Option<usize> {
    for i in 0..b.len() {
        if b[i] == b'\n' {
            if i + 1 < b.len() && b[i + 1] == b'\n' {
                return Some(i + 2);
            }
            if i + 3 < b.len() && b[i + 1] == b'\r' && b[i + 2] == b'\n' && b[i + 3] == b'\n' {
                return Some(i + 4);
            }
            // `\n\r\n`
            if i + 1 < b.len() && b[i + 1] == b'\r' && i + 2 < b.len() && b[i + 2] == b'\n' {
                return Some(i + 3);
            }
        }
    }
    None
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
        let end = find_frame_boundary(s.as_bytes()).unwrap();
        assert_eq!(&s[..end], "event: a\ndata: b\r\n\r\n");
    }

    #[test]
    fn frame_boundary_survives_split_utf8() {
        // “ 的 UTF-8 是 E4 B8 80,切在中间时不能误判出帧边界
        let s = "data: {\"a\":\"中";
        let bytes = s.as_bytes();
        let cut = bytes.len() - 1; // 最后一个字节是多字节字符的尾巴
        let mut buf = Vec::new();
        buf.extend_from_slice(&bytes[..cut]);
        assert!(find_frame_boundary(&buf).is_none());
        buf.extend_from_slice(&bytes[cut..]);
        assert_eq!(find_frame_boundary(&buf), None, "没有空行就没有帧");
        let framed = format!("{s}\n\n");
        let mut buf2 = framed.as_bytes().to_vec();
        // 补上不完整字节也不影响已缓冲的完整帧定位
        buf2.push(0xE4);
        let end = find_frame_boundary(&buf2).unwrap();
        assert_eq!(framed.as_bytes(), &buf2[..end]);
    }

    #[test]
    fn body_includes_tools_and_thinking() {
        let req = ChatRequest {
            model: "m".into(),
            system: "sys".into(),
            messages: vec![Msg::user("hi")],
            tools: std::sync::Arc::new(vec![ToolSpec {
                name: "read_file".into(),
                description: "read".into(),
                input_schema: json!({"type":"object"}),
            }]),
            max_tokens: 4096,
            thinking_level: 2,
            workspace: String::new(),
        };
        let body = build_body(&req, true);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert!(body["max_tokens"].as_i64().unwrap() > 4096);
    }
}
