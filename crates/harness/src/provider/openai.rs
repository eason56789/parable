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
use std::collections::{BTreeMap, HashSet};

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
                        // 附件芯片元数据只服务界面；模型侧内容已注入 Text。
                        UserPart::File { .. } => {}
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

pub(crate) fn build_chat_body(req: &ChatRequest) -> Value {
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
    // 其余模型保持 max_tokens。None 回落到 agent 历史默认 8192(与改动前
    // 每次请求都带 8192 的行为一致)。
    let max_tokens = req.max_tokens.unwrap_or(8192);
    let reasoning_model = is_reasoning_model(&req.model);
    if reasoning_model {
        body["max_completion_tokens"] = json!(max_tokens);
    } else {
        body["max_tokens"] = json!(max_tokens);
    }
    // 温度:未配置不发送(与改动前一致);配置了才带上。o 系列 / gpt-5 一族
    // 只认默认采样参数,temperature 会被直接拒绝,推理模型一律不发。
    if let Some(t) = req.temperature {
        if !reasoning_model {
            body["temperature"] = json!(t);
        }
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

/// 解析工具调用参数;坏 JSON 不再静默成空对象,带回原因与原始串前 200 字节,
/// 让模型看到真实问题并自行修正。空参数串是合法的「无参调用」,直接给空
/// 对象,不注入错误。
fn parse_args(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return json!({});
    }
    serde_json::from_str(raw).unwrap_or_else(|_| {
        json!({
            "__args_error": "arguments were not valid JSON",
            "raw": truncate(raw, 200),
        })
    })
}

/// 按 index 顺序取出尚未 flush 的工具调用并解析参数(BTreeMap 保证升序,
/// 多调用不会乱序);缺 id 的合成稳定的 `call_{index}`。条目 flush 后留在
/// map 里并记入 `flushed`:finish_reason 之后个别中转还会补发同调用的
/// args delta,清空 map 会让它重新成条,再次 flush 就变成同 id 两条
/// ToolUse——跳过已 flush 的 index 才不会重复收尾。
fn flush_calls(
    calls: &mut BTreeMap<i64, (String, String, String)>,
    flushed: &mut HashSet<i64>,
) -> Vec<(String, String, Value)> {
    let mut out = Vec::new();
    for (idx, entry) in calls.iter_mut() {
        if !flushed.insert(*idx) {
            continue;
        }
        if entry.0.is_empty() {
            entry.0 = format!("call_{idx}");
        }
        out.push((entry.0.clone(), entry.1.clone(), parse_args(&entry.2)));
    }
    out
}

/// chat-completions 的 SSE 解析。错误流统一为 `HarnessError`，好让
/// OpenCode 免费通道（wreq 传输）与 reqwest 传输共用同一套解析。
pub(crate) fn stream_chat_completions<S>(
    byte_stream: S,
) -> impl Stream<Item = Result<StreamChunk, HarnessError>> + Send
where
    S: Stream<Item = Result<Bytes, HarnessError>> + Send + 'static,
{
    async_stream::stream! {
        futures::pin_mut!(byte_stream);
        let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
        let mut usage = Usage::default();
        // index -> (id, name, args);BTreeMap 让多个调用按 index 顺序收尾
        let mut calls: BTreeMap<i64, (String, String, String)> = BTreeMap::new();
        // 已 flush 过的 index:finish_reason 收尾后迟到的 args delta 不再
        // 触发第二次 ToolCallDone(同 id 两条 ToolUse 的根源)。
        let mut flushed: HashSet<i64> = HashSet::new();
        // finish_reason 透传(stop/tool_calls/length/content_filter…),
        // Done 时带上;没有 finish_reason 的中转缺省按 stop 处理。
        let mut stop_reason: Option<String> = None;
        // 多 choice 警告只记一次,免得每个 chunk 都刷日志。
        let mut warned_multi_choice = false;

        while let Some(item) = byte_stream.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(e) => { yield Err(e); return; }
            };
            buf.extend_from_slice(&chunk);
            for raw in take_frames(&mut buf) {
                for data in sse_data_lines(&raw) {
                    if data == "[DONE]" {
                        // 收尾:把还没 flush 的工具调用合成完成事件再结束
                        for (id, name, args) in flush_calls(&mut calls, &mut flushed) {
                            yield Ok(StreamChunk::ToolCallDone { id, name, args });
                        }
                        yield Ok(StreamChunk::Done {
                            usage,
                            stop_reason: Some(stop_reason.unwrap_or_else(|| "stop".into())),
                        });
                        return;
                    }
                    let v: Value = match serde_json::from_str(&data) { Ok(v) => v, Err(_) => continue };
                    if let Some(u) = v.get("usage") {
                        usage.input_tokens = u["prompt_tokens"].as_u64().unwrap_or(0);
                        usage.output_tokens = u["completion_tokens"].as_u64().unwrap_or(0);
                        usage.cache_read_input_tokens =
                            u.pointer("/prompt_tokens_details/cached_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                    }
                    if !warned_multi_choice {
                        if let Some(choices) = v.get("choices").and_then(|c| c.as_array()) {
                            if choices.len() > 1 {
                                warned_multi_choice = true;
                                tracing::warn!(
                                    "chat-completions 流返回了 {} 个 choices,只解析第一个",
                                    choices.len()
                                );
                            }
                        }
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
                            for (id, name, args) in flush_calls(&mut calls, &mut flushed) {
                                yield Ok(StreamChunk::ToolCallDone { id, name, args });
                            }
                        }
                        // length / content_filter 等原样透传,上层据此判断轮次
                        // 是被截断还是被拦,不再一律报 stop。
                        stop_reason = Some(finish.to_string());
                    }
                }
            }
        }
        // 流自然结束(没有 [DONE] / finish_reason 的中转):同样补齐工具调用
        for (id, name, args) in flush_calls(&mut calls, &mut flushed) {
            yield Ok(StreamChunk::ToolCallDone { id, name, args });
        }
        yield Ok(StreamChunk::Done {
            usage,
            stop_reason: Some(stop_reason.unwrap_or_else(|| "stop".into())),
        });
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
                        // 附件芯片元数据只服务界面；模型侧内容已注入 Text。
                        UserPart::File { .. } => {}
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
    // max_output_tokens:Responses API 此前从不发送(默认给到模型上限),
    // 保持 None = 不发送;配置了 max_tokens 才带上,避免静默收窄输出。
    if let Some(mt) = req.max_tokens {
        body["max_output_tokens"] = json!(mt);
    }
    // 温度:None 不发送(与改动前一致);o 系列 / gpt-5 只认默认采样,推理
    // 模型一律不发,免得被 API 直接拒绝。
    if let Some(t) = req.temperature {
        if !is_reasoning_model(&req.model) {
            body["temperature"] = json!(t);
        }
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
        // 代理感知共享 client（设置 → 网络与隐私 → HTTP 代理）。
        // 回环地址走 300ms 快速失败 client：本地服务没起时秒级进「正在重连」，
        // 不等满 10s（见 config::is_loopback_base_url）。
        let client = crate::config::http_client_for(&url);
        // 429/5xx 退避重试:此时尚未产生任何流输出、请求体固定,重发是安全的;
        // 拿到成功响应(或不可重试的失败)后循环结束,流式阶段不再重试。
        // 传输错误(连接拒绝/DNS)不在此层重试——agent 层按 1s 间隔统一重连。
        let mut attempt = 0u32;
        let resp = loop {
            let sent = client
                .post(&url)
                .headers(self.headers())
                .json(&body)
                .send()
                .await;
            match sent {
                Ok(r) => {
                    let status = r.status();
                    if !status.is_success() {
                        let retry_after = retry_after_ms(&r);
                        let text = r.text().await.unwrap_or_default();
                        if retryable_status(status) && attempt < MAX_RETRIES {
                            attempt += 1;
                            let delay = retry_after.unwrap_or_else(|| backoff_delay(attempt - 1));
                            tracing::warn!(
                                "openai: HTTP {status}(第 {attempt}/{MAX_RETRIES} 次重试,{}ms 后)",
                                delay.as_millis()
                            );
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                        // 直接按字节切片会在 UTF-8 边界上 panic,用边界安全截断
                        return Err(HarnessError::Provider(format!(
                            "HTTP {status}: {}",
                            truncate(&text, 1500)
                        )));
                    }
                    break r;
                }
                // 传输错误（连接拒绝/失败/DNS）不在这一层重试：agent 层的
                // 「正在重连 x/10」按 1s 间隔统一接管，两层叠加会让重连间隔
                // 变成 退避+1s 的好几秒。429/5xx 的带 Retry-After 退避保留。
                Err(e) => return Err(HarnessError::Http(e)),
            }
        };
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
                let s = stream_chat_completions(
                    resp.bytes_stream().map(|r| r.map_err(HarnessError::Http)),
                );
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
        // 代理感知共享 client；回环地址走 300ms 快速失败（同 stream 路径）。
        // The models list validates the key without spending tokens.
        let url = self.endpoint("models");
        let client = crate::config::http_client_for(&url);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(model: &str, max_tokens: Option<u32>, temperature: Option<f32>) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            system: String::new(),
            messages: vec![Msg::user("hi")],
            tools: std::sync::Arc::new(Vec::new()),
            max_tokens,
            temperature,
            thinking_level: 0,
            workspace: String::new(),
        }
    }

    /// chat 路径:max_tokens None 回落 8192(与改动前一致),有值时透传;
    /// temperature 只有配置了才发送。
    #[test]
    fn chat_body_applies_sampling_params() {
        let body = build_chat_body(&mk("gpt-4o", None, None));
        assert_eq!(body["max_tokens"], json!(8192));
        assert!(body.get("temperature").is_none());

        let body = build_chat_body(&mk("gpt-4o", Some(2048), Some(0.3)));
        assert_eq!(body["max_tokens"], json!(2048));
        // f32 经 serde_json 以 f64 表示，期望值同样按 f32 构造。
        assert_eq!(body["temperature"], json!(0.3_f32));

        // 推理模型:参数名换成 max_completion_tokens,temperature 一律不发。
        let body = build_chat_body(&mk("o4-mini", Some(1024), Some(0.3)));
        assert_eq!(body["max_completion_tokens"], json!(1024));
        assert!(body.get("max_tokens").is_none());
        assert!(body.get("temperature").is_none());
    }

    /// 回环快速失败：本地服务没监听时，stream_chat 必须在 ~1s 内失败
    /// （300ms 回环连接超时，见 config::is_loopback_base_url），让 agent 层
    /// 的「正在重连 x/10」按 1s 间隔跑起来——此前连接层内部还有一层退避
    /// 重试，两层叠加导致首拒 13s、重连间隔远超 1s（用户实测回归）。
    #[tokio::test]
    async fn loopback_refused_fails_fast() {
        // 先绑定再释放，拿到一个必然拒绝连接的空闲回环端口。
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let cfg: ProviderConfig = serde_json::from_value(serde_json::json!({
            "id": "local", "name": "local", "api": "openai", "key": "k",
            "base_url": format!("http://127.0.0.1:{port}/v1"),
            "models": [{"id": "m1"}]
        }))
        .unwrap();
        let p = chat(&cfg);
        let t0 = std::time::Instant::now();
        let r = <OpenAiBase as crate::provider::Provider>::stream_chat(&p, mk("m1", None, None)).await;
        let elapsed = t0.elapsed();
        assert!(r.is_err(), "连接已关闭的本地端口必须失败");
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "loopback 拒绝应在 2s 内失败（300ms 连接超时），实际 {elapsed:?}"
        );
    }

    /// responses 路径:此前从不发送输出上限,None 必须保持不发送;配置了
    /// 才带 max_output_tokens。
    #[test]
    fn responses_body_applies_sampling_params() {
        let body = build_responses_body(&mk("gpt-4o", None, None));
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("temperature").is_none());

        let body = build_responses_body(&mk("gpt-4o", Some(4096), Some(0.7)));
        assert_eq!(body["max_output_tokens"], json!(4096));
        assert_eq!(body["temperature"], json!(0.7_f32));
    }
}
