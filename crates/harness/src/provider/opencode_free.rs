//! OpenCode 免费通道 provider：匿名直连 `opencode.ai/inference` 上的免费模型。
//!
//! 这条通道有一套「是不是真 OpenCode 客户端」的门禁，2026-11-14 实测（单变量
//! A/B 探针）确认四道硬门，任何一个不满足一律 429 `FreeUsageLimitError`
//! （注意：429 是通用拒绝文案，不一定是额度耗尽）：
//!
//! 1. **TLS ClientHello JA4** 必须是 Bun/BoringSSL 密码表（cipher hash
//!    `5b57614c22b0`）——reqwest/rustls 的指纹直接被拒，必须用 wreq +
//!    `cipher_list` 伪装（wreq-js 的 Rust 内核，同款参数）。
//! 2. **`stream: true`**——非流式一律拒。
//! 3. **工具名 `read` + `shell` 同时在场**——查的是工具名；描述/参数实测
//!    不比对（但按上游「严格窗口」的行为记录，定义仍用逐字官方文本保险）。
//! 4. **system 提示词含 `<env>` 标记**——块内容不比对。
//!
//! 软门（实测不查，但真实 CLI 与 9router 的线上观测记录过严格窗口会查，
//! 全部按官方行为实现，跨窗口存活）：
//! - session id 编码 `ses_<12hex><14base62>`：`~((ms << 12) + counter)` 取
//!   低 48 位大端 hex（opencode `identifier.ts` 算法），同毫秒计数器自增。
//!   会话要复用；每次新铸会话烧额度，被拒才换新。
//! - 头部组：`Authorization: Bearer public`、UA `opencode/<channel>/<ver>/<name>`、
//!   `x-opencode-client/session(-id)/project`、`x-session-affinity/id`。
//!
//! 工具名的边界映射：wire 上补两个逐字指纹工具 `read`/`shell`，模型调用它们
//! 时回流改名成 Parable 自己的 `read_file`/`bash` 并做参数改名
//! （workdir→cwd、timeout→timeout_ms），因此工具注册表白名单、执行分发、
//! 会话回放全部零改动。

use super::*;
use crate::config::ProviderConfig;
use crate::error::HarnessError;
use crate::models::UpstreamModel;
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};

// ---------------------------------------------------------------- 常量

/// Bun/BoringSSL 密码表（JA4 cipher hash `5b57614c22b0`），逐字节照抄，
/// 顺序也不能动。来源：OpenCode v2.0.26 CLI（Bun）线上实测。
pub const CIPHER_LIST: &str = "TLS_AES_128_GCM_SHA256:TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:ECDHE-ECDSA-AES128-SHA:ECDHE-RSA-AES128-SHA:ECDHE-ECDSA-AES256-SHA:ECDHE-RSA-AES256-SHA:AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA:AES256-SHA";

/// 上游 UA 形状是 `opencode/<channel>/<version>/<name>`（`app.ts`）；网关只查
/// 形状不查具体版本，9router 的这个字面量实测长期可用。
const OPENCODE_UA: &str = "opencode/latest/2.0.26/cli";

/// 免费模型的 chat 端点（实测路径；目录里的
/// `api.chat-completions.base_url` 是 `/inference/openai/v1`，网关两者皆收）。
const CHAT_URL: &str = "https://opencode.ai/inference/v1/chat/completions";

/// 免费模型目录（OpenAI 列表形状 + cost/capabilities 扩展字段）。免鉴权、
/// 免 TLS 指纹，普通 reqwest 就能读。
const ZEN_MODELS_URL: &str = "https://opencode.ai/zen/v1/models";

/// `read` 指纹工具描述（v2.0.26 CLI 逐字，不要改写）。
const READ_DESC: &str = "Read the contents of a file or directory. Supports text files, images, and PDFs. Images and PDFs are presented directly to the model. Each text line is prefixed by its 1-based line number as <line>: <content>. The prefix is for reference and is not part of the file content. Directory entries are returned one per line. Use offset and limit to read large files or directories in sections. Prefer one larger read over many small slices, and use grep to find specific content in large files.";

/// `shell` 指纹工具描述（v2.0.26 CLI 逐字）。
const SHELL_DESC: &str = "Execute a shell command and return its output. Commands run on Linux using bash. Quote file paths containing spaces or special characters. Prefer dedicated tools over shell commands when possible. When output is large, the full result is saved to a file and a truncated preview is returned. Rely on automatic truncation unless filtering the output is more useful. Commands accept an optional timeout, background commands have no timeout by default. Background commands return immediately, and you will be notified when they complete.";

// ---------------------------------------------------------------- session id

/// opencode `identifier.ts` 的会话 id 编码：
/// `current = (ms << 12) + counter`，取反后低 48 位大端 hex 12 字符，
/// 再拼 14 位 base62 随机。counter 是同毫秒内的创建序号（1 起）。
pub fn mint_session_id(ts_ms: u64, counter: u32) -> String {
    let current = ((ts_ms as u128) << 12) + (counter as u128);
    let descending = (!current) & ((1u128 << 48) - 1);
    format!("ses_{descending:012x}{}", rand_base62(14))
}

fn rand_base62(n: usize) -> String {
    const B62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let uuid = uuid::Uuid::new_v4();
    uuid.as_bytes()
        .iter()
        .chain(uuid.as_bytes().iter())
        .take(n)
        .map(|b| B62[(b % 62) as usize] as char)
        .collect()
}

/// 会话铸币器：同一毫秒内 counter 自增，跨毫秒归 1。
struct SessionMint {
    last_ts: u64,
    counter: u32,
    current: String,
}

impl SessionMint {
    fn new() -> Self {
        let mut m = Self { last_ts: 0, counter: 0, current: String::new() };
        m.mint();
        m
    }

    fn mint(&mut self) -> String {
        let ts = crate::event::now_ms();
        if ts == self.last_ts {
            self.counter += 1;
        } else {
            self.last_ts = ts;
            self.counter = 1;
        }
        self.current = mint_session_id(ts, self.counter);
        self.current.clone()
    }

    fn current(&self) -> String {
        self.current.clone()
    }
}

fn session() -> &'static Mutex<SessionMint> {
    static SESSION: OnceLock<Mutex<SessionMint>> = OnceLock::new();
    SESSION.get_or_init(|| Mutex::new(SessionMint::new()))
}

fn session_current() -> String {
    session().lock().unwrap().current()
}

fn session_remint() -> String {
    session().lock().unwrap().mint()
}

// ---------------------------------------------------------------- <env> 块

/// OpenCode 的 `<env>` 环境标记（`core/environment` 指令形状）。门禁只认
/// `<env>` 标记本身，但内容照真实工作区写对模型也有用。
pub fn env_block(working_dir: &str, is_git: bool) -> String {
    let dir = if working_dir.is_empty() { "." } else { working_dir };
    let platform = match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    };
    let tmp = std::env::temp_dir().join("opencode");
    format!(
        "Here is some useful information about the environment you are running in:\n\
         <env>\n\
         \x20 Working directory: {dir}\n\
         \x20 Workspace root folder: {dir}\n\
         \x20 Is directory a git repo: {}\n\
         \x20 Platform: {platform}\n\
         \x20 Prefer {} over generic system temporary directories such as /tmp; it is pre-created and approved for external access.\n\
         </env>",
        if is_git { "yes" } else { "no" },
        tmp.display(),
    )
}

/// 把 `<env>` 块追加进 system（幂等：已有标记就不再加）。
pub fn with_env(system: &str, working_dir: &str) -> String {
    if system.contains("<env>") {
        return system.to_string();
    }
    let is_git = !working_dir.is_empty()
        && std::path::Path::new(working_dir).join(".git").exists();
    let mut s = system.to_string();
    if !s.is_empty() {
        s.push_str("\n\n");
    }
    s.push_str(&env_block(working_dir, is_git));
    s
}

// ---------------------------------------------------------------- 指纹工具

/// wire 上补的两个逐字指纹工具（`read`/`shell`，v2.0.26 CLI 定义）。
/// 名字与定义是门禁的一部分；模型真调用它们时由 `map_tool_*` 映射回
/// Parable 自己的执行器。
pub fn fingerprint_tools() -> Vec<ToolSpec> {
    let defs = json!([
        {
            "name": "read",
            "description": READ_DESC,
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File or directory to read"},
                    "offset": {"type": "integer", "minimum": 0, "description": "The line or directory entry to start reading from (1-based)"},
                    "limit": {"type": "integer", "minimum": 0, "description": "The maximum number of lines or directory entries to read (defaults to and capped at 2000)"},
                },
                "required": ["path"],
                "additionalProperties": false,
            }
        },
        {
            "name": "shell",
            "description": SHELL_DESC,
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command string to execute"},
                    "workdir": {"type": "string", "description": "Working directory to execute the command in. Defaults to the current working directory. When possible, avoid changing directories in the command and set the working directory here instead."},
                    "timeout": {"type": "integer", "minimum": 0, "description": "Timeout in milliseconds. Set to 0 to disable the timeout. Defaults to 120000 for foreground commands. Background commands have no timeout by default."},
                    "background": {"type": "boolean", "description": "Run the command in the background and return immediately (useful for dev servers and long-running builds). You do not need to use '&' at the end of the command when using this parameter. You will be notified when it completes. DO NOT poll for completion."},
                },
                "required": ["command"],
                "additionalProperties": false,
            }
        }
    ]);
    defs.as_array()
        .unwrap()
        .iter()
        .map(|d| ToolSpec {
            name: d["name"].as_str().unwrap().to_string(),
            description: d["description"].as_str().unwrap().to_string(),
            input_schema: d["parameters"].clone(),
        })
        .collect()
}

// ---------------------------------------------------------------- 工具边界映射

/// 回流工具名：`read`→`read_file`、`shell`→`bash`，其余原样。
pub fn map_tool_name(name: &str) -> &str {
    match name {
        "read" => "read_file",
        "shell" => "bash",
        other => other,
    }
}

/// 回流工具参数：shell 的 workdir/timeout 对应 bash 的 cwd/timeout_ms，
/// background 我们的执行器不支持、丢弃；read 的参数同构原样放行。
pub fn map_tool_args(name: &str, args: Value) -> Value {
    if name != "shell" {
        return args;
    }
    let mut out = serde_json::Map::new();
    if let Some(obj) = args.as_object() {
        for (k, v) in obj {
            match k.as_str() {
                "workdir" => {
                    out.insert("cwd".into(), v.clone());
                }
                "timeout" => {
                    out.insert("timeout_ms".into(), v.clone());
                }
                "background" => {}
                other => {
                    out.insert(other.into(), v.clone());
                }
            }
        }
    }
    Value::Object(out)
}

fn map_chunk(chunk: StreamChunk) -> StreamChunk {
    match chunk {
        StreamChunk::ToolCallStart { id, name } => StreamChunk::ToolCallStart {
            id,
            name: map_tool_name(&name).to_string(),
        },
        StreamChunk::ToolCallDone { id, name, args } => {
            let args = map_tool_args(&name, args);
            StreamChunk::ToolCallDone {
                id,
                name: map_tool_name(&name).to_string(),
                args,
            }
        }
        other => other,
    }
}

// ---------------------------------------------------------------- 429 分类

/// 429/403 拒绝的两类：`overloaded` 是上游过载（退避重试），其余一律按
/// 「会话被烧/被标记」处理（换新会话重试一次）。上游只有一种文案
/// `FreeUsageLimitError`，靠内容区分。
#[derive(Debug, PartialEq, Eq)]
pub enum Rejection {
    Overloaded,
    Session,
}

pub fn classify_error(body: &str) -> Rejection {
    if body.to_ascii_lowercase().contains("overloaded") {
        Rejection::Overloaded
    } else {
        Rejection::Session
    }
}

// ---------------------------------------------------------------- 模型目录

/// 解析 `zen/v1/models` 的目录（OpenAI 列表形状 + cost/capabilities 扩展）。
/// 只留免费（文本输入/输出价为 0 或缺失）且支持 chat-completions 的模型。
pub fn parse_free_models(text: &str) -> Option<Vec<UpstreamModel>> {
    let v: Value = serde_json::from_str(text).ok()?;
    let arr = v.get("data").and_then(|d| d.as_array())?;
    let out: Vec<UpstreamModel> = arr
        .iter()
        .filter(|m| {
            let chat = m
                .pointer("/api/chat-completions")
                .is_some_and(|c| !c.is_null());
            let price_free = ["input", "output"].iter().all(|side| {
                m.pointer(&format!("/cost/text/{side}/price"))
                    .and_then(|p| p.as_f64())
                    .unwrap_or(0.0)
                    == 0.0
            });
            chat && price_free
        })
        .map(|m| {
            let id = m["id"].as_str().unwrap_or_default().to_string();
            let name = m["name"]
                .as_str()
                .filter(|s| !s.is_empty() && *s != id)
                .map(String::from);
            let context = m.pointer("/limit/context").and_then(|c| c.as_u64());
            let vision = m
                .pointer("/modalities/input")
                .and_then(|i| i.as_array())
                .is_some_and(|i| i.iter().any(|x| x.as_str().is_some_and(|s| s.starts_with("image/"))));
            let thinking = m
                .pointer("/capabilities/reasoning/supported")
                .and_then(|r| r.as_bool());
            UpstreamModel {
                id,
                name,
                context,
                vision: Some(vision),
                thinking,
                raw: Some(m.clone()),
            }
        })
        .filter(|m| !m.id.is_empty())
        .collect();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 拉免费模型目录。端点免鉴权免指纹，普通 reqwest 即可。
pub async fn list_free_models() -> Result<Vec<UpstreamModel>, HarnessError> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let resp = client.get(ZEN_MODELS_URL).send().await?;
    let text = resp.text().await.unwrap_or_default();
    parse_free_models(&text).ok_or_else(|| {
        HarnessError::Provider("opencode free: model list came back empty".into())
    })
}

// ---------------------------------------------------------------- transport

fn wreq_client() -> &'static wreq::Client {
    use wreq::tls::TlsOptions;
    use wreq::Emulation;
    static CLIENT: OnceLock<wreq::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        wreq::Client::builder()
            .emulation(
                Emulation::builder()
                    .tls_options(TlsOptions::builder().cipher_list(CIPHER_LIST).build())
                    .build(Default::default()),
            )
            .build()
            .expect("wreq client builds")
    })
}

// ---------------------------------------------------------------- provider

pub struct OpenCodeFree {
    base_url: String,
    model: String,
}

impl OpenCodeFree {
    pub fn new(cfg: &ProviderConfig) -> Self {
        // base_url 可选覆盖（默认官方端点）；key 无意义，免费通道匿名。
        let base_url = if cfg.base_url.trim().is_empty() {
            CHAT_URL.to_string()
        } else {
            cfg.base_url.trim().trim_end_matches('/').to_string()
        };
        Self {
            base_url,
            model: String::new(),
        }
    }

    pub fn with_model(mut self, model: &str) -> Self {
        self.model = model.to_string();
        self
    }

    fn url(&self) -> String {
        if self.base_url.ends_with("/chat/completions") {
            self.base_url.clone()
        } else {
            format!("{}/inference/v1/chat/completions", self.base_url)
        }
    }
}

/// 一次发送（带指纹头）。返回响应体或错误分类。
async fn send_once(
    url: &str,
    body: &str,
    session: &str,
) -> Result<(u16, String, Option<wreq::Response>), HarnessError> {
    let resp = wreq_client()
        .post(url)
        .header("Content-Type", "application/json")
        .header("Authorization", "Bearer public")
        .header("User-Agent", OPENCODE_UA)
        .header("x-opencode-client", "cli")
        .header("x-opencode-session", session)
        .header("x-opencode-session-id", session)
        .header("x-session-affinity", session)
        .header("x-session-id", session)
        .header("x-opencode-project", "global")
        .header("Accept", "text/event-stream")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| HarnessError::Provider(format!("opencode free: transport: {e}")))?;
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        Ok((status, String::new(), Some(resp)))
    } else {
        let text = resp.text().await.unwrap_or_default();
        Ok((status, text, None))
    }
}

/// 429/403 的退避：overloaded 最多 8 次（800ms*n 封顶 4s + 0~250ms 抖动，
/// 线上过载毛刺约 20s 内消化）；会话类拒绝换新会话重试一次。
fn backoff_ms(attempt: u32) -> u64 {
    let base = (800 * attempt as u64).min(4000);
    let jitter = crate::event::now_ms() % 250;
    base + jitter
}

#[async_trait]
impl Provider for OpenCodeFree {
    fn kind(&self) -> &'static str {
        "opencode-free"
    }

    async fn stream_chat(&self, mut req: ChatRequest) -> Result<ChunkStream, HarnessError> {
        if req.model.is_empty() {
            req.model = self.model.clone();
        }
        if req.model.is_empty() {
            return Err(HarnessError::Provider(
                "opencode free: no model id set for this request".into(),
            ));
        }
        // 请求变换：system 补 <env>，tools 追加逐字指纹工具（幂等），
        // stream 由 build_chat_body 恒为 true。
        req.system = with_env(&req.system, &req.workspace);
        let mut tools = (*req.tools).clone();
        for fp in fingerprint_tools() {
            if !tools.iter().any(|t| t.name == fp.name) {
                tools.push(fp);
            }
        }
        req.tools = std::sync::Arc::new(tools);
        let body = super::openai::build_chat_body(&req).to_string();
        let url = self.url();

        let mut attempt = 0u32;
        let mut reminted = false;
        loop {
            let sess = session_current();
            match send_once(&url, &body, &sess).await {
                Ok((_, _, Some(resp))) => {
                    let byte_stream = resp.bytes_stream().map(|r| {
                        r.map_err(|e| HarnessError::Other(format!("opencode free: stream: {e}")))
                    });
                    let parsed = super::openai::stream_chat_completions(byte_stream);
                    let mapped = parsed.map(|item| item.map(map_chunk));
                    let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamChunk, HarnessError>>(64);
                    tokio::spawn(async move {
                        futures::pin_mut!(mapped);
                        while let Some(item) = mapped.next().await {
                            if tx.send(item).await.is_err() {
                                break;
                            }
                        }
                    });
                    return Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)));
                }
                Ok((status, text, None)) => {
                    if status == 429 || status == 403 {
                        match classify_error(&text) {
                            Rejection::Overloaded if attempt < 7 => {
                                attempt += 1;
                                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms(attempt)))
                                    .await;
                                continue;
                            }
                            Rejection::Overloaded => {
                                return Err(HarnessError::Provider(
                                    "opencode free: upstream overloaded, give it a minute".into(),
                                ));
                            }
                            Rejection::Session if !reminted => {
                                reminted = true;
                                session_remint();
                                continue;
                            }
                            Rejection::Session => {
                                return Err(HarnessError::Provider(format!(
                                    "opencode free: HTTP {status}: {}",
                                    truncate(&text, 400)
                                )));
                            }
                        }
                    }
                    return Err(HarnessError::Provider(format!(
                        "opencode free: HTTP {status}: {}",
                        truncate(&text, 400)
                    )));
                }
                Err(e) => {
                    if attempt < 2 {
                        attempt += 1;
                        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms(attempt)))
                            .await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    async fn test(&self) -> Result<TestReport, HarnessError> {
        let t0 = std::time::Instant::now();
        // 目录端点免鉴权，但那只证明网络通；真正要验的是「能不能过免费门禁
        // 拿到模型输出」，所以先取目录，再发一次最小真请求。
        let models = match list_free_models().await {
            Ok(m) => m,
            Err(e) => {
                return Ok(TestReport {
                    ok: false,
                    latency_ms: t0.elapsed().as_millis() as u64,
                    message: format!("model list failed: {e}"),
                    models: vec![],
                })
            }
        };
        let model = if !self.model.is_empty() {
            self.model.clone()
        } else {
            models.first().map(|m| m.id.clone()).unwrap_or_default()
        };
        if model.is_empty() {
            return Ok(TestReport {
                ok: false,
                latency_ms: t0.elapsed().as_millis() as u64,
                message: "no free models available".into(),
                models: vec![],
            });
        }
        let req = ChatRequest {
            model: model.clone(),
            system: with_env("You are opencode, an interactive CLI tool that helps users with software engineering tasks.", ""),
            messages: vec![Msg::user("reply with the single word: pong")],
            tools: std::sync::Arc::new(fingerprint_tools()),
            max_tokens: 32,
            thinking_level: 0,
            workspace: String::new(),
        };
        let mut stream = self.stream_chat(req).await?;
        let mut text = String::new();
        while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
            match chunk? {
                StreamChunk::Text(t) => text.push_str(&t),
                StreamChunk::Done { .. } => break,
                _ => {}
            }
        }
        let latency = t0.elapsed().as_millis() as u64;
        let ids: Vec<String> = models.iter().map(|m| m.id.clone()).take(30).collect();
        if text.trim().is_empty() {
            Ok(TestReport {
                ok: false,
                latency_ms: latency,
                message: "gate passed but the model returned nothing".into(),
                models: ids,
            })
        } else {
            Ok(TestReport {
                ok: true,
                latency_ms: latency,
                message: format!("OK · {}", text.trim().chars().take(40).collect::<String>()),
                models: ids,
            })
        }
    }
}

/// 边界安全截断（错误信息用），与 openai.rs 同款语义。
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 独立按规格写的解码器，验证 (ms<<12)+counter 打包无损。
    fn decode_session(hex12: &str) -> (u64, u32) {
        let prefix = u128::from_str_radix(hex12, 16).expect("hex");
        let x = (!prefix) & ((1u128 << 48) - 1);
        ((x >> 12) as u64, (x & 0xfff) as u32)
    }

    #[test]
    fn session_id_format_and_roundtrip() {
        let id = mint_session_id(1_700_000_000_000, 1);
        let re = regex::Regex::new(r"^ses_[0-9a-f]{12}[0-9A-Za-z]{14}$").unwrap();
        assert!(re.is_match(&id), "bad id shape: {id}");
        let (ts, counter) = decode_session(&id[4..16]);
        // 12 hex = 48 位 = 36 位时间 + 12 位 counter，时间高位按上游算法
        // （identifier.ts 取 value 低 6 字节）本来就装不下，验证低位无损。
        assert_eq!(ts, 1_700_000_000_000 & 0xF_FFFF_FFFF);
        assert_eq!(counter, 1);
    }

    #[test]
    fn session_id_counter_packs_into_low_12_bits() {
        let id = mint_session_id(12_345, 0xabc);
        let (ts, counter) = decode_session(&id[4..16]);
        assert_eq!(ts, 12_345);
        assert_eq!(counter, 0xabc);
    }

    #[test]
    fn env_block_has_marker_and_workspace() {
        let b = env_block("C:/ws", true);
        assert!(b.contains("<env>"));
        assert!(b.contains("</env>"));
        assert!(b.contains("Working directory: C:/ws"));
        assert!(b.contains("Is directory a git repo: yes"));
    }

    #[test]
    fn with_env_is_idempotent() {
        let once = with_env("hello", "/w");
        assert_eq!(once.matches("<env>").count(), 1);
        let twice = with_env(&once, "/w");
        assert_eq!(twice, once);
    }

    #[test]
    fn tool_mapping_renames_and_remaps_args() {
        assert_eq!(map_tool_name("read"), "read_file");
        assert_eq!(map_tool_name("shell"), "bash");
        assert_eq!(map_tool_name("grep"), "grep");

        let args = json!({"command": "ls", "workdir": "/w", "timeout": 5000, "background": true});
        let mapped = map_tool_args("shell", args);
        assert_eq!(mapped["command"], "ls");
        assert_eq!(mapped["cwd"], "/w");
        assert_eq!(mapped["timeout_ms"], 5000);
        assert!(mapped.get("background").is_none());
        assert!(mapped.get("workdir").is_none());

        let read_args = json!({"path": "a.txt", "offset": 2});
        assert_eq!(map_tool_args("read", read_args.clone()), read_args);
    }

    #[test]
    fn chunk_mapping_applies_to_tool_events() {
        let c = map_chunk(StreamChunk::ToolCallDone {
            id: "call_1".into(),
            name: "shell".into(),
            args: json!({"command": "ls", "workdir": "/w"}),
        });
        match c {
            StreamChunk::ToolCallDone { name, args, .. } => {
                assert_eq!(name, "bash");
                assert_eq!(args["cwd"], "/w");
            }
            other => panic!("wrong chunk: {other:?}"),
        }
    }

    #[test]
    fn rejection_classification() {
        assert_eq!(
            classify_error(r#"{"type":"error","error":{"type":"FreeUsageLimitError"}}"#),
            Rejection::Session
        );
        assert_eq!(classify_error("model is OVERLOADED"), Rejection::Overloaded);
    }

    #[test]
    fn fingerprint_tools_are_the_gated_names() {
        let tools = fingerprint_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["read", "shell"]);
        let read = &tools[0];
        assert!(read.description.starts_with("Read the contents of a file or directory."));
        assert_eq!(read.input_schema["required"][0], "path");
        assert_eq!(read.input_schema["additionalProperties"], false);
        let shell = &tools[1];
        assert!(shell.description.starts_with("Execute a shell command and return its output."));
        assert_eq!(shell.input_schema["required"][0], "command");
    }

    #[test]
    fn parse_free_models_filters_and_maps() {
        let text = r#"{"object":"list","data":[
            {"id":"deepseek-v4.1-flash-free","name":"DeepSeek V4.1 Flash Free",
             "cost":{"text":{"input":{"price":0},"output":{"price":0}}},
             "api":{"chat-completions":{"base_url":"https://opencode.ai/inference/openai/v1"}},
             "limit":{"context":200000},"modalities":{"input":["text"]},
             "capabilities":{"reasoning":{"supported":true}}},
            {"id":"paid-model","name":"Paid",
             "cost":{"text":{"input":{"price":3},"output":{"price":6}}},
             "api":{"chat-completions":{}}},
            {"id":"no-chat","name":"Embed",
             "cost":{"text":{"input":{"price":0},"output":{"price":0}}},
             "api":{"embeddings":{}}}
        ]}"#;
        let models = parse_free_models(text).expect("one free model");
        assert_eq!(models.len(), 1);
        let m = &models[0];
        assert_eq!(m.id, "deepseek-v4.1-flash-free");
        assert_eq!(m.name.as_deref(), Some("DeepSeek V4.1 Flash Free"));
        assert_eq!(m.context, Some(200000));
        assert_eq!(m.thinking, Some(true));
        assert_eq!(m.vision, Some(false));
    }

    #[test]
    fn parse_free_models_vision_detection() {
        let text = r#"{"data":[{"id":"m","cost":{"text":{"input":{"price":0},"output":{"price":0}}},
            "api":{"chat-completions":{}},"modalities":{"input":["text","image/png"]}}]}"#;
        let models = parse_free_models(text).expect("parsed");
        assert_eq!(models[0].vision, Some(true));
    }
}
