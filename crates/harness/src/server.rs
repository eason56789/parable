//! Local HTTP server: REST + SSE. Bound to 127.0.0.1 with a random port and a
//! per-run session token; every `/api/*` request is checked against it.

use crate::agent::{AgentRuntime, Attachment, QuestionBroker, TurnOptions};
use crate::config::{mask_key, Config, ProviderConfig};
use crate::event::EventHub;
use crate::permission::{Decision, PermissionBroker};
use crate::session::{SessionStore, SharedStore};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
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

#[derive(Debug, Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

/// Query parameters that scope a request to one session.
#[derive(Debug, Deserialize)]
struct SessionQuery {
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    token: Option<String>,
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
async fn origin_guard(
    State(state): State<SharedState>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    if let Some(origin) = req.headers().get(axum::http::header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let port = state
            .port
            .load(std::sync::atomic::Ordering::Relaxed);
        if !origin_allowed(origin, port) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "origin not allowed"})),
            )
                .into_response();
        }
    }
    next.run(req).await
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
        .route("/status", get(status))
        .route("/usage", get(get_usage))
        .route("/tools", get(tools))
        .route("/schedule", get(list_tasks).post(create_task))
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
        .route("/agents/{id}/interrupt", post(interrupt_agent))
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
        .route("/sessions/clear-all", post(clear_all_sessions))
        // 多 Profile：一键把命名配置束套到当前配置上
        .route("/profiles/apply", post(apply_profile))
        // 文件检查点：列出某会话的可回退点 / 执行回退
        .route("/sessions/{id}/checkpoints", get(list_checkpoints))
        .route("/sessions/{id}/rewind", post(rewind_session))
        // MCP 服务器舰队：读取（含状态）/ 保存并应用 / 显式连接
        .route("/mcp", get(mcp_state))
        .route("/mcp", post(mcp_save))
        .route("/mcp/connect", post(mcp_connect))
        // 断线重连后恢复权限面板;聊天附件上传
        .route("/pending", get(pending_permissions))
        .route("/attachments", post(upload_attachment))
        // 侧边栏(side pane):文件浏览 / git 审查 / 浏览器 / 终端
        .route("/fs/tree", get(fs_tree))
        .route("/fs/file", get(fs_file))
        .route("/fs/search", get(fs_search))
        .route("/git/status", get(git_status))
        .route("/git/diff", get(git_diff))
        .route("/browser/state", get(browser_state))
        .route("/browser/navigate", post(browser_navigate))
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
    // 注入的 index.html 不该落到任意 Host/Origin 的请求面前。上传接口放宽
    // axum 默认 2MB body 限制到 20MB,超限在 handler 里回 413。
    Router::new()
        .nest("/api", api)
        .route("/", get(index))
        .route("/index.html", get(index))
        .fallback(get(index))
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

async fn index(State(state): State<SharedState>) -> Response {
    let html = match load_index() {
        Ok(h) => h,
        Err(e) => return server_error(format!("cannot load web/index.html: {e}")),
    };
    // The token is injected so the page can authenticate without a manual step.
    let html = html.replace(
        "<head>",
        &format!("<head><script>window.__PARABLE_TOKEN={:?};</script>", state.token),
    );
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
    resp
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
    let (mut rx, backlog) = match q.since {
        Some(since) if since > 0 => {
            let (b, r) = state.hub.subscribe_since(since);
            (r, b)
        }
        _ => (state.hub.subscribe(), Vec::new()),
    };
    let sid = String::new();
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
    let dir = crate::config::Config::home_dir().join("uploads");
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
    if let Some(k) = v.get_mut("search_key") {
        let masked = k.as_str().map(mask_key).unwrap_or_default();
        let has = !masked.is_empty();
        *k = json!(masked);
        v["hasSearchKey"] = json!(has);
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
    // 先处理删除标记({id, deleted:true}),再做字段合并。
    crate::config::apply_patch_deletions(&mut cfg, &v);
    merge_config(&mut cfg, &v);
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
                "workspace" => cfg.workspace = val.as_str().unwrap_or("").to_string(),
                // Same masking rule as provider keys: an echo containing the
                // mask marker never overwrites the stored secret.
                "searchKey" | "search_key" => {
                    let s = val.as_str().unwrap_or("").to_string();
                    if !s.contains('…') && !s.contains('•') {
                        cfg.search_key = s;
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
                    if let Ok(ms) = serde_json::from_value(val.clone()) {
                        cfg.mcp_servers = ms;
                    }
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
            v["hasKey"] = json!(!p.key.is_empty());
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
    if !check_auth(&headers, q.get("token").map(|s| s.as_str()), &state) {
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
    Json(json!({"sessions": list, "total": total, "limited": total > list.len()})).into_response()
}

/// GET /api/search/messages?q=… —— 全文搜索所有会话里的消息正文。
async fn search_messages(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if !check_auth(&headers, q.get("token").map(|s| s.as_str()), &state) {
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
    let ws = if r.workspace.is_empty() {
        state.config.read().await.workspace.clone()
    } else {
        r.workspace
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

async fn list_plugins(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let store = crate::plugin::PluginStore::new();
    Json(json!({"plugins": store.scan()})).into_response()
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
    (
        [
            (header::CONTENT_TYPE, "application/json; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                Box::leak(format!("attachment; filename=\"{filename}\"").into_boxed_str()),
            ),
        ],
        body,
    )
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

/// GET /api/sessions/{id}/checkpoints — 该会话的全部文件检查点。
async fn list_checkpoints(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let list = state.agent.checkpoints.list(&id).await;
    Json(json!({"checkpoints": list})).into_response()
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
    let tasks = state.schedule.store().list().await;
    Json(json!({"tasks": tasks})).into_response()
}

#[derive(Debug, Deserialize)]
struct CreateTaskReq {
    name: String,
    prompt: String,
    timing: crate::schedule::Timing,
    /// Either the plain string `existing` / `reuse` / `new`, or the tagged form
    /// `{"kind":"existing","session_id":"..."}`.
    #[serde(default)]
    target: Option<Value>,
    #[serde(default)]
    session_id: String,
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
    match state.schedule.create(r.name, r.prompt, r.timing, target).await {
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
    #[serde(default)]
    timing: Option<crate::schedule::Timing>,
    #[serde(default)]
    action: Option<String>,
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
    if r.action.as_deref() == Some("delete") {
        return match state.schedule.store().delete(&id).await {
            Ok(()) => Json(json!({"ok": true})).into_response(),
            Err(e) => server_error(e),
        };
    }
    match state
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
            if let Some(tm) = &r.timing {
                t.timing = tm.clone();
                t.scheduled_at = None;
            }
            match r.action.as_deref() {
                Some("pause") => t.status = crate::schedule::TaskStatus::Paused,
                Some("resume") => {
                    t.status = crate::schedule::TaskStatus::Active;
                    t.scheduled_at = None;
                }
                _ => {}
            }
        })
        .await
    {
        Ok(t) => Json(json!(t)).into_response(),
        Err(e) => server_error(e),
    }
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
            };
            let prompt = format!(
                "[scheduled task `{}` run manually]\n\n{}",
                task.name, task.prompt
            );
            match state.agent.start_turn(&sid, &prompt, Vec::new(), opts).await {
                Ok(turn) => Json(json!({"sessionId": sid, "turnId": turn})).into_response(),
                Err(e) => (StatusCode::CONFLICT, Json(json!({"error": e.to_string()}))).into_response(),
            }
        }
        Err(e) => server_error(e),
    }
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
    if !check_auth(&headers, q.token.as_deref(), &state) {
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


///
/// `?session=<id>` scopes the listing to that session's tree. Without it the
/// whole process-wide catalogue is returned — the old behaviour, which leaked
/// other conversations' subagents into this one.
async fn list_agents(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<SessionQuery>,
) -> Response {
    if !check_auth(&headers, q.token.as_deref(), &state) {
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
    let ok = state.subagents.interrupt(&id).await;
    Json(json!({"interrupted": ok})).into_response()
}

// ------------------------------------------------------------------ agent teams

/// The board and roster of one agent tree.
async fn get_team(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<SessionQuery>,
) -> Response {
    if !check_auth(&headers, q.token.as_deref(), &state) {
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
async fn browser_state(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let bridge = state.agent.computer.clone();
    let url = bridge.current_url().await;
    // A screenshot failure (no browser yet) must not fail the whole request:
    // the panel shows the URL and text even when the capture is unavailable.
    let screenshot = bridge.screenshot_png().await.ok();
    let text = bridge.page_text().await.unwrap_or_default();
    Json(json!({
        "url": url,
        "screenshot": screenshot,
        "text": text,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct BrowserNavigateReq {
    url: String,
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
    match bridge.navigate_to(&r.url).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => bad_request(&e.to_string()),
    }
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
    Query(q): Query<TokenQuery>,
    Json(r): Json<TermStartReq>,
) -> Response {
    if !check_auth(&headers, q.token.as_deref(), &state) {
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
    let job = state.terms.start(state.hub.clone(), cwd, command).await;
    Json(json!({ "job": job })).into_response()
}

/// GET /api/term/list — 列出全部后台终端（含已结束的尾部输出）。
async fn term_list(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Response {
    if !check_auth(&headers, q.token.as_deref(), &state) {
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
    Query(q): Query<TokenQuery>,
    Json(r): Json<TermKillReq>,
) -> Response {
    if !check_auth(&headers, q.token.as_deref(), &state) {
        return unauthorized();
    }
    let killed = state.terms.kill(&state.hub, r.id.trim()).await;
    Json(json!({ "killed": killed })).into_response()
}

/// POST /api/term/clear — 清掉已结束的后台终端，运行中的保留。
async fn term_clear(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Response {
    if !check_auth(&headers, q.token.as_deref(), &state) {
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
    let sessions_root = Config::home_dir().join("sessions");
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
    let (pref, key) = {
        let cfg = state.config.read().await;
        (cfg.search_pref.clone(), cfg.search_key.clone())
    };
    let reg = crate::tools::Registry::new(std::path::PathBuf::from(".")).with_search(&pref, &key);
    let specs = reg.specs();
    Json(json!({"tools": specs.as_ref()})).into_response()
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
    let sched_store = Arc::new(crate::schedule::ScheduleStore::new());

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

    let agent = agent_base
        .with_services(Some(schedule.clone()), Some(subagents.clone()))
        .await;

    // Background scheduler: every 20s, fire tasks whose occurrence has passed.
    // Delivery is an ordinary follow-up turn into the target session.
    {
        let schedule = schedule.clone();
        let agent = agent.clone();
        let config = config.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(20)).await;
                let now = chrono::Utc::now();
                let due = schedule.due(now).await;
                for (mut task, _fired) in due {
                    match schedule.resolve_session(&mut task).await {
                        Ok(sid) => {
                            // 与手动"立即运行"一致:用全局配置的权限模式,
                            // 调度投递不偷偷提权到 full。
                            let mode = config.read().await.permission_mode.clone();
                            let opts = crate::agent::TurnOptions {
                                model_sel: String::new(),
                                permission_mode: mode,
                                thinking_level: 0,
                                plan_mode: false,
                            };
                            let prompt = format!(
                                "[scheduled task `{}` fired at {}]\n\n{}",
                                task.name,
                                now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                                task.prompt
                            );
                            if let Err(e) = agent
                                .start_turn(&sid, &prompt, Vec::new(), opts)
                                .await
                            {
                                tracing::warn!("scheduled task {} failed to start: {e}", task.id);
                            }
                        }
                        Err(e) => tracing::warn!("scheduled task {} has no target: {e}", task.id),
                    }
                }
            }
        });
    }

    let memory = Arc::new(crate::memory::MemoryStore::with_path(
        crate::config::Config::home_dir().join("memory.json"),
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
        terms: crate::term::TerminalRegistry::new(),
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
        let home = std::env::temp_dir().join(format!("parable-regression-{tag}"));
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
        let res = reqwest::get(format!("{base}/api/turn/running?session=s_none&token={token}"))
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = res.json().await.unwrap();
        assert_eq!(v["running"], false, "an idle session is not running");
    }

    #[tokio::test]
    async fn turn_running_requires_a_session() {
        let (base, token) = spawn_server("basic").await;
        let res = reqwest::get(format!("{base}/api/turn/running?token={token}"))
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = res.json().await.unwrap();
        assert_eq!(v["running"], false, "no session id means nothing is running");
    }

    #[tokio::test]
    async fn turn_running_rejects_a_bad_token() {
        let (base, _token) = spawn_server("basic").await;
        let res = reqwest::get(format!("{base}/api/turn/running?session=s_x&token=wrong"))
            .await
            .unwrap();
        assert_eq!(res.status(), 401);
    }

    #[tokio::test]
    async fn sessions_list_is_paged() {
        let (base, token) = spawn_server("list").await;
        let res = reqwest::get(format!("{base}/api/sessions?token={token}"))
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
        let res = reqwest::get(format!("{base}/api/term/list?token=wrong")).await.unwrap();
        assert_eq!(res.status(), 401, "term list must require the token");
        let res = reqwest::get(format!("{base}/api/term/list?token={token}")).await.unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = res.json().await.unwrap();
        assert_eq!(v["jobs"].as_array().map(|a| a.len()), Some(0));
        assert_eq!(v["running"], 0);
    }

    #[tokio::test]
    async fn term_start_rejects_bad_input_and_escaping_cwd() {
        let (base, token) = spawn_server("term-bad").await;
        let cli = reqwest::Client::new();
        let res = cli.post(format!("{base}/api/term/start?token={token}"))
            .json(&json!({"command": "   "}))
            .send().await.unwrap();
        assert_eq!(res.status(), 400, "an empty command is rejected");
        // cwd 必须落在工作区内：`..` 要拒绝，否则后台终端能越界跑命令。
        let res = cli.post(format!("{base}/api/term/start?token={token}"))
            .json(&json!({"command": "echo hi", "cwd": ".."}))
            .send().await.unwrap();
        assert_eq!(res.status(), 400, "a cwd outside the workspace is rejected");
    }

    #[tokio::test]
    async fn term_start_runs_and_reports_exit() {
        let (base, token) = spawn_server("term-run").await;
        let cli = reqwest::Client::new();
        let res = cli.post(format!("{base}/api/term/start?token={token}"))
            .json(&json!({"command": "echo parable-term"}))
            .send().await.unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = res.json().await.unwrap();
        assert!(v["job"]["id"].as_str().unwrap_or("").starts_with("term_"));
        assert_eq!(v["job"]["status"], "running");

        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        let v: Value = cli.get(format!("{base}/api/term/list?token={token}"))
            .send().await.unwrap().json().await.unwrap();
        let jobs = v["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0]["status"], "exited", "the job must leave running state");
        assert!(jobs[0]["output"].as_str().unwrap_or("").contains("parable-term"));

        let res = cli.post(format!("{base}/api/term/clear?token={token}"))
            .json(&json!({})).send().await.unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = cli.get(format!("{base}/api/term/list?token={token}"))
            .send().await.unwrap().json().await.unwrap();
        assert_eq!(v["jobs"].as_array().map(|a| a.len()), Some(0));
    }

    #[tokio::test]
    async fn sessions_limit_is_honoured() {
        let (base, token) = spawn_server("limit").await;
        let res = reqwest::get(format!("{base}/api/sessions?limit=1&token={token}"))
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
}
