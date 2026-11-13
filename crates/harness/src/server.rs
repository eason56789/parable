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
        .route("/models", get(get_models))
        .route("/sessions", get(list_sessions))
        .route("/session/new", post(new_session))
        .route("/sessions/{id}", patch(patch_session).delete(delete_session))
        .route("/sessions/{id}/messages", get(session_messages).delete(truncate_messages))
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
        .route("/tools", get(tools))
        .route("/schedule", get(list_tasks).post(create_task))
        .route("/schedule/{id}", get(get_task).patch(edit_task).delete(delete_task))
        .route("/schedule/{id}/run", post(run_task_now))
        .route("/agents", get(list_agents))
        .route("/agents/{id}/interrupt", post(interrupt_agent))
        .route("/plugins", get(list_plugins))
        .route("/plugins/{id}/toggle", post(toggle_plugin))
        // 断线重连后恢复权限面板;聊天附件上传
        .route("/pending", get(pending_permissions))
        .route("/attachments", post(upload_attachment))
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
    Html(html).into_response()
}

fn load_index() -> std::io::Result<String> {
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
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "web/index.html not found next to the executable or in ./web",
    ))
}

// ------------------------------------------------------------------ sse

async fn sse_events(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Response {
    if !check_auth(&headers, q.token.as_deref(), &state) {
        return unauthorized();
    }
    let mut rx = state.hub.subscribe();
    let sid = String::new();
    let state2 = state.clone();
    let stream = async_stream::stream! {
        // Handshake so the frontend knows the pipe is live.
        let hello = json!({"type":"hello","sessionId": sid, "seq": 0, "ts": crate::event::now_ms()});
        if let Ok(ev) = Event::default().json_data(&hello) {
            yield Ok::<_, Infallible>(ev);
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
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
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

    state
        .broker
        .set_session(&sid)
        .await;

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

    match state
        .agent
        .start_turn(&sid, &req.text, req.attachments, opts)
        .await
    {
        Ok(turn_id) => Json(json!({"sessionId": sid, "turnId": turn_id})).into_response(),
        Err(e) => (StatusCode::CONFLICT, Json(json!({"error": e.to_string()}))).into_response(),
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
                if let Some(mc) = p.models.iter_mut().find(|m| m.id == mid) {
                    if let Some(n) = m.get("name").and_then(|x| x.as_str()) {
                        mc.name = n.to_string();
                    }
                    if let Some(c) = m.get("context").and_then(|x| x.as_u64()) {
                        mc.context = c;
                    }
                    if let Some(t) = m.get("vision").and_then(|x| x.as_bool()) {
                        mc.vision = t;
                    }
                    if let Some(t) = m.get("thinking").and_then(|x| x.as_bool()) {
                        mc.thinking = t;
                    }
                    if let Some(e) = m.get("enabled").and_then(|x| x.as_bool()) {
                        mc.enabled = e;
                    }
                } else if let Ok(mc) = serde_json::from_value::<crate::config::ModelConfig>(m.clone()) {
                    p.models.push(mc);
                }
            }
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
        let m = p.models.iter().find(|m| m.enabled).map(|m| m.id.clone()).unwrap_or_default();
        let t = p.models.iter().find(|m| m.enabled).map(|m| m.thinking).unwrap_or(false);
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
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let query = q.get("q").cloned().unwrap_or_default();
    let list = state.store.list(&query).await;
    Json(json!({"sessions": list})).into_response()
}

#[derive(Debug, Deserialize)]
struct NewSessionReq {
    #[serde(default)]
    workspace: String,
    #[serde(default)]
    title: String,
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
    }));
    Json(json!({"session": s.meta})).into_response()
}

#[derive(Debug, Deserialize)]
struct EditMessageReq {
    text: String,
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
    match state.store.edit_message_text(&id, idx, r.to, &r.text).await {
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

async fn list_agents(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    let kids = state.subagents.list().await;
    Json(json!({"agents": kids})).into_response()
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

async fn health(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !check_auth(&headers, None, &state) {
        return unauthorized();
    }
    Json(json!({"ok": true, "uptimeMs": crate::event::now_ms().saturating_sub(state.started_at)}))
        .into_response()
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
    Json(json!({"tools": specs})).into_response()
}

// ------------------------------------------------------------------ bootstrap

pub async fn serve(
    token: String,
    config: Arc<RwLock<Config>>,
    port: u16,
) -> anyhow::Result<(u16, SharedState, tokio::task::JoinHandle<()>)> {
    let hub = EventHub::new();
    let broker = Arc::new(PermissionBroker::new(hub.clone()));
    let questions = Arc::new(QuestionBroker::new());
    let store: SharedStore = Arc::new(SessionStore::new());
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

    // AgentRuntime 先建基础体;subagents 的 with_runtime 需要 Arc<AgentRuntime>,
    // 而 runtime 挂 subagents 又需要 subagents——回环只能用克隆句柄解:
    // AgentRuntime 是 Clone 且内部全是 Arc 共享字段。句柄上挂 schedule(子代理
    // turn 可用定时任务);subagents 无法回环注入,子代理里再 spawn_agent 会
    // 得到明确的"subagents are not mounted"错误,而不是静默失效。
    let agent_base = AgentRuntime::new(
        config.clone(),
        hub.clone(),
        broker.clone(),
        questions.clone(),
        store.clone(),
    );
    let child_runtime: Arc<AgentRuntime> = Arc::new(
        agent_base
            .clone()
            .with_services(Some(schedule.clone()), None),
    );
    let subagents: crate::subagent::SharedSubagents = Arc::new(
        crate::subagent::SubagentService::new(hub.clone()).with_runtime(child_runtime),
    );

    let agent = agent_base.with_services(Some(schedule.clone()), Some(subagents.clone()));

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
