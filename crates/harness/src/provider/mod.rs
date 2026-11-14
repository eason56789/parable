//! LLM providers. `neutral` is the conversation format the agent loop keeps;
//! each provider converts it to its own wire format and back.

pub mod anthropic;
pub mod openai;
pub mod opencode_free;

use crate::error::HarnessError;
use crate::token::Usage;
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::pin::Pin;

// ---------------------------------------------------------------- neutral types

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UserPart {
    Text(String),
    /// Base64 image for vision (computer use screenshots).
    Image {
        media_type: String,
        data: String,
    },
    ToolResult {
        id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AsstPart {
    Text(String),
    Thinking {
        text: String,
        /// Provider signature, required when replaying thinking blocks.
        #[serde(default)]
        signature: String,
    },
    ToolUse {
        id: String,
        name: String,
        args: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Msg {
    User(Vec<UserPart>),
    Assistant(Vec<AsstPart>),
}

impl Msg {
    pub fn user(text: impl Into<String>) -> Self {
        Msg::User(vec![UserPart::Text(text.into())])
    }
    pub fn assistant(text: impl Into<String>) -> Self {
        Msg::Assistant(vec![AsstPart::Text(text.into())])
    }

    /// Rough token estimate for this message.
    pub fn estimate(&self) -> usize {
        match self {
            Msg::User(parts) => parts
                .iter()
                .map(|p| match p {
                    UserPart::Text(t) => crate::token::estimate(t),
                    // 视觉图按固定价估算（截图约一张 ~1100 tok 量级）。
                    UserPart::Image { .. } => 1100,
                    UserPart::ToolResult { content, .. } => crate::token::estimate(content) + 8,
                })
                .sum(),
            Msg::Assistant(parts) => parts
                .iter()
                .map(|p| match p {
                    AsstPart::Text(t) => crate::token::estimate(t),
                    AsstPart::Thinking { text, .. } => crate::token::estimate(text) + 8,
                    AsstPart::ToolUse { args, .. } => crate::token::estimate(&args.to_string()) + 8,
                })
                .sum(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Msg>,
    /// Tool schemas. Shared behind an `Arc` so the per-step clone in the
    /// agent loop does not copy 19 JSON schemas every time.
    pub tools: std::sync::Arc<Vec<ToolSpec>>,
    pub max_tokens: u32,
    /// 思考档位:0 = 显式关闭(不发送任何 thinking/reasoning 参数),1 = low,
    /// 2 = medium,3 = high。各 provider 自行映射到线上协议的具体字段。
    pub thinking_level: u32,
    /// 工作区目录（wire 无关的元数据；OpenCode 免费通道拿它拼 `<env>` 块，
    /// 其他 provider 忽略）。
    pub workspace: String,
}

/// One item coming back from a streaming completion.
#[derive(Debug, Clone)]
pub enum StreamChunk {
    Text(String),
    Reasoning(String),
    /// 思考流结束:携带 provider 的 thinking 签名,上层回填到
    /// `AsstPart::Thinking.signature`,Anthropic 回放时必需。
    ReasoningDone {
        signature: String,
    },
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallArgsDelta {
        id: String,
        delta: String,
    },
    ToolCallDone {
        id: String,
        name: String,
        args: Value,
    },
    Done {
        usage: Usage,
        stop_reason: Option<String>,
    },
}

pub type ChunkStream = Pin<Box<dyn Stream<Item = Result<StreamChunk, HarnessError>> + Send>>;

#[async_trait]
pub trait Provider: Send + Sync {
    /// Human name, e.g. "anthropic".
    fn kind(&self) -> &'static str;
    async fn stream_chat(&self, req: ChatRequest) -> Result<ChunkStream, HarnessError>;
    /// Cheap liveness check against the configured endpoint.
    async fn test(&self) -> Result<TestReport, HarnessError>;
}

#[derive(Debug, Clone, Serialize)]
pub struct TestReport {
    pub ok: bool,
    pub latency_ms: u64,
    pub message: String,
    pub models: Vec<String>,
}

/// Build a provider instance from config.
pub fn build(cfg: &crate::config::ProviderConfig) -> Result<Box<dyn Provider>, HarnessError> {
    match cfg.api.as_str() {
        "anthropic" => Ok(Box::new(anthropic::AnthropicProvider::new(cfg))),
        "openai" => Ok(Box::new(openai::chat(cfg))),
        "responses" => Ok(Box::new(openai::responses(cfg))),
        "opencode-free" => Ok(Box::new(opencode_free::OpenCodeFree::new(cfg))),
        other => Err(HarnessError::Config(format!("unknown api format: {other}"))),
    }
}

/// Estimate tokens for the whole request, used for the context meter.
pub fn estimate_request(req: &ChatRequest) -> (usize, usize, usize) {
    let system = crate::token::estimate(&req.system);
    let tools: usize = req
        .tools
        .iter()
        .map(|t| crate::token::estimate(&t.description) + crate::token::estimate(&t.input_schema.to_string()) + 12)
        .sum();
    let msgs: usize = req.messages.iter().map(|m| m.estimate()).sum();
    (system, tools, msgs)
}
