//! Local HTTP server: REST + SSE. Bound to 127.0.0.1 with a random port and a
//! per-run session token; every `/api/*` request is checked against it.

use crate::agent::{AgentRuntime, Attachment, QuestionBroker, TurnOptions};
use crate::config::{mask_key, Config, ProviderConfig};
use crate::event::EventHub;
use crate::permission::{Decision, PermissionBroker};
use crate::session::{SessionStore, SharedStore};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct AppState {
    pub token: String,
    pub hub: EventHub,
    pub config: Arc<RwLock<Config>>,
    pub broker: Arc<PermissionBroker>,
    pub questions: Arc<QuestionBroker>,
    pub store: SharedStore,
    pub agent: AgentRuntime,
    pub schedule: crate::schedule::SharedSchedule,
    pub subagents: crate::subagent::SharedSubagents,
    /// Persistent usage journal behind `GET /api/usage`.
    pub usage: Arc<crate::usage::UsageJournal>,
    /// Long-lived memories injected into every system prompt.
    pub memory: Arc<crate::memory::MemoryStore>,
    /// 后台终端会话表（右上角悬浮窗的「后台终端」分区）。
    pub terms: crate::term::TerminalRegistry,
    pub started_at: u64,
    /// Actual bound port (0 until `serve` resolves it); used by the Origin
    /// whitelist. Requests cannot arrive before it is set.
    pub port: std::sync::atomic::AtomicU16,
}

pub type SharedState = Arc<AppState>;

// ------------------------------------------------------------------ auth

// CHANGELOG（渐进收紧）：query token 属于凭证入 URL 的弱实践（会留在浏览器
// 历史、访问日志、Referer 里），计划废弃，后续以 cookie 会话方案替代落地。
// 当前只保留真正需要它的最小集合：
//   - `/?token=`（index）：桌面壳/启动链接拼接的唯一入口；
//   - `/browser-boot?token=`（browser_boot）：内置浏览器引导页同一口径；
//   - `/api/events?token=`（SSE）：兼容旧版 EventSource 客户端与 perf 脚本
//     （现行前端已改 fetch + X-Parable-Token 头，不再依赖）。
// 其余端点（sessions/agents/team/term/environment/telemetry 等）已收敛为仅
// 接受 `X-Parable-Token` 头。所有端点的 Origin/Referer 外来来源守卫见
// origin_guard —— 带 query token 的请求若 Referer 指向外部站点直接 403。

/// Query parameters that scope a request to one session. 不再携带 `token`：
/// 这些端点已收敛为仅接受 `X-Parable-Token` 头（见上方 CHANGELOG）。
#[derive(Debug, Deserialize)]
struct SessionQuery {
    #[serde(default)]
    session: Option<String>,
}

/// 常量时间比较:xor 折叠整个输入(长度差也折进结果),不逐字节提前返回,
/// 避免时序侧信道一点一点试出 token。
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u32;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
        diff |= x as u32;
    }
    diff == 0
}

fn check_auth(headers: &HeaderMap, token: Option<&str>, state: &AppState) -> bool {
    let from_header = headers
        .get("x-parable-token")
        .and_then(|v| v.to_str().ok())
        .map(|s| ct_eq(s, &state.token))
        .unwrap_or(false);
    let from_query = token.map(|t| ct_eq(t, &state.token)).unwrap_or(false);
    from_header || from_query
}

/// C-06：会话 id 字符白名单——只允许字母数字与 `-_`，≤64 字符，防路径穿越。
fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

// ------------------------------------------------------------------ local guards

/// DNS rebinding 防护:Host 必须是 `127.0.0.1:<port>` 或 `localhost:<port>`。
/// 恶意网页把 evil.com 解析到 127.0.0.1 时,Host 头会是 evil.com —— 直接 403。
fn host_allowed(host: &str, port: u16) -> bool {
    if port == 0 {
        return false;
    }
    let h = host.trim().to_ascii_lowercase();
    h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}")
}

/// Host 校验中间件,覆盖全部路由(含 `/`、/index.html 与 fallback 注入 token
/// 的静态页——页面里有明文 token,不能落到任意 Host 的请求面前)。
async fn host_guard(
    State(state): State<SharedState>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let allowed = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|h| host_allowed(h, state.port.load(std::sync::atomic::Ordering::Relaxed)))
        .unwrap_or(false);
    if !allowed {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "host not allowed"})),
        )
            .into_response();
    }
    next.run(req).await
}

/// Origins the desktop shell and a local browser tab may present. Everything
/// else (a hostile page driving the user's browser against 127.0.0.1) is
/// rejected before auth is even evaluated.
fn origin_allowed(origin: &str, port: u16) -> bool {
    const TAURI: [&str; 3] = [
        "tauri://localhost",
        "https://tauri.localhost",
        "http://tauri.localhost",
    ];
    if TAURI.contains(&origin) {
        return true;
    }
    if port == 0 {
        return false;
    }
    origin == format!("http://127.0.0.1:{port}") || origin == format!("http://localhost:{port}")
}

/// Reject cross-origin requests whose `Origin` is not whitelisted. Requests
/// without an `Origin` header (curl, same-origin GET, EventSource in Chromium)
/// pass through. Covers every route, not just `/api`: the static shell injects
/// the session token into the page, so a hostile origin must never see it.
///
/// P2 收紧：`Origin` 缺省时再看 `Referer`——带 `?token=` 的端点会被浏览器自动
/// 补 Referer，恶意页面驱动用户浏览器请求 127.0.0.1 时 Referer 是对方站点，
/// host 与白名单不符直接拒。Origin 与 Referer 都没有的直连（curl / 浏览器
/// 地址栏 / fetch+header 同源请求）放行，鉴权交给 token 本身。
async fn origin_guard(
    State(state): State<SharedState>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let port = state
        .port
        .load(std::sync::atomic::Ordering::Relaxed);
    if let Some(origin) = req.headers().get(axum::http::header::ORIGIN).and_then(|v| v.to_str().ok()) {
        if !origin_allowed(origin, port) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "origin not allowed"})),
            )
                .into_response();
        }
    } else if let Some(referer) = req
        .headers()
        .get(axum::http::header::REFERER)
        .and_then(|v| v.to_str().ok())
    {
        if !referer_allowed(referer, port) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "referer not allowed"})),
            )
                .into_response();
        }
    }
    next.run(req).await
}

/// `Referer` 守卫：取 `scheme://authority` 前缀按 Origin 白名单同一条规则校验。
/// 解析不出 origin（如 `Referer: <about:blank>` 一类非 http URL）一律拒绝。
fn referer_allowed(referer: &str, port: u16) -> bool {
    let Some((scheme, rest)) = referer.trim().split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if authority.is_empty() {
        return false;
    }
    origin_allowed(&format!("{scheme}://{authority}"), port)
}

/// P1：全局响应安全头。`nosniff` 覆盖所有响应（/api/* 的 JSON、HTML 页面、
/// 导出文件），防止浏览器把响应按嗅探出的类型解释成可执行内容。HTML 专属的
/// CSP / X-Frame-Options / Referrer-Policy 在 `index()` 与 `browser_boot()`
/// 里逐点加（那是仅有的两个 HTML 响应点）。
async fn security_headers_guard(req: axum::extract::Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    resp.headers_mut().insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({"error":"bad token"}))).into_response()
}

fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response()
}

fn server_error(e: impl std::fmt::Display) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": e.to_string()})),
    )
        .into_response()
}

fn not_found(what: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": format!("not found: {what}")})),
    )
        .into_response()
}

// ------------------------------------------------------------------ router

/// 上传上限:20MB 原始字节。
const MAX_UPLOAD_BYTES: usize = 20 * 1024 * 1024;

pub fn build_router(state: SharedState) -> Router {
    let api = Router::new()
        .route("/events", get(sse_events))
        .route("/chat", post(chat))
        .route("/interrupt", post(interrupt))
        .route("/permission", post(permission))
        .route("/answer", post(answer))
        .route("/config", get(get_config).patch(patch_config))
        .route("/providers", get(get_providers))
        .route("/providers/test", post(test_provider))
        // 从上游拉模型目录（OpenAI/Anthropic/Ollama 各自的清单端点）
        .route("/providers/{id}/models", get(list_provider_models))
        .route("/models", get(get_models))
        .route("/sessions", get(list_sessions))
        .route("/search/messages", get(search_messages))
        .route("/session/new", post(new_session))
        .route("/session/fork", post(fork_session))
        .route("/sessions/{id}", patch(patch_session).delete(delete_session))
        .route("/sessions/{id}/messages", get(session_messages).delete(truncate_messages))
        .route("/sessions/{id}/pending", get(list_pending))
        .route(
            "/sessions/{id}/pending/{pid}",
            patch(edit_pending).delete(delete_pending),
        )
        .route("/sessions/{id}/compact", post(compact_session))
        .route("/sessions/{id}/archive", post(archive_session))
        .route("/sessions/{id}/unarchive", post(unarchive_session))
        .route("/sessions/{id}/goal", get(get_goal).post(post_goal))
        .route("/sessions/{id}/prompt", get(session_prompt))
        .route(
            "/sessions/{id}/messages/{idx}",
            patch(edit_message).delete(delete_message),
        )
        // 重试某条用户提问：追加一条 hidden 指令后空 prompt 重跑模型，
        // 新回答追加在会话末尾 —— 不删除、不改写任何既有消息。
        .route("/sessions/{id}/messages/{idx}/retry", post(retry_message))
        .route("/status", get(status))
        .route("/environment", get(environment))
        .route("/telemetry", get(telemetry))
        .route("/usage", get(get_usage))
        .route("/tools", get(tools))
        .route("/schedule", get(list_tasks).post(create_task))
        // 运行历史必须注册在 /schedule/{id} 之前：静态段优先，显式提前更稳。
        .route("/schedule/runs", get(list_run_records))
        .route("/schedule/{id}", get(get_task).patch(edit_task).delete(delete_task))
        .route("/schedule/{id}/run", post(run_task_now))
        .route("/agents", get(list_agents))
        // 权限规则（permissions.toml）的读写，设置页用它编辑规则
        .route("/permissions", get(get_permissions).post(put_permissions))
        // 记忆：长期偏好/事实，注入每一轮的 system prompt
        .route("/memory", get(list_memory).post(save_memory).delete(clear_memory))
        .route("/memory/{id}", patch(update_memory).delete(delete_memory))
        // 技能：磁盘上的 SKILL.md，按 用户/项目 两个作用域
        .route("/skills", get(list_skills).post(save_skill))
        .route("/skills/{scope}/{id}", get(get_skill).delete(delete_skill))
        .route("/skills/{scope}/{id}/enabled", post(toggle_skill))
        // 智能体库（agent definitions）。挂在 /agents 下的静态分支 defs，
        // 与子代理目录的 /agents/{id}/... 动态分支互不干扰。
        .route("/agents/defs", get(list_agent_defs).post(save_agent_def))
        .route("/agents/defs/run", post(run_agent_def))
        .route("/agents/defs/{scope}/{id}", axum::routing::delete(delete_agent_def))
        .route("/agents/defs/{scope}/{id}/enabled", post(toggle_agent_def))
        .route("/agents/{id}/interrupt", post(interrupt_agent))
        // 向子代理发消息：运行中=steer 注入；已收尾=在其子会话续开新轮
        .route("/agents/{id}/say", post(agent_say))
        // 该会话当前是否还有 turn 在跑。前端用它核对"我以为在生成"的状态：
        // 如果服务端说没有在跑，前端就复位 busy（事件丢失时的自愈）。
        .route("/turn/running", get(turn_running))
        // 智能体团队：共享任务板 + 成员名册
        .route("/team", get(get_team))
        .route("/team/mode", post(set_team_mode))
        .route("/team/tasks", post(create_team_task))
        .route("/team/tasks/{id}", patch(update_team_task))
        .route("/plugins", get(list_plugins))
        .route("/plugins/{id}/toggle", post(toggle_plugin))
        .route("/plugins/{id}", axum::routing::delete(delete_plugin))
        // 会话数据治理：全量导出 / 危险区清空（设置 → 数据）
        .route("/sessions/export", get(export_sessions))
        .route("/sessions/import", post(import_sessions))
        .route("/sessions/clear-all", post(clear_all_sessions))
        // 多 Profile：一键把命名配置束套到当前配置上
        .route("/profiles/apply", post(apply_profile))
        // 文件检查点：列出某会话的可回退点（带逐文件统计）/ 单文件审查 diff / 执行回退
        .route("/sessions/{id}/checkpoints", get(list_checkpoints))
        .route("/sessions/{id}/checkpoint-diff", get(checkpoint_diff))
        .route("/sessions/{id}/rewind", post(rewind_session))
        // 内置浏览器 app 模式的引导页（无敏感信息、无需鉴权——本地 only）
        .route("/browser-boot", get(browser_boot))
        // MCP 服务器舰队：读取（含状态）/ 保存并应用 / 显式连接
        .route("/mcp", get(mcp_state))
        .route("/mcp", post(mcp_save))
        .route("/mcp/connect", post(mcp_connect))
        // 断线重连后恢复权限面板;聊天附件上传
        .route("/pending", get(pending_permissions))
        .route("/attachments", post(upload_attachment))
        .route("/files/reveal", post(reveal_file))
        // 设置 → 关于：用系统浏览器打开本机页面 / 停止服务进程
        .route("/service/open", post(service_open))
        .route("/open-external", post(open_external))
        .route("/shutdown", post(shutdown_service))
        // 侧边栏(side pane):文件浏览 / git 审查 / 浏览器 / 终端
        .route("/fs/tree", get(fs_tree))
        .route("/fs/file", get(fs_file))
        .route("/fs/search", get(fs_search))
        .route("/git/status", get(git_status))
        .route("/git/diff", get(git_diff))
        .route("/browser/state", get(browser_state))
        .route("/browser/meta", get(browser_meta))
        .route("/browser/text", get(browser_text))
        .route("/browser/navigate", post(browser_navigate))
        .route("/browser/scroll", post(browser_scroll))
        .route("/browser/input", post(browser_input))
        .route("/browser/viewport", post(browser_viewport))
        .route("/browser/stop", post(browser_stop))
        // 内置浏览器操作：后退 / 前进 / 刷新 / 侧栏点击穿透
        .route("/browser/back", post(browser_history_go::<-1>))
        .route("/browser/forward", post(browser_history_go::<1>))
        .route("/browser/reload", post(browser_reload))
        .route("/browser/click", post(browser_click))
        // 浏览器 cookie 导入：一键把 Chrome/Edge/Brave/Vivaldi/Firefox 的
        // 登录状态搬进内置浏览器（登录态 jar 持久化在 ~/.parable）。
        .route("/browser/cookies/sources", get(browser_cookie_sources))
        .route("/browser/cookies/import", post(browser_cookie_import))
        .route("/browser/cookies/clear", post(browser_cookie_clear))
        .route("/browser/cookies/paste", post(browser_cookie_paste))
        .route("/term/exec", post(term_exec))
        // 后台终端（右上角悬浮窗）：/term/exec 是单次同步命令，这一组是常驻
        // 会话——启动即返回、可看输出、可终止、刷新后能重新列出来。
        .route("/term/start", post(term_start))
        .route("/term/list", get(term_list))
        .route("/term/kill", post(term_kill))
        .route("/term/clear", post(term_clear))
        .route("/health", get(health))
        .with_state(state.clone());

    // Host / Origin 中间件覆盖全部路由(含 `/` 与 fallback 的静态页):token
    // 注入的 index.html 不该落到任意 Host/Origin 的请求面前。security_headers
    // 给全部响应补 nosniff。上传接口放宽 axum 默认 2MB body 限制到 20MB,
    // 超限在 handler 里回 413。
    Router::new()
        .nest("/api", api)
        .route("/", get(index))
        .route("/index.html", get(index))
        .fallback(get(index))
        .layer(axum::middleware::from_fn(security_headers_guard))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            origin_guard,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            host_guard,
        ))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .with_state(state)
}

// ------------------------------------------------------------------ index

#[derive(Debug, Deserialize)]
struct IndexQuery {
    token: Option<String>,
}

async fn index(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<IndexQuery>,
) -> Response {
    let html = match load_index() {
        Ok(h) => h,
        Err(e) => return server_error(format!("cannot load web/index.html: {e}")),
    };
    // C-04：裸页不再无差别内联 token。启动链接（main.rs 拼 `?token=…`）或
    // 已带凭证的请求通过 check_auth 后才把 token 注入页面；未鉴权请求只拿到
    // 无凭据外壳，避免本机任意进程靠裸页 harvest 明文 token。
    let authed = check_auth(&headers, q.token.as_deref(), &state);
    let html = if authed {
        html.replace(
            "<head>",
            &format!("<head><script>window.__PARABLE_TOKEN={:?};</script>", state.token),
        )
    } else {
        html
    };
    // 页面每次请求都是现读盘，绝不能被浏览器启发式缓存：
    // 一旦拿到旧 JS，刷新也还是旧行为（侧聊逐段叠行就是这么来的）。
    let mut resp = Html(html).into_response();
    resp.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store, must-revalidate"),
    );
    resp.headers_mut().insert(
        axum::http::header::PRAGMA,
        axum::http::HeaderValue::from_static("no-cache"),
    );
    apply_html_security_headers(resp.headers_mut());
    resp
}

/// P1：HTML 响应的 CSP / 反嵌入 / 引用策略。
///
/// CSP 允许面与页面里的 meta CSP 保持一致（单文件 UI 必须内联脚本，blob:
/// 截图、data: 字体、GitHub 更新检查都在用），头部策略是在 meta 之上的
/// 第二道闸（两条策略同时生效，取交集）；`frame-ancestors` 与 `form-action`
/// 是 meta CSP **不支持**的指令，只能靠响应头补：
///  - `frame-ancestors 'none'` + `X-Frame-Options: DENY`：拒绝被任意站点
///    iframe 嵌入（配合 Host/Origin 守卫，防点击劫持与嵌入驱动的 token 泄露）；
///  - `form-action 'self'`：页面表单只能提交回本服务，防表单劫持外传。
fn apply_html_security_headers(headers: &mut axum::http::HeaderMap) {
    const CSP: &str = "default-src 'self'; script-src 'self' 'unsafe-inline'; \
        style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; \
        font-src 'self' data:; connect-src 'self' https://api.github.com; \
        frame-ancestors 'none'; form-action 'self'; base-uri 'self'";
    headers.insert(
        axum::http::header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(
        axum::http::header::X_FRAME_OPTIONS,
        HeaderValue::from_static("DENY"),
    );
    headers.insert(
        axum::http::header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
}

fn load_index() -> std::io::Result<String> {
    // 桌面安装包不能依赖项目和 web/ 目录一定还在磁盘上；编译期嵌入一份
    // 兜底页面，开发时仍然优先读磁盘，改 HTML 刷新即可生效。
    const EMBEDDED: &str = include_str!("../../../web/index.html");
    let candidates = [
        std::path::PathBuf::from("web/index.html"),
        std::path::PathBuf::from("index.html"),
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../web/index.html"),
    ];
    for c in candidates {
        if c.exists() {
            return std::fs::read_to_string(c);
        }
    }
    // 安装后的桌面壳没有源码目录，使用内置页面；这样 exe 是自包含的。
    Ok(EMBEDDED.to_string())
}

// ------------------------------------------------------------------ sse

#[derive(Debug, Deserialize)]
struct EventsQuery {
    token: Option<String>,
    /// 客户端已收到的最大 `seq`。带上它表示「断线重连」：先把缓冲里
    /// `seq > since` 的事件补发一遍，再进入实时流。缺省 = 首次连接，不回放。
    #[serde(default)]
    since: Option<u64>,
    /// B-42：可选会话维度。带上后 backlog 与实时流只推该会话事件；缺省维持全局。
    #[serde(default)]
    session: Option<String>,
}

async fn sse_events(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<EventsQuery>,
) -> Response {
    if !check_auth(&headers, q.token.as_deref(), &state) {
        return unauthorized();
    }
    // 先订阅再取快照：两步之间发布的事件会同时躺在 backlog 和 channel 里，
    // 客户端按 seq 去重，宁可重复不可丢失。
    let sid = q.session.clone().unwrap_or_default();
    // 先订阅再取快照：两步之间发布的事件会同时躺在 backlog 和 channel 里，
    // 客户端按 seq 去重，宁可重复不可丢失。
    let (mut rx, backlog) = match q.since {
        Some(since) if since > 0 => {
            let (b, r) = state.hub.subscribe_since_scoped(since, if sid.is_empty() { None } else { Some(&sid) });
            (r, b)
        }
        _ => (state.hub.subscribe(), Vec::new()),
    };
    let state2 = state.clone();
    let replay = backlog.len();
    // 请求的 since 比环形缓冲里最老的一条还旧：中间那段已经蒸发了，光补发
    // 剩下的会让前端以为「没丢」。单独发一条 events.lost，前端收到后会以
    // 存储为准校正尾部（reconcileTail），而不是干等一个不会来的 turn.end。
    let gap = match (q.since, backlog.first()) {
        (Some(since), Some(first)) => since + 1 < first.seq,
        (Some(since), None) => since < state.hub.latest_seq(),
        _ => false,
    };
    let stream = async_stream::stream! {
        // Handshake so the frontend knows the pipe is live.
        let hello = json!({"type":"hello","sessionId": sid, "seq": 0, "ts": crate::event::now_ms(),
            "replay": replay, "latest": state.hub.latest_seq()});
        if let Ok(ev) = Event::default().json_data(&hello) {
            yield Ok::<_, Infallible>(ev);
        }
        if gap {
            let lost = json!({"type":"events.lost","sessionId": sid, "ts": crate::event::now_ms()});
            if let Ok(ev) = Event::default().json_data(&lost) {
                yield Ok::<_, Infallible>(ev);
            }
        }
        // 断线期间错过的事件：按原顺序补发。这一步让丢失的 `turn.end` /
        // 思考尾段还能回来，而不是永久停在「思考中」。
        for ev in backlog {
            if let Ok(sse) = Event::default().json_data(&ev) {
                yield Ok::<_, Infallible>(sse);
            }
        }
        loop {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(20)) => {
                    yield Ok(Event::default().comment("keepalive"));
                }
                item = rx.recv() => {
                    match item {
                        Ok(ev) => {
                            // A client may subscribe to one session or to all.
                            let accepts = sid.is_empty() || ev.session_id == sid || ev.session_id.is_empty();
                            if accepts {
                                if let Ok(sse) = Event::default().json_data(&ev) {
                                    yield Ok(sse);
                                }
                            }
                        }
                        // The client fell too far behind and events were dropped.
                        // Silently continuing is what caused permanently "stuck
                        // on thinking" frontends: the missing `turn.end` never
                        // arrived, so the UI never cleared its busy state.
                        // Tell the client to re-sync instead — it can rebuild
                        // the view from the store.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("SSE client lagged, {n} events dropped");
                            let gap = json!({
                                "type": "events.lost",
                                "sessionId": sid,
                                "seq": 0,
                                "ts": crate::event::now_ms(),
                                "dropped": n,
                            });
                            if let Ok(sse) = Event::default().json_data(&gap) {
                                yield Ok(sse);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
        let _ = &state2;
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

// ------------------------------------------------------------------ chat

#[derive(Debug, Deserialize)]
struct ChatReq {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    attachments: Vec<Attachment>,
    #[serde(default)]
    model: String,
    #[serde(default)]
    permission_mode: String,
    #[serde(default)]
    thinking_level: u32,
    #[serde(default)]
    plan_mode: bool,
    /// Explicit mode for this message while busy: "queue" | "steer". Empty
    /// falls back to the persisted followup_mode setting.
    #[serde(default)]
    delivery: String,
    /// Re-run the model on the current history without appending a new user
    /// message — the "regenerate" path after the client truncated the old
    /// assistant reply.
    #[serde(default)]
    retry: bool,
}

async fn chat(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<ChatReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    if req.retry {
        if req.session_id.is_empty() {
            return bad_request("retry requires an existing session_id");
        }
    } else if req.text.trim().is_empty() && req.attachments.is_empty() {
        return bad_request("text or attachments is required");
    }
    // permission_mode 白名单:只接受 ask|ws|full,空串=沿用配置默认。
    if !req.permission_mode.is_empty()
        && !crate::config::PERMISSION_MODES.contains(&req.permission_mode.as_str())
    {
        return bad_request("permission_mode must be ask|ws|full");
    }

    // 在创建会话之前先确认模型可用：没有 provider/API Key 时直接 409，
    // 前端能在发送请求的 catch 里立刻显示原因，不会留下空会话和停在
    // 「工作中」的假运行状态。
    if let Err(e) = state.agent.validate_model(&req.model).await {
        return (StatusCode::CONFLICT, Json(json!({"error": e.to_string()}))).into_response();
    }

    let sid = if req.session_id.is_empty() {
        let cfg = state.config.read().await;
        let ws = cfg.workspace.clone();
        let s = state.store.create(&ws, &title_from(&req.text)).await;
        state.hub.publish(&s.meta.id, "session.start", json!({
            "sessionId": s.meta.id,
            "title": s.meta.title,
            "workspace": s.meta.workspace,
        }));
        s.meta.id
    } else {
        req.session_id.clone()
    };

    let mode = if req.permission_mode.is_empty() {
        state.config.read().await.permission_mode.clone()
    } else {
        req.permission_mode.clone()
    };
    let opts = TurnOptions {
        model_sel: req.model.clone(),
        permission_mode: mode,
        // 前端永远发送 0-3:0=显式关闭,不再把 0 当"未指定"回退配置值
        thinking_level: req.thinking_level.min(3),
        plan_mode: req.plan_mode,
        ..TurnOptions::default()
    };

    // Regenerate never enqueues: it is meaningless without the running history
    // that the truncate path just wrote.
    if req.retry {
        return match state
            .agent
            .start_turn(&sid, "", req.attachments.clone(), opts.clone())
            .await
        {
            Ok(turn_id) => Json(json!({"sessionId": sid, "turnId": turn_id})).into_response(),
            Err(e) => (StatusCode::CONFLICT, Json(json!({"error": e.to_string()}))).into_response(),
        };
    }

    let default_mode = state.config.read().await.followup_mode.clone();
    let requested = match req.delivery.as_str() {
        "queue" | "steer" => req.delivery.clone(),
        _ => default_mode,
    };

    if state.agent.is_running(&sid).await {
        return enqueue_busy(&state, &sid, &req, opts, &requested).await;
    }

    match state
        .agent
        .start_turn(&sid, &req.text, req.attachments.clone(), opts.clone())
        .await
    {
        Ok(turn_id) => Json(json!({"sessionId": sid, "turnId": turn_id})).into_response(),
        Err(e) => {
            // A turn may have started between the check and start_turn. In that
            // case the correct behavior is the configured busy path, not a 409.
            if state.agent.is_running(&sid).await {
                enqueue_busy(&state, &sid, &req, opts, &requested).await
            } else {
                (StatusCode::CONFLICT, Json(json!({"error": e.to_string()}))).into_response()
            }
        }
    }
}

async fn enqueue_busy(
    state: &SharedState,
    sid: &str,
    req: &ChatReq,
    opts: TurnOptions,
    requested: &str,
) -> Response {
    match state
        .agent
        .enqueue_input(
            sid,
            &req.text,
            req.attachments.clone(),
            opts,
            requested,
        )
        .await
    {
        Ok(item) => Json(json!({
            "sessionId": sid,
            "queued": true,
            "pendingId": item.id,
            "mode": item.kind,
        }))
        .into_response(),
        Err(e) => (StatusCode::CONFLICT, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct PendingEditReq {
    #[serde(default)]
    text: String,
}

async fn list_pending(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    Json(json!({"pending": state.agent.pending_inputs(&id).await})).into_response()
}

async fn edit_pending(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path((id, pending_id)): Path<(String, String)>,
    Json(req): Json<PendingEditReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    if req.text.trim().is_empty() {
        return bad_request("pending text is required");
    }
    match state
        .agent
        .update_pending_input(&id, &pending_id, &req.text)
        .await
    {
        Some(item) => Json(json!({"ok": true, "pending": item})).into_response(),
        None => not_found("pending message"),
    }
}

async fn delete_pending(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path((id, pending_id)): Path<(String, String)>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    match state.agent.remove_pending_input(&id, &pending_id).await {
        Some(item) => Json(json!({"ok": true, "pending": item})).into_response(),
        None => not_found("pending message"),
    }
}

fn title_from(text: &str) -> String {
    let t = text.trim();
    if t.is_empty() {
        return "New chat".into();
    }
    let mut out = String::new();
    for (i, ch) in t.chars().enumerate() {
        if i >= 32 {
            out.push('…');
            break;
        }
        if ch == '\n' {
            break;
        }
        out.push(ch);
    }
    out
}

#[derive(Debug, Deserialize)]
struct SessionRef {
    #[serde(default)]
    session_id: String,
}

async fn interrupt(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<SessionRef>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let ok = state.agent.interrupt(&r.session_id).await;
    Json(json!({"interrupted": ok})).into_response()
}

#[derive(Debug, Deserialize)]
struct PermReq {
    call_id: String,
    decision: String,
}

async fn permission(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<PermReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let Some(d) = Decision::from_str(&r.decision) else {
        return bad_request("decision must be once|always|deny");
    };
    let ok = state.broker.resolve(&r.call_id, d).await;
    Json(json!({"resolved": ok})).into_response()
}

#[derive(Debug, Deserialize)]
struct AnswerReq {
    call_id: String,
    answer: String,
}

async fn answer(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<AnswerReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let ok = state.questions.answer(&r.call_id, &r.answer).await;
    Json(json!({"answered": ok})).into_response()
}

// ------------------------------------------------- attachments & pending

/// GET /api/pending:当前挂起的权限请求。前端断线重连后用它恢复面板,
/// 字段形状与 permission.request 事件一致(`id` 即事件里的 `callId`)。
async fn pending_permissions(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let list = state.broker.pending_list().await;
    Json(json!({"permissions": list})).into_response()
}

/// POST /api/attachments?name=<urlencoded filename>:body 为原始字节
/// (octet-stream),与其他 POST 一致用 X-Parable-Token 鉴权。落盘到
/// `~/.parable/uploads/`,返回的 `path` 直接可被 ChatReq.attachments
/// (`Attachment{path,name,size}`)引用。
async fn upload_attachment(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
    body: axum::body::Bytes,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let Some(name) = q
        .get("name")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return bad_request("name query parameter is required");
    };
    if body.len() > MAX_UPLOAD_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({
                "error": format!("attachment too large: {} bytes (limit 20 MB)", body.len())
            })),
        )
            .into_response();
    }
    let dir = crate::config::Config::uploads_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return server_error(e);
    }
    // 时间戳前缀防同名覆盖;原名只取文件名部分并清洗非法字符,防路径穿越
    let fname = format!("{}-{}", crate::event::now_ms(), sanitize_filename(&name));
    let path = dir.join(&fname);
    if let Err(e) = std::fs::write(&path, &body) {
        return server_error(e);
    }
    let kind = if is_image_name(&name) { "image" } else { "file" };
    Json(json!({
        "path": path.to_string_lossy(),
        "kind": kind,
        "name": name,
        "size": body.len(),
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct RevealReq {
    path: String,
}

/// POST /api/files/reveal:在系统文件管理器里打开文件所在位置
/// （Windows 资源管理器定位到文件、macOS Finder、Linux 目录管理器）。
/// 附件芯片点击用——文件在 `~/.parable/uploads/` 或工作区内。
#[derive(Debug, Deserialize)]
struct OpenUrlReq {
    /// 要交给系统浏览器的完整地址（已带 token）。
    url: String,
}

// spawn_system_browser 在 crate::computer 里（computer_navigate 的
// target=system 也用它），这里直接引用。

/// POST /api/service/open —— 用系统默认浏览器打开本服务的页面。
///
/// 设置 → 关于 →「在浏览器中打开」用它：桌面壳里的内嵌 WebView 与浏览器标签
/// 是两个独立前端，用户想在浏览器里用同一份会话时不需要自己去翻端口号。
/// C-05：回环地址判定——解析 authority 取 host 比较，显式拒绝带 userinfo
///（`user:pass@host`）的形似回环地址，避免 `http://127.0.0.1:80@evil.com/`
/// 这类前缀绕过。
fn is_loopback_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://")) else {
        return false;
    };
    // authority 到第一个 /、?、# 结束（"?token=..." 直接跟在 host:port 后时
    // 没有 path，旧实现 split('/') 会把 query 一起算进 authority）。
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if authority.contains('@') {
        return false;
    }
    // 剥端口：旧实现 rsplit(':').next() 取到的是端口号而不是主机名——
    // 桌面壳的 location.origin 永远带端口（127.0.0.1:7789），于是 host 被
    // 解析成 "7789"，回环校验恒失败，「在浏览器中打开」点了没反应。
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped.split(']').next().unwrap_or(stripped).to_string()
    } else {
        match authority.rsplit_once(':') {
            Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
                h.to_string()
            }
            _ => authority.to_string(),
        }
    };
    let host = host.to_ascii_lowercase();
    matches!(host.as_str(), "localhost" | "::1")
        || host
            .parse::<std::net::Ipv4Addr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
        || host
            .parse::<std::net::Ipv6Addr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

async fn service_open(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<OpenUrlReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let url = r.url.trim();
    // 只允许打开本机地址：这个接口是「打开发布页」，不是通用 URL 启动器。
    if !is_loopback_url(url) {
        return bad_request("only loopback URLs may be opened from here");
    }
    match crate::computer::spawn_system_browser(url) {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => server_error(e),
    }
}

/// POST /api/open-external —— 用户点外链时交给系统默认浏览器。
///
/// 与 `service_open` 的区别是这里接受任意 http/https 地址：它是 WebView
/// 打不开新标签（没有 new-window handler）时的唯一出路。仍然只认
/// http/https，避免这个接口被当成任意命令启动器使用。
async fn open_external(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<OpenUrlReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let url = r.url.trim();
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return bad_request("only http(s) URLs may be opened");
    }
    // 控制字符会把 cmd /C start 拆成多条命令，必须在拼进 shell 前挡掉。
    if url.chars().any(|c| c.is_control()) {
        return bad_request("invalid URL");
    }
    match crate::computer::spawn_system_browser(url) {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => server_error(e),
    }
}

/// POST /api/shutdown —— 停掉本地服务进程。
///
/// 先回响应再退出：直接 `exit` 会让客户端拿到连接被重置而不是「已停止」。
async fn shutdown_service(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    tracing::info!("shutdown requested from the UI");
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        std::process::exit(0);
    });
    Json(json!({"ok": true})).into_response()
}

async fn reveal_file(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<RevealReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let p = std::path::PathBuf::from(r.path.trim());
    if r.path.trim().is_empty() {
        return bad_request("path is required");
    }
    if !p.exists() {
        return bad_request("file not found");
    }
    // C-18：只允许定位工作区或 ~/.parable/uploads 下的文件，避免借资源管理器
    // 暴露本机任意路径的文件名/位置。
    let uploads = crate::config::Config::uploads_dir();
    let mut allowed_roots: Vec<std::path::PathBuf> = vec![uploads];
    for s in state.store.list("").await {
        if !s.workspace.is_empty() {
            allowed_roots.push(std::path::PathBuf::from(s.workspace));
        }
    }
    // 当前配置的工作区即使还没开过任何会话，也要允许定位：文件树右键
    // 「在文件夹中显示」在首会话之前就可用，否则会以「不在工作区」被拒。
    {
        let cfg = state.config.read().await;
        if !cfg.workspace.is_empty() {
            allowed_roots.push(std::path::PathBuf::from(&cfg.workspace));
        }
    }
    let within = allowed_roots
        .iter()
        .any(|root| crate::permission::inside_workspace(root, &p));
    if !within {
        return bad_request("路径不在工作区或上传目录内，已拒绝定位");
    }
    #[cfg(windows)]
    let spawned = std::process::Command::new("explorer")
        .arg(format!("/select,{}", p.display()))
        .spawn();
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open")
        .arg("-R")
        .arg(&p)
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let spawned = std::process::Command::new("xdg-open")
        .arg(p.parent().unwrap_or(std::path::Path::new("/")))
        .spawn();
    match spawned {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => server_error(crate::error::HarnessError::Io(e)),
    }
}

/// 只取最后一段文件名(剥掉任何目录成分),替换文件系统非法字符,去掉结尾
/// 的点/空白(Windows 语义),并控制长度。
fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("").trim();
    let mut out = String::new();
    for c in base.chars().take(120) {
        match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => out.push('_'),
            c if (c as u32) < 32 => out.push('_'),
            c => out.push(c),
        }
    }
    let out = out.trim().trim_end_matches('.');
    if out.is_empty() {
        "upload".into()
    } else {
        out.into()
    }
}

/// 图片判断:先看扩展名 jpg/jpeg/png/gif/webp/bmp(魔数校验可选,交给读取侧)。
fn is_image_name(name: &str) -> bool {
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp")
}

// ------------------------------------------------------------------ config

async fn get_config(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let cfg = state.config.read().await;
    // 历史遗留值在回显侧也归一（让 UI 立即看到正确 context）

    let mut v = serde_json::to_value(&*cfg).unwrap_or_default();
    // Never hand the raw API keys to the UI.
    if let Some(arr) = v.get_mut("providers").and_then(|p| p.as_array_mut()) {
        for p in arr {
            let masked = p.get("key").and_then(|x| x.as_str()).map(mask_key);
            if let Some(m) = masked {
                let has = !m.is_empty();
                p["key"] = json!(m);
                p["hasKey"] = json!(has);
            }
        }
    }
    // P3：MCP 服务器的 env / headers value 常含令牌（npx 的 API key、http 传
    // 输的鉴权头），回显同样打码。写回侧由 merge_config 的 mask 标记规则兜底
    // （含 `…`/`•` 的值不覆盖真值），masked echo 不会落盘。
    if let Some(servers) = v.get_mut("mcp_servers").and_then(|s| s.as_array_mut()) {
        for s in servers {
            for field in ["env", "headers"] {
                if let Some(pairs) = s.get_mut(field).and_then(|x| x.as_array_mut()) {
                    for pair in pairs.iter_mut() {
                        if let Some(val) = pair.get(1).and_then(|x| x.as_str()).map(mask_key) {
                            pair[1] = json!(val);
                        }
                    }
                }
            }
        }
    }
    if let Some(k) = v.get_mut("search_key") {
        let masked = k.as_str().map(mask_key).unwrap_or_default();
        let has = !masked.is_empty();
        *k = json!(masked);
        v["hasSearchKey"] = json!(has);
    }
    if let Some(k) = v.get_mut("anysearch_key") {
        let masked = k.as_str().map(mask_key).unwrap_or_default();
        let has = !masked.is_empty();
        *k = json!(masked);
        v["hasAnySearchKey"] = json!(has);
    }
    Json(v).into_response()
}

async fn patch_config(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(v): Json<Value>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let mut cfg = state.config.write().await;
    // 别的进程可能刚改过配置（桌面壳 + 浏览器开发网关共用一份 config.toml）：
    // 先取盘上最新状态再合并本次 PATCH，否则旧快照落盘会把对方新增的
    // 供应商整段抹掉——这就是「重启后配置好的供应商消失」的根因。
    cfg.refresh_from_disk();
    // 先处理删除标记({id, deleted:true}),再做字段合并。
    crate::config::apply_patch_deletions(&mut cfg, &v);
    merge_config(&mut cfg, &v);
    // 历史遗留非法值归一（如 context=0），写盘前统一修复
    cfg.normalize();
    // 浏览器无头开关即时生效（切到可见/无头都重建实例）
    state
        .agent
        .computer
        .set_headless(cfg.browser_headless);
    // 代理设置即时生效（下次请求用新 client）
    crate::config::set_proxy(cfg.http_proxy.clone(), cfg.http_no_proxy.clone());
    if let Err(e) = cfg.save() {
        return server_error(e);
    }
    Json(json!({"ok": true})).into_response()
}

/// Merge a partial config update. `key` is only replaced when it is non-empty,
/// so the masked echo coming back from the UI never clobbers the real secret.
fn merge_config(cfg: &mut Config, v: &Value) {
    if let Some(o) = v.as_object() {
        for (k, val) in o {
            match k.as_str() {
                "name" => cfg.name = val.as_str().unwrap_or("").to_string(),
                "lang" => cfg.lang = val.as_str().unwrap_or("zh").to_string(),
                "theme" => cfg.theme = val.as_str().unwrap_or("auto").to_string(),
                "font" => cfg.font = val.as_str().unwrap_or("serif").to_string(),
                "size" => cfg.size = val.as_u64().unwrap_or(16) as u32,
                // 排版个性化：字号 / 行距 / 代码字号 / 密度。范围在此收口，
                // 前端滑块再离谱也写不进一份没法看的配置。
                "ui_size" | "uiSize" => {
                    cfg.ui_size = val.as_u64().unwrap_or(13).clamp(11, 18) as u32
                }
                "line_height" | "lineHeight" => {
                    cfg.line_height = (val.as_f64().unwrap_or(1.5) as f32).clamp(1.2, 2.4)
                }
                "code_size" | "codeSize" => {
                    cfg.code_size = val.as_u64().unwrap_or(13).clamp(10, 20) as u32
                }
                "density" => {
                    let s = val.as_str().unwrap_or("cozy").to_string();
                    if ["airy", "cozy", "compact"].contains(&s.as_str()) {
                        cfg.density = s;
                    }
                }
                "width" => cfg.width = val.as_u64().unwrap_or(760) as u32,
                "accent" => cfg.accent = val.as_str().unwrap_or("#d97757").to_string(),
                "speed" => cfg.speed = val.as_u64().unwrap_or(22) as u32,
                "cd" => cfg.cd = val.as_u64().unwrap_or(650) as u32,
                "blur" => cfg.blur = val.as_u64().unwrap_or(8) as u32,
                "sf" => cfg.sf = val.as_f64().unwrap_or(1.5) as f32,
                "anim" => cfg.anim = val.as_f64().unwrap_or(1.0) as f32,
                "stream" => cfg.stream = val.as_bool().unwrap_or(true),
                "think" => cfg.think = val.as_bool().unwrap_or(true),
                "side" => cfg.side = val.as_bool().unwrap_or(true),
                "prefs" => cfg.prefs = val.as_str().unwrap_or("").to_string(),
                "model" => cfg.model = val.as_str().unwrap_or("").to_string(),
                "permissionMode" | "permission_mode" => {
                    let s = val.as_str().unwrap_or("ask").to_string();
                    if crate::config::PERMISSION_MODES.contains(&s.as_str()) {
                        cfg.permission_mode = s;
                    }
                }
                // 0-3 直传:0=显式关闭,不回退默认值
                "thinkingLevel" | "thinking_level" => {
                    cfg.thinking_level = val.as_u64().unwrap_or(2).min(3) as u32
                }
                "workspace" => {
                    let s = val.as_str().unwrap_or("").trim().to_string();
                    // P3：空串保留「恢复默认工作区」语义；非空值走 C-21 校验，
                    // 不存在/不可读时回退旧值（resolve 内部会 tracing::warn），
                    // 防止把拼错或已删除的路径写进 config.toml。
                    if s.is_empty() {
                        cfg.workspace = String::new();
                    } else {
                        cfg.workspace = resolve_requested_workspace(&s, &cfg.workspace);
                    }
                }
                // Same masking rule as provider keys: an echo containing the
                // mask marker never overwrites the stored secret.
                "searchKey" | "search_key" => {
                    let s = val.as_str().unwrap_or("").to_string();
                    if !s.contains('…') && !s.contains('•') {
                        cfg.search_key = s;
                    }
                }
                "anysearchKey" | "anysearch_key" => {
                    let s = val.as_str().unwrap_or("").to_string();
                    if !s.contains('…') && !s.contains('•') {
                        cfg.anysearch_key = s;
                    }
                }
                "searchPref" | "search_pref" => {
                    let s = val.as_str().unwrap_or("auto");
                    if matches!(s, "auto" | "deepseek" | "anysearch" | "multi") {
                        cfg.search_pref = s.to_string();
                    }
                }
                "followupMode" | "followup_mode" => {
                    let s = val.as_str().unwrap_or("queue");
                    if matches!(s, "queue" | "steer") {
                        cfg.followup_mode = s.to_string();
                    }
                }
                // ---- Agent 行为 ----
                "compactionEnabled" | "compaction_enabled" => {
                    cfg.compaction_enabled = val.as_bool().unwrap_or(true);
                }
                "compactionPct" | "compaction_pct" => {
                    cfg.compaction_pct = val.as_u64().unwrap_or(85).clamp(50, 95) as u32;
                }
                "maxSteps" | "max_steps" => {
                    cfg.max_steps = val.as_u64().unwrap_or(100).clamp(20, 500) as u32;
                }
                "askAutoResolve" | "ask_auto_resolve" => {
                    cfg.ask_auto_resolve = val.as_bool().unwrap_or(false);
                }
                "keepAwake" | "keep_awake" => {
                    cfg.keep_awake = val.as_bool().unwrap_or(false);
                }
                "ctxWarnPct" | "ctx_warn_pct" => {
                    cfg.ctx_warn_pct = val.as_u64().unwrap_or(80).clamp(50, 95) as u32;
                }
                "browserHeadless" | "browser_headless" => {
                    cfg.browser_headless = val.as_bool().unwrap_or(true);
                }
                "linkTarget" | "link_target" => {
                    // 只接受已知值：未知值原样落盘会让前端路由到不存在的分支。
                    let s = val.as_str().unwrap_or("side");
                    cfg.link_target = if matches!(s, "side" | "window" | "system") {
                        s.to_string()
                    } else {
                        "side".to_string()
                    };
                }
                // ---- 通知 ----
                "notify" => cfg.notify = val.as_bool().unwrap_or(true),
                "notifySound" | "notify_sound" => {
                    cfg.notify_sound = val.as_bool().unwrap_or(false);
                }
                // ---- 会话管理 ----
                "autoArchiveDays" | "auto_archive_days" => {
                    cfg.auto_archive_days = val.as_u64().unwrap_or(0).clamp(0, 365) as u32;
                }
                // ---- 代码块 ----
                "codeLineNumbers" | "code_line_numbers" => {
                    cfg.code_line_numbers = val.as_bool().unwrap_or(true);
                }
                "codeWrap" | "code_wrap" => {
                    cfg.code_wrap = val.as_bool().unwrap_or(false);
                }
                // ---- 网络与隐私 ----
                "httpProxy" | "http_proxy" => {
                    cfg.http_proxy = val.as_str().unwrap_or("").trim().to_string();
                }
                "httpNoProxy" | "http_no_proxy" => {
                    cfg.http_no_proxy = val.as_str().unwrap_or("").trim().to_string();
                }
                "modelIoLog" | "model_io_log" => {
                    cfg.model_io_log = val.as_bool().unwrap_or(false);
                }
                // ---- 输入行为 ----
                "enterMode" | "enter_mode" => {
                    let s = val.as_str().unwrap_or("enter");
                    if matches!(s, "enter" | "ctrl") {
                        cfg.enter_mode = s.to_string();
                    }
                }
                // ---- 数据治理 ----
                "sessionRetentionDays" | "session_retention_days" => {
                    cfg.session_retention_days =
                        val.as_u64().unwrap_or(0).clamp(0, 3650) as u32;
                }
                // ---- hooks ----
                "hooks" => {
                    if let Ok(hs) = serde_json::from_value(val.clone()) {
                        cfg.hooks = hs;
                    }
                }
                // ---- MCP ----
                "mcpServers" | "mcp_servers" => {
                    if let Ok(mut ms) = serde_json::from_value::<Vec<crate::mcp::McpServerConfig>>(val.clone()) {
                        // GET /api/config 的回显是打码过的（mask_key 的 `…`/`•`）。
                        // 含掩码标记的 env/headers 值不覆盖已存的真值——与
                        // provider key / search key 同一条规则，防止 UI 原样
                        // PATCH 回来时把真机密换成掩码串。
                        for s in &mut ms {
                            let old = cfg.mcp_servers.iter().find(|x| x.id == s.id);
                            if let Some(old) = old {
                                restore_masked_pairs(&mut s.env, &old.env);
                                restore_masked_pairs(&mut s.headers, &old.headers);
                            }
                        }
                        cfg.mcp_servers = ms;
                    }
                }
                // ---- 模型路由 ----
                // （「自动选模型」开关已移除：模型一律由用户显式选择；
                //  fallbackChain 的失败降级仍可配置。autoModel 键按未知键忽略。）
                "fallbackChain" | "fallback_chain" => {
                    let chain: Vec<String> = val
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    cfg.fallback_chain = chain;
                    // 去重 + 截断到 5 条在 normalize 里收口。
                    cfg.normalize();
                }
                // ---- profiles ----
                "profiles" => {
                    if let Ok(ps) = serde_json::from_value(val.clone()) {
                        cfg.profiles = ps;
                    }
                }
                "activeProfile" | "active_profile" => {
                    cfg.active_profile = val.as_str().unwrap_or("").to_string();
                }
                "systemPrompt" | "system_prompt" => {
                    cfg.system_prompt = val.as_str().unwrap_or("").to_string()
                }
                "systemPromptRaw" | "system_prompt_raw" => {
                    cfg.system_prompt_raw = val.as_str().unwrap_or("").to_string()
                }
                "providers" => {
                    if let Some(arr) = val.as_array() {
                        for p in arr {
                            merge_provider(cfg, p);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn merge_provider(cfg: &mut Config, v: &Value) {
    let Some(id) = v.get("id").and_then(|x| x.as_str()) else {
        return;
    };
    if let Some(p) = cfg.providers.iter_mut().find(|p| p.id == id) {
        if let Some(name) = v.get("name").and_then(|x| x.as_str()) {
            if !name.is_empty() {
                p.name = name.to_string();
            }
        }
        if let Some(c) = v.get("color").and_then(|x| x.as_str()) {
            p.color = c.to_string();
        }
        if let Some(i) = v.get("icon").and_then(|x| x.as_str()) {
            p.icon = i.to_string();
        }
        if let Some(b) = v.get("enabled").and_then(|x| x.as_bool()) {
            p.enabled = b;
        }
        if let Some(u) = v.get("baseUrl").and_then(|x| x.as_str()).or_else(|| v.get("base_url").and_then(|x| x.as_str())) {
            if !u.is_empty() {
                p.base_url = u.to_string();
            }
        }
        if let Some(a) = v.get("api").and_then(|x| x.as_str()) {
            if !a.is_empty() {
                p.api = a.to_string();
            }
        }
        if let Some(k) = v.get("key").and_then(|x| x.as_str()) {
            // A masked key (contains …) must not overwrite the stored one.
            if !k.is_empty() && !k.contains('…') && !k.contains('•') {
                p.key = k.to_string();
            }
        }
        if let Some(models) = v.get("models").and_then(|x| x.as_array()) {
            for m in models {
                let Some(mid) = m.get("id").and_then(|x| x.as_str()) else {
                    continue;
                };
                // 删除标记已经由 apply_patch_deletions 处理过了：这里必须跳过，
                // 否则被删掉的模型会以一份默认字段重新追加回来——表现就是
                // 「删了 / 改完之后又冒出一个同名条目」。
                if m.get("deleted").and_then(|x| x.as_bool()) == Some(true) {
                    continue;
                }
                // 改名：编辑器会把旧 id 放在 oldId 里。先把旧条目摘掉，再按新 id 落库，
                // 否则 upsert 只认 id，旧的那条会原样留着，看起来像「没生效 + 复制了一份」。
                if let Some(old) = m.get("oldId").and_then(|x| x.as_str()) {
                    if !old.is_empty() && old != mid {
                        p.models.retain(|x| x.id != old);
                    }
                }
                if let Some(mc) = p.models.iter_mut().find(|x| x.id == mid) {
                    if let Some(n) = m.get("name").and_then(|x| x.as_str()) {
                        mc.name = n.to_string();
                    }
                    if let Some(c) = m.get("context").and_then(|x| x.as_u64()) {
                        mc.context = c;
                    }
                    if let Some(c) = m.get("max_output").and_then(|x| x.as_u64()) {
                        mc.max_output = c;
                    }
                    if let Some(t) = m.get("vision").and_then(|x| x.as_bool()) {
                        mc.vision = t;
                    }
                    if let Some(t) = m.get("tools").and_then(|x| x.as_bool()) {
                        mc.tools = t;
                    }
                    if let Some(t) = m.get("thinking").and_then(|x| x.as_bool()) {
                        mc.thinking = t;
                    }
                    if let Some(t) = m.get("structured").and_then(|x| x.as_bool()) {
                        mc.structured = t;
                    }
                    if let Some(t) = m.get("native_search").and_then(|x| x.as_bool()) {
                        mc.native_search = t;
                    }
                    if let Some(t) = m.get("enabled").and_then(|x| x.as_bool()) {
                        mc.enabled = t;
                    }
                    if let Some(l) = m.get("levels").and_then(|x| x.as_array()) {
                        mc.levels = l
                            .iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect();
                    }
                    if let Some(x) = m.get("mapping").and_then(|x| x.as_str()) {
                        mc.mapping = x.to_string();
                    }
                } else if let Ok(mc) = serde_json::from_value::<crate::config::ModelConfig>(m.clone()) {
                    p.models.push(mc);
                }
            }
            // 同一个 id 只保留最后一条：历史 PATCH 已经可能写进重复项，顺手清掉。
            let mut seen = std::collections::HashSet::new();
            let mut kept: Vec<crate::config::ModelConfig> = Vec::new();
            for mc in p.models.drain(..).rev() {
                if seen.insert(mc.id.clone()) {
                    kept.push(mc);
                }
            }
            kept.reverse();
            p.models = kept;
        }
    } else if let Ok(p) = serde_json::from_value::<ProviderConfig>(v.clone()) {
        cfg.providers.push(p);
    }
}

/// MCP env/headers 的掩码回显保护：PATCH 里含掩码标记（`…` / `•`）的 value
/// 用已存的真值顶回去，同 key 才顶（key 变了就是用户在改条目，照单全收）。
/// 与 provider key / search key 的 mask-echo 规则一致。
fn restore_masked_pairs(incoming: &mut [(String, String)], stored: &[(String, String)]) {
    for (k, v) in incoming.iter_mut() {
        if (v.contains('…') || v.contains('•')) && !v.is_empty() {
            if let Some((_, real)) = stored.iter().find(|(sk, _)| sk == k) {
                *v = real.clone();
            }
        }
    }
}

// ------------------------------------------------------------------ providers

async fn get_providers(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let cfg = state.config.read().await;
    let list: Vec<Value> = cfg
        .providers
        .iter()
        .map(|p| {
            let mut v = serde_json::to_value(p.masked()).unwrap_or_default();
            // 免费通道不需要 key（匿名），hasKey=true 以免 UI 把它标成缺配置。
            v["hasKey"] = json!(!p.key.is_empty() || p.api == "opencode-free");
            v
        })
        .collect();
    Json(json!({"providers": list})).into_response()
}

#[derive(Debug, Deserialize)]
struct TestReq {
    provider_id: String,
    /// Test this specific model instead of the provider's first enabled one.
    #[serde(default)]
    model: String,
}

/// GET /api/providers/{id}/models —— 从上游拉取该供应商的模型目录。
async fn list_provider_models(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let cfg = state.config.read().await;
    let Some(p) = cfg.providers.iter().find(|p| p.id == id) else {
        return bad_request("unknown provider");
    };
    // 用未打码的 key 去问上游（上游要的就是真 key）。
    match crate::models::list_upstream(p).await {
        Ok(models) => Json(json!({ "models": models })).into_response(),
        Err(e) => bad_request(&e.to_string()),
    }
}

async fn test_provider(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<TestReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let (base, api, key, model_id, thinking) = {
        let cfg = state.config.read().await;
        let Some(p) = cfg.providers.iter().find(|p| p.id == r.provider_id) else {
            return bad_request("unknown provider");
        };
        // 指定模型就测它，否则测第一个启用的模型。
        let chosen = if !r.model.is_empty() {
            p.models.iter().find(|m| m.id == r.model)
        } else {
            p.models.iter().find(|m| m.enabled)
        };
        let m = chosen.map(|m| m.id.clone()).unwrap_or_default();
        let t = chosen.map(|m| m.thinking).unwrap_or(false);
        (p.clone(), p.api.clone(), p.key.clone(), m, t)
    };
    let probe = crate::config::ProviderConfig {
        api: api.clone(),
        base_url: base.base_url.clone(),
        key: key.clone(),
        ..base.clone()
    };
    let bound: Box<dyn crate::provider::Provider> = match api.as_str() {
        "anthropic" => Box::new(
            crate::provider::anthropic::AnthropicProvider::new(&probe)
                .with_model(&model_id, thinking),
        ),
        "openai" => Box::new(crate::provider::openai::chat(&probe).with_model(&model_id)),
        "opencode-free" => Box::new(
            crate::provider::opencode_free::OpenCodeFree::new(&probe).with_model(&model_id),
        ),
        _ => Box::new(crate::provider::openai::responses(&probe).with_model(&model_id)),
    };
    match bound.test().await {
        Ok(report) => Json(json!(report)).into_response(),
        Err(e) => server_error(e),
    }
}

async fn get_models(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let cfg = state.config.read().await;
    let mut out = Vec::new();
    for p in cfg.providers.iter().filter(|p| p.enabled) {
        for m in p.models.iter().filter(|m| m.enabled) {
            out.push(json!({
                "value": format!("{}/{}", p.id, m.id),
                "label": m.name,
                "provider": p.name,
                "context": m.context,
                "vision": m.vision,
                "thinking": m.thinking,
            }));
        }
    }
    Json(json!({"models": out, "current": cfg.model})).into_response()
}

// ------------------------------------------------------------------ sessions

async fn list_sessions(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    // query token 已收敛（见文件头 CHANGELOG）：此端点仅接受 X-Parable-Token 头。
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let query = q.get("q").cloned().unwrap_or_default();
    let mut list = state.store.list(&query).await;
    // The sidebar renders every entry it gets. With thousands of sessions
    // that is a multi-second DOM build — the page appears frozen. Cap the
    // default page and let the client ask for more.
    let limit: usize = q
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(200)
        .clamp(1, 1000);
    let total = list.len();
    list.truncate(limit);
    // 侧栏状态小球：待提问（淡黄）> 运行中（转圈）> 最近一轮结局
    // （失败红 / 完成蓝）。state 是按会话实时算的，不在存储里。
    let mut out = Vec::with_capacity(list.len());
    for s in list {
        let mut v = serde_json::to_value(&s).unwrap_or_default();
        let st = if state.questions.has_pending_for(&s.id).await {
            "waiting"
        } else if state.agent.is_running(&s.id).await {
            "running"
        } else if s.outcome == "failed" {
            "failed"
        } else {
            "ok"
        };
        v["state"] = json!(st);
        out.push(v);
    }
    Json(json!({"sessions": out, "total": total, "limited": total > out.len()})).into_response()
}

/// GET /api/search/messages?q=… —— 全文搜索所有会话里的消息正文。
async fn search_messages(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    // query token 已收敛（见文件头 CHANGELOG）：此端点仅接受 X-Parable-Token 头。
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let query = q.get("q").cloned().unwrap_or_default();
    if query.trim().is_empty() {
        return Json(json!({"hits": []})).into_response();
    }
    let hits = state.store.search_messages(&query, 40).await;
    Json(json!({"hits": hits})).into_response()
}

#[derive(Debug, Deserialize)]
struct NewSessionReq {
    #[serde(default)]
    workspace: String,
    #[serde(default)]
    title: String,
    /// 侧聊（辅助对话）专用会话：只服务右侧面板，主对话区不得认领它。
    #[serde(default)]
    side: bool,
}

async fn new_session(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<NewSessionReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let fallback_ws = state.config.read().await.workspace.clone();
    let ws = if r.workspace.is_empty() {
        fallback_ws.clone()
    } else {
        resolve_requested_workspace(&r.workspace, &fallback_ws)
    };
    let s = state.store.create(&ws, &r.title).await;
    state.hub.publish(&s.meta.id, "session.start", json!({
        "sessionId": s.meta.id,
        "title": s.meta.title,
        "workspace": s.meta.workspace,
        "side": r.side,
    }));
    Json(json!({"session": s.meta})).into_response()
}

#[derive(Debug, Deserialize)]
struct ForkSessionReq {
    /// 源会话（通常是主对话当前那个）。
    session_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    side: bool,
}

/// POST /session/fork —— 复制一个会话给侧聊用：历史全带走，主对话不受影响。
async fn fork_session(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<ForkSessionReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let ws = state.config.read().await.workspace.clone();
    let title = if r.title.is_empty() { "Side chat".to_string() } else { r.title };
    let (s, forked_at) = match state.store.fork(&r.session_id, &ws, &title).await {
        Some(v) => v,
        None => return server_error(format!("fork: source session {} not found", r.session_id)),
    };
    state.hub.publish(&s.meta.id, "session.start", json!({
        "sessionId": s.meta.id,
        "title": s.meta.title,
        "workspace": s.meta.workspace,
        "side": r.side,
        "forkedFrom": r.session_id,
    }));
    Json(json!({
        "session": s.meta,
        "forked_at": forked_at,
    })).into_response()
}

#[derive(Debug, Deserialize, Default)]
struct ToolEditReq {
    id: String,
    #[serde(default)]
    step: Option<usize>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    args: Option<serde_json::Value>,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    is_error: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
struct EditMessageReq {
    #[serde(default)]
    text: Option<String>,
    /// 结构化编辑：一次气泡里的工具调用 / 工具返回。缺省表示只改正文。
    #[serde(default)]
    tools: Option<Vec<ToolEditReq>>,
    /// Exclusive end of the edited range. Omitted = single message. The
    /// frontend sends the whole assistant-bubble group so its concatenated
    /// text folds into one blob (thinking / tool parts are preserved).
    #[serde(default)]
    to: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct RetryMessageReq {}

/// POST /api/sessions/{id}/messages/{idx}/retry — 重新生成一条用户提问的回答。
///
/// 语义：**替换**，不是追加。删掉这条提问之后的整段（含后续轮次），让历史
/// 重新以这条提问收尾，再以空 prompt 重跑模型 —— 新回答落在原来的位置，
/// 不会在会话末尾凭空多出一条。idx 为存储下标。
async fn retry_message(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path((id, idx)): Path<(String, usize)>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let msgs = state.store.messages(&id).await;
    let Some(m) = msgs.get(idx) else {
        return bad_request(&format!("message {idx} not found"));
    };
    if m.role != "user" {
        return bad_request("retry targets a user message");
    }
    let Some(q) = crate::session::message_text(&m.parts) else {
        return bad_request("message has no text to retry");
    };
    if q.trim().is_empty() {
        return bad_request("message has no text to retry");
    }
    // 截断这条提问之后的一切：留下「以提问收尾」的历史，模型自然会作答。
    // 旧实现是追加一条 hidden 指令 + 末尾再生成一条回答，看起来像凭空多
    // 出来一条 agent 消息，且原回答还留在上面。
    if let Err(e) = state
        .store
        .delete_range(&id, idx + 1, Some(msgs.len()))
        .await
    {
        return bad_request(&e.to_string());
    }
    // 权限模式沿用全局配置（与 /api/chat 无显式模式时一致），而不是
    // current_turn_options —— 唤醒/重试链会残留上一轮的 ask，把本该
    // 自动执行的工具卡进确认门。
    let mode = state.config.read().await.permission_mode.clone();
    let opts = TurnOptions {
        permission_mode: mode,
        ..TurnOptions::default()
    };
    match state.agent.start_turn(&id, "", Vec::new(), opts).await {
        Ok(turn_id) => Json(json!({"ok": true, "turnId": turn_id})).into_response(),
        Err(e) => bad_request(&e.to_string()),
    }
}

async fn edit_message(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path((id, idx)): Path<(String, usize)>,
    Json(r): Json<EditMessageReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let res = match r.tools {
        None => {
            state
                .store
                .edit_message_text(&id, idx, r.to, r.text.as_deref().unwrap_or(""))
                .await
        }
        Some(list) => {
            let tools: Vec<crate::session::ToolEdit> = list
                .into_iter()
                .map(|t| crate::session::ToolEdit {
                    id: t.id,
                    step: t.step,
                    name: t.name,
                    args: t.args,
                    result: t.result,
                    is_error: t.is_error,
                })
                .collect();
            state
                .store
                .edit_message_parts(&id, idx, r.to, r.text.as_deref(), &tools)
                .await
        }
    };
    match res {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => bad_request(&e.to_string()),
    }
}

async fn delete_message(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path((id, idx)): Path<(String, usize)>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    match state.store.delete_message(&id, idx).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => bad_request(&e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
struct TruncateReq {
    #[serde(default)]
    from: usize,
    /// Exclusive end of the deleted range. Omitted = truncate to the end.
    #[serde(default)]
    to: Option<usize>,
}

async fn truncate_messages(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(r): Json<TruncateReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    match state.store.delete_range(&id, r.from, r.to).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => bad_request(&e.to_string()),
    }
}

async fn session_messages(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let msgs = state.store.neutral_messages(&id).await;
    // hidden 消息（子代理完成通报）随消息流下发：前端把它渲染成回答上方
    // 的细标识行，而不是普通气泡；模型历史照常包含。
    let stored = state.store.messages(&id).await;
    Json(json!({"messages": stored, "neutral": msgs})).into_response()
}

async fn compact_session(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let model = state.config.read().await.model.clone();
    match state.agent.compact_now(&id, &model).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => bad_request(&e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
struct GoalReq {
    /// "show" | "set" (default when objective is given) | "pause" | "resume" | "clear"
    #[serde(default)]
    action: String,
    #[serde(default)]
    objective: String,
}

async fn get_goal(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let goal = state.store.get_goal(&id).await;
    Json(json!({"goal": goal})).into_response()
}

/// Debug endpoint: the exact `system` string the next turn on this session will
/// send to the provider (raw override / built-in + goal + plugin sections).
async fn session_prompt(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let system = state.agent.system_preview(&id).await;
    let builtin = state
        .config
        .read()
        .await
        .system_prompt_raw
        .trim()
        .is_empty();
    Json(json!({"system": system, "builtin": builtin})).into_response()
}

async fn post_goal(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(r): Json<GoalReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    if state.store.load(&id).await.is_none() {
        return bad_request("session not found");
    }
    let act = if r.action.is_empty() {
        if r.objective.trim().is_empty() {
            "show"
        } else {
            "set"
        }
    } else {
        r.action.as_str()
    };
    match act {
        "show" => {}
        "clear" => {
            let _ = state.store.set_goal(&id, None).await;
        }
        "pause" | "resume" => {
            match state.store.get_goal(&id).await {
                Some(mut g) => {
                    g.status = if act == "pause" { "paused" } else { "active" }.into();
                    if act == "resume" {
                        g.stalls = 0;
                    }
                    g.updated_at = crate::event::now_ms();
                    let _ = state.store.set_goal(&id, Some(g)).await;
                }
                None => return bad_request("no goal set"),
            }
        }
        _ => {
            let obj = r.objective.trim();
            if obj.is_empty() {
                return bad_request("objective is required");
            }
            if obj.len() > 4000 {
                return bad_request("objective too long (max 4000 chars)");
            }
            let _ = state
                .store
                .set_goal(&id, Some(crate::session::GoalState::new(obj)))
                .await;
        }
    }
    let goal = state.store.get_goal(&id).await;
    state.hub.publish(&id, "goal", json!({"goal": goal}));
    Json(json!({"goal": goal})).into_response()
}

/// 插件命令/斜杠命令名的服务端白名单：字母开头，后接字母数字、`-` 或 `_`，
/// ≤64 字符。前端按这个名字渲染 `/<name>` 斜杠命令，恶意插件包把 `<script>`
/// 之类塞进 name 时，响应侧直接过滤（与前端 XSS 修复构成纵深防御）。
fn valid_plugin_command_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    name.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

async fn list_plugins(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let store = crate::plugin::PluginStore::new();
    let mut plugins = store.scan();
    for p in &mut plugins {
        let before = p.commands.len();
        if before == 0 {
            continue;
        }
        p.commands.retain(|c| {
            let ok = valid_plugin_command_name(&c.name);
            if !ok {
                tracing::warn!(
                    "plugin `{}` command name `{}` 不符合 ^[a-zA-Z][a-zA-Z0-9_-]{{0,63}}$，已过滤",
                    p.id,
                    c.name
                );
            }
            ok
        });
        if p.commands.len() != before {
            tracing::warn!("plugin `{}` filtered {} invalid command name(s)", p.id, before - p.commands.len());
        }
    }
    Json(json!({"plugins": plugins})).into_response()
}

async fn toggle_plugin(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let store = crate::plugin::PluginStore::new();
    match store.toggle(&id) {
        Ok(enabled) => Json(json!({"ok": true, "enabled": enabled})).into_response(),
        Err(e) => bad_request(&e.to_string()),
    }
}

async fn delete_plugin(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let store = crate::plugin::PluginStore::new();
    match store.delete(&id) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => bad_request(&e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
struct PatchSession {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    pinned: Option<bool>,
    #[serde(default)]
    archived: Option<bool>,
    #[serde(default)]
    group: Option<String>,
    /// 侧栏状态小球的「未读」位：进会话时前端 PATCH false 消费。
    #[serde(default)]
    dot: Option<bool>,
}

async fn patch_session(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(r): Json<PatchSession>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let res = state
        .store
        .update_meta(&id, |m| {
            if let Some(t) = &r.title {
                if !t.is_empty() {
                    m.title = t.clone();
                }
            }
            if let Some(p) = r.pinned {
                m.pinned = p;
            }
            if let Some(a) = r.archived {
                m.archived = a;
            }
            if let Some(g) = &r.group {
                m.group = g.clone();
            }
            if let Some(d) = r.dot {
                m.dot = d;
            }
        })
        .await;
    match res {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => server_error(e),
    }
}

async fn delete_session(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    match state.store.delete(&id).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => server_error(e),
    }
}

/// GET /api/sessions/export — 全部会话打包为单个 JSON（备份/迁移用）。
async fn export_sessions(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    use axum::http::header;
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let mut out: Vec<Value> = Vec::new();
    for s in state.store.list("").await {
        if let Some(full) = state.store.load(&s.id).await {
            if let Ok(v) = serde_json::to_value(&full) {
                out.push(v);
            }
        }
    }
    let body = serde_json::to_vec(&json!({
        "app": "parable",
        "exported_at": crate::event::now_ms(),
        "count": out.len(),
        "sessions": out,
    }))
    .unwrap_or_else(|_| b"{}".to_vec());
    let filename = format!(
        "parable-sessions-{}.json",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );
    // HeaderValue::from_str 代替 Box::leak：此前为满足 &'static 约束把
    // Content-Disposition 泄漏进进程终身堆，每次导出漏一份，改 owned 值即消。
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment"));
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8")),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        body,
    )
        .into_response()
}

/// POST /api/sessions/import —— 导入导出文件里的会话。
///
/// 与 `/sessions/export` 对称：接受同一个 `{app, sessions:[…]}` 信封，
/// 也接受裸数组与单个会话对象（用户手改过的文件、别的版本导出的文件）。
///
/// 三条规则，都是为了「导入不能破坏现有数据」：
///  1. **ID 冲突就换新 ID**，不覆盖本地已有会话（`rename: true` 开关）；
///  2. **跳过而不是中止**：一条会话坏了，其余照常导入，最后一次性报告
///     成功几条、跳过几条以及每条的原因；
///  3. **消息逐条做结构校验**，缺 `role`/`parts` 的条目丢弃，不让坏消息
///     进存储——它会一直卡在重放与模型历史里。
async fn import_sessions(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    // 三种可接受形状 → 统一的 Vec<Value>
    let raw_sessions: Vec<Value> = if let Some(arr) = body.get("sessions").and_then(|s| s.as_array()) {
        arr.clone()
    } else if let Some(arr) = body.as_array() {
        arr.clone()
    } else if body.get("messages").is_some() || body.get("id").is_some() {
        vec![body.clone()]
    } else {
        return bad_request(
            "expected an export envelope (`sessions: [...]`), an array of sessions, or one session object",
        );
    };
    if raw_sessions.is_empty() {
        return bad_request("the file contains no sessions");
    }

    let existing: std::collections::HashSet<String> =
        state.store.list("").await.into_iter().map(|s| s.id).collect();
    let mut imported = 0usize;
    let mut renamed = 0usize;
    let mut skipped: Vec<String> = Vec::new();
    let mut used: std::collections::HashSet<String> = existing.clone();
    let mut ids_out: Vec<String> = Vec::new();

    for (i, raw) in raw_sessions.iter().enumerate() {
        // 反序列化成 Session；旧版本缺字段靠 serde default 补齐。
        let mut s: crate::session::Session = match serde_json::from_value(raw.clone()) {
            Ok(v) => v,
            Err(e) => {
                skipped.push(format!("#{}: not a valid session ({e})", i + 1));
                continue;
            }
        };
        // 消息级校验：role 必须存在且非空，parts 必须是对象/数组。
        let before = s.messages.len();
        s.messages.retain(|m| {
            !m.role.trim().is_empty()
                && (m.parts.is_object() || m.parts.is_array() || !m.parts.is_null())
        });
        let dropped = before - s.messages.len();
        if s.messages.is_empty() && before > 0 {
            skipped.push(format!("#{}: every message was malformed", i + 1));
            continue;
        }

        // ID 冲突或含路径穿越字符 → 重新生成一个，绝不覆盖本地会话。
        // C-06：id 只允许字母数字 `-_`，防 `../` 经 path_for 写出 sessions 根目录。
        let mut id = s.meta.id.trim().to_string();
        if id.is_empty() || !valid_session_id(&id) || used.contains(&id) {
            let fresh = format!(
                "s_{}",
                &uuid::Uuid::new_v4().simple().to_string()[..12]
            );
            if !id.is_empty() {
                renamed += 1;
            }
            id = fresh;
        }
        used.insert(id.clone());
        s.meta.id = id.clone();
        // 归档/置顶状态不随导入带进来：用户导入的是内容，不是本地视图状态。
        s.meta.archived = false;
        s.meta.pinned = false;
        // 运行中的 pending 队列是瞬态，导入后不应自动开跑。
        s.pending.clear();
        if s.meta.title.trim().is_empty() {
            s.meta.title = "Imported chat".into();
        }

        match state.store.insert_full(s.clone()).await {
            Ok(()) => {
                imported += 1;
                ids_out.push(id);
                let mut note = format!("imported `{}` ({} messages)", s.meta.title, s.messages.len());
                if dropped > 0 {
                    note.push_str(&format!(", dropped {dropped} malformed message(s)"));
                }
                tracing::info!("session import: {note}");
            }
            Err(e) => skipped.push(format!("#{}: {e}", i + 1)),
        }
    }

    Json(json!({
        "ok": true,
        "imported": imported,
        "renamed": renamed,
        "skipped": skipped,
        "sessionIds": ids_out,
    }))
    .into_response()
}

/// POST /api/sessions/clear-all — 删除全部会话（危险区，前端二次确认）。
async fn clear_all_sessions(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let list = state.store.list("").await;
    let mut n = 0usize;
    for s in &list {
        if state.store.delete(&s.id).await.is_ok() {
            n += 1;
        }
    }
    Json(json!({"ok": true, "deleted": n})).into_response()
}

#[derive(Debug, Deserialize)]
struct ApplyProfileReq {
    id: String,
}

/// POST /api/profiles/apply — 把命名 Profile 的 model/permission/thinking
/// 套到当前配置并激活（前端模型菜单/设置页一键切换）。
async fn apply_profile(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<ApplyProfileReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let mut cfg = state.config.write().await;
    cfg.refresh_from_disk();
    let Some(p) = cfg.profiles.iter().find(|p| p.id == r.id).cloned() else {
        return bad_request("profile 不存在");
    };
    cfg.model = p.model.clone();
    cfg.permission_mode = p.permission_mode.clone();
    cfg.thinking_level = p.thinking_level;
    cfg.active_profile = p.id.clone();
    crate::config::set_proxy(cfg.http_proxy.clone(), cfg.http_no_proxy.clone());
    if let Err(e) = cfg.save() {
        return server_error(e);
    }
    Json(json!({
        "ok": true,
        "model": cfg.model,
        "permission_mode": cfg.permission_mode,
        "thinking_level": cfg.thinking_level,
    }))
    .into_response()
}

/// GET /api/sessions/{id}/checkpoints — 该会话的全部文件检查点（带逐文件
/// 状态与增删统计；已回退的 turn 盘上内容等于快照，状态为 unchanged）。
async fn list_checkpoints(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let list = state.agent.checkpoints.list_detail(&id).await;
    Json(json!({"checkpoints": list})).into_response()
}

/// GET /api/sessions/{id}/checkpoint-diff?turn=&path= — 单文件的审查 diff
/// （快照 vs 当前盘上内容，行级、长段未变更区已折叠）。
async fn checkpoint_diff(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let Some(turn) = q.get("turn").cloned() else {
        return server_error("checkpoint-diff: missing turn");
    };
    let Some(path) = q.get("path").cloned() else {
        return server_error("checkpoint-diff: missing path");
    };
    // full=1：不折叠未变更区，改动审查侧栏展示完整文件 + 增减行标记
    let full = q.get("full").map(|v| v == "1" || v == "true").unwrap_or(false);
    match state.agent.checkpoints.diff_for(&id, &turn, &path, full).await {
        Some((status, added, removed, lines)) => {
            Json(json!({"status": status, "added": added, "removed": removed, "lines": lines}))
                .into_response()
        }
        None => server_error("checkpoint-diff: no checkpoint for this turn/path"),
    }
}

#[derive(Debug, Deserialize)]
struct RewindReq {
    turn: String,
}

/// POST /api/sessions/{id}/rewind — 回退某 turn 的文件改动。
async fn rewind_session(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(r): Json<RewindReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    match state.agent.checkpoints.restore(&id, &r.turn).await {
        Ok(files) => Json(json!({"ok": true, "files": files})).into_response(),
        Err(e) => server_error(e),
    }
}

// ---------------------------------------------------------------- MCP

/// GET /api/mcp — 配置 + 各服务器实时状态。
async fn mcp_state(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let cfg = state.config.read().await;
    let servers = cfg.mcp_servers.clone();
    drop(cfg);
    let status = state.agent.mcp.list_status().await;
    Json(json!({"servers": servers, "status": status})).into_response()
}

#[derive(Debug, Deserialize)]
struct McpSaveReq {
    servers: Vec<crate::mcp::McpServerConfig>,
}

/// POST /api/mcp — 保存服务器清单并应用到管理器（移除的杀进程）。
async fn mcp_save(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<McpSaveReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    {
        let mut cfg = state.config.write().await;
        cfg.refresh_from_disk();
        cfg.mcp_servers = r.servers.clone();
        if let Err(e) = cfg.save() {
            return server_error(e);
        }
    }
    state.agent.mcp.set_servers(r.servers).await;
    let status = state.agent.mcp.list_status().await;
    Json(json!({"ok": true, "status": status})).into_response()
}

#[derive(Debug, Deserialize)]
struct McpConnectReq {
    id: String,
}

/// POST /api/mcp/connect — 显式连接一台服务器并拉工具清单。
async fn mcp_connect(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<McpConnectReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    if let Err(e) = state.agent.mcp.connect(&r.id).await {
        return server_error(e);
    }
    let status = state.agent.mcp.list_status().await;
    Json(json!({"ok": true, "status": status})).into_response()
}

// ---------------------------------------------------------------- archive

#[derive(Debug, Deserialize)]
struct ArchiveReq {
    /// Stop a running turn before archiving (dsh's `stopActivity`).
    #[serde(default)]
    stop: bool,
}

async fn archive_session(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(r): Json<ArchiveReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    match state.agent.archive_session(&id, r.stop).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => server_error(e),
    }
}

async fn unarchive_session(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    match state.agent.unarchive_session(&id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => server_error(e),
    }
}

// ---------------------------------------------------------------- schedule & agents

async fn list_tasks(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let store = state.schedule.store();
    let tasks = store.list().await;
    // 最近一次运行的结果：前端用来标"上次跑成功/失败"，没有历史时为 null。
    let last = store.last_runs().await;
    let items: Vec<Value> = tasks
        .iter()
        .map(|t| {
            let mut v = serde_json::to_value(t).unwrap_or(Value::Null);
            let lr = last.get(&t.id);
            if let Some(obj) = v.as_object_mut() {
                // lastRunOk：最近一次运行是否成功；没有运行历史时为 null。
                obj.insert(
                    "lastRunOk".into(),
                    match lr {
                        Some(r) => Value::Bool(r.ok),
                        None => Value::Null,
                    },
                );
                // lastRunAt：任务自带字段（触发时刻）优先；没有则回退到
                // 运行记录的开始时刻，保证两者不打架。
                if t.last_run_at.is_none() {
                    obj.insert(
                        "lastRunAt".into(),
                        match lr {
                            Some(r) => Value::String(r.started_at.clone()),
                            None => Value::Null,
                        },
                    );
                }
            }
            v
        })
        .collect();
    Json(json!({"tasks": items})).into_response()
}

/// GET /api/schedule/runs?task=<id> —— 运行历史。
///
/// 不带 `task` 参数返回全部记录；带 `task` 只返回该任务的记录。
/// 按开始时间倒序（最新在前）。
async fn list_run_records(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let task = q.get("task").map(|s| s.trim()).filter(|s| !s.is_empty());
    let runs = state.schedule.store().runs_for(task).await;
    Json(json!({"runs": runs})).into_response()
}

#[derive(Debug, Deserialize)]
struct CreateTaskReq {
    name: String,
    #[serde(default)]
    prompt: String,
    /// `{"kind":"agent"}`（默认）或 `{"kind":"shell","command":"…"}`。
    #[serde(default)]
    action: Option<Value>,
    /// 触发时用哪个模型（`provider/model`）；空 = 跟随全局。
    #[serde(default)]
    model: String,
    timing: crate::schedule::Timing,
    /// Either the plain string `existing` / `reuse` / `new`, or the tagged form
    /// `{"kind":"existing","session_id":"..."}`.
    #[serde(default)]
    target: Option<Value>,
    #[serde(default)]
    session_id: String,
}

/// 把请求里的 action 解析成 `TaskAction`；缺省 = 投递提示词（老客户端不变）。
fn parse_action(v: Option<&Value>) -> crate::schedule::TaskAction {
    use crate::schedule::TaskAction;
    let Some(obj) = v.and_then(|x| x.as_object()) else {
        return TaskAction::Agent;
    };
    match obj.get("kind").and_then(|k| k.as_str()) {
        Some("shell") => TaskAction::Shell {
            command: obj
                .get("command")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        },
        _ => TaskAction::Agent,
    }
}

fn parse_target(v: Option<&Value>, fallback_session: &str) -> crate::schedule::Target {
    use crate::schedule::Target;
    match v {
        Some(Value::String(s)) => match s.as_str() {
            "existing" => Target::Existing {
                session_id: fallback_session.to_string(),
            },
            "new" => Target::NewEachTime,
            _ => Target::Reuse,
        },
        Some(obj) => {
            if obj.get("kind").and_then(|k| k.as_str()) == Some("existing") {
                Target::Existing {
                    session_id: obj
                        .get("session_id")
                        .and_then(|x| x.as_str())
                        .unwrap_or(fallback_session)
                        .to_string(),
                }
            } else if obj.get("kind").and_then(|k| k.as_str()) == Some("new_each_time") {
                Target::NewEachTime
            } else {
                Target::Reuse
            }
        }
        None => {
            if fallback_session.is_empty() {
                Target::Reuse
            } else {
                Target::Existing {
                    session_id: fallback_session.to_string(),
                }
            }
        }
    }
}

async fn create_task(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<CreateTaskReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let target = parse_target(r.target.as_ref(), &r.session_id);
    let action = parse_action(r.action.as_ref());
    match state
        .schedule
        .create(r.name, r.prompt, action, r.model, r.timing, target)
        .await
    {
        Ok(t) => Json(json!(t)).into_response(),
        Err(e) => bad_request(&e.to_string()),
    }
}

async fn get_task(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    match state.schedule.store().get(&id).await {
        Some(t) => Json(json!(t)).into_response(),
        None => bad_request("unknown task"),
    }
}

#[derive(Debug, Deserialize)]
struct EditTaskReq {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    /// 改触发规则；改动后会重新提交下一次触发。
    #[serde(default)]
    timing: Option<crate::schedule::Timing>,
    /// 动作：老接口塞的是字符串（`pause` / `resume` / `delete`），
    /// 也接受 `{"kind":"agent"}` / `{"kind":"shell","command":"…"}` 标签形式
    /// （与 `create_task` 的 `parse_action` 同口径）。
    #[serde(default)]
    action: Option<Value>,
    /// 改用哪个模型（`provider/model`）；空串 = 跟随全局模型。
    #[serde(default)]
    model: Option<String>,
    /// 改投递目标，解析规则同 `parse_target`。
    #[serde(default)]
    target: Option<Value>,
    /// `active` / `paused` / `ended`。
    #[serde(default)]
    status: Option<String>,
    /// shell 动作的命令（配合 action 一起改）。
    #[serde(default)]
    command: Option<String>,
}

/// 把请求里的 status 解析成 `TaskStatus`；不认识的取值返回 None。
fn parse_status(v: &str) -> Option<crate::schedule::TaskStatus> {
    use crate::schedule::TaskStatus;
    match v.trim().to_ascii_lowercase().as_str() {
        "active" | "resume" | "running" => Some(TaskStatus::Active),
        "paused" | "pause" => Some(TaskStatus::Paused),
        "ended" | "end" => Some(TaskStatus::Ended),
        _ => None,
    }
}

async fn edit_task(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(r): Json<EditTaskReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    // action 可能是字符串（pause/resume/delete）或标签对象。
    let action_str = r.action.as_ref().and_then(|v| v.as_str()).map(|s| s.to_string());
    let action_obj = r.action.as_ref().filter(|v| v.is_object()).cloned();
    if action_str.as_deref() == Some("delete") {
        return match state.schedule.store().delete(&id).await {
            Ok(()) => Json(json!({"ok": true})).into_response(),
            Err(e) => server_error(e),
        };
    }
    // 改 timing 前先校验：不合法就直接拒绝，别把任务改成永远不触发的状态。
    if let Some(tm) = &r.timing {
        if let Err(e) = tm.validate() {
            return bad_request(&e.to_string());
        }
    }
    // action / status 互相冲突时以 status 为准（status 更明确）。
    let status = r
        .status
        .as_deref()
        .and_then(parse_status)
        .or_else(|| match action_str.as_deref() {
            Some("pause") => Some(crate::schedule::TaskStatus::Paused),
            Some("resume") => Some(crate::schedule::TaskStatus::Active),
            _ => None,
        });
    if let Some(s) = r.status.as_deref() {
        if s.trim().is_empty() || parse_status(s).is_none() {
            return bad_request("status must be one of: active, paused, ended");
        }
    }
    let now = chrono::Utc::now();
    // 目标在闭包外解析：解析结果要先校验（existing 必须带会话 id）。
    let new_target = r.target.as_ref().map(|tv| parse_target(Some(tv), ""));
    if let Some(crate::schedule::Target::Existing { session_id }) = &new_target {
        if session_id.trim().is_empty() {
            return bad_request("target `existing` requires a session_id");
        }
    }
    // 标签形式的 action → 新的 TaskAction（shell 时命令取对象里的 command）。
    let new_action = action_obj.as_ref().map(|v| {
        let mut a = parse_action(Some(v));
        if let (crate::schedule::TaskAction::Shell { command }, Some(cmd)) = (&mut a, &r.command) {
            if !cmd.trim().is_empty() {
                *command = cmd.clone();
            }
        }
        a
    });
    let out = match state
        .schedule
        .store()
        .update(&id, |t| {
            if let Some(n) = &r.name {
                if !n.is_empty() {
                    t.name = n.clone();
                }
            }
            if let Some(p) = &r.prompt {
                if !p.is_empty() {
                    t.prompt = p.clone();
                }
            }
            if let Some(m) = &r.model {
                t.model_sel = m.clone();
            }
            if let Some(tv) = &new_target {
                // 目标变了：旧的绑定会话不再适用，清掉让下次触发重新绑定。
                t.target = tv.clone();
                t.bound_session_id = None;
            }
            if let Some(a) = &new_action {
                t.action = a.clone();
            }
            // 只给了 command（没给 action）：切到 shell 动作并换命令。
            if let Some(cmd) = &r.command {
                if !cmd.trim().is_empty() && new_action.is_none() {
                    t.action = crate::schedule::TaskAction::Shell {
                        command: cmd.clone(),
                    };
                }
            }
            // 先记下旧状态：判断"是否从暂停恢复"要用改之前的值。
            let was_paused = matches!(t.status, crate::schedule::TaskStatus::Paused);
            if let Some(s) = status {
                t.status = s;
            }
            // timing 变了要重新提交下一次触发（否则卡在旧时间点上）。
            let timing_changed = if let Some(tm) = &r.timing {
                t.timing = tm.clone();
                true
            } else {
                false
            };
            // 从 paused 恢复成 active：同样要重新提交下一次触发，
            // 但保留 anchor（every 的相位不因暂停而整体平移）。
            // 只在"确实是从暂停恢复"时重算——已经 active 的任务改别的东西
            // 不该动它的下一次触发时刻。
            if timing_changed {
                t.recompute_next(now);
            } else if was_paused
                && matches!(status, Some(crate::schedule::TaskStatus::Active))
            {
                t.resume_next(now);
            }
        })
        .await
    {
        Ok(t) => t,
        Err(e) => return server_error(e),
    };
    // 任务状态变了，推一次事件让前端刷新。
    state.schedule.publish_task(&out);
    Json(json!(out)).into_response()
}

async fn delete_task(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    match state.schedule.store().delete(&id).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => server_error(e),
    }
}

/// 当前 UTC 时刻（RFC 3339，毫秒精度）——运行历史的时间戳统一用它。
fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Run a task immediately, as the UI's "run now" affordance does.
async fn run_task_now(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let Some(mut task) = state.schedule.store().get(&id).await else {
        return bad_request("unknown task");
    };
    let store = state.schedule.store();
    // shell 任务「立即运行」：直接执行，不建会话、不动模型。
    if let crate::schedule::TaskAction::Shell { command } = task.action.clone() {
        let cfg = state.config.read().await;
        let ws = if cfg.workspace.is_empty() {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        } else {
            std::path::PathBuf::from(&cfg.workspace)
        };
        drop(cfg);
        let job = match state.terms.start(state.hub.clone(), ws, command).await {
            Ok(j) => j,
            Err(e) => {
                // 启动失败也算一次运行（失败）。
                let mut rec = crate::schedule::RunRecord::new(&task.id, &task.name);
                rec.ok = false;
                rec.finished_at = Some(now_iso());
                rec.error = Some(e.to_string());
                store.record_run(rec).await;
                return bad_request(&e.to_string());
            }
        };
        let label = format!("{} · {}", task.name, job.id);
        let now_iso = now_iso();
        if let Err(e) = store
            .update(&id, |t| {
                t.last_run_at = Some(now_iso.clone());
                t.run_count += 1;
            })
            .await
        {
            tracing::warn!("recording manual run failed for {id}: {e}");
        }
        // 运行历史：先落一条"已启动"，命令结束后由后台轮询回填退出码与输出。
        let mut rec = crate::schedule::RunRecord::new(&task.id, &task.name);
        rec.ok = true;
        rec.output = format!("started in background terminal {}", job.id);
        store.record_run(rec.clone()).await;
        spawn_term_run_watch(state.clone(), store.clone(), job.id.clone(), rec.id.clone());
        return Json(json!({"ok": true, "jobId": job.id, "label": label})).into_response();
    }
    match state.schedule.resolve_session(&mut task).await {
        Ok(sid) => {
            // 用用户自己的全局权限模式;选了 full 那是用户自己的选择,
            // 定时任务不再偷偷提权到 full。
            let mode = state.config.read().await.permission_mode.clone();
            let opts = crate::agent::TurnOptions {
                model_sel: String::new(),
                permission_mode: mode,
                thinking_level: 0,
                plan_mode: false,
                ..TurnOptions::default()
            };
            let prompt = format!(
                "[scheduled task `{}` run manually]\n\n{}",
                task.name, task.prompt
            );
            // 手动触发也算一次运行：推进 last_run_at / run_count（与 shell 分支一致）。
            let stamp = now_iso();
            if let Err(e) = store
                .update(&id, |t| {
                    t.last_run_at = Some(stamp.clone());
                    t.run_count += 1;
                })
                .await
            {
                tracing::warn!("recording manual run failed for {id}: {e}");
            }
            match state.agent.start_turn(&sid, &prompt, Vec::new(), opts).await {
                Ok(turn) => {
                    // 投递成功：落一条运行记录（模型还在跑，finished_at 暂空）。
                    let mut rec = crate::schedule::RunRecord::new(&task.id, &task.name);
                    rec.ok = true;
                    rec.session_id = Some(sid.clone());
                    rec.output = format!("turn started: {turn}");
                    store.record_run(rec).await;
                    Json(json!({"sessionId": sid, "turnId": turn})).into_response()
                }
                Err(e) => {
                    let mut rec = crate::schedule::RunRecord::new(&task.id, &task.name);
                    rec.ok = false;
                    rec.finished_at = Some(now_iso());
                    rec.error = Some(e.to_string());
                    rec.session_id = Some(sid.clone());
                    store.record_run(rec).await;
                    (StatusCode::CONFLICT, Json(json!({"error": e.to_string()}))).into_response()
                }
            }
        }
        Err(e) => {
            // 目标解析失败（会话不存在/已归档）也算一次失败运行。
            let mut rec = crate::schedule::RunRecord::new(&task.id, &task.name);
            rec.ok = false;
            rec.finished_at = Some(now_iso());
            rec.error = Some(e.to_string());
            store.record_run(rec).await;
            server_error(e)
        }
    }
}

/// 轮询后台终端，等命令结束后回填运行历史的退出码与输出。
///
/// 后台终端是异步的（`terms.start` 立即返回 job id），这里用一个轻量
/// 轮询把它和运行历史对上；超时（10 分钟）还没结束就按"仍在运行"收尾。
fn spawn_term_run_watch(
    state: SharedState,
    store: Arc<crate::schedule::ScheduleStore>,
    job_id: String,
    run_id: String,
) {
    tokio::spawn(async move {
        for _ in 0..600 {
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
            let jobs = state.terms.list().await;
            let Some(j) = jobs.into_iter().find(|j| j.id == job_id) else {
                // 终端记录已被清理：收尾为"未知结果"。
                let _ = store
                    .update_run(&run_id, |r| {
                        r.finished_at = Some(now_iso());
                        r.error = Some("terminal job record was pruned".into());
                    })
                    .await;
                return;
            };
            if j.status == "running" {
                continue;
            }
            let exit = j.exit;
            let _ = store
                .update_run(&run_id, |r| {
                    r.finished_at = Some(now_iso());
                    r.exit_code = exit;
                    r.ok = exit == Some(0);
                    r.output = crate::schedule::tail_output(&j.output);
                    if !r.ok {
                        r.error = Some(match exit {
                            Some(c) => format!("command exited with code {c}"),
                            None => "command was killed".into(),
                        });
                    }
                })
                .await;
            return;
        }
        // 10 分钟还没结束：保留"进行中"状态，不强行标失败。
    });
}

/// GET /api/turn/running?session=<id> —— 该会话是否还有 turn 在跑。
///
/// 前端用它做自愈核对：如果 UI 认为正在生成、而服务端说没有在跑，说明
/// `turn.end` 事件丢了（慢客户端被 broadcast 缓冲甩掉），UI 必须复位。
async fn turn_running(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<SessionQuery>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let sid = q.session.unwrap_or_default();
    let running = if sid.is_empty() {
        false
    } else {
        state.agent.is_running(&sid).await
    };
    Json(json!({"running": running})).into_response()
}

/// GET /api/permissions —— 读出权限规则（permissions.toml）。
/// POST /api/permissions —— 覆盖写入规则。
///
/// 规则模型：{tool, path, action: allow|deny, exact}。
/// `exact: true` 表示 path 存的是字面量（shell 存命令原文），匹配按等值比较；
/// 否则按 glob 语义。
async fn get_permissions(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let rs = crate::permission::RuleSet::load();
    Json(json!({ "allow": rs.allow, "deny": rs.deny, "ask": rs.ask })).into_response()
}

async fn put_permissions(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    // 整体反序列化：allow / deny / ask 三张表 + 每条规则的 cmd 命令模式
    let rs: crate::permission::RuleSet = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return bad_request(&format!("invalid JSON: {e}")),
    };
    if let Err(e) = rs.save() {
        return server_error(e);
    }
    Json(json!({"ok": true})).into_response()
}

// ------------------------------------------------------------------ memory

async fn list_memory(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let items = state.memory.list().await;
    Json(json!({"memories": items})).into_response()
}

async fn save_memory(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(input): Json<crate::memory::MemoryInput>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    match state.memory.upsert(input).await {
        Ok(m) => Json(json!({"ok": true, "memory": m})).into_response(),
        Err(e) => bad_request(&e.to_string()),
    }
}

async fn update_memory(
    State(state): State<SharedState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<u64>,
    Json(mut input): Json<crate::memory::MemoryInput>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    input.id = Some(id);
    match state.memory.upsert(input).await {
        Ok(m) => Json(json!({"ok": true, "memory": m})).into_response(),
        Err(e) => bad_request(&e.to_string()),
    }
}

async fn delete_memory(
    State(state): State<SharedState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<u64>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    match state.memory.remove(id).await {
        Ok(removed) => Json(json!({"ok": true, "removed": removed})).into_response(),
        Err(e) => server_error(e),
    }
}

async fn clear_memory(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    match state.memory.clear().await {
        Ok(n) => Json(json!({"ok": true, "cleared": n})).into_response(),
        Err(e) => server_error(e),
    }
}

// ------------------------------------------------------------------ skills

/// `GET /api/skills?scope=user|project`
async fn list_skills(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let ws = workspace_of(&state).await;
    let scope = q.get("scope").map(|s| s.as_str());
    let listing = crate::skills::list(&ws, scope);
    Json(json!({
        "skills": listing.skills,
        "problems": listing.problems,
        "roots": {
            "user": crate::skills::user_dir().display().to_string(),
            "project": crate::skills::project_dir(&ws).display().to_string(),
        }
    }))
    .into_response()
}

async fn get_skill(
    State(state): State<SharedState>,
    headers: HeaderMap,
    axum::extract::Path((scope, id)): axum::extract::Path<(String, String)>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let ws = workspace_of(&state).await;
    match crate::skills::read_body(&ws, &scope, &id) {
        Some(s) => Json(json!(s)).into_response(),
        None => not_found(&format!("skill {id}")),
    }
}

async fn save_skill(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(input): Json<crate::skills::SkillInput>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let ws = workspace_of(&state).await;
    match crate::skills::save(&ws, &input) {
        Ok(s) => Json(json!({"ok": true, "skill": s})).into_response(),
        Err(e) => bad_request(&e.to_string()),
    }
}

async fn delete_skill(
    State(state): State<SharedState>,
    headers: HeaderMap,
    axum::extract::Path((scope, id)): axum::extract::Path<(String, String)>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let ws = workspace_of(&state).await;
    match crate::skills::remove(&ws, &scope, &id) {
        Ok(removed) => Json(json!({"ok": true, "removed": removed})).into_response(),
        Err(e) => bad_request(&e.to_string()),
    }
}

async fn toggle_skill(
    State(state): State<SharedState>,
    headers: HeaderMap,
    axum::extract::Path((scope, id)): axum::extract::Path<(String, String)>,
    Json(v): Json<Value>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let enabled = v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(true);
    let ws = workspace_of(&state).await;
    match crate::skills::set_enabled(&ws, &scope, &id, enabled) {
        Ok(ok) => Json(json!({"ok": ok})).into_response(),
        Err(crate::HarnessError::NotFound(_)) => not_found(&format!("skill {id}")),
        Err(e) => bad_request(&e.to_string()),
    }
}

// ------------------------------------------------------------------ 智能体库（agent definitions）

async fn list_agent_defs(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let ws = workspace_of(&state).await;
    let scope = q.get("scope").map(|s| s.as_str());
    let listing = crate::agents::list(&ws, scope);
    Json(json!({
        "agents": listing.agents,
        "problems": listing.problems,
        "roots": {
            "user": crate::agents::user_dir().display().to_string(),
            "project": crate::agents::project_dir(&ws).display().to_string(),
        }
    }))
    .into_response()
}

async fn save_agent_def(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(input): Json<crate::agents::AgentInput>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let ws = workspace_of(&state).await;
    match crate::agents::save(&ws, &input) {
        Ok(d) => Json(json!({"ok": true, "agent": d})).into_response(),
        Err(e) => bad_request(&e.to_string()),
    }
}

async fn delete_agent_def(
    State(state): State<SharedState>,
    headers: HeaderMap,
    axum::extract::Path((scope, id)): axum::extract::Path<(String, String)>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let ws = workspace_of(&state).await;
    match crate::agents::remove(&ws, &scope, &id) {
        Ok(removed) => Json(json!({"ok": true, "removed": removed})).into_response(),
        Err(e) => bad_request(&e.to_string()),
    }
}

async fn toggle_agent_def(
    State(state): State<SharedState>,
    headers: HeaderMap,
    axum::extract::Path((scope, id)): axum::extract::Path<(String, String)>,
    Json(v): Json<Value>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let enabled = v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(true);
    let ws = workspace_of(&state).await;
    match crate::agents::set_enabled(&ws, &scope, &id, enabled) {
        Ok(ok) => Json(json!({"ok": ok})).into_response(),
        Err(crate::HarnessError::NotFound(_)) => not_found(&format!("agent {id}")),
        Err(e) => bad_request(&e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
struct AgentRunReq {
    session_id: String,
    /// Profile id or name (智能体库).
    name: String,
    /// The task the launched child should carry out.
    task: String,
}

/// Launch a saved agent profile directly from the team page: spawn a child on
/// the given session's tree wearing the profile's name/persona/model/tools.
async fn run_agent_def(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<AgentRunReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    if r.task.trim().is_empty() {
        return bad_request("task must not be empty");
    }
    let ws = workspace_of(&state).await;
    let Some(def) = crate::agents::find(&ws, &r.name) else {
        return not_found(&format!("agent {}", r.name));
    };
    if !def.enabled {
        return bad_request(&format!("agent {} is disabled", def.name));
    }
    let starter: std::sync::Arc<dyn crate::tools::TurnStarter> =
        std::sync::Arc::new(state.agent.clone());
    let req = crate::subagent::SpawnRequest {
        label: def.name.clone(),
        name: Some(def.name.clone()),
        task: r.task,
        cwd: None,
        seed_history: false,
        model: def.model.clone(),
        system_prompt: def.system_prompt.clone(),
        tools: def.tools.clone(),
        delivery: Some(crate::subagent::Delivery::Parent),
    };
    match state
        .subagents
        .spawn(starter, &r.session_id, &ws.to_string_lossy(), req)
        .await
    {
        Ok(id) => Json(json!({"ok": true, "agentId": id, "name": def.name})).into_response(),
        Err(e) => server_error(e),
    }
}

/// The workspace the agent is working in, resolved fresh each call so a
/// workspace switch is picked up without a restart.
async fn workspace_of(state: &SharedState) -> std::path::PathBuf {
    let w = state.config.read().await.workspace.clone();
    if w.trim().is_empty() {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    } else {
        std::path::PathBuf::from(w)
    }
}

/// C-21：规范化并校验会话工作区。相对路径按当前目录解析；不存在/不可读则回退默认工作区，
/// 避免把一个拼错或已删除的路径串写进会话元数据，导致后续工具在错误根目录下运行。
fn resolve_requested_workspace(requested: &str, fallback: &str) -> String {
    let raw = requested.trim();
    if raw.is_empty() {
        return fallback.to_string();
    }
    let p = std::path::PathBuf::from(raw);
    let abs = if p.is_absolute() {
        p
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    match abs.canonicalize() {
        Ok(real) if real.is_dir() => real.to_string_lossy().into_owned(),
        _ => {
            tracing::warn!("workspace `{raw}` 不存在或不可读，回退默认工作区");
            fallback.to_string()
        }
    }
}


///
/// `?session=<id>` scopes the listing to that session's tree. Without it the
/// whole process-wide catalogue is returned — the old behaviour, which leaked
/// other conversations' subagents into this one.
async fn list_agents(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<SessionQuery>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let kids = match q.session {
        Some(sid) if !sid.is_empty() => {
            let root = root_of(&state, &sid).await;
            state.subagents.tree(&root).await
        }
        _ => state.subagents.list().await,
    };
    Json(json!({"agents": kids})).into_response()
}

/// The root session of the tree `session_id` belongs to.
async fn root_of(state: &SharedState, session_id: &str) -> String {
    match state.subagents.catalog.find_by_session(session_id).await {
        Some(child) => child.root_session,
        None => session_id.to_string(),
    }
}

async fn interrupt_agent(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    // 面板请求不带调用方会话：先查出子代理所属根，再在其子树内中断。
    let root = state
        .subagents
        .resolve_any(&id)
        .await
        .map(|c| c.root_session)
        .unwrap_or_else(|| id.clone());
    let ok = state.subagents.interrupt(&root, &id).await;
    Json(json!({"interrupted": ok})).into_response()
}

#[derive(Debug, Deserialize)]
struct AgentSayReq {
    text: String,
    #[serde(default)]
    attachments: Vec<crate::agent::Attachment>,
    /// 与 /api/chat 的字段一致：空则按全局配置。
    #[serde(default)]
    model: String,
    #[serde(default)]
    permission_mode: String,
    #[serde(default)]
    thinking_level: Option<u32>,
    #[serde(default)]
    plan_mode: bool,
}

/// POST /api/agents/{id}/say — 向子代理发消息。uid=197610(eason) gid=197121 groups=197121 接受子代理 id 或其
/// 子会话 id（前端面板手头只有会话 id）。运行中注入 steering，已收尾
/// 则续开新轮（子会话历史完整保留）。
async fn agent_say(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(r): Json<AgentSayReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    // C-11：与 /api/chat 同一套 permission_mode 白名单，非法值直接 400。
    if !r.permission_mode.is_empty()
        && !crate::config::PERMISSION_MODES.contains(&r.permission_mode.as_str())
    {
        return bad_request("permission_mode must be ask|ws|full");
    }
    let mut opts = TurnOptions::default();
    if !r.model.is_empty() {
        opts.model_sel = r.model.clone();
    }
    if !r.permission_mode.is_empty() {
        opts.permission_mode = r.permission_mode.clone();
    }
    if let Some(l) = r.thinking_level {
        opts.thinking_level = l;
    }
    opts.plan_mode = r.plan_mode;
    // 面板请求不带调用方会话：先查出子代理所属根，再在其子树内投递消息。
    let root = state
        .subagents
        .resolve_any(&id)
        .await
        .map(|c| c.root_session)
        .unwrap_or_else(|| id.clone());
    match state.subagents.say(&root, &id, &r.text, r.attachments, opts).await {
        Ok((agent_id, session_id, delivered)) => Json(json!({
            "ok": true, "agentId": agent_id, "sessionId": session_id, "delivered": delivered,
        }))
        .into_response(),
        Err(e) => server_error(e),
    }
}

// ------------------------------------------------------------------ agent teams

/// The board and roster of one agent tree.
async fn get_team(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<SessionQuery>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let sid = match q.session {
        Some(s) if !s.is_empty() => s,
        _ => return bad_request("`session` query parameter is required"),
    };
    let root = root_of(&state, &sid).await;
    let board = state.store.get_team(&root).await;
    let members = state.subagents.tree(&root).await;
    Json(json!({
        "root": root,
        "enabled": board.as_ref().map(|b| b.enabled).unwrap_or(false),
        "tasks": board.as_ref().map(crate::team::board_view).unwrap_or_default(),
        "members": members,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct TeamModeReq {
    session_id: String,
    enabled: bool,
}

/// Turn team mode on or off for the tree rooted at `session_id`.
///
/// Only the root may change it: a member's mode would be a second, divergent
/// copy of the same decision.
async fn set_team_mode(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<TeamModeReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let root = root_of(&state, &r.session_id).await;
    if root != r.session_id {
        return bad_request("team mode can only be changed from the root session");
    }
    let mut board = state
        .store
        .get_team(&root)
        .await
        .unwrap_or_else(crate::team::TeamState::new);
    board.enabled = r.enabled;
    if let Err(e) = state.store.set_team(&root, Some(board)).await {
        return server_error(e);
    }
    state.hub.publish(
        &root,
        "team.mode",
        json!({"enabled": r.enabled}),
    );
    Json(json!({"enabled": r.enabled})).into_response()
}

#[derive(Debug, Deserialize)]
struct TeamTaskReq {
    session_id: String,
    subject: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    blocked_by: Vec<String>,
    #[serde(default)]
    write_scopes: Vec<String>,
}

/// Create one task on the shared board.
async fn create_team_task(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<TeamTaskReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let root = root_of(&state, &r.session_id).await;
    // T-01：整板读-改-写放在 per-root 锁内，并发 create/update 不互相覆盖。
    let board_lock = state.store.team_lock(&root).await;
    let _board_guard = board_lock.lock().await;
    let mut board = match state.store.get_team(&root).await {
        Some(b) if b.enabled => b,
        _ => return bad_request("agent teams are not enabled for this session"),
    };
    let caller = crate::team::Caller {
        session_id: r.session_id.clone(),
        name: state
            .subagents
            .catalog
            .find_by_session(&r.session_id)
            .await
            .map(|c| c.name)
            .unwrap_or_else(|| "lead".to_string()),
        root_id: root.clone(),
        is_lead: r.session_id == root,
    };
    let req = crate::team::CreateTask {
        subject: r.subject,
        description: r.description,
        blocked_by: r.blocked_by,
        write_scopes: r.write_scopes,
    };
    let view = match crate::team::create_task(&mut board, &caller, &req) {
        Ok(v) => v,
        Err(e) => return bad_request(&e.to_string()),
    };
    if let Err(e) = state.store.set_team(&root, Some(board)).await {
        return server_error(e);
    }
    state.hub.publish(
        &root,
        "team.task",
        json!({"taskId": view.id, "status": view.status.as_str(), "revision": view.revision}),
    );
    Json(json!({"task": view})).into_response()
}

#[derive(Debug, Deserialize)]
struct TeamTaskUpdateReq {
    session_id: String,
    expected_revision: u64,
    action: String,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    blocked_by: Option<Vec<String>>,
    #[serde(default)]
    write_scopes: Option<Vec<String>>,
    #[serde(default)]
    owner: Option<String>,
}

/// Apply one compare-and-set transition to a shared task.
async fn update_team_task(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
    Json(r): Json<TeamTaskUpdateReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let action = match crate::team::TaskAction::parse(&r.action) {
        Some(a) => a,
        None => return bad_request("unknown action; expected claim | release | edit | set_dependencies | complete | reopen | reassign | delete"),
    };
    let root = root_of(&state, &r.session_id).await;
    // T-01：整板读-改-写放在 per-root 锁内，并发 create/update 不互相覆盖。
    let board_lock = state.store.team_lock(&root).await;
    let _board_guard = board_lock.lock().await;
    let mut board = match state.store.get_team(&root).await {
        Some(b) if b.enabled => b,
        _ => return bad_request("agent teams are not enabled for this session"),
    };
    // The owner is named; resolve it to a session id inside this tree.
    let (owner_id, owner_name) = match r.owner.as_deref() {
        Some(name) if !name.is_empty() => match state
            .subagents
            .tree(&root)
            .await
            .into_iter()
            .find(|c| c.name == name || c.id == name)
        {
            Some(c) => (Some(c.session_id), Some(c.name)),
            None => return bad_request(&format!("no agent named `{name}` in this team")),
        },
        _ => (None, None),
    };
    let caller = crate::team::Caller {
        session_id: r.session_id.clone(),
        name: state
            .subagents
            .catalog
            .find_by_session(&r.session_id)
            .await
            .map(|c| c.name)
            .unwrap_or_else(|| "lead".to_string()),
        root_id: root.clone(),
        is_lead: r.session_id == root,
    };
    let req = crate::team::UpdateTask {
        task_id,
        expected_revision: r.expected_revision,
        action,
        subject: r.subject,
        description: r.description,
        blocked_by: r.blocked_by,
        write_scopes: r.write_scopes,
        owner: owner_id,
        owner_name,
    };
    let view = match crate::team::update_task(&mut board, &caller, &req) {
        Ok(v) => v,
        Err(e) => return bad_request(&e.to_string()),
    };
    if let Err(e) = state.store.set_team(&root, Some(board)).await {
        return server_error(e);
    }
    state.hub.publish(
        &root,
        "team.task",
        json!({"taskId": view.id, "status": view.status.as_str(), "revision": view.revision}),
    );
    Json(json!({"task": view})).into_response()
}

async fn health(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    Json(json!({"ok": true, "uptimeMs": crate::event::now_ms().saturating_sub(state.started_at)}))
        .into_response()
}

// ------------------------------------------------------------ side pane
//
// The side pane (the right-hand tabbed panel) needs a few read-only views
// over the workspace plus two interactive ones. All of them sit behind the
// same token/host/origin guards as the rest of the API.

/// The workspace root the side pane browses.
///
/// Falls back to the process's current directory, matching how the agent
/// runtime resolves an empty `workspace` setting.
async fn workspace_root(state: &AppState) -> std::path::PathBuf {
    let ws = state.config.read().await.workspace.clone();
    if ws.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    } else {
        std::path::PathBuf::from(ws)
    }
}

/// Map a domain error onto an HTTP response.
fn ws_error(e: &crate::HarnessError) -> Response {
    match e {
        crate::HarnessError::PermissionDenied(m) => {
            (StatusCode::FORBIDDEN, Json(json!({"error": m}))).into_response()
        }
        crate::HarnessError::NotFound(m) => {
            (StatusCode::NOT_FOUND, Json(json!({"error": m}))).into_response()
        }
        _ => bad_request(&e.to_string()),
    }
}

/// GET /api/fs/tree — one directory level of the workspace.
async fn fs_tree(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let ws = workspace_root(&state).await;
    let path = q.get("path").cloned().unwrap_or_default();
    match crate::wsfs::tree(&ws, &path) {
        Ok(entries) => Json(json!({"entries": entries})).into_response(),
        Err(e) => ws_error(&e),
    }
}

/// GET /api/fs/file — read a text file.
async fn fs_file(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let ws = workspace_root(&state).await;
    let path = q.get("path").cloned().unwrap_or_default();
    let offset = q.get("offset").and_then(|v| v.parse().ok()).unwrap_or(1);
    let limit = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(500);
    match crate::wsfs::read(&ws, &path, offset, limit) {
        Ok(f) => Json(json!({"file": f})).into_response(),
        Err(e) => ws_error(&e),
    }
}

/// GET /api/fs/search — find files by name.
async fn fs_search(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let ws = workspace_root(&state).await;
    let query = q.get("q").cloned().unwrap_or_default();
    match crate::wsfs::search(&ws, &query) {
        Ok(matches) => Json(json!({"matches": matches})).into_response(),
        Err(e) => ws_error(&e),
    }
}

/// GET /api/git/status — working-tree status of the workspace.
async fn git_status(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let ws = workspace_root(&state).await;
    match crate::git::status(&ws) {
        Ok(s) => Json(json!({"status": s})).into_response(),
        Err(e) => ws_error(&e),
    }
}

/// GET /api/git/diff — diff for one file.
async fn git_diff(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let ws = workspace_root(&state).await;
    let path = q.get("path").cloned().unwrap_or_default();
    match crate::git::diff(&ws, &path) {
        Ok(d) => Json(json!({"diff": d})).into_response(),
        Err(e) => ws_error(&e),
    }
}

/// GET /api/browser/state — the controlled browser's current page.
async fn browser_history_go<const DELTA: i32>(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    match state.agent.computer.history_go(DELTA).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => server_error(e),
    }
}

#[derive(Debug, Deserialize, Default)]
struct BrowserReloadQuery {
    /// 接受 `?hard=1` 与 `?hard=true` 两种写法；bool 反序列化只认 true/false，
    /// 侧栏 JS 用的是 1。
    #[serde(default)]
    hard: Option<String>,
}

async fn browser_reload(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<BrowserReloadQuery>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let hard = q
        .hard
        .as_deref()
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let bridge = state.agent.computer.clone();
    let res = if hard {
        bridge.reload_hard().await
    } else {
        bridge.reload().await
    };
    match res {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => server_error(e),
    }
}

#[derive(Debug, Deserialize)]
struct BrowserClickReq {
    x: f64,
    y: f64,
}

async fn browser_click(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<BrowserClickReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    match state.agent.computer.click_at(r.x, r.y).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => server_error(e),
    }
}

#[derive(Debug, Deserialize)]
struct BrowserScrollReq {
    x: f64,
    y: f64,
    #[serde(default)]
    dx: f64,
    #[serde(default)]
    dy: f64,
}

async fn browser_scroll(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<BrowserScrollReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    // 页面坐标防越界：侧栏换算后可能因图片取整多出 0.5px。
    let x = r.x.clamp(0.0, 100_000.0);
    let y = r.y.clamp(0.0, 100_000.0);
    let dx = r.dx.clamp(-5000.0, 5000.0);
    let dy = r.dy.clamp(-5000.0, 5000.0);
    if dx == 0.0 && dy == 0.0 {
        return Json(json!({"ok": true})).into_response();
    }
    match state.agent.computer.scroll_at(x, y, dx, dy).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => server_error(e),
    }
}

#[derive(Debug, Deserialize)]
struct BrowserInputReq {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    key: Option<String>,
}

async fn browser_input(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<BrowserInputReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let bridge = state.agent.computer.clone();
    let res = if let Some(key) = r.key.as_deref().filter(|k| !k.is_empty()) {
        bridge.press_key_combo(key).await
    } else if let Some(text) = r.text.as_deref() {
        if text.chars().count() > 4096 {
            return bad_request("一次输入的文本过长");
        }
        bridge.insert_text(text).await
    } else {
        return bad_request("text 或 key 必填");
    };
    match res {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => server_error(e),
    }
}

#[derive(Debug, Deserialize, Default)]
struct BrowserViewportReq {
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
}

async fn browser_viewport(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<BrowserViewportReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let bridge = state.agent.computer.clone();
    let (w, h) = match (r.width, r.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (w, h),
        _ => {
            return match bridge.clear_viewport().await {
                Ok(()) => Json(json!({"ok": true, "viewport": null})).into_response(),
                Err(e) => server_error(e),
            };
        }
    };
    if !(320..=3840).contains(&w) || !(240..=2160).contains(&h) {
        return bad_request("视口尺寸需在 320–3840 × 240–2160 之间");
    }
    match bridge.set_viewport(w, h).await {
        Ok(()) => Json(json!({"ok": true, "viewport": {"width": w, "height": h}}))
            .into_response(),
        Err(e) => server_error(e),
    }
}

async fn browser_stop(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    match state.agent.computer.stop_loading().await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => server_error(e),
    }
}
/// about:blank 会被 Chromium 无视而回退普通窗口。页面本身是惰性占位。
/// C-16：必须携带有效 token；此页禁止注入凭据/状态数据。
async fn browser_boot(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<IndexQuery>,
) -> Response {
    if !check_auth(&headers, q.token.as_deref(), &state) {
        return unauthorized();
    }
    let mut resp = axum::response::Html(
        "<!doctype html><title>Parable Browser</title>         <body style=\"margin:0;display:grid;place-items:center;height:100vh;         background:#1f1e1d;color:#8a8a85;font:14px/1.6 system-ui\">         Parable Browser</body>",
    )
    .into_response();
    // 第二个 HTML 响应点：与 index() 同一套安全头（CSP 在无脚本的占位页上
    // 只关 'self' 之外的口子；frame-ancestors 是关键补充）。
    apply_html_security_headers(resp.headers_mut());
    resp
}

/// Browser 侧栏的公共元信息（不含截图，1s 轮询很轻）。
fn browser_meta_json(m: &crate::computer::BrowserUiMeta) -> Value {
    json!({
        "url": m.url,
        "title": m.title,
        "ready": m.ready,
        "canBack": m.can_back,
        "canForward": m.can_forward,
        "viewport": {"width": m.width, "height": m.height},
        "viewportLocked": m.viewport_locked,
    })
}

/// GET /api/browser/meta — URL / 标题 / 加载状态 / 历史按钮 / 视口。
async fn browser_meta(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let bridge = state.agent.computer.clone();
    Json(browser_meta_json(&bridge.ui_meta().await)).into_response()
}

/// GET /api/browser/state — 元信息 + 当前画面。
async fn browser_state(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let bridge = state.agent.computer.clone();
    let meta = bridge.ui_meta().await;
    // 截图失败（浏览器还没起来）不拖垮整个请求：侧栏仍能显示 URL 与标题。
    let screenshot = bridge.ui_screenshot_png().await.ok();
    let mut v = browser_meta_json(&meta);
    v["screenshot"] = json!(screenshot);
    Json(v).into_response()
}

/// GET /api/browser/text — 页面可见文本，只在用户展开「页面文本」时取。
async fn browser_text(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    match state.agent.computer.page_text().await {
        Ok(text) => Json(json!({"text": text})).into_response(),
        Err(e) => server_error(e),
    }
}

#[derive(Debug, Deserialize)]
struct BrowserNavigateReq {
    url: String,
    /// "window" 时按可见窗口模式打开（不管 browser_headless 设置）：
    /// 用户选了「内置浏览器·窗口」就是要看见窗口。缺省 / 其它值按普通
    /// 导航处理（沿用当前 headless 状态）。
    #[serde(default)]
    mode: Option<String>,
}

/// POST /api/browser/navigate — point the controlled browser at a URL.
async fn browser_navigate(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<BrowserNavigateReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let bridge = state.agent.computer.clone();
    let res = if r.mode.as_deref() == Some("window") {
        bridge.navigate_in_window(&r.url).await
    } else {
        bridge.navigate_to(&r.url).await
    };
    match res {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => bad_request(&e.to_string()),
    }
}

// ------------------------------------------------------------------ 浏览器 cookie 导入

/// GET /api/browser/cookies/sources — 本机可导入的浏览器/配置清单。
async fn browser_cookie_sources(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let sources = crate::cookies::detect_sources();
    let jar = crate::cookies::CookieJar::load();
    Json(json!({
        "sources": sources,
        "jarCount": jar.cookies.len(),
        "jarUpdatedAt": jar.updated_at,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct BrowserCookieImportReq {
    /// Source ids from `sources`, e.g. `["edge:Default", "firefox:xxx"]`.
    sources: Vec<String>,
    /// 允许为解锁 cookie 库临时关闭运行中的源浏览器（抽完自动重开并
    /// 恢复标签页）。前端对锁定来源先弹确认框再置位。
    #[serde(default)]
    close: bool,
}

/// POST /api/browser/cookies/import — 抽取所选浏览器的 cookie，合并进
/// 持久 jar，再注入内置浏览器。单个来源失败不拖垮整批。
async fn browser_cookie_import(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<BrowserCookieImportReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    if r.sources.is_empty() {
        return bad_request("没有选择导入来源");
    }
    let mut per: Vec<Value> = Vec::new();
    let mut extracted = 0usize;
    let mut incoming = Vec::new();
    let mut closed_any = false;
    let mut reopened_any = false;
    for id in &r.sources {
        match crate::cookies::extract_source_ex(id, r.close).await {
            Ok(rep) => {
                extracted += rep.cookies.len();
                closed_any |= rep.closed;
                reopened_any |= rep.reopened;
                per.push(json!({"id": id, "count": rep.cookies.len()}));
                incoming.extend(rep.cookies);
            }
            Err(e) => per.push(json!({"id": id, "error": e.to_string()})),
        }
    }
    let mut jar = crate::cookies::CookieJar::load();
    let total = jar.merge(incoming);
    let _ = jar.save();
    // 注入内置浏览器：只在真抽到东西时才动浏览器（全部来源失败时
    // jar 原样，浏览器保持现状）。浏览器起不来（机器上没有 Edge/Chrome）
    // 也不算导入失败——jar 已落盘，浏览器下次启动自动回放。
    let mut injected = 0usize;
    let mut inject_error = None;
    if extracted > 0 {
        match state.agent.computer.import_cookies(&jar.cookies).await {
            Ok((ok, _fail)) => injected = ok,
            Err(e) => inject_error = Some(e.to_string()),
        }
    }
    let mut body = json!({
        "ok": true,
        "extracted": extracted,
        "jarTotal": total,
        "injected": injected,
        "closed": closed_any,
        "reopened": reopened_any,
        "sources": per,
    });
    if let Some(e) = inject_error {
        body["injectError"] = json!(e);
    }
    Json(body).into_response()
}

/// POST /api/browser/cookies/clear — 清空持久 jar 与内置浏览器 cookie。
async fn browser_cookie_clear(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let mut jar = crate::cookies::CookieJar::load();
    jar.clear();
    let _ = jar.save();
    // 浏览器没起时无需拉起——没有 cookie 可清。
    let _ = state.agent.computer.clear_cookies().await;
    Json(json!({"ok": true})).into_response()
}

#[derive(Debug, Deserialize)]
struct BrowserCookiePasteReq {
    /// `document.cookie` 原文（`a=1; b=2`）或 EditThisCookie 导出的 JSON。
    text: String,
    /// 这些 cookie 归属的域名，如 `.bilibili.com`。
    domain: String,
}

/// POST /api/browser/cookies/paste — 手动粘贴导入。
///
/// ABE（App-Bound Encryption，Chrome/Edge 127+ 默认）使大部分加密 cookie
/// 无法从其他浏览器还原，这条路径是用户的可靠兜底。
async fn browser_cookie_paste(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<BrowserCookiePasteReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let entries = match crate::cookies::parse_pasted(&r.text, &r.domain) {
        Ok(e) => e,
        Err(e) => return bad_request(&e.to_string()),
    };
    let n = entries.len();
    let mut jar = crate::cookies::CookieJar::load();
    let total = jar.merge(entries);
    let _ = jar.save();
    let mut injected = 0usize;
    let mut inject_error = None;
    match state.agent.computer.import_cookies(&jar.cookies).await {
        Ok((ok, _)) => injected = ok,
        Err(e) => inject_error = Some(e.to_string()),
    }
    let mut body = json!({"ok": true, "parsed": n, "jarTotal": total, "injected": injected});
    if let Some(e) = inject_error {
        body["injectError"] = json!(e);
    }
    Json(body).into_response()
}

#[derive(Debug, Deserialize)]
struct TermExecReq {
    command: String,
    /// Optional client-chosen id for this invocation. The client needs the id
    /// *before* the command finishes in order to filter the streamed
    /// `tool.progress` events, so it may supply one; otherwise the server
    /// generates it.
    #[serde(default)]
    call_id: String,
}

/// POST /api/term/exec — run one command in the workspace.
///
/// This deliberately does *not* go through the model's permission gateway:
/// the command was typed by the user in the side pane, not proposed by a
/// model, and asking for approval on your own keystrokes would be noise.
/// The command still runs with the workspace as its root.
async fn term_exec(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<TermExecReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let command = r.command.trim().to_string();
    if command.is_empty() {
        return bad_request("command is required");
    }
    let ws = workspace_root(&state).await;
    // The call id ties the streamed `tool.progress` events to this command.
    let call_id = if r.call_id.trim().is_empty() {
        format!("term_{}", &uuid::Uuid::new_v4().simple().to_string()[..10])
    } else {
        r.call_id.trim().to_string()
    };
    // The session id is empty so the progress events broadcast to every
    // connected client; the client filters by `callId`.
    let result = crate::tools::shell::bash(
        &command,
        &ws,
        120_000,
        &state.hub,
        "",
        &call_id,
        "",
    )
    .await;
    match result {
        Ok(out) => {
            let stdout = match &out.display {
                crate::tools::DisplayKind::Shell { stdout, .. } => stdout.clone(),
                _ => out.for_model.clone(),
            };
            let exit = match &out.display {
                crate::tools::DisplayKind::Shell { exit, .. } => *exit,
                _ => 0,
            };
            Json(json!({"callId": call_id, "stdout": stdout, "exit": exit}))
                .into_response()
        }
        Err(e) => Json(json!({"callId": call_id, "error": e.to_string()})).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct TermStartReq {
    command: String,
    #[serde(default)]
    cwd: String,
}

/// POST /api/term/start — 启动一个后台终端会话，立刻返回。
///
/// 与 `/term/exec` 的区别：exec 会一直等到命令结束才响应，悬浮窗需要的是
/// 「启动即可见、输出边跑边看、随时能杀」的常驻会话。
async fn term_start(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<TermStartReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let command = r.command.trim().to_string();
    if command.is_empty() {
        return bad_request("command is required");
    }
    let root = workspace_root(&state).await;
    let cwd = if r.cwd.trim().is_empty() {
        root.clone()
    } else {
        match crate::tools::fs::resolve_within(&root, &r.cwd) {
            Ok(p) if p.is_dir() => p,
            Ok(_) => return bad_request("cwd is not a directory"),
            Err(e) => return bad_request(&e.to_string()),
        }
    };
    let job = match state.terms.start(state.hub.clone(), cwd, command).await {
        Ok(j) => j,
        Err(e) => return bad_request(&e.to_string()),
    };
    Json(json!({ "job": job })).into_response()
}

/// GET /api/term/list — 列出全部后台终端（含已结束的尾部输出）。
async fn term_list(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let jobs = state.terms.list().await;
    Json(json!({ "jobs": jobs, "running": jobs.iter().filter(|j| j.status == "running").count() }))
        .into_response()
}

#[derive(Debug, Deserialize)]
struct TermKillReq {
    id: String,
}

/// POST /api/term/kill — 终止一个正在跑的后台终端。
async fn term_kill(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(r): Json<TermKillReq>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let killed = state.terms.kill(&state.hub, r.id.trim()).await;
    Json(json!({ "killed": killed })).into_response()
}

/// POST /api/term/clear — 清掉已结束的后台终端，运行中的保留。
async fn term_clear(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    state.terms.clear().await;
    Json(json!({ "ok": true })).into_response()
}

async fn status(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let cfg = state.config.read().await;
    Json(json!({
        "uptimeMs": crate::event::now_ms().saturating_sub(state.started_at),
        "model": cfg.model,
        "permissionMode": cfg.permission_mode,
        "thinkingLevel": cfg.thinking_level,
        "workspace": cfg.workspace,
    }))
    .into_response()
}

/// 真实主机名：优先 `COMPUTERNAME`（Windows），退 `HOSTNAME`，再退 `unknown`。
/// 不引入 hostname crate 这类重依赖。
fn real_hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

/// 粗略估算一段会话最近一次请求的上下文占用。
///
/// 只在「内存没有最近请求、落盘也没有 `ctx_used`」时兜底（旧会话 / 重启后）。
/// 两条下界取大：① 消息正文估算（不含 system prompt / 工具 schema，只偏小）；
/// ② 累计输入 token ÷ 模型调用次数（调用次数 ≈ steps + turns，含每轮首调）。
/// 后者能补上体积很大的系统提示词与工具定义，但仍标注为估算。
fn estimate_context_tokens(sess: &crate::session::Session) -> u64 {
    fn walk(v: &Value) -> u64 {
        match v {
            Value::String(s) => crate::token::estimate(s) as u64,
            Value::Array(a) => a.iter().map(walk).sum(),
            Value::Object(o) => o.values().map(walk).sum(),
            _ => 0,
        }
    }
    let history: u64 = sess.messages.iter().map(|m| walk(&m.parts)).sum();
    let calls = (sess.usage.steps + sess.usage.turns).max(1);
    let avg = sess.usage.input_tokens / calls;
    history.max(avg)
}

/// GET /api/environment —— Environment 面板的只读环境快照（契约 §2.1）。
///
/// 全部取真实值：主机名 / 系统 / 架构 / 版本 / pid / workspace / home 都来自
/// 运行时与 `Config`；`context.used` 取最近活动会话**最后一次请求**的上下文
/// 占用（不是会话累计计费 token），无会话为 0 且 `estimated:false`。
async fn environment(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let (model_json, ctx_limit, workspace, permission_mode, sandbox_mode, fallback_chain) = {
        let cfg = state.config.read().await;
        let (model_json, limit) = match cfg.active_model() {
            Some((p, m)) => (
                json!({
                    "provider": p.id,
                    "model": m.id,
                    "label": m.name,
                    "thinkingLevel": cfg.thinking_level,
                    "contextLimit": m.context,
                }),
                m.context,
            ),
            None => (
                json!({
                    "provider": "",
                    "model": "",
                    "label": "",
                    "thinkingLevel": cfg.thinking_level,
                    "contextLimit": 0,
                }),
                0u64,
            ),
        };
        (
            model_json,
            limit,
            cfg.workspace.clone(),
            cfg.permission_mode.clone(),
            cfg.sandbox.mode.clone(),
            cfg.fallback_chain.clone(),
        )
    };

    // context.used：最近活动会话（list 已按置顶 + updated_at 倒序）**最近一次
    // 请求**的上下文占用。此前误用会话累计 input+output，一轮十几次工具调用会
    // 堆到窗口的数倍（200K 窗口显示 197%）。内存 stats 有最近请求就优先用，
    // 进程重启后回退到落盘的 `usage.ctx_used`。
    let recent = state.store.list("").await.into_iter().next();
    let used: u64 = match recent {
        Some(s) => {
            let live = state.agent.stats_snapshot(&s.id).await;
            let live_used = live.ctx_system + live.ctx_tools + live.ctx_messages;
            if live_used > 0 {
                live_used
            } else {
                match state.store.load(&s.id).await {
                    Some(full) => {
                        if full.usage.ctx_used > 0 {
                            full.usage.ctx_used
                        } else {
                            estimate_context_tokens(&full)
                        }
                    }
                    None => 0,
                }
            }
        }
        None => 0,
    };
    let pct = if ctx_limit > 0 {
        used as f64 / ctx_limit as f64
    } else {
        0.0
    };

    // MCP：与 /api/mcp 同一份状态表，按 id 对齐配置与运行状态。
    let servers_cfg = state.config.read().await.mcp_servers.clone();
    let status = state.agent.mcp.list_status().await;
    let mut connected = 0u64;
    let mut failed = 0u64;
    let mut out_servers = Vec::with_capacity(servers_cfg.len());
    for s in &servers_cfg {
        let st = status.iter().find(|x| x.id == s.id);
        let (status_str, error) = if !s.enabled {
            ("disabled", Value::Null)
        } else {
            match st {
                Some(x) if x.connected => {
                    connected += 1;
                    ("connected", Value::Null)
                }
                // list_status 在握手锁未抢到/进行中时填 "(连接中…)"。
                Some(x) if x.error == "(连接中…)" => ("connecting", Value::Null),
                Some(x) => {
                    failed += 1;
                    let err = if x.error.is_empty() {
                        Value::Null
                    } else {
                        Value::String(x.error.clone())
                    };
                    ("failed", err)
                }
                // 已启用但尚未登记连接（启动后还没握手）：归为 connecting。
                None => ("connecting", Value::Null),
            }
        };
        out_servers.push(json!({
            "id": s.id,
            "name": s.name,
            "transport": if s.r#type == "http" { "http" } else { "stdio" },
            "status": status_str,
            "error": error,
        }));
    }

    Json(json!({
        "hostname": real_hostname(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
        "workspace": workspace,
        "home": Config::home_dir(),
        "model": model_json,
        "context": {"used": used, "limit": ctx_limit, "pct": pct, "estimated": false},
        "mcp": {
            "connected": connected,
            "failed": failed,
            "total": servers_cfg.len(),
            "servers": out_servers,
        },
        "permissionMode": permission_mode,
        "sandbox": {"mode": sandbox_mode, "policy": "v1"},
        // 备用链来自 config（PATCH /api/config 可写）；「自动选模型」已移除。
        "auto": {"fallbackChain": fallback_chain},
    }))
    .into_response()
}

/// GET /api/telemetry?session=<id> —— 单会话遥测（契约 §2.2）。
///
/// 会话不存在 / 未开始也返回 200（全零 + tps.value=null），不 500。TPS 取最近
/// 10s 滑动窗口内的真实输出采样；cache 仅在 provider usage 真给过读/写字段
/// （累计 > 0）时 `supported:true`，否则前端显示 `—`。
async fn telemetry(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<SessionQuery>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let sid = q.session.unwrap_or_default();
    let now = crate::event::now_ms();

    let exists = !sid.is_empty() && state.store.exists(&sid).await;
    let sess = if exists { state.store.load(&sid).await } else { None };
    let live = state.agent.stats_snapshot(&sid).await;
    let running = state.agent.is_running(&sid).await;
    let started_at = state
        .agent
        .turn_started_at(&sid)
        .await
        .or_else(|| sess.as_ref().map(|s| s.meta.created_at))
        .unwrap_or(0);

    let u = sess.as_ref().map(|s| &s.usage);
    // 内存 stats 在进程启动时会用落盘 usage 回填（hydrate_stats），若直接相加，
    // 重启后同一份历史会被算两遍（实测 192K 变 384K）。取两者较大值：重启后
    // 两者相等；运行中 live 已含「历史 + 本轮新增」，也不会重复。
    let prompt = u
        .map(|x| x.input_tokens)
        .unwrap_or(0)
        .max(live.tokens.input_tokens);
    let completion = u
        .map(|x| x.output_tokens)
        .unwrap_or(0)
        .max(live.tokens.output_tokens);
    let cache_read = u
        .map(|x| x.cache_read)
        .unwrap_or(0)
        .max(live.tokens.cache_read_input_tokens);
    let cache_write = u
        .map(|x| x.cache_write)
        .unwrap_or(0)
        .max(live.tokens.cache_creation_input_tokens);
    let rounds = live.turns.max(u.map(|x| x.turns).unwrap_or(0));
    let steps = live.steps.max(u.map(|x| x.steps).unwrap_or(0));
    let elapsed_ms = if running {
        now.saturating_sub(started_at)
    } else {
        u.map(|x| x.wall_ms).unwrap_or(0)
    };

    let (tps_value, samples) = live.tps_window(now);

    // cache：只有累计读到过读/写字段才算支持；hitRate = read/(prompt+read+write)。
    let cache_supported = cache_read > 0 || cache_write > 0;
    let cache_hit_rate = if cache_supported {
        let denom = prompt + cache_read + cache_write;
        if denom > 0 {
            Some(cache_read as f64 / denom as f64)
        } else {
            None
        }
    } else {
        None
    };

    let (ctx_limit, model_json) = {
        let cfg = state.config.read().await;
        let sel = sess
            .as_ref()
            .map(|s| s.meta.model.as_str())
            .filter(|m| !m.is_empty())
            .unwrap_or(cfg.model.as_str());
        match cfg.resolve_model(sel) {
            Some((p, m)) => (m.context, json!({"provider": p.id, "model": m.id})),
            None => (
                cfg.active_model().map(|(_, m)| m.context).unwrap_or(0),
                json!({"provider": "", "model": ""}),
            ),
        }
    };
    // 上下文占用 = 最近一次请求的 system + tools + messages 估计；取不到时
    // 回退到落盘值。绝不能用 prompt+completion——那是整轮累加的计费 token。
    let live_used = live.ctx_system + live.ctx_tools + live.ctx_messages;
    let used = if live_used > 0 {
        live_used
    } else if let Some(x) = u.map(|x| x.ctx_used).filter(|x| *x > 0) {
        x
    } else {
        sess.as_ref()
            .map(estimate_context_tokens)
            .unwrap_or(0)
    };
    let pct = if ctx_limit > 0 {
        used as f64 / ctx_limit as f64
    } else {
        0.0
    };

    Json(json!({
        "session": sid,
        "turn": {
            "rounds": rounds,
            "steps": steps,
            "startedAt": started_at,
            "elapsedMs": elapsed_ms,
            "running": running,
            // 计时字段只在进程内有实时观测（运行中/本轮刚结束）时非零；
            // 历史会话按设计为 0——前端据此显示 0 秒而不是上一个会话的残留。
            "modelMs": live.model_ms,
            "toolMs": live.tool_ms,
            "ttftMs": live.ttft_ms,
        },
        "tokens": {"prompt": prompt, "completion": completion, "total": prompt + completion},
        "tps": {"value": tps_value, "windowSec": 10, "samples": samples},
        "cache": {
            "hitRate": cache_hit_rate,
            "read": cache_read,
            "write": cache_write,
            "supported": cache_supported,
        },
        // 上下文细分同样只有运行中的实时观测；历史会话全 0，前端条宽归零。
        "context": {
            "used": used, "limit": ctx_limit, "pct": pct,
            "system": live.ctx_system, "tools": live.ctx_tools, "messages": live.ctx_messages,
        },
        "model": model_json,
    }))
    .into_response()
}

/// Aggregate usage statistics for the 设置 → 使用统计 page.
///
/// `range` scopes the chart series (`all` | `7d` | `30d`); the summary
/// strip and the heatmap are always all-time.
async fn get_usage(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let range = match q.get("range").map(|s| s.as_str()) {
        Some("7d") => "7d",
        Some("30d") => "30d",
        _ => "all",
    };
    let sessions_root = Config::sessions_dir();
    let mut snap = state.usage.snapshot(range, &sessions_root).await;
    // The journal only knows session ids; the title lives in the session
    // file. Enrich the "top sessions" list so the UI can show something
    // meaningful instead of a raw id.
    if let Some(list) = snap.get_mut("topSessions").and_then(|v| v.as_array_mut()) {
        let summaries = state.store.list("").await;
        let titles: std::collections::HashMap<&str, &str> = summaries
            .iter()
            .map(|s| (s.id.as_str(), s.title.as_str()))
            .collect();
        for item in list.iter_mut() {
            let Some(id) = item.get("sessionId").and_then(|v| v.as_str()) else {
                continue;
            };
            let title = titles.get(id).copied().unwrap_or("");
            if let Some(o) = item.as_object_mut() {
                o.insert("title".to_string(), json!(title));
            }
        }
    }
    Json(snap).into_response()
}

async fn tools(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    // Same search-preference tailoring as a real turn, so the UI listing
    // matches what the model actually gets.
    let (pref, key, akey) = {
        let cfg = state.config.read().await;
        (cfg.search_pref.clone(), cfg.search_key.clone(), cfg.anysearch_key.clone())
    };
    let reg = crate::tools::Registry::new(std::path::PathBuf::from(".")).with_search(&pref, &key, &akey);
    // 阶段四：强 ETag（schema 版本 + 插件数量 + MCP 数量/世代 + search_pref）。
    // 命中 If-None-Match 返回 304，不序列化工具 body。
    let etag = reg.schema_etag();
    let etag_value = HeaderValue::from_str(&etag).unwrap_or_else(|_| HeaderValue::from_static(""));
    if let Some(incoming) = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        if incoming.trim() == etag {
            return (
                StatusCode::NOT_MODIFIED,
                [(axum::http::header::ETAG, etag_value)],
            )
                .into_response();
        }
    }
    let specs = reg.specs();
    let mut resp = Json(json!({"tools": specs.as_ref()})).into_response();
    resp.headers_mut()
        .insert(axum::http::header::ETAG, etag_value);
    resp
}

// ------------------------------------------------------------------ bootstrap

pub async fn serve(
    token: String,
    config: Arc<RwLock<Config>>,
    port: u16,
) -> anyhow::Result<(u16, SharedState, tokio::task::JoinHandle<()>)> {
    serve_in(token, config, port, None).await
}

/// Like [`serve`], but optionally rooted at an explicit data home.
///
/// `home` overrides `Config::home_dir()` for the session store and the usage
/// journal. Tests use it to run against a scratch directory instead of the
/// user's real `~/.parable`.
pub async fn serve_in(
    token: String,
    config: Arc<RwLock<Config>>,
    port: u16,
    home: Option<std::path::PathBuf>,
) -> anyhow::Result<(u16, SharedState, tokio::task::JoinHandle<()>)> {
    let home = home.unwrap_or_else(Config::home_dir);
    let hub = EventHub::new();
    let broker = Arc::new(PermissionBroker::new(hub.clone()));
    let questions = Arc::new(QuestionBroker::new());
    let store: SharedStore = Arc::new(SessionStore::with_root(home.join("sessions")));
    // 用量日志：每轮一条记录，供「使用统计」页聚合。
    let usage = Arc::new(crate::usage::UsageJournal::new(home.join("usage")));

    // 代理设置初始化（http_client / apply_proxy 工厂用）。
    {
        let cfg = config.read().await;
        crate::config::set_proxy(cfg.http_proxy.clone(), cfg.http_no_proxy.clone());
    }
    // 免费模型目录后台补全：启动时拉一次 OpenCode 免费模型清单，填进内置
    // provider 的模型表并落盘。失败不阻塞启动（目录端点偶发不通）。
    {
        let config = config.clone();
        tokio::spawn(async move {
            let models = match crate::provider::opencode_free::list_free_models().await {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("free model list unavailable: {e}");
                    return;
                }
            };
            let mut cfg = config.write().await;
            // 启动时后台补免费目录也会落盘：先取盘上最新配置，别把别的
            // 进程（桌面壳）刚写入的供应商用启动时的旧快照盖掉。
            cfg.refresh_from_disk();
            let Some(p) = cfg.providers.iter_mut().find(|p| p.id == "opencode-free") else {
                return;
            };
            let mut changed = false;
            for m in models {
                if !p.models.iter().any(|x| x.id == m.id) {
                    p.models.push(crate::config::ModelConfig {
                        id: m.id.clone(),
                        name: m.name.clone().unwrap_or_else(|| m.id.clone()),
                        context: m.context.unwrap_or(128_000),
                        vision: m.vision.unwrap_or(false),
                        tools: true,
                        thinking: m.thinking.unwrap_or(false),
                        max_output: 0,
                        levels: Vec::new(),
                        mapping: String::new(),
                        structured: false,
                        native_search: false,
                        enabled: true,
                        fallbacks: Vec::new(),
                    });
                    changed = true;
                }
            }
            if changed {
                tracing::info!("free provider: model list refreshed");
                let _ = cfg.save();
            }
        });
    }
    // 自动归档：N 天未活跃的非置顶会话移入归档（设置 → 会话管理；0 = 关闭）。
    {
        let store = store.clone();
        let config = config.clone();
        tokio::spawn(async move {
            let days = config.read().await.auto_archive_days;
            if days == 0 {
                return;
            }
            let cutoff = crate::event::now_ms() as u64 - (days as u64) * 86_400_000;
            let mut n = 0usize;
            for s in store.list("").await {
                if s.archived || s.pinned || s.updated_at >= cutoff {
                    continue;
                }
                if store.update_meta(&s.id, |m| m.archived = true).await.is_ok() {
                    n += 1;
                }
            }
            if n > 0 {
                tracing::info!("auto-archived {n} stale sessions (>{days}d)");
            }
            // 保留清理：已归档会话超过 session_retention_days 直接删除
            // （0 = 永不清理；只动已归档的，活跃/置顶永不碰）。
            let keep_days = config.read().await.session_retention_days;
            if keep_days > 0 {
                let cutoff = crate::event::now_ms() as u64 - (keep_days as u64) * 86_400_000;
                let mut deleted = 0usize;
                for s in store.list("").await {
                    if !s.archived || s.pinned || s.updated_at >= cutoff {
                        continue;
                    }
                    if store.delete(&s.id).await.is_ok() {
                        deleted += 1;
                    }
                }
                if deleted > 0 {
                    tracing::info!("retention: deleted {deleted} archived sessions (>{keep_days}d)");
                }
            }
        });
    }
    // 任务表与运行历史都落在本次服务的数据根下（测试/沙箱不碰用户真实 home）。
    let sched_store = Arc::new(crate::schedule::ScheduleStore::with_path(
        home.join("schedule.json"),
    ));

    // schedule 不依赖 runtime,先建;workspace 根取全局配置(定时任务建会话用),
    // 空配置时与 agent 同款退回 current_dir。
    let ws_root = {
        let ws = config.read().await.workspace.clone();
        if ws.is_empty() {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        } else {
            std::path::PathBuf::from(ws)
        }
    };
    let schedule: crate::schedule::SharedSchedule = Arc::new(
        crate::schedule::ScheduleService::new(sched_store, hub.clone())
            .with_sessions(store.clone())
            .with_workspace(ws_root),
    );

    // AgentRuntime 与 subagents 互相需要：runtime 挂 subagents 才能挂载
    // agent_* 工具，subagents 需要 runtime 才能起子轮。构造期成环，改用
    // **后绑定**：先建 runtime（subagents 槽为空），再建 subagents 服务并把
    // runtime 装进去，最后把服务装回 runtime 的槽。
    //
    // 关键点：subagents 槽是 `Arc<RwLock<..>>`，而 AgentRuntime 的 clone
    // 共享同一个 Arc。所以根 runtime 与子 runtime 看到的是**同一个**服务——
    // 子代理再 spawn 孙代理时拿到的是同一个服务，嵌套委托因此可用（此前
    // 子代理拿到的是 None，只会得到 "subagents are not mounted"）。
    let agent_base = AgentRuntime::new(
        config.clone(),
        hub.clone(),
        broker.clone(),
        questions.clone(),
        store.clone(),
    )
    .with_usage(usage.clone());
    // 内置浏览器：app 引导页 + 无头开关（默认隐藏窗口，侧栏实况即视图）。
    // C-16：引导页同样走鉴权，token 经 query 带入（与桌面启动链接同一口径）。
    agent_base
        .computer
        .set_boot_url(&format!("http://127.0.0.1:{port}/browser-boot?token={token}"));
    {
        let cfg = config.read().await;
        agent_base.computer.set_headless(cfg.browser_headless);
    }

    let subagents: crate::subagent::SharedSubagents = Arc::new(
        crate::subagent::SubagentService::new(hub.clone()).with_store(store.clone()),
    );
    // 子代理的 runtime 与根 runtime 共享全部内部状态（config/hub/broker/
    // store/运行槽），差别只在于它自己也是那棵树的成员。
    let child_runtime: Arc<AgentRuntime> = Arc::new(
        agent_base
            .clone()
            .with_services(Some(schedule.clone()), None)
            .await,
    );
    subagents.set_runtime(child_runtime).await;

    // 后台终端注册表：UI 的「后台终端」面板与工具的 background 模式共用一份，
    // 所以先建出来，再注入 agent。
    let terms = crate::term::TerminalRegistry::new();

    let agent = agent_base
        .with_terms(terms.clone())
        .with_services(Some(schedule.clone()), Some(subagents.clone()))
        .await;
    // 统计恢复：会话的轮次/token/耗时随文件持久化，但内存里的 SessStats
    // 重启即清空——不回填的话状态栏会显示 0 tok/s、0 轮次，像是数据丢了。
    {
        let agent = agent.clone();
        tokio::spawn(async move { agent.hydrate_stats().await });
    }
    // Background scheduler: every 20s, fire tasks whose occurrence has passed.
    // Delivery is an ordinary follow-up turn into the target session.
    {
        let schedule = schedule.clone();
        let agent = agent.clone();
        let config = config.clone();
        let broker = broker.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(20)).await;
                let now = chrono::Utc::now();
                let due = schedule.due(now).await;
                for (mut task, _fired) in due {
                    // shell 动作不经过模型、不需要会话：直接执行命令并记录结果。
                    if let crate::schedule::TaskAction::Shell { command } = task.action.clone() {
                        let cfg = config.read().await;
                        let ws = if cfg.workspace.is_empty() {
                            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
                        } else {
                            std::path::PathBuf::from(&cfg.workspace)
                        };
                        let perm = cfg.permission_mode.clone();
                        drop(cfg);
                        // C-01：无人值守通道也要过权限网关。后台无法弹确认，
                        // auto_allow 为 false（ws/ask 模式、deny/ask 规则）一律
                        // 拒绝并留痕；只有 full 模式或显式 allow 规则放行。
                        let allowed = broker
                            .auto_allow(
                                &perm,
                                "bash",
                                &ws.to_string_lossy(),
                                Some(&command),
                                crate::permission::Risk::Shell,
                                &ws,
                            )
                            .await;
                        if !allowed {
                            tracing::warn!(
                                "scheduled shell task {} 被权限网关拒绝（{perm} 模式，无人值守不可确认）: {command}",
                                task.id
                            );
                            // 被拒也算一次运行（失败），留痕便于排查。
                            let mut rec = crate::schedule::RunRecord::new(&task.id, &task.name);
                            rec.ok = false;
                            rec.finished_at = Some(now_iso());
                            rec.error = Some(format!(
                                "blocked by permission gateway ({perm} mode, unattended runs cannot confirm)"
                            ));
                            schedule.store().record_run(rec).await;
                            continue;
                        }
                        tracing::info!(
                            "scheduled shell task {} firing ({} mode): {}",
                            task.id, perm, command
                        );
                        // 运行记录先落一条"已启动"，命令结束后回填退出码与输出。
                        let mut rec = crate::schedule::RunRecord::new(&task.id, &task.name);
                        rec.ok = false;
                        rec.finished_at = None;
                        let run_id = rec.id.clone();
                        let store = schedule.store();
                        store.record_run(rec).await;
                        let store = store.clone();
                        tokio::spawn(async move {
                            // 到期的 shell 任务用 10 分钟上限，避免一个卡死命令
                            // 把后台调度线程拖住（每次 fire 都是独立 task）。
                            let res = crate::tools::shell::bash(
                                &command,
                                &ws,
                                600_000,
                                &crate::event::EventHub::new(),
                                "",
                                "",
                                "",
                            )
                            .await;
                            match res {
                                Ok(out) => {
                                    tracing::info!(
                                        "scheduled shell task {} finished: {}",
                                        task.id, out.title
                                    );
                                    // 退出码与输出尾部回填到运行记录。
                                    let (exit, output) = match &out.display {
                                        crate::tools::DisplayKind::Shell { exit, stdout, stderr, .. } => (
                                            Some(*exit),
                                            format!("{}{}", stdout, stderr),
                                        ),
                                        _ => (None, out.for_model.clone()),
                                    };
                                    let ok = out.ok && exit.map(|c| c == 0).unwrap_or(true);
                                    let _ = store
                                        .update_run(&run_id, |r| {
                                            r.finished_at = Some(now_iso());
                                            r.exit_code = exit;
                                            r.ok = ok;
                                            r.output = crate::schedule::tail_output(&output);
                                            if !ok {
                                                r.error = Some(match exit {
                                                    Some(c) => format!("command exited with code {c}"),
                                                    None => "command failed".into(),
                                                });
                                            }
                                        })
                                        .await;
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "scheduled shell task {} failed: {e}",
                                        task.id
                                    );
                                    let msg = e.to_string();
                                    let _ = store
                                        .update_run(&run_id, |r| {
                                            r.finished_at = Some(now_iso());
                                            r.ok = false;
                                            r.exit_code = Some(-1);
                                            r.error = Some(msg.clone());
                                        })
                                        .await;
                                }
                            }
                        });
                        continue;
                    }
                    match schedule.resolve_session(&mut task).await {
                        Ok(sid) => {
                            // 与手动"立即运行"一致:用全局配置的权限模式,
                            // 调度投递不偷偷提权到 full。
                            let mode = config.read().await.permission_mode.clone();
                            // 任务自带模型优先；没指定才跟随全局。
                            let opts = crate::agent::TurnOptions {
                                model_sel: task.model_sel.clone(),
                                permission_mode: mode,
                                thinking_level: 0,
                                plan_mode: false,
                                ..TurnOptions::default()
                            };
                            let prompt = format!(
                                "[scheduled task `{}` fired at {}]\n\n{}",
                                task.name,
                                now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                                task.prompt
                            );
                            // 投递结果落运行记录：成功=已投进会话，失败=记原因。
                            match agent.start_turn(&sid, &prompt, Vec::new(), opts).await {
                                Ok(turn) => {
                                    let mut rec =
                                        crate::schedule::RunRecord::new(&task.id, &task.name);
                                    rec.ok = true;
                                    rec.session_id = Some(sid.clone());
                                    rec.output = format!("delivered to session as turn {turn}");
                                    schedule.store().record_run(rec).await;
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "scheduled task {} failed to start: {e}",
                                        task.id
                                    );
                                    let mut rec =
                                        crate::schedule::RunRecord::new(&task.id, &task.name);
                                    rec.ok = false;
                                    rec.finished_at = Some(now_iso());
                                    rec.session_id = Some(sid.clone());
                                    rec.error = Some(e.to_string());
                                    schedule.store().record_run(rec).await;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("scheduled task {} has no target: {e}", task.id);
                            // 目标解析失败也算一次失败运行。
                            let mut rec =
                                crate::schedule::RunRecord::new(&task.id, &task.name);
                            rec.ok = false;
                            rec.finished_at = Some(now_iso());
                            rec.error = Some(e.to_string());
                            schedule.store().record_run(rec).await;
                        }
                    }
                }
            }
        });
    }

    let memory = Arc::new(crate::memory::MemoryStore::with_path(
        crate::config::Config::memory_path(),
    ));

    let state: SharedState = Arc::new(AppState {
        token: token.clone(),
        hub,
        config,
        broker,
        questions,
        store,
        agent,
        schedule,
        subagents,
        usage,
        memory,
        terms,
        started_at: crate::event::now_ms(),
        port: std::sync::atomic::AtomicU16::new(0),
    });

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let port = listener.local_addr()?.port();
    // Publish the resolved port before the server task starts so the Origin
    // whitelist can admit same-origin webview traffic.
    state
        .port
        .store(port, std::sync::atomic::Ordering::Relaxed);
    let app = build_router(state.clone());

    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        {
            tracing::error!("server error: {e}");
        }
    });

    Ok((port, state, handle))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回环校验必须经得起「origin 永远带端口」：桌面壳的 location.origin 是
    /// 127.0.0.1:7789，旧实现 rsplit(':') 把端口当主机名 → 恒不匹配 →
    /// 「在浏览器中打开」400 被拒（用户实测点按钮毫无反应）。
    #[test]
    fn loopback_url_accepts_port_and_query() {
        assert!(is_loopback_url("http://127.0.0.1:7789/?token=abc"));
        assert!(is_loopback_url("http://127.0.0.1:59182"));
        assert!(is_loopback_url("http://127.0.0.1"));
        assert!(is_loopback_url("http://localhost:3000/x"));
        assert!(is_loopback_url("http://LOCALHOST"));
        assert!(is_loopback_url("http://[::1]:8080/"));
        assert!(is_loopback_url("http://127.0.0.2:1/"));
        assert!(is_loopback_url("https://127.0.0.1:7789/"));
        // 非回环 / 伪造（userinfo、十进制 IP 变体、外网域名）一律拒绝。
        assert!(!is_loopback_url("http://evil.com:7789/"));
        assert!(!is_loopback_url("http://127.0.0.1.evil.com/"));
        assert!(!is_loopback_url("http://user@127.0.0.1:7789/"));
        assert!(!is_loopback_url("http://192.168.1.5:4000/"));
        assert!(!is_loopback_url("file:///C:/x"));
        assert!(!is_loopback_url(""));
    }

    #[test]
    fn origin_whitelist() {
        assert!(origin_allowed("tauri://localhost", 0));
        assert!(origin_allowed("https://tauri.localhost", 0));
        assert!(origin_allowed("http://tauri.localhost", 0));
        assert!(origin_allowed("http://127.0.0.1:65113", 65113));
        assert!(origin_allowed("http://localhost:65113", 65113));
        assert!(!origin_allowed("http://127.0.0.1:9999", 65113));
        assert!(!origin_allowed("http://evil.com", 65113));
        // No port resolved yet: only the tauri schemes pass.
        assert!(!origin_allowed("http://127.0.0.1:65113", 0));
    }

    #[test]
    fn host_header_whitelist() {
        assert!(host_allowed("127.0.0.1:65113", 65113));
        assert!(host_allowed("localhost:65113", 65113));
        assert!(host_allowed("LOCALHOST:65113", 65113));
        assert!(!host_allowed("evil.com", 65113));
        // rebind 变体:子域/端口篡改都不行
        assert!(!host_allowed("127.0.0.1.evil.com:65113", 65113));
        assert!(!host_allowed("127.0.0.1:1", 65113));
        assert!(!host_allowed("", 65113));
        // 端口未解析前一律拒绝
        assert!(!host_allowed("127.0.0.1:65113", 0));
    }

    #[test]
    fn constant_time_eq() {
        assert!(ct_eq("abc", "abc"));
        assert!(ct_eq("", ""));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "abcd"));
        assert!(!ct_eq("", "a"));
    }

    #[test]
    fn upload_name_sanitize() {
        // 路径穿越成分被剥掉
        assert_eq!(sanitize_filename("a/b/c.png"), "c.png");
        assert_eq!(sanitize_filename("..\\..\\x.txt"), "x.txt");
        assert_eq!(sanitize_filename("../../../etc/passwd"), "passwd");
        // 非法字符被替换
        assert_eq!(sanitize_filename("a:b*c?.txt"), "a_b_c_.txt");
        // 空与只有分隔符
        assert_eq!(sanitize_filename(""), "upload");
        assert_eq!(sanitize_filename("///"), "upload");
        // Windows 结尾的点被去掉
        assert_eq!(sanitize_filename("evil.exe."), "evil.exe");
        // 长度受控
        let long = "x".repeat(300);
        assert_eq!(sanitize_filename(&long).chars().count(), 120);
    }

    #[test]
    fn image_kind_by_extension() {
        assert!(is_image_name("shot.png"));
        assert!(is_image_name("photo.JPEG"));
        assert!(is_image_name("a/b/c.webp"));
        assert!(!is_image_name("notes.txt"));
        assert!(!is_image_name("noext"));
        assert!(!is_image_name("png")); // 无扩展名,只有名字
    }
}

#[cfg(test)]
mod regression {
    //! 回归测试：曾经让桌面端永久卡在「思考中」的两个缺陷。
    //!
    //! 1. SSE 广播缓冲溢出后静默丢事件（`Lagged => continue`）——
    //!    丢掉的可能是 `turn.end`，前端就再也复位不了。
    //! 2. `/api/sessions` 一次性返回全部会话 —— 上千个会话时侧栏渲染
    //!    要数秒，页面看起来就是卡死。

    use super::*;
    use crate::config::Config;
    use std::sync::Arc as StdArc;

    /// 起一个真实的服务器（随机端口），返回 (base, token)。
    /// Start a server rooted at a scratch data home, so the test never
    /// touches the user's real `~/.parable`.
    ///
    /// The home is passed per-server (`serve_in`) rather than through the
    /// process-wide `PARABLE_HOME`, so tests stay independent of each other.
    async fn spawn_server(tag: &str) -> (String, String) {
        let token = "regression-token".to_string();
        let cfg = Config::default();
        // tag 会被多个用例复用（如 "basic"），home 再拼 UUID 保证并行用例
        // 不共享同一临时根：否则一个用例开头的 remove_dir_all 会清掉另一个的 home。
        let home = std::env::temp_dir().join(format!(
            "parable-regression-{}-{}",
            tag,
            uuid::Uuid::new_v4().simple()
        ));
        let _ = std::fs::remove_dir_all(&home);
        let (port, _state, _handle) =
            serve_in(token.clone(), StdArc::new(RwLock::new(cfg)), 0, Some(home))
                .await
                .unwrap();
        (format!("http://127.0.0.1:{port}"), token)
    }

    #[tokio::test]
    async fn turn_running_reports_idle_sessions() {
        let (base, token) = spawn_server("basic").await;
        // A session that never ran: not running.
        // query token 已收敛：turn_running 只认 X-Parable-Token 头。
        let res = reqwest::Client::new()
            .get(format!("{base}/api/turn/running?session=s_none"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = res.json().await.unwrap();
        assert_eq!(v["running"], false, "an idle session is not running");
    }

    #[tokio::test]
    async fn turn_running_requires_a_session() {
        let (base, token) = spawn_server("basic").await;
        let res = reqwest::Client::new()
            .get(format!("{base}/api/turn/running"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = res.json().await.unwrap();
        assert_eq!(v["running"], false, "no session id means nothing is running");
    }

    #[tokio::test]
    async fn turn_running_rejects_a_bad_token() {
        let (base, _token) = spawn_server("basic").await;
        let res = reqwest::Client::new()
            .get(format!("{base}/api/turn/running?session=s_x"))
            .header("x-parable-token", "wrong")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401);
    }

    #[tokio::test]
    async fn sessions_list_is_paged() {
        let (base, token) = spawn_server("list").await;
        let res = reqwest::Client::new()
            .get(format!("{base}/api/sessions"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = res.json().await.unwrap();
        assert!(v.get("sessions").is_some(), "sessions array must be present");
        assert!(v.get("total").is_some(), "total must be reported");
        assert!(v.get("limited").is_some(), "limited flag must be reported");
    }

    #[tokio::test]
    async fn term_list_is_empty_for_a_fresh_server() {
        let (base, token) = spawn_server("term-empty").await;
        let cli = reqwest::Client::new();
        let res = cli.get(format!("{base}/api/term/list"))
            .header("x-parable-token", "wrong").send().await.unwrap();
        assert_eq!(res.status(), 401, "term list must require the token");
        let res = cli.get(format!("{base}/api/term/list"))
            .header("x-parable-token", &token).send().await.unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = res.json().await.unwrap();
        assert_eq!(v["jobs"].as_array().map(|a| a.len()), Some(0));
        assert_eq!(v["running"], 0);
    }

    #[tokio::test]
    async fn term_start_rejects_bad_input_and_escaping_cwd() {
        let (base, token) = spawn_server("term-bad").await;
        let cli = reqwest::Client::new();
        let res = cli.post(format!("{base}/api/term/start"))
            .header("x-parable-token", &token)
            .json(&json!({"command": "   "}))
            .send().await.unwrap();
        assert_eq!(res.status(), 400, "an empty command is rejected");
        // cwd 必须落在工作区内：`..` 要拒绝，否则后台终端能越界跑命令。
        let res = cli.post(format!("{base}/api/term/start"))
            .header("x-parable-token", &token)
            .json(&json!({"command": "echo hi", "cwd": ".."}))
            .send().await.unwrap();
        assert_eq!(res.status(), 400, "a cwd outside the workspace is rejected");
    }

    #[tokio::test]
    async fn term_start_runs_and_reports_exit() {
        let (base, token) = spawn_server("term-run").await;
        let cli = reqwest::Client::new();
        let res = cli.post(format!("{base}/api/term/start"))
            .header("x-parable-token", &token)
            .json(&json!({"command": "echo parable-term"}))
            .send().await.unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = res.json().await.unwrap();
        assert!(v["job"]["id"].as_str().unwrap_or("").starts_with("term_"));
        assert_eq!(v["job"]["status"], "running");

        // 轮询等待命令退出：固定 800ms 在负载高的机器上会偶发失败
        // （进程还没被回收就断言 status=exited）。
        let mut jobs: Vec<Value> = Vec::new();
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let v: Value = cli.get(format!("{base}/api/term/list"))
                .header("x-parable-token", &token)
                .send().await.unwrap().json().await.unwrap();
            jobs = v["jobs"].as_array().cloned().unwrap_or_default();
            if jobs.first().map(|j| j["status"] == "exited").unwrap_or(false) {
                break;
            }
        }
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0]["status"], "exited", "the job must leave running state");
        assert!(jobs[0]["output"].as_str().unwrap_or("").contains("parable-term"));

        let res = cli.post(format!("{base}/api/term/clear"))
            .header("x-parable-token", &token)
            .json(&json!({})).send().await.unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = cli.get(format!("{base}/api/term/list"))
            .header("x-parable-token", &token)
            .send().await.unwrap().json().await.unwrap();
        assert_eq!(v["jobs"].as_array().map(|a| a.len()), Some(0));
    }

    #[tokio::test]
    async fn sessions_limit_is_honoured() {
        let (base, token) = spawn_server("limit").await;
        let res = reqwest::Client::new()
            .get(format!("{base}/api/sessions?limit=1"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = res.json().await.unwrap();
        assert!(v["sessions"].as_array().unwrap().len() <= 1);
    }

    #[tokio::test]
    async fn sse_stream_sends_events_and_does_not_silently_drop() {
        // The stream must deliver events to a connected client, and the server
        // must never drop them without telling the client.
        let (base, token) = spawn_server("basic").await;
        let res = reqwest::get(format!("{base}/api/events?token={token}")).await.unwrap();
        assert_eq!(res.status(), 200);
        let mut stream = res.bytes_stream();
        // The handshake arrives immediately.
        use futures::StreamExt;
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("handshake must arrive")
            .expect("stream item")
            .expect("bytes");
        let text = String::from_utf8_lossy(&first);
        assert!(
            text.contains("hello"),
            "expected a hello handshake, got: {text}"
        );
    }

    #[tokio::test]
    async fn agent_defs_crud_end_to_end() {
        let (base, token) = spawn_server("agent-defs").await;
        let cli = reqwest::Client::new();
        // defs 端点与 skills 一样只认 X-Parable-Token header（不收 query token）。
        let url = |p: &str| format!("{base}/api{p}");
        let hdr = ("x-parable-token", token.as_str());

        // Create: validation is enforced server-side.
        // user 作用域落在真实 home（serve_in 的 home 只覆盖会话存储）——
        // 可能有同名残留，先清理保证可重跑。
        let _ = cli.delete(url("/agents/defs/user/文档整理师"))
            .header(hdr.0, hdr.1).send().await;
        let res = cli.post(url("/agents/defs"))
            .header(hdr.0, hdr.1)
            .json(&json!({"name": "文档整理师", "description": "", "system_prompt": "x"}))
            .send().await.unwrap();
        assert_eq!(res.status(), 400, "an empty description is rejected");

        let res = cli.post(url("/agents/defs"))
            .header(hdr.0, hdr.1)
            .json(&json!({
                "name": "文档整理师",
                "description": "归档文档并生成索引",
                "color": "cyan",
                "model": "zhipu/glm-5",
                "thinking": 2,
                "tools": ["read_file", "grep"],
                "system_prompt": "你是文档整理师。",
                "scope": "user"
            }))
            .send().await.unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = res.json().await.unwrap();
        assert_eq!(v["agent"]["id"], "文档整理师");
        assert_eq!(v["agent"]["enabled"], true);
        assert_eq!(v["agent"]["tools"][0], "read_file");

        // Duplicate create is refused; list contains the profile.
        let res = cli.post(url("/agents/defs"))
            .header(hdr.0, hdr.1)
            .json(&json!({"name": "文档整理师", "description": "d", "system_prompt": "s"}))
            .send().await.unwrap();
        assert_eq!(res.status(), 400);
        let v: Value = cli.get(url("/agents/defs"))
            .header(hdr.0, hdr.1).send().await.unwrap().json().await.unwrap();
        // user 作用域是真实 home，可能有用户自己的 profile——按 id 断言，不断言总数。
        let agents = v["agents"].as_array().unwrap();
        let mine: Vec<&Value> = agents
            .iter()
            .filter(|a| a["id"] == "文档整理师")
            .collect();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0]["name"], "文档整理师");

        // Disable via the sidecar; the profile stays listed but disabled.
        let res = cli.post(url("/agents/defs/user/文档整理师/enabled"))
            .header(hdr.0, hdr.1)
            .json(&json!({"enabled": false}))
            .send().await.unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = cli.get(url("/agents/defs"))
            .header(hdr.0, hdr.1).send().await.unwrap().json().await.unwrap();
        let mine: Vec<&Value> = v["agents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["id"] == "文档整理师")
            .collect();
        assert_eq!(mine[0]["enabled"], false);

        // Rename via rename_from.
        let _ = cli.delete(url("/agents/defs/user/档案管理员"))
            .header(hdr.0, hdr.1).send().await;
        let res = cli.post(url("/agents/defs"))
            .header(hdr.0, hdr.1)
            .json(&json!({
                "name": "档案管理员",
                "description": "归档文档并生成索引",
                "system_prompt": "你是文档整理师。",
                "scope": "user",
                "rename_from": "文档整理师"
            }))
            .send().await.unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = cli.get(url("/agents/defs"))
            .header(hdr.0, hdr.1).send().await.unwrap().json().await.unwrap();
        let agents = v["agents"].as_array().unwrap();
        assert!(
            !agents.iter().any(|a| a["id"] == "文档整理师"),
            "the old file must be gone after a rename"
        );
        assert!(agents.iter().any(|a| a["id"] == "档案管理员"));

        // Launching an unknown profile is rejected with a clear status.
        let res = cli.post(url("/agents/defs/run"))
            .header(hdr.0, hdr.1)
            .json(&json!({"session_id": "s_none", "name": "不存在", "task": "t"}))
            .send().await.unwrap();
        assert_eq!(res.status(), 404, "an unknown profile is not found");

        // Delete.
        let res = cli.delete(url("/agents/defs/user/档案管理员"))
            .header(hdr.0, hdr.1).send().await.unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = cli.get(url("/agents/defs"))
            .header(hdr.0, hdr.1).send().await.unwrap().json().await.unwrap();
        assert!(
            !v["agents"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["id"] == "档案管理员")
        );
    }

    #[tokio::test]
    async fn browser_cookies_endpoints() {
        // cookie jar 落在真实 home（Config::home_dir），测试先备份、结束还原，
        // 绝不毁用户数据。
        let jar = crate::cookies::jar_path();
        let backup = std::fs::read(&jar).ok();
        let restore = |backup: Option<Vec<u8>>| {
            match backup {
                Some(b) => {
                    let _ = std::fs::write(&jar, b);
                }
                None => {
                    let _ = std::fs::remove_file(&jar);
                }
            }
        };
        let (base, token) = spawn_server("cookies").await;
        let cli = reqwest::Client::new();
        let hdr = ("x-parable-token", token.as_str());
        let url = |p: &str| format!("{base}/api{p}");

        // sources：结构完整即可（本机装了什么浏览器因机器而异）。
        let res = cli
            .get(url("/browser/cookies/sources"))
            .header(hdr.0, hdr.1)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = res.json().await.unwrap();
        assert!(v["sources"].as_array().is_some());
        assert!(v.get("jarCount").is_some());

        // 空来源拒绝。
        let res = cli
            .post(url("/browser/cookies/import"))
            .header(hdr.0, hdr.1)
            .json(&json!({"sources": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);

        // 不存在的来源：逐来源报 error，整批 ok（不拖垮）。
        let res = cli
            .post(url("/browser/cookies/import"))
            .header(hdr.0, hdr.1)
            .json(&json!({"sources": ["firefox:definitely-not-a-profile"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = res.json().await.unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["extracted"], 0);
        assert!(
            v["sources"][0].get("error").is_some(),
            "a bogus source must report its error"
        );

        // clear。
        let res = cli
            .post(url("/browser/cookies/clear"))
            .header(hdr.0, hdr.1)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        restore(backup);
    }

    #[tokio::test]
    async fn environment_reports_real_fields_and_mcp_matches_mcp_endpoint() {
        let (base, token) = spawn_server("env").await;
        let cli = reqwest::Client::new();
        // 鉴权：坏 token 401（query token 已收敛，environment 只认 header）。
        let res = cli.get(format!("{base}/api/environment"))
            .header("x-parable-token", "wrong")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401);

        let res = cli.get(format!("{base}/api/environment"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let env: Value = res.json().await.unwrap();

        // 契约 §2.1 顶层字段存在性。
        for key in ["hostname", "os", "arch", "version", "pid", "workspace", "home"] {
            assert!(env.get(key).is_some(), "environment missing `{key}`");
        }
        assert!(env["hostname"].as_str().is_some(), "hostname must be a real string");
        assert!(env["pid"].as_u64().unwrap_or(0) > 0, "pid must be real");
        assert!(env["model"].get("provider").is_some());
        assert!(env["model"].get("model").is_some());
        assert!(env["context"].get("used").is_some());
        assert!(env["context"].get("limit").is_some());
        assert!(env["context"].get("pct").is_some());
        assert_eq!(env["context"]["estimated"], false, "fresh server has no real usage");
        assert!(env["sandbox"].get("mode").is_some());
        assert_eq!(env["sandbox"]["policy"], "v1");
        assert!(env["permissionMode"].as_str().is_some());
        // 「自动选模型」已移除：auto 对象只剩 fallbackChain（失败降级链）。
        assert!(env["auto"].get("enabled").is_none());
        assert!(env["auto"].get("fallbackChain").is_some());

        // mcp.* 必须与 GET /api/mcp 同源计数（/api/mcp 只认 header token）。
        let mcp: Value = cli
            .get(format!("{base}/api/mcp"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let servers = mcp["servers"].as_array().cloned().unwrap_or_default();
        let status = mcp["status"].as_array().cloned().unwrap_or_default();
        let connected = status.iter().filter(|s| s["connected"] == true).count() as u64;
        assert_eq!(env["mcp"]["total"].as_u64(), Some(servers.len() as u64));
        assert_eq!(env["mcp"]["connected"].as_u64(), Some(connected));
        // environment 逐服务器条目与配置清单一一对应。
        let out = env["mcp"]["servers"].as_array().unwrap();
        assert_eq!(out.len(), servers.len());
        for entry in out {
            assert!(entry.get("id").is_some());
            assert!(entry.get("transport").is_some());
            assert!(["connected", "failed", "connecting", "disabled"]
                .contains(&entry["status"].as_str().unwrap_or("")));
        }
    }

    #[tokio::test]
    async fn telemetry_unknown_session_is_200_without_500() {
        let (base, token) = spawn_server("telemetry").await;
        let cli = reqwest::Client::new();
        // 鉴权：坏 token 401（query token 已收敛，telemetry 只认 header）。
        let res = cli.get(format!("{base}/api/telemetry?session=s_none"))
            .header("x-parable-token", "wrong")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401);

        // 不存在的会话：仍 200，全零 + tps.value=null，不许 500。
        let res = cli.get(format!("{base}/api/telemetry?session=s_none"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = res.json().await.unwrap();
        assert_eq!(v["session"], "s_none");
        assert_eq!(v["turn"]["rounds"].as_u64(), Some(0));
        assert_eq!(v["turn"]["steps"].as_u64(), Some(0));
        assert_eq!(v["turn"]["running"], false);
        assert_eq!(v["tokens"]["prompt"].as_u64(), Some(0));
        assert_eq!(v["tokens"]["completion"].as_u64(), Some(0));
        assert_eq!(v["tokens"]["total"].as_u64(), Some(0));
        assert!(v["tps"]["value"].is_null(), "no samples → tps.value null");
        assert_eq!(v["tps"]["windowSec"].as_u64(), Some(10));
        assert_eq!(v["tps"]["samples"].as_u64(), Some(0));
        assert_eq!(v["cache"]["supported"], false);
        assert!(v["cache"]["hitRate"].is_null());
        assert_eq!(v["cache"]["read"].as_u64(), Some(0));
        assert_eq!(v["cache"]["write"].as_u64(), Some(0));
        assert!(v["context"].get("used").is_some());
        assert!(v["context"].get("limit").is_some());
        assert!(v["model"].get("provider").is_some());
        assert!(v["model"].get("model").is_some());

        // 不带 session 参数同样 200（空会话不 500）。
        let res = cli.get(format!("{base}/api/telemetry"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = res.json().await.unwrap();
        assert_eq!(v["tokens"]["total"].as_u64(), Some(0));
        assert!(v["tps"]["value"].is_null());
    }

    #[tokio::test]
    async fn patch_config_caps_fallback_chain_and_ignores_removed_auto_model() {
        let (base, token) = spawn_server("autocfg").await;
        let cli = reqwest::Client::new();
        // 7 条备用链：归一化去重 + 截断到 5；autoModel 键已随「自动选模型」
        // 功能移除——PATCH 侧当作未知键忽略，不报错也不落字段。
        let body = json!({
            "autoModel": true,
            "fallbackChain": ["p/a", "p/b", "p/a", "q/c", "q/d", "q/e", "q/f"]
        });
        let res = cli
            .patch(format!("{base}/api/config"))
            .header("x-parable-token", &token)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        let cfg: Value = cli
            .get(format!("{base}/api/config"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(cfg.get("auto_model").is_none(), "auto_model 字段应已移除");
        let chain = cfg["fallback_chain"].as_array().unwrap();
        assert_eq!(chain.len(), 5);
        assert_eq!(
            chain.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>(),
            vec!["p/a", "p/b", "q/c", "q/d", "q/e"]
        );
    }
}

#[cfg(test)]
mod schedule_api {
    //! 定时任务后端：运行历史、编辑能力、one-shot 结束语义。

    use super::*;
    use crate::config::Config;
    use std::sync::Arc as StdArc;

    /// 起一个真实的服务器（随机端口），返回 (base, token)。
    async fn spawn_server(tag: &str) -> (String, String) {
        let token = "schedule-token".to_string();
        let cfg = Config::default();
        let home = std::env::temp_dir().join(format!(
            "parable-schedule-{}-{}",
            tag,
            uuid::Uuid::new_v4().simple()
        ));
        let _ = std::fs::remove_dir_all(&home);
        let (port, _state, _handle) =
            serve_in(token.clone(), StdArc::new(RwLock::new(cfg)), 0, Some(home))
                .await
                .unwrap();
        (format!("http://127.0.0.1:{port}"), token)
    }

    /// 建一个 agent 动作的任务，返回 (id, 原始 json)。
    async fn create_agent_task(
        cli: &reqwest::Client,
        base: &str,
        token: &str,
        name: &str,
        timing: Value,
    ) -> (String, Value) {
        let body = json!({
            "name": name,
            "prompt": "hello",
            "action": {"kind": "agent"},
            "timing": timing,
            "target": "reuse",
        });
        let res = cli
            .post(format!("{base}/api/schedule"))
            .header("x-parable-token", token)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200, "create task must succeed");
        let v: Value = res.json().await.unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        (id, v)
    }

    /// 运行历史接口：结构正确。
    #[tokio::test]
    async fn schedule_runs_endpoint_returns_records() {
        let (base, token) = spawn_server("runs").await;
        let cli = reqwest::Client::new();

        // 空历史：返回空数组而不是 404。
        let res = cli
            .get(format!("{base}/api/schedule/runs"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = res.json().await.unwrap();
        assert!(v.get("runs").is_some(), "runs array must be present");
        assert_eq!(v["runs"].as_array().unwrap().len(), 0);

        // 不存在的 task 过滤 → 空数组。
        let v: Value = cli
            .get(format!("{base}/api/schedule/runs?task=sc_nope"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(v["runs"].as_array().unwrap().len(), 0);
    }

    /// 运行历史：写入后能按 task_id 过滤读回。
    #[tokio::test]
    async fn schedule_runs_are_filtered_by_task() {
        let (base, token) = spawn_server("runs-filter").await;
        let cli = reqwest::Client::new();
        let (id1, _) = create_agent_task(
            &cli,
            &base,
            &token,
            "one",
            json!({"kind": "every", "every_seconds": 3600}),
        )
        .await;
        let (id2, _) = create_agent_task(
            &cli,
            &base,
            &token,
            "two",
            json!({"kind": "every", "every_seconds": 3600}),
        )
        .await;

        // "立即运行"会落一条运行记录（无论投递成功与否）。
        let _ = cli
            .post(format!("{base}/api/schedule/{id1}/run"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap();
        let _ = cli
            .post(format!("{base}/api/schedule/{id2}/run"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap();

        let all: Value = cli
            .get(format!("{base}/api/schedule/runs"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let runs = all["runs"].as_array().unwrap();
        assert!(!runs.is_empty(), "manual runs must be recorded");

        // 每条记录字段齐全，且带 taskId。
        for r in runs {
            assert!(r["id"].is_string(), "run id missing");
            assert!(r["taskId"].is_string(), "taskId missing");
            assert!(r["taskName"].is_string(), "taskName missing");
            assert!(r["startedAt"].is_string(), "startedAt missing");
            assert!(r["ok"].is_boolean(), "ok must be a bool");
        }

        // 按 task 过滤：只返回该任务的记录。
        let filtered: Value = cli
            .get(format!("{base}/api/schedule/runs?task={id1}"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let fr = filtered["runs"].as_array().unwrap();
        assert!(!fr.is_empty(), "filtered runs must not be empty");
        assert!(
            fr.iter().all(|r| r["taskId"] == id1.as_str()),
            "filter must only return the requested task"
        );

        // 不存在的 task 返回空数组。
        let empty: Value = cli
            .get(format!("{base}/api/schedule/runs?task=sc_nope"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(empty["runs"].as_array().unwrap().len(), 0);
    }

    /// 运行历史接口必须校验 token。
    #[tokio::test]
    async fn schedule_runs_requires_token() {
        let (base, _token) = spawn_server("runs-auth").await;
        let res = reqwest::Client::new()
            .get(format!("{base}/api/schedule/runs"))
            .header("x-parable-token", "wrong")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401);
    }

    /// PATCH 改 name/model/status/timing 后，GET 能读到。
    #[tokio::test]
    async fn schedule_patch_edits_task() {
        let (base, token) = spawn_server("patch").await;
        let cli = reqwest::Client::new();
        let (id, _) = create_agent_task(
            &cli,
            &base,
            &token,
            "before",
            json!({"kind": "every", "every_seconds": 3600}),
        )
        .await;

        let body = json!({
            "name": "after",
            "model": "openai/gpt-4o",
            "status": "paused",
            "timing": {"kind": "every", "every_seconds": 600},
        });
        let res = cli
            .patch(format!("{base}/api/schedule/{id}"))
            .header("x-parable-token", &token)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let patched: Value = res.json().await.unwrap();
        assert_eq!(patched["name"], "after");
        assert_eq!(patched["modelSel"], "openai/gpt-4o");
        assert_eq!(patched["status"], "paused");
        assert_eq!(patched["timing"]["every_seconds"], 600);

        // GET 能读到同样的值。
        let got: Value = cli
            .get(format!("{base}/api/schedule/{id}"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(got["name"], "after");
        assert_eq!(got["modelSel"], "openai/gpt-4o");
        assert_eq!(got["status"], "paused");
        assert_eq!(got["timing"]["every_seconds"], 600);
        // timing 改了要重新提交下一次触发（不能停在旧时间点）。
        assert!(
            got["scheduledAt"].is_string(),
            "timing change must re-commit scheduledAt"
        );
    }

    /// PATCH 未传的字段保持不变（向后兼容）。
    #[tokio::test]
    async fn schedule_patch_keeps_untouched_fields() {
        let (base, token) = spawn_server("patch-partial").await;
        let cli = reqwest::Client::new();
        let (id, created) = create_agent_task(
            &cli,
            &base,
            &token,
            "keep",
            json!({"kind": "every", "every_seconds": 3600}),
        )
        .await;
        let before_prompt = created["prompt"].as_str().unwrap().to_string();

        let res = cli
            .patch(format!("{base}/api/schedule/{id}"))
            .header("x-parable-token", &token)
            .json(&json!({"name": "renamed"}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = res.json().await.unwrap();
        assert_eq!(v["name"], "renamed");
        assert_eq!(v["prompt"], before_prompt, "prompt must not change");
        assert_eq!(v["timing"], created["timing"], "timing must not change");
    }

    /// PATCH 改 target 与 command 也要生效。
    #[tokio::test]
    async fn schedule_patch_edits_target_and_command() {
        let (base, token) = spawn_server("patch-target").await;
        let cli = reqwest::Client::new();
        let (id, _) = create_agent_task(
            &cli,
            &base,
            &token,
            "tgt",
            json!({"kind": "every", "every_seconds": 3600}),
        )
        .await;

        let res = cli
            .patch(format!("{base}/api/schedule/{id}"))
            .header("x-parable-token", &token)
            .json(&json!({
                "target": {"kind": "new_each_time"},
                "command": "echo hi",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = res.json().await.unwrap();
        assert_eq!(v["target"]["kind"], "new_each_time");
        assert_eq!(v["action"]["kind"], "shell");
        assert_eq!(v["action"]["command"], "echo hi");
    }

    /// PATCH 的 action 支持标签形式：`{"kind":"shell","command":"…"}`。
    #[tokio::test]
    async fn schedule_patch_accepts_tagged_action() {
        let (base, token) = spawn_server("patch-action").await;
        let cli = reqwest::Client::new();
        let (id, _) = create_agent_task(
            &cli,
            &base,
            &token,
            "act",
            json!({"kind": "every", "every_seconds": 3600}),
        )
        .await;

        let res = cli
            .patch(format!("{base}/api/schedule/{id}"))
            .header("x-parable-token", &token)
            .json(&json!({"action": {"kind": "shell", "command": "echo tagged"}}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = res.json().await.unwrap();
        assert_eq!(v["action"]["kind"], "shell");
        assert_eq!(v["action"]["command"], "echo tagged");
    }

    /// 老接口的 pause / resume 动作词仍然有效（向后兼容）。
    #[tokio::test]
    async fn schedule_patch_pause_and_resume() {
        let (base, token) = spawn_server("patch-pause").await;
        let cli = reqwest::Client::new();
        let (id, _) = create_agent_task(
            &cli,
            &base,
            &token,
            "pausable",
            json!({"kind": "every", "every_seconds": 3600}),
        )
        .await;

        let res = cli
            .patch(format!("{base}/api/schedule/{id}"))
            .header("x-parable-token", &token)
            .json(&json!({"action": "pause"}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = res.json().await.unwrap();
        assert_eq!(v["status"], "paused");

        // 恢复后要重新提交下一次触发（不能停在暂停前的旧时间点）。
        let res = cli
            .patch(format!("{base}/api/schedule/{id}"))
            .header("x-parable-token", &token)
            .json(&json!({"action": "resume"}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = res.json().await.unwrap();
        assert_eq!(v["status"], "active");
        assert!(v["scheduledAt"].is_string(), "resume must re-commit scheduledAt");
    }

    /// PATCH 的 action=delete 仍然能删任务（老接口）。
    #[tokio::test]
    async fn schedule_patch_delete_still_works() {
        let (base, token) = spawn_server("patch-del").await;
        let cli = reqwest::Client::new();
        let (id, _) = create_agent_task(
            &cli,
            &base,
            &token,
            "deletable",
            json!({"kind": "every", "every_seconds": 3600}),
        )
        .await;
        let res = cli
            .patch(format!("{base}/api/schedule/{id}"))
            .header("x-parable-token", &token)
            .json(&json!({"action": "delete"}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let list: Value = cli
            .get(format!("{base}/api/schedule"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(list["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["id"] != id.as_str()));
    }

    /// PATCH 非法 status 要被拒。
    #[tokio::test]
    async fn schedule_patch_rejects_bad_status() {
        let (base, token) = spawn_server("patch-bad").await;
        let cli = reqwest::Client::new();
        let (id, _) = create_agent_task(
            &cli,
            &base,
            &token,
            "bad",
            json!({"kind": "every", "every_seconds": 3600}),
        )
        .await;
        let res = cli
            .patch(format!("{base}/api/schedule/{id}"))
            .header("x-parable-token", &token)
            .json(&json!({"status": "bogus"}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    /// PATCH 非法 timing 要被拒，且任务不被改坏。
    #[tokio::test]
    async fn schedule_patch_rejects_bad_timing() {
        let (base, token) = spawn_server("patch-bad-timing").await;
        let cli = reqwest::Client::new();
        let (id, _) = create_agent_task(
            &cli,
            &base,
            &token,
            "bad-timing",
            json!({"kind": "every", "every_seconds": 3600}),
        )
        .await;
        let res = cli
            .patch(format!("{base}/api/schedule/{id}"))
            .header("x-parable-token", &token)
            .json(&json!({"timing": {"kind": "every", "every_seconds": 5}}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
        // 原任务还在，时序没被改坏。
        let got: Value = cli
            .get(format!("{base}/api/schedule/{id}"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(got["timing"]["every_seconds"], 3600);
    }

    /// 列表接口要带 lastRunOk（没有历史时为 null）。
    #[tokio::test]
    async fn schedule_list_reports_last_run_ok() {
        let (base, token) = spawn_server("list-lr").await;
        let cli = reqwest::Client::new();
        let (id, _) = create_agent_task(
            &cli,
            &base,
            &token,
            "lr",
            json!({"kind": "every", "every_seconds": 3600}),
        )
        .await;

        let list: Value = cli
            .get(format!("{base}/api/schedule"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let tasks = list["tasks"].as_array().unwrap();
        let t = tasks.iter().find(|t| t["id"] == id.as_str()).unwrap();
        assert!(t.get("lastRunOk").is_some(), "lastRunOk must be present");
        assert!(t["lastRunOk"].is_null(), "no history yet => lastRunOk is null");

        // 跑一次之后 lastRunOk 应该变成布尔值。
        let _ = cli
            .post(format!("{base}/api/schedule/{id}/run"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap();
        let list: Value = cli
            .get(format!("{base}/api/schedule"))
            .header("x-parable-token", &token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let tasks = list["tasks"].as_array().unwrap();
        let t = tasks.iter().find(|t| t["id"] == id.as_str()).unwrap();
        assert!(
            t["lastRunOk"].is_boolean(),
            "after a run lastRunOk must be a bool"
        );
    }

    /// 非法 timing 的创建请求要给出可读的 400。
    #[tokio::test]
    async fn schedule_create_reports_readable_errors() {
        let (base, token) = spawn_server("create-err").await;
        let cli = reqwest::Client::new();

        let res = cli
            .post(format!("{base}/api/schedule"))
            .header("x-parable-token", &token)
            .json(&json!({
                "name": "bad",
                "prompt": "hi",
                "timing": {"kind": "every", "every_seconds": 5},
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
        let v: Value = res.json().await.unwrap();
        let msg = v["error"].as_str().unwrap();
        assert!(msg.contains("every_seconds"), "got: {msg}");

        let res = cli
            .post(format!("{base}/api/schedule"))
            .header("x-parable-token", &token)
            .json(&json!({
                "name": "bad",
                "prompt": "hi",
                "timing": {"kind": "cron", "expression": "not a cron", "timeZone": "UTC"},
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
        let v: Value = res.json().await.unwrap();
        let msg = v["error"].as_str().unwrap();
        assert!(msg.contains("cron"), "got: {msg}");
    }

    /// one-shot（after）任务触发一次后，status 必须变 ended。
    ///
    /// 后台调度每 20s 轮询一次，测试里等一会儿即可观察到 ended。
    #[tokio::test]
    async fn one_shot_task_ends_after_firing() {
        let (base, token) = spawn_server("one-shot").await;
        let cli = reqwest::Client::new();
        // after_seconds = 1：很快就会到期。
        let (id, _) = create_agent_task(
            &cli,
            &base,
            &token,
            "one-shot",
            json!({"kind": "after", "after_seconds": 1}),
        )
        .await;

        // 等后台调度（20s 一轮）把它触发掉。
        let mut ended = false;
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
            let got: Value = cli
                .get(format!("{base}/api/schedule/{id}"))
                .header("x-parable-token", &token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if got["status"] == "ended" {
                ended = true;
                break;
            }
        }
        assert!(ended, "a one-shot task must end after it fires");
    }
}
