//! MCP (Model Context Protocol) 客户端管理器。
//!
//! 管理一组 stdio MCP 服务器进程（JSON-RPC 2.0 over stdin/stdout 行协议），
//! 把它们的工具暴露为 Parable 工具（`mcp_<server>_<tool>`）。
//! 协议：initialize → notifications/initialized → tools/list → tools/call。
//!
//! 传输形态：**换行分隔的 JSON**（不是 LSP 的 Content-Length 分帧）。每台
//! 服务器在线时由「一个子进程 + 两个后台任务 + 一个共享层」构成：
//! - stdout 读行任务：解析每行 JSON，按请求 id 经 oneshot 分发响应；
//!   通知与服务器→客户端请求、非 JSON 行（启动横幅）一律忽略；
//!   EOF（进程退出）→ 清空 pending（全部报错）+ 标记断开。
//! - stderr 收集任务：滚动保留尾部若干行，握手/调用失败时附进错误信息。
//! - 共享层 `Shared`：写端用 Mutex 串行化；请求 id 单调递增，
//!   响应按 id 进 `PendingMap`（HashMap<u64, oneshot::Sender>）分发。
//!
//! 生命周期：**惰性连接**——登记（set_servers）不 spawn，首次 call_tool /
//! 显式 connect 才拉起并握手；请求失败（非 Rpc 错误）或超时 → 杀进程、
//! 标记断开，下次调用自动重连。

use crate::error::HarnessError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{oneshot, Mutex};

/// 一台 MCP 服务器的声明（设置 → MCP；存 ~/.parable/mcp.json 或 config）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub id: String,
    pub name: String,
    /// 启动命令（如 npx / python / 可执行文件路径）。
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 每次工具调用超时（毫秒）。
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

fn default_true() -> bool {
    true
}
fn default_timeout() -> u64 {
    60_000
}

/// 运行中的服务器状态。
#[derive(Debug, Clone, Serialize)]
pub struct McpStatus {
    pub id: String,
    pub name: String,
    pub connected: bool,
    pub tools: Vec<String>,
    pub error: String,
}

/// 握手（initialize / tools/list）超时；tools/call 用每服务器的 timeout_ms。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// stderr 尾部最多保留的行数（错误报告用）。
const STDERR_TAIL_LINES: usize = 16;

// ------------------------------------------------------------------ 解析层
//
// 纯函数 + 纯数据，不碰进程；所有协议细节都收在这里，可独立测试。

/// tools/list 里的一个工具定义。
#[derive(Debug, Clone, Deserialize)]
struct McpTool {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default, rename = "inputSchema")]
    input_schema: Value,
}

/// 服务器名 → 暴露名片段：转小写，非 [a-z0-9_] 一律换 '_'；空名回落 "server"。
fn sanitize_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("server");
    }
    out
}

/// JSON-RPC 请求行（换行分隔协议——不是 LSP 的 Content-Length 分帧）。
fn format_request(id: u64, method: &str, params: &Value) -> String {
    format!(
        "{}\n",
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    )
}

/// 通知行：无 id，服务器不用回复。
fn format_notification(method: &str) -> String {
    format!("{}\n", json!({"jsonrpc": "2.0", "method": method}))
}

/// JSON-RPC error 对象 → 一行可读文本。
fn format_rpc_error(err: &Value) -> String {
    let msg = err
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("未知错误");
    match err.get("code").and_then(|c| c.as_i64()) {
        Some(code) => format!("[{code}] {msg}"),
        None => msg.to_string(),
    }
}

/// 一行 stdout 的解析结果。
#[derive(Debug, PartialEq)]
enum Line {
    /// 带 id 的响应（result 与 error 二选一）。
    Response {
        id: u64,
        result: Option<Value>,
        error: Option<Value>,
    },
    /// 合法 JSON 但不是响应（通知、服务器→客户端请求）——忽略。
    Other,
    /// 非 JSON 行（启动横幅等）——忽略。
    Noise,
}

/// 解析一行 stdout。id 为数字且带 result/error 才算响应；
/// 其余（通知/横幅/空行）交由上层忽略——MCP 服务器常把日志打到 stdout。
fn parse_line(raw: &str) -> Line {
    let s = raw.trim();
    if s.is_empty() {
        return Line::Noise;
    }
    let Ok(v) = serde_json::from_str::<Value>(s) else {
        return Line::Noise;
    };
    let Some(id) = v.get("id").and_then(|i| i.as_u64()) else {
        return Line::Other;
    };
    let result = if v.get("result").is_some() {
        Some(v["result"].clone())
    } else {
        None
    };
    let error = if v.get("error").is_some() {
        Some(v["error"].clone())
    } else {
        None
    };
    if result.is_none() && error.is_none() {
        return Line::Other;
    }
    Line::Response { id, result, error }
}

/// tools/list 的 result → 工具表。缺 name 的坏条目跳过（容忍不健全的服务器）。
fn parse_tools(result: &Value) -> Vec<McpTool> {
    result
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| serde_json::from_value::<McpTool>(t.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// initialize 的 result → (serverInfo.name, protocolVersion)。
/// 协议版本不强校验（服务器可能协商降级），仅记录。
fn parse_init(result: &Value) -> Option<(String, String)> {
    let name = result.pointer("/serverInfo/name")?.as_str()?.to_string();
    let ver = result
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Some((name, ver))
}

/// result.content[] 中的 text 块（跳过 image 等非文本块）。
fn text_blocks(result: &Value) -> Vec<String> {
    result
        .get("content")
        .and_then(|c| c.as_array())
        .map(|items| {
            items
                .iter()
                .filter(|it| it.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|it| {
                    it.get("text")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// tools/call 的 result → 文本：content[] 里 type=="text" 的 text 以换行拼接；
/// isError==true → Err（文本作为错误详情返回给模型）。
fn extract_call_text(result: &Value) -> Result<String, HarnessError> {
    let text = text_blocks(result).join("\n");
    if result
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Err(HarnessError::Tool(if text.is_empty() {
            "MCP 工具执行失败（isError=true，无文本详情）".to_string()
        } else {
            format!("MCP 工具执行失败: {text}")
        }));
    }
    Ok(text)
}

// ------------------------------------------------------------------ 在线会话

/// pending 表里的一条应答。
type Reply = Result<Value, ReplyErr>;

#[derive(Debug)]
enum ReplyErr {
    /// 服务器明确返回 JSON-RPC error（连接本身还活着）。
    Rpc(String),
    /// 连接死亡广播（进程退出/读任务终止）。
    Dead(String),
}

/// 请求 id → 应答通道；读任务按 id 分发，请求方 insert + recv。
type PendingMap = HashMap<u64, oneshot::Sender<Reply>>;

/// 一次请求的失败原因。
#[derive(Debug)]
enum ReqFail {
    /// 等响应超时。
    Timeout,
    /// 连接已断开（进程退出广播 / 通道取消）。
    Closed(String),
    /// 服务器返回 JSON-RPC error。
    Rpc(String),
    /// 请求行写失败（管道断了）。
    Send(String),
}

impl ReqFail {
    /// 非 Rpc 失败都视为连接级故障：拆掉连接，下次调用重连。
    fn is_fatal(&self) -> bool {
        !matches!(self, ReqFail::Rpc(_))
    }

    /// 转成给调用方的错误（带方法名上下文）。
    fn into_error(self, method: &str) -> HarnessError {
        match self {
            ReqFail::Timeout => HarnessError::Tool(format!(
                "MCP {method} 超时（服务器无响应，连接已重置，可重试）"
            )),
            ReqFail::Closed(m) => HarnessError::Tool(format!("MCP {method}: {m}")),
            ReqFail::Rpc(m) => HarnessError::Tool(format!("MCP {method} 失败: {m}")),
            ReqFail::Send(m) => HarnessError::Tool(format!("MCP {method}: {m}")),
        }
    }
}

/// 在线共享层：请求方（任意数量）与读行任务都持有它。
struct Shared {
    /// JSON-RPC 请求 id，单调递增（从 1 起）。
    next_id: AtomicU64,
    /// 按 id 分发响应。
    pending: Mutex<PendingMap>,
    /// 子进程 stdin（行写端）。None = 已拆。
    stdin: Mutex<Option<Box<dyn AsyncWrite + Send + Unpin>>>,
    /// 读任务 EOF 后置 false——进程退出/管道断裂的唯一真相源。
    alive: AtomicBool,
    /// 断开原因（含 stderr 尾部）。
    death: Mutex<String>,
    /// stderr 尾部（最后 STDERR_TAIL_LINES 行），错误报告用。
    tail: std::sync::Mutex<Vec<String>>,
}

impl Shared {
    fn new(stdin: Box<dyn AsyncWrite + Send + Unpin>) -> Self {
        Self {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            stdin: Mutex::new(Some(stdin)),
            alive: AtomicBool::new(true),
            death: Mutex::new(String::new()),
            tail: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// 发一个请求并等按 id 匹配的响应（写行 → 注册 pending → 超时等待）。
    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, ReqFail> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let line = format_request(id, method, &params);
        if let Err(e) = write_line(self, &line).await {
            self.pending.lock().await.remove(&id);
            return Err(ReqFail::Send(e.to_string()));
        }
        match tokio::time::timeout(timeout, rx).await {
            // 超时：摘掉登记，迟到的响应会被读任务静默丢弃
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(ReqFail::Timeout)
            }
            // sender 被丢弃（表被异常清空）
            Ok(Err(_)) => Err(ReqFail::Closed("MCP 连接已断开".into())),
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => match e {
                ReplyErr::Rpc(m) => Err(ReqFail::Rpc(m)),
                ReplyErr::Dead(m) => Err(ReqFail::Closed(m)),
            },
        }
    }

    /// 进程退出：清空 pending（全部报错）+ 标记断开。读任务在 EOF 后调用。
    async fn die(&self, note: &str) {
        // swap 保证只有首个死亡写原因、只清一次表
        if self.alive.swap(false, Ordering::SeqCst) {
            *self.death.lock().await = note.to_string();
            let mut map = self.pending.lock().await;
            for (_, tx) in map.drain() {
                let _ = tx.send(Err(ReplyErr::Dead(note.to_string())));
            }
        }
    }
}

/// 写一行 JSON 到子进程 stdin。写端没了/管道断了 = 连接已死。
async fn write_line(shared: &Shared, line: &str) -> Result<(), HarnessError> {
    let mut guard = shared.stdin.lock().await;
    match guard.as_mut() {
        Some(w) => match w.write_all(line.as_bytes()).await {
            Ok(()) => w
                .flush()
                .await
                .map_err(|e| HarnessError::Tool(format!("MCP 写入失败: {e}"))),
            Err(e) => Err(HarnessError::Tool(format!("MCP 写入失败: {e}"))),
        },
        None => Err(HarnessError::Tool("MCP 写端已关闭".into())),
    }
}

/// stdout 读行任务：解析每行 JSON，按 id 分发响应；通知/横幅忽略。
/// EOF（进程退出）→ fail_all + 标记断开。
async fn read_loop<R: AsyncRead + Unpin + Send + 'static>(reader: R, shared: Arc<Shared>) {
    let mut reader = tokio::io::BufReader::new(reader);
    let mut buf = Vec::with_capacity(4096);
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buf);
        match parse_line(&line) {
            Line::Response { id, result, error } => {
                let reply: Reply = match error {
                    Some(e) => Err(ReplyErr::Rpc(format_rpc_error(&e))),
                    None => Ok(result.unwrap_or(Value::Null)),
                };
                if let Some(tx) = shared.pending.lock().await.remove(&id) {
                    let _ = tx.send(reply);
                }
            }
            // 通知 / 服务器→客户端请求 / 横幅：忽略
            Line::Other | Line::Noise => {}
        }
    }
    let tail = stderr_tail_text(&shared.tail);
    let note = if tail.is_empty() {
        "MCP 服务器进程已退出".to_string()
    } else {
        format!("MCP 服务器进程已退出；stderr 尾部: {tail}")
    };
    shared.die(&note).await;
}

/// stderr 收集任务：滚动保留最后 STDERR_TAIL_LINES 行，供错误报告。
async fn stderr_loop<R: AsyncRead + Unpin + Send + 'static>(
    reader: R,
    shared: Arc<Shared>,
) {
    let mut lines = tokio::io::BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut t = shared
            .tail
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        t.push(line);
        let excess = t.len().saturating_sub(STDERR_TAIL_LINES);
        t.drain(..excess);
    }
}

fn stderr_tail_text(tail: &std::sync::Mutex<Vec<String>>) -> String {
    let t = tail.lock().unwrap_or_else(|e| e.into_inner());
    t.join("\n")
}

/// 给错误补上 stderr 尾部（服务器通常把崩溃原因打到 stderr）。
fn with_tail(err: HarnessError, shared: &Shared) -> HarnessError {
    let tail = stderr_tail_text(&shared.tail);
    if tail.is_empty() {
        return err;
    }
    HarnessError::Tool(format!("{err}；stderr 尾部: {tail}"))
}

/// HarnessError → 状态页用的纯文本原因。
fn error_reason(e: &HarnessError) -> String {
    match e {
        HarnessError::Tool(t) => t.clone(),
        other => other.to_string(),
    }
}

// ------------------------------------------------------------------ 连接

/// 一台服务器连接的本地状态（懒连接；进程死亡由读任务经 Shared 标记）。
#[derive(Default)]
struct Conn {
    /// 在线时的共享层（写端 + pending 表 + 存活标记）。None = 未连接。
    live: Option<Arc<Shared>>,
    /// 子进程句柄（kill 用；wait 交给后台任务回收，避免僵尸）。
    child: Option<tokio::process::Child>,
    /// 最近一次 tools/list 的缓存。
    tools: Vec<McpTool>,
    /// 最近一次错误（spawn/握手失败原因），状态页展示。
    error: String,
}

impl Conn {
    /// 进程是否还活着。
    fn is_alive(&self) -> bool {
        self.live
            .as_ref()
            .map(|s| s.alive.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// 杀进程 + 清状态（幂等）。reason 会出现在状态页。
    fn teardown(&mut self, reason: &str) {
        self.live = None;
        self.tools.clear();
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            // 回收子进程（Unix 僵尸 / Windows 句柄），管道随之关闭，
            // 读行任务自行退出。
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
        self.error = reason.to_string();
    }
}

/// spawn 子进程 + initialize 握手 + initialized 通知 + tools/list。
/// 任一步失败：杀掉子进程回收，错误带 stderr 尾部。
async fn spawn_and_init(
    cfg: &McpServerConfig,
) -> Result<(tokio::process::Child, Arc<Shared>, Vec<McpTool>), HarnessError> {
    let mut cmd = tokio::process::Command::new(&cfg.command);
    cmd.args(&cfg.args)
        .envs(cfg.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| {
        HarnessError::Tool(format!("启动 MCP 服务器 `{}` 失败: {e}", cfg.name))
    })?;
    match handshake(&mut child).await {
        Ok((shared, tools)) => Ok((child, shared, tools)),
        Err(e) => {
            let _ = child.start_kill();
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            Err(e)
        }
    }
}

/// 与刚 spawn 的子进程完成 MCP 握手。管线任务（stdout 分发 / stderr 收集）
/// 在这里启动；失败时由调用方杀进程，任务随管道关闭自行结束。
async fn handshake(child: &mut tokio::process::Child) -> Result<(Arc<Shared>, Vec<McpTool>), HarnessError> {
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| HarnessError::Tool("MCP 子进程缺少 stdin".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HarnessError::Tool("MCP 子进程缺少 stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| HarnessError::Tool("MCP 子进程缺少 stderr".into()))?;
    let shared = Arc::new(Shared::new(Box::new(stdin)));
    tokio::spawn(read_loop(stdout, Arc::clone(&shared)));
    tokio::spawn(stderr_loop(stderr, Arc::clone(&shared)));

    let init = shared
        .request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "parable", "version": "0.1.0"},
            }),
            HANDSHAKE_TIMEOUT,
        )
        .await
        .map_err(|f| handshake_fail(f, "initialize", &shared))?;
    if let Some((server, proto)) = parse_init(&init) {
        tracing::debug!(server = %server, protocol = %proto, "MCP initialize 完成");
    }

    // initialized 通知：无 id，不发等响应。
    write_line(&shared, &format_notification("notifications/initialized"))
        .await
        .map_err(|e| with_tail(e, &shared))?;

    let listed = shared
        .request("tools/list", json!({}), HANDSHAKE_TIMEOUT)
        .await
        .map_err(|f| handshake_fail(f, "tools/list", &shared))?;
    Ok((shared, parse_tools(&listed)))
}

fn handshake_fail(f: ReqFail, step: &str, shared: &Shared) -> HarnessError {
    with_tail(f.into_error(step), shared)
}

/// 惰性连接：活着直接复用；死了/没连过就 spawn + 握手。
/// 持 conn 锁完成整个握手，避免并发首调重复 spawn。
async fn ensure_connected(
    cfg: &McpServerConfig,
    conn: &Arc<Mutex<Conn>>,
) -> Result<Arc<Shared>, HarnessError> {
    let mut guard = conn.lock().await;
    if let Some(s) = guard.live.as_ref() {
        if s.alive.load(Ordering::SeqCst) {
            return Ok(Arc::clone(s));
        }
    }
    // 死连接：先以死亡原因说明再拆（比笼统的"已失效"更有用）
    if guard.live.is_some() {
        let reason = match guard.live.as_ref() {
            Some(s) => s.death.lock().await.clone(),
            None => String::new(),
        };
        let reason = if reason.is_empty() { "连接已失效".into() } else { reason };
        guard.teardown(&reason);
    }
    match spawn_and_init(cfg).await {
        Ok((child, shared, tools)) => {
            guard.child = Some(child);
            guard.tools = tools;
            guard.live = Some(Arc::clone(&shared));
            guard.error.clear();
            Ok(shared)
        }
        Err(e) => {
            guard.error = error_reason(&e);
            Err(e)
        }
    }
}

// ------------------------------------------------------------------ 管理器

/// 管理器：持有全部连接，异步侧通过它交互。
pub struct McpManager {
    conns: Mutex<Vec<(McpServerConfig, Arc<Mutex<Conn>>)>>,
}

impl McpManager {
    pub fn new() -> Self {
        Self { conns: Mutex::new(Vec::new()) }
    }

    /// 更新服务器列表：新增的登记待连（惰性），被移除/禁用的杀进程回收，
    /// 仍存在且启用的保留现有连接状态（配置变了才断开待重连）。
    pub async fn set_servers(&self, servers: Vec<McpServerConfig>) {
        let mut conns = self.conns.lock().await;
        let mut next = Vec::with_capacity(servers.len());
        for s in servers {
            let existing = conns
                .iter()
                .position(|(c, _)| c.id == s.id)
                .map(|i| conns.swap_remove(i));
            match existing {
                Some((old, conn)) => {
                    if !s.enabled {
                        conn.lock().await.teardown("已禁用");
                        next.push((s, conn));
                        continue;
                    }
                    // 连接参数变了 → 丢掉旧连接，下次调用用新配置重连
                    let changed = old.command != s.command
                        || old.args != s.args
                        || old.env != s.env
                        || old.timeout_ms != s.timeout_ms;
                    if changed {
                        conn.lock().await.teardown("配置已更新");
                    }
                    next.push((s, conn));
                }
                // 新登记：待连（禁用的也登记，状态页可见，只是永不连）
                None => next.push((s, Arc::new(Mutex::new(Conn::default())))),
            }
        }
        // 剩下的都是被移除的服务器：杀进程回收
        for (_, conn) in conns.drain(..) {
            conn.lock().await.teardown("已移除");
        }
        *conns = next;
    }

    /// 各服务器状态（设置页列表）。只报告现状，不主动拉起进程；
    /// 正在握手的连接用 try_lock 探测，绝不阻塞状态轮询。
    pub async fn list_status(&self) -> Vec<McpStatus> {
        let conns = self.conns.lock().await;
        let mut out = Vec::with_capacity(conns.len());
        for (cfg, conn) in conns.iter() {
            let guard = match conn.try_lock() {
                Ok(g) => g,
                Err(_) => {
                    out.push(McpStatus {
                        id: cfg.id.clone(),
                        name: cfg.name.clone(),
                        connected: false,
                        tools: Vec::new(),
                        error: "(连接中…)".into(),
                    });
                    continue;
                }
            };
            let (connected, tools, error) = if guard.is_alive() {
                (
                    true,
                    guard.tools.iter().map(|t| t.name.clone()).collect(),
                    String::new(),
                )
            } else {
                let error = match guard.live.as_ref() {
                    Some(s) => s.death.lock().await.clone(),
                    None => guard.error.clone(),
                };
                (false, Vec::new(), error)
            };
            out.push(McpStatus {
                id: cfg.id.clone(),
                name: cfg.name.clone(),
                connected,
                tools,
                error,
            });
        }
        out
    }

    /// 显式连接一台服务器并拉工具列表（设置页「连接」按钮）。
    pub async fn connect(&self, id: &str) -> Result<(), HarnessError> {
        let (cfg, conn) = {
            let conns = self.conns.lock().await;
            conns
                .iter()
                .find(|(c, _)| c.id == id)
                .cloned()
                .ok_or_else(|| HarnessError::NotFound(format!("MCP 服务器 {id} 未登记")))?
        };
        if !cfg.enabled {
            return Err(HarnessError::Tool(format!(
                "MCP 服务器 `{}` 已禁用",
                cfg.name
            )));
        }
        ensure_connected(&cfg, &conn).await?;
        Ok(())
    }

    /// 所有 enabled 且已连接服务器的工具，暴露为 Parable 工具清单。
    /// 返回 (暴露名 mcp_<server>_<tool>, 描述, 参数 schema JSON)。
    pub async fn all_tools(&self) -> Vec<(String, String, Value)> {
        let conns = self.conns.lock().await;
        let mut out = Vec::new();
        for (cfg, conn) in conns.iter() {
            if !cfg.enabled {
                continue;
            }
            // 正在握手的连接尚无工具缓存，跳过（不阻塞）
            let Ok(guard) = conn.try_lock() else {
                continue;
            };
            if !guard.is_alive() {
                continue;
            }
            let prefix = format!("mcp_{}_", sanitize_name(&cfg.name));
            for t in &guard.tools {
                out.push((
                    format!("{prefix}{}", t.name),
                    t.description.clone(),
                    t.input_schema.clone(),
                ));
            }
        }
        out
    }

    /// 调用一个 MCP 工具，返回文本结果。`exposed` 是 all_tools 给出的暴露名。
    pub async fn call_tool(&self, exposed: &str, args: Value) -> Result<String, HarnessError> {
        let (cfg, conn, tool) = self.route(exposed).await?;
        let shared = ensure_connected(&cfg, &conn).await?;
        match shared
            .request(
                "tools/call",
                json!({"name": tool, "arguments": args}),
                Duration::from_millis(cfg.timeout_ms),
            )
            .await
        {
            Ok(result) => extract_call_text(&result),
            Err(f) => {
                // 非 Rpc 失败 = 连接级故障：杀进程标记断开，下次调用重连
                let fatal = f.is_fatal();
                let msg = with_tail(f.into_error("tools/call"), &shared);
                if fatal {
                    conn.lock().await.teardown(&error_reason(&msg));
                }
                Err(msg)
            }
        }
    }

    /// 暴露名 → (服务器配置, 连接, 工具名)。最长前缀匹配——两个服务器
    /// sanitize 后互为前缀时也能区分归属。
    async fn route(
        &self,
        exposed: &str,
    ) -> Result<(McpServerConfig, Arc<Mutex<Conn>>, String), HarnessError> {
        let conns = self.conns.lock().await;
        let mut hit: Option<(usize, usize)> = None; // (prefix 长度, 下标)
        for (i, (cfg, _)) in conns.iter().enumerate() {
            if !cfg.enabled {
                continue;
            }
            let prefix = format!("mcp_{}_", sanitize_name(&cfg.name));
            if exposed.starts_with(&prefix)
                && hit.map(|(l, _)| prefix.len() > l).unwrap_or(true)
            {
                hit = Some((prefix.len(), i));
            }
        }
        let Some((plen, i)) = hit else {
            return Err(HarnessError::Tool(format!(
                "未知的 MCP 工具 `{exposed}`"
            )));
        };
        let (cfg, conn) = conns[i].clone();
        Ok((cfg, conn, exposed[plen..].to_string()))
    }

    /// 全部进程回收（应用退出 / 配置清空时）。
    pub async fn remove_all(&self) {
        let mut conns = self.conns.lock().await;
        for (_, conn) in conns.drain(..) {
            conn.lock().await.teardown("已移除");
        }
    }
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- 纯解析层（不起进程） ----------

    #[test]
    fn sanitize_name_rules() {
        assert_eq!(sanitize_name("GitHub Copilot!"), "github_copilot_");
        assert_eq!(sanitize_name("My-Tool.2"), "my_tool_2");
        assert_eq!(sanitize_name("MCP Server 2"), "mcp_server_2");
        assert_eq!(sanitize_name("ABCdef_123"), "abcdef_123");
        assert_eq!(sanitize_name("服 务"), "___");
        assert_eq!(sanitize_name(""), "server");
    }

    #[test]
    fn parse_line_classifies() {
        // 带 id + result → Response
        let line = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#;
        match parse_line(line) {
            Line::Response { id, result, error } => {
                assert_eq!(id, 1);
                assert_eq!(result.unwrap()["protocolVersion"], "2024-11-05");
                assert!(error.is_none());
            }
            other => panic!("期望 Response，得到 {other:?}"),
        }
        // 带 id + error → Response
        match parse_line(r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"Method not found"}}"#) {
            Line::Response { id, error, .. } => {
                assert_eq!(id, 7);
                assert_eq!(format_rpc_error(&error.unwrap()), "[-32601] Method not found");
            }
            other => panic!("期望 Response，得到 {other:?}"),
        }
        // 通知（无 id）与服务器→客户端请求（id + method，无 result/error）：忽略
        assert_eq!(
            parse_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            Line::Other
        );
        assert_eq!(
            parse_line(r#"{"jsonrpc":"2.0","id":5,"method":"sampling/createMessage"}"#),
            Line::Other
        );
        // 启动横幅 / 空行：忽略
        assert_eq!(parse_line("Server starting on port 3000..."), Line::Noise);
        assert_eq!(parse_line("   \n"), Line::Noise);
    }

    #[test]
    fn request_line_format() {
        let line = format_request(3, "tools/call", &json!({"name": "t"}));
        assert!(line.ends_with('\n'), "行协议必须以换行结尾");
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 3);
        assert_eq!(v["method"], "tools/call");
        assert_eq!(v["params"]["name"], "t");
        // 通知没有 id
        let n: Value =
            serde_json::from_str(format_notification("notifications/initialized").trim()).unwrap();
        assert!(n.get("id").is_none());
        assert_eq!(n["method"], "notifications/initialized");
        assert!(format_notification("notifications/initialized").ends_with('\n'));
    }

    #[test]
    fn call_result_text_and_is_error() {
        // content[] 拼接 text 块，跳过 image 等其他类型
        let ok = json!({
            "content": [
                {"type": "text", "text": "first"},
                {"type": "image", "data": "Zm9v", "mimeType": "image/png"},
                {"type": "text", "text": "second"}
            ],
            "isError": false
        });
        assert_eq!(extract_call_text(&ok).unwrap(), "first\nsecond");
        // isError=true → Err，详情取自文本
        let bad = json!({
            "content": [{"type": "text", "text": "disk full"}],
            "isError": true
        });
        assert!(extract_call_text(&bad).unwrap_err().to_string().contains("disk full"));
        // isError=true 且无文本 → 仍是 Err
        assert!(extract_call_text(&json!({"isError": true})).is_err());
        // 无 content → 空文本
        assert_eq!(extract_call_text(&json!({})).unwrap(), "");
    }

    #[test]
    fn parse_init_official_example() {
        // MCP 规范 lifecycle 一节的 initialize 响应示例
        let raw = r#"{
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "experimental": {},
                    "logging": {},
                    "prompts": {"listChanged": true},
                    "resources": {"subscribe": true, "listChanged": true},
                    "tools": {"listChanged": true}
                },
                "serverInfo": {"name": "mcp-server", "version": "1.6.2"}
            }
        }"#;
        let v: Value = serde_json::from_str(raw).unwrap();
        let (name, proto) = parse_init(&v["result"]).unwrap();
        assert_eq!(name, "mcp-server");
        assert_eq!(proto, "2024-11-05");
    }

    #[test]
    fn parse_tools_official_shape() {
        let raw = r#"{
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "tools": [
                    {
                        "name": "get_current_weather",
                        "description": "Get the current weather in a location",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"location": {"type": "string"}},
                            "required": ["location"]
                        }
                    },
                    {"name": "no_desc"}
                ]
            }
        }"#;
        let v: Value = serde_json::from_str(raw).unwrap();
        let tools = parse_tools(&v["result"]);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "get_current_weather");
        assert_eq!(tools[0].description, "Get the current weather in a location");
        assert_eq!(tools[0].input_schema["required"][0], "location");
        assert_eq!(tools[1].description, "");
        // 坏条目（缺 name）被跳过
        let broken = json!({"tools": [{"description": "no name"}, {"name": "ok"}]});
        assert_eq!(parse_tools(&broken).len(), 1);
    }

    // ---------- 分发层（duplex 模拟子进程，不起真进程） ----------

    /// 搭一条假 MCP 会话：请求方向 / 响应方向各一根 duplex 管道，由
    /// `responder` 扮演服务器（读到一行请求 → 决定回什么）。
    async fn fake_session(
        responder: impl FnOnce(&Value) -> Option<String> + Send + 'static,
    ) -> (Arc<Shared>, tokio::task::JoinHandle<()>) {
        let (client_w, mut server_r) = tokio::io::duplex(4096); // 请求：client → server
        let (mut server_w, client_r) = tokio::io::duplex(4096); // 响应：server → client
        let shared = Arc::new(Shared::new(Box::new(client_w)));
        tokio::spawn(read_loop(client_r, Arc::clone(&shared)));
        let handle = tokio::spawn(async move {
            let mut server_r = tokio::io::BufReader::new(server_r);
            let mut buf = Vec::new();
            if server_r.read_until(b'\n', &mut buf).await.unwrap_or(0) == 0 {
                return;
            }
            let Ok(req) = serde_json::from_slice::<Value>(&buf) else {
                return;
            };
            if let Some(line) = responder(&req) {
                let _ = server_w.write_all(line.as_bytes()).await;
            } else {
                // 永不响应的假服务器：驻留以保持管道存活，让客户端真正
                // 走到超时（否则管道关闭触发读任务 EOF，请求会以
                // "连接断开"而非超时失败）。
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
        (shared, handle)
    }

    #[tokio::test]
    async fn request_round_trip_via_dispatch() {
        let (shared, srv) = fake_session(|req| {
            Some(format!(
                "{}\n",
                json!({"jsonrpc": "2.0", "id": req["id"], "result": {"pong": true, "echoed": req["method"]}})
            ))
        })
        .await;
        let v = shared
            .request("ping", json!({"x": 1}), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(v["pong"], true);
        assert_eq!(v["echoed"], "ping"); // 请求行的 method 正确送达
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn rpc_error_maps_to_err() {
        let (shared, srv) = fake_session(|req| {
            Some(format!(
                "{}\n",
                json!({"jsonrpc": "2.0", "id": req["id"],
                       "error": {"code": -32601, "message": "no such tool"}})
            ))
        })
        .await;
        let err = shared
            .request("tools/call", json!({}), Duration::from_secs(5))
            .await
            .unwrap_err();
        // Rpc 错误不算连接级故障（不触发重连）
        assert!(matches!(err, ReqFail::Rpc(ref m) if m.contains("no such tool")));
        assert!(!err.is_fatal());
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn timeout_when_server_never_responds() {
        // 服务器读到请求但永不回复 → 极短超时
        let (shared, _srv) = fake_session(|_| None).await;
        let err = shared
            .request("slow", json!({}), Duration::from_millis(30))
            .await
            .unwrap_err();
        assert!(matches!(err, ReqFail::Timeout));
        assert!(err.is_fatal(), "超时是连接级故障");
        // 超时后登记已摘除（迟到响应不会误分发）
        assert!(shared.pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn death_fails_all_pending() {
        let (client_w, _keep_server_r) = tokio::io::duplex(4096);
        let (_keep_server_w, client_r) = tokio::io::duplex(4096);
        let shared = Arc::new(Shared::new(Box::new(client_w)));
        tokio::spawn(read_loop(client_r, Arc::clone(&shared)));
        let jh = {
            let s = Arc::clone(&shared);
            tokio::spawn(async move { s.request("x", json!({}), Duration::from_secs(30)).await })
        };
        // 等请求注册进 pending 表
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(shared.pending.lock().await.len(), 1);
        // 进程退出：die → 清空 pending（全部报错）+ 标记断开
        shared.die("MCP 服务器进程已退出").await;
        let res = jh.await.unwrap();
        assert!(matches!(&res, Err(ReqFail::Closed(m)) if m.contains("进程已退出")));
        assert!(!shared.alive.load(Ordering::SeqCst));
        assert!(shared.pending.lock().await.is_empty());
    }

    // ---------- 管理器 diff / 路由（不 spawn 任何进程） ----------

    fn cfg(id: &str, name: &str, enabled: bool) -> McpServerConfig {
        McpServerConfig {
            id: id.into(),
            name: name.into(),
            command: "cmd-that-does-not-exist".into(),
            args: vec![],
            env: vec![],
            enabled,
            timeout_ms: 1_000,
        }
    }

    #[tokio::test]
    async fn set_servers_diff_and_status() {
        let m = McpManager::new();
        m.set_servers(vec![cfg("a", "Server One", true), cfg("b", "Server-Two", false)]).await;
        let st = m.list_status().await;
        assert_eq!(st.len(), 2);
        assert_eq!(st[0].id, "a");
        assert!(!st[0].connected);
        assert!(st[0].tools.is_empty()); // 只报告现状，不拉起进程
        // 换清单：a 移除、b 保留（仍禁用）、c 新增
        m.set_servers(vec![cfg("b", "Server-Two", false), cfg("c", "third", true)]).await;
        let st = m.list_status().await;
        assert_eq!(st.len(), 2);
        assert!(st.iter().all(|s| s.id != "a"));
        assert!(st.iter().any(|s| s.id == "b"));
        // 清空
        m.remove_all().await;
        assert!(m.list_status().await.is_empty());
    }

    #[tokio::test]
    async fn route_prefers_longest_prefix() {
        let m = McpManager::new();
        m.set_servers(vec![cfg("a", "foo", true), cfg("b", "foo_bar", true)]).await;
        let (_, _, tool) = m.route("mcp_foo_bar_baz").await.unwrap();
        assert_eq!(tool, "baz"); // 归属 foo_bar（最长前缀），不是 foo + "_bar_baz"
        assert!(m.route("mcp_qux").await.is_err());
    }

    #[tokio::test]
    async fn unknown_tool_errors_without_spawning() {
        let m = McpManager::new();
        m.set_servers(vec![cfg("a", "Server One", true)]).await;
        let err = m.call_tool("mcp_totally_unknown", json!({})).await.unwrap_err();
        assert!(err.to_string().contains("未知的 MCP 工具"));
        assert!(m.all_tools().await.is_empty());
        assert!(m.connect("missing").await.is_err());
    }
}
