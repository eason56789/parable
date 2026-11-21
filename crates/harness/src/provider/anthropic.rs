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
                    // 附件芯片元数据只服务界面；模型侧内容已注入 Text。
                    .filter(|p| !matches!(p, UserPart::File { .. }))
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
                        UserPart::File { .. } => unreachable!("filtered above"),
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
                        AsstPart::Thinking { text, signature, .. } => {
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
                    // 折叠后一条内容都不剩（比如 thinking 块因缺 signature 被
                    // 丢弃、空 Text 被过滤）：整条跳过而不是发空 text 块——
                    // 真实 API 会把 `{"type":"text","text":""}` 判 400。
                    // 对重放语义这是最安全的落点：空 assistant 轮不携带任何
                    // 信息，跳过它只少一个空轮；Anthropic 对连续同角色消息
                    // 会自动合并，不会破坏后续 tool_result 的 user 轮。
                    continue;
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

    // max_tokens：None 回落到 agent 历史默认 8192（与改动前行为一致），
    // 配置了就用配置值；1024 是 API 的硬下限。
    let mut max_tokens = req.max_tokens.unwrap_or(8192).max(1024);
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
        // With thinking enabled the API requires temperature to be unset or 1:
        // 即使配置了 temperature 也保持不发送（None 与 Some(x) 行为一致）。
    } else {
        // 温度：未配置保持既有默认 1.0；配置了才覆盖。
        body["temperature"] = json!(req.temperature.unwrap_or(1.0));
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
        // 429/5xx/网络错误退避重试:此时尚未产生任何流输出、请求体固定,
        // 重发是安全的;拿到成功响应(或不可重试的失败)后循环结束,流式
        // 阶段不再重试。
        let mut attempt = 0u32;
        let resp = loop {
            let sent = client
                .post(self.url())
                .headers(self.headers())
                .json(&body)
                .send()
                .await;
            match sent {
                Ok(r) => {
                    let status = r.status();
                    // 成功响应原样交给流式解析;错误体在这里读完,判重试。
                    if status.is_success() {
                        break r;
                    }
                    let retry_after = retry_after_ms(&r);
                    let text = r.text().await.unwrap_or_default();
                    // Anthropic 过载时错误体 type 是 overloaded_error,即使
                    // 状态码不在常规列表里也值得退避重试。
                    let overloaded = serde_json::from_str::<Value>(&text)
                        .ok()
                        .and_then(|v| {
                            v.pointer("/error/type")
                                .and_then(|t| t.as_str())
                                .map(String::from)
                        })
                        .is_some_and(|t| t == "overloaded_error");
                    if (retryable_status(status) || overloaded) && attempt < MAX_RETRIES {
                        attempt += 1;
                        let delay = retry_after.unwrap_or_else(|| backoff_delay(attempt - 1));
                        tracing::warn!(
                            "anthropic: HTTP {status}(第 {attempt}/{MAX_RETRIES} 次重试,{}ms 后)",
                            delay.as_millis()
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(HarnessError::Provider(format!(
                        "HTTP {status}: {}",
                        truncate(&text, 2000)
                    )));
                }
                Err(e) if attempt < MAX_RETRIES => {
                    attempt += 1;
                    let delay = backoff_delay(attempt - 1);
                    tracing::warn!(
                        "anthropic: 网络错误 {e}(第 {attempt}/{MAX_RETRIES} 次重试,{}ms 后)",
                        delay.as_millis()
                    );
                    tokio::time::sleep(delay).await;
                }
                Err(e) => return Err(HarnessError::Http(e)),
            }
        };

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

// ---------------------------------------------------------------- 退避重试

/// 首字节前失败的最大重试次数(429/5xx/网络错误)。
const MAX_RETRIES: u32 = 3;

/// 可重试状态码:429(限流)与 500/502/503/529(上游过载/网关故障);
/// 其余 4xx 是请求本身的问题,重发也是同样结果,不重试。
fn retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 500 | 502 | 503 | 529)
}

/// 退避时长:800ms * 2^n 封顶 8s,再加 0~250ms 抖动(同 opencode_free 的
/// 写法,用毫秒时钟低位当抖动源)。
fn backoff_delay(retry: u32) -> std::time::Duration {
    let base = (800u64 << retry.min(4)).min(8_000);
    let jitter = crate::event::now_ms() % 250;
    std::time::Duration::from_millis(base + jitter)
}

/// 解析 Retry-After 头(只认秒数形态;HTTP 日期形态回落到常规退避)。
fn retry_after_ms(resp: &reqwest::Response) -> Option<std::time::Duration> {
    let v = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(std::time::Duration::from_millis(v.saturating_mul(1000)))
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
                                    // 空参数串是合法的「无参调用」,直接给空对象;
                                    // 只有非空且解析失败才注入 __args_error,
                                    // 带回原因与原始串,让模型看到真实问题。
                                    let args: Value = if b.args.trim().is_empty() {
                                        json!({})
                                    } else {
                                        serde_json::from_str(&b.args).unwrap_or_else(|_| {
                                            json!({
                                                "__args_error": "arguments were not valid JSON",
                                                "raw": truncate(&b.args, 200),
                                            })
                                        })
                                    };
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

/// 在字节缓冲里定位下一个 SSE 帧的结束位置。SSE 以空行分帧:标准 `\n\n` /
/// `\r\n\r\n`,个别中转只有 `\r\r`。返回帧总字节数(含分隔符);None 表示
/// 还没有完整帧。只在字节层扫描,跨 chunk 被切断的 UTF-8 字符不会丢帧。
/// (与 openai.rs 的实现保持一致。)
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
            max_tokens: Some(4096),
            temperature: None,
            thinking_level: 2,
            workspace: String::new(),
        };
        let body = build_body(&req, true);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert!(body["max_tokens"].as_i64().unwrap() > 4096);
    }

    /// max_tokens / temperature 透传：None 走既有默认（8192 / 1.0），有值
    /// 时原样进请求体。
    #[test]
    fn body_applies_configured_sampling_params() {
        let mk = |max_tokens: Option<u32>, temperature: Option<f32>, thinking: u32| ChatRequest {
            model: "m".into(),
            system: String::new(),
            messages: vec![Msg::user("hi")],
            tools: std::sync::Arc::new(Vec::new()),
            max_tokens,
            temperature,
            thinking_level: thinking,
            workspace: String::new(),
        };
        // 默认值：行为与改动前完全一致。
        let body = build_body(&mk(None, None, 0), true);
        assert_eq!(body["max_tokens"], json!(8192));
        assert_eq!(body["temperature"], json!(1.0));
        // 有值：透传（f32 经 serde_json 以 f64 表示，期望值同样按 f32 构造）。
        let body = build_body(&mk(Some(2048), Some(0.3), 0), true);
        assert_eq!(body["max_tokens"], json!(2048));
        assert_eq!(body["temperature"], json!(0.3_f32));
        // 思考模式：温度不发送（API 要求思考开启时温度缺省或 1），max_tokens 仍生效。
        let body = build_body(&mk(Some(16384), Some(0.3), 1), true);
        assert_eq!(body["max_tokens"], json!(16384));
        assert!(body.get("temperature").is_none());
    }

    /// 折叠后空内容的 assistant 消息整条跳过：真实 API 会把空 text 块判 400，
    /// 旧行为（补 `{"type":"text","text":""}`）在线上必炸。
    #[test]
    fn empty_assistant_messages_are_skipped() {
        let msgs = vec![
            Msg::user("hi"),
            // 只有 thinking 且无 signature → 折叠后为空。
            Msg::Assistant(vec![AsstPart::Thinking {
                text: "hidden".into(),
                signature: String::new(),
                ms: 0,
            }]),
            Msg::user("next"),
            // 完全空的 assistant。
            Msg::Assistant(vec![]),
            Msg::assistant("final"),
        ];
        let out = build_messages(&msgs);
        let arr = out.as_array().unwrap();
        assert_eq!(arr.len(), 3, "两个空 assistant 轮都应被跳过");
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[1]["role"], "user");
        assert_eq!(arr[2]["role"], "assistant");
        assert_eq!(arr[2]["content"][0]["text"], "final");
    }
}
