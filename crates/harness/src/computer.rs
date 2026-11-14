//! Computer use — drive a Chromium browser over the DevTools protocol.
//!
//! 视觉 + 无障碍树双观察路径（参考 ZCode CUA / Anthropic computer use /
//! Playwright ariaSnapshot 的共同形态）：
//! - **视觉**：`Page.captureScreenshot` → base64 PNG，模型以坐标操作
//!   （`Input.dispatchMouseEvent` / `insertText` / key events）。
//! - **无障碍**：`Accessibility.getFullAXTree` → 带 `[rN]` 稳定引用 + 边界框
//!   的紧凑树，模型优先按 ref 操作（语义准确、不依赖像素）。
//!
//! 关键设计：**ref 只在动作发生时才实时解析**。快照把 ref → backendDOMNodeId
//! 存进映射；click/type 等动作拿着 backendNodeId 现场取盒模型/滚动进视口，
//! 因此 DOM 变化（弹层、重排）后 ref 依然语义有效——失效时给出明确报错，
//! 让模型重新快照，而不是点到错误的元素上。
//!
//! 浏览器 lazily 拉起（Edge/Chrome，独立 user-data-dir，强制 DPR=1 使
//! 截图像素 == CSS 坐标），进程与 WS 会话在本桥内复用；支持多标签页。

use crate::error::HarnessError;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

/// One captured screenshot waiting to be folded into the model context.
#[derive(Debug, Clone)]
pub struct PendingImage {
    pub media_type: String,
    pub data: String,
}

struct Cdp {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

impl Cdp {
    async fn connect(url: &str) -> Result<Self, HarnessError> {
        let (ws, _) = connect_async(url)
            .await
            .map_err(|e| HarnessError::Tool(format!("CDP 连接失败: {e}")))?;
        Ok(Self { ws, next_id: 1 })
    }

    /// Send one command and await its matching response (events are skipped).
    async fn cmd(&mut self, method: &str, params: Value) -> Result<Value, HarnessError> {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({"id": id, "method": method, "params": params});
        self.ws
            .send(Message::Text(req.to_string().into()))
            .await
            .map_err(|e| HarnessError::Tool(format!("CDP 发送失败: {e}")))?;
        loop {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(10), self.ws.next())
                .await
                .map_err(|_| HarnessError::Tool("CDP 响应超时".into()))?
                .ok_or_else(|| HarnessError::Tool("CDP 连接已关闭".into()))?
                .map_err(|e| HarnessError::Tool(format!("CDP 读取失败: {e}")))?;
            let data = match msg {
                Message::Text(t) => t.to_string(),
                Message::Ping(_) | Message::Pong(_) => continue,
                Message::Close(_) => return Err(HarnessError::Tool("CDP 连接被关闭".into())),
                other => {
                    let _ = other;
                    continue;
                }
            };
            let v: Value = serde_json::from_str(&data)
                .map_err(|e| HarnessError::Tool(format!("CDP 帧解析失败: {e}")))?;
            if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
                if let Some(err) = v.get("error") {
                    return Err(HarnessError::Tool(format!(
                        "CDP {method} 失败: {}",
                        err.get("message").and_then(|m| m.as_str()).unwrap_or("?")
                    )));
                }
                return Ok(v.get("result").cloned().unwrap_or(Value::Null));
            }
            // 其他 id 的响应或事件：跳过
        }
    }
}

/// One accessibility node the model can act on.
#[derive(Debug, Clone)]
struct AxNode {
    role: String,
    name: String,
    /// 文本框/组合框的当前值（模型的宝贵上下文）。
    value: String,
    backend: Option<i64>,
    /// 视口 CSS 像素下的 (x, y, w, h)；与截图像素一一对应（DPR=1）。
    box_: Option<[f64; 4]>,
    focused: bool,
}

impl AxNode {
}

/// 视口 CSS 像素下的矩形。
#[derive(Debug, Clone, Copy)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// Per-process bridge: owns the launched browser and the live CDP session.
pub struct ComputerBridge {
    port: u16,
    child: Mutex<Option<tokio::process::Child>>,
    cdp: Mutex<Option<Cdp>>,
    /// 当前绑定的页面 target（切标签页后重连要回到同一个）。
    target_id: Mutex<Option<String>>,
    /// 最近一次快照的 ref → 节点映射。ref 只对最近一次快照负责。
    ax_nodes: Mutex<HashMap<i64, AxNode>>,
    /// app 窗口标题只设置一次。
    titled_once: std::sync::OnceLock<bool>,
    /// 无头运行开关（默认开 = 桌面零窗口，侧栏实况截图即视图）。
    headless: std::sync::RwLock<bool>,
    /// app 模式引导页 URL（--app= 需要真实 URL，about:blank 会被 Edge 无视
    /// 而回退普通窗口）。由 serve_in 注入 harness 自身的 /browser-boot 页。
    boot_url: std::sync::RwLock<String>,
    pending: Mutex<Option<PendingImage>>,
    /// cookie jar 是否已注入当前浏览器（每次启动注入一次；导入时置位）。
    jar_injected: std::sync::atomic::AtomicBool,
}

impl ComputerBridge {
    /// 注入 app 模式引导页（serve_in 拿到端口后调用）。
    pub fn set_boot_url(&self, url: &str) {
        if let Ok(mut g) = self.boot_url.write() {
            *g = url.to_string();
        }
    }

    /// 无头开关（默认 true）。false 时回到可见 app 窗口（围观 agent 操作）。
    /// 开关切换后需重建浏览器才生效：杀掉现有实例，下次调用按新模式拉起。
    pub fn set_headless(&self, on: bool) {
        let changed = self.headless.read().map(|g| *g != on).unwrap_or(false);
        if changed {
            if let Ok(mut g) = self.headless.write() {
                *g = on;
            }
            self.kill_browser();
        }
    }

    fn is_headless(&self) -> bool {
        self.headless.read().map(|g| *g).unwrap_or(true)
    }

    pub fn new() -> Self {
        // 调试端口随机取一个空闲端口,不再固定 9222:固定端口等于给本机其他
        // 进程一个可抢占/伪装的稳定入口。PARABLE_CDP_PORT 仍可显式指定。
        let port = std::env::var("PARABLE_CDP_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(pick_free_port);
        Self {
            port,
            child: Mutex::new(None),
            cdp: Mutex::new(None),
            target_id: Mutex::new(None),
            ax_nodes: Mutex::new(HashMap::new()),
            titled_once: std::sync::OnceLock::new(),
            headless: std::sync::RwLock::new(true),
            boot_url: std::sync::RwLock::new(String::new()),
            pending: Mutex::new(None),
            jar_injected: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Take the screenshot captured by the latest computer tool call.
    pub async fn take_image(&self) -> Option<PendingImage> {
        self.pending.lock().await.take()
    }

    // ---------------------------------------------------------------
    // Side-pane surface
    //
    // The tool functions below drive the browser for the *model*. The side
    // pane needs to observe the same browser from the *UI*, so these
    // wrappers expose read-only views plus navigation.
    // ---------------------------------------------------------------

    /// Current page URL, or an empty string when no page is attached.
    pub async fn current_url(&self) -> String {
        self.page_meta()
            .await
            .map(|m| m.0)
            .unwrap_or_default()
    }

    /// 历史后退/前进（delta = -1 | 1）：Page.getNavigationHistory 拿条目 id
    /// 再 navigateToHistoryEntry——比组合键可靠（无头下按键也无焦点问题）。
    pub async fn history_go(&self, delta: i32) -> Result<(), HarnessError> {
        let r = self.cmd("Page.getNavigationHistory", json!({})).await?;
        let idx = r.get("currentIndex").and_then(|v| v.as_i64()).unwrap_or(0);
        let entries = r
            .get("entries")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let target = idx + delta as i64;
        if target < 0 || target as usize >= entries.len() {
            return Err(HarnessError::Tool(
                if delta < 0 { "没有更早的历史了" } else { "没有更晚的历史了" }.into(),
            ));
        }
        let entry_id = entries[target as usize]
            .get("id")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1);
        self.cmd("Page.navigateToHistoryEntry", json!({"entryId": entry_id}))
            .await?;
        Ok(())
    }

    /// 刷新当前页。
    pub async fn reload(&self) -> Result<(), HarnessError> {
        self.cmd("Page.reload", json!({"ignoreCache": false})).await?;
        Ok(())
    }

    /// 侧栏点击穿透：把面板截图上的坐标换算成的视口物理坐标真实点击。
    pub async fn click_at(&self, x: f64, y: f64) -> Result<(), HarnessError> {
        self.click_xy(x, y, "left", 1).await
    }

    /// Screenshot of the current page, base64-encoded PNG.
    pub async fn screenshot_png(&self) -> Result<String, HarnessError> {
        self.screenshot_raw().await
    }

    /// Compact accessibility tree of the current page.
    pub async fn ax_tree_text(&self) -> Result<String, HarnessError> {
        self.ax_tree().await.map(|(text, _)| text)
    }

    /// Visible text of the current page.
    pub async fn page_text(&self) -> Result<String, HarnessError> {
        let r = self
            .cmd(
                "Runtime.evaluate",
                json!({
                    "expression": "document.body ? document.body.innerText : ''",
                    "returnByValue": true,
                }),
            )
            .await?;
        Ok(r.get("result")
            .and_then(|v| v.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string())
    }

    /// Navigate the controlled browser to `url`.
    ///
    /// Only `http://` and `https://` are allowed — `file://` would let a
    /// page read local files, and `javascript:` would run code in the
    /// browser's origin. The server never fetches the URL itself, so there
    /// is no SSRF surface beyond what the user typed.
    pub async fn navigate_to(&self, url: &str) -> Result<(), HarnessError> {
        let url = normalize_url(url)?;
        self.cmd("Page.navigate", json!({ "url": url })).await?;
        Ok(())
    }

    /// Non-consuming read for the display path.
    async fn peek_image(&self) -> Option<PendingImage> {
        self.pending.lock().await.clone()
    }

    async fn store_image(&self, media_type: &str, data: &str) {
        *self.pending.lock().await = Some(PendingImage {
            media_type: media_type.to_string(),
            data: data.to_string(),
        });
    }

    // ---------------------------------------------------------------
    // Page metadata & readiness
    // ---------------------------------------------------------------

    /// (url, title) of the current page. Empty strings when detached.
    async fn page_meta(&self) -> Option<(String, String)> {
        let r = self
            .cmd(
                "Runtime.evaluate",
                json!({
                    "expression": "JSON.stringify({u: location.href, t: document.title, s: document.readyState})",
                    "returnByValue": true,
                }),
            )
            .await
            .ok()?;
        let v = r.pointer("/result/value")?.as_str()?;
        let v: Value = serde_json::from_str(v).ok()?;
        Some((
            v.get("u").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            v.get("t").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        ))
    }

    /// 轮询 document.readyState 到 complete（或 load 语义够了）为止。
    /// SPA 不会真正 complete，超时静默返回——后面的截图仍然反映当前状态。
    async fn wait_ready(&self, max_ms: u64) {
        let step = std::time::Duration::from_millis(150);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(max_ms);
        while tokio::time::Instant::now() < deadline {
            if let Ok(r) = self
                .cmd(
                    "Runtime.evaluate",
                    json!({
                        "expression": "document.readyState",
                        "returnByValue": true,
                    }),
                )
                .await
            {
                if r.pointer("/result/value").and_then(|v| v.as_str()) == Some("complete") {
                    return;
                }
            }
            tokio::time::sleep(step).await;
        }
    }

    // ---------------------------------------------------------------
    // Browser lifecycle & CDP plumbing
    // ---------------------------------------------------------------

    /// 杀掉当前受控浏览器实例（模式切换后重建用）。
    async fn kill_browser(&self) {
        let mut cdp = self.cdp.lock().await;
        if let Some(c) = cdp.as_mut() {
            let _ = c.ws.close(None).await;
        }
        *cdp = None;
        *self.target_id.lock().await = None;
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.kill().await;
        }
        // 浏览器重启后 cookie 全空，下次 ensure 重新注入 jar。
        self.jar_injected
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// 把持久 cookie jar（~/.parable/browser-cookies.json）注入当前浏览器。
    /// 每次浏览器启动后调用一次；失败只记日志，不阻塞工具调用。
    ///
    /// 注意：直接拿 cdp 锁发命令，不走 `cmd()`——那会形成
    /// cmd → ensure → inject_jar_once → cmd 的 async 递归环（编译期 E0733）。
    async fn inject_jar_once(&self) {
        if self
            .jar_injected
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let jar = crate::cookies::CookieJar::load();
        if jar.cookies.is_empty() {
            return;
        }
        let mut ok = 0usize;
        {
            let mut g = self.cdp.lock().await;
            let Some(c) = g.as_mut() else {
                return;
            };
            for ck in &jar.cookies {
                if let Ok(v) = c.cmd("Network.setCookie", ck.to_set_cookie_params()).await {
                    if v.get("success").and_then(|s| s.as_bool()).unwrap_or(false) {
                        ok += 1;
                    }
                }
            }
        }
        tracing::info!("cookie jar 注入内置浏览器: {ok}/{}", jar.cookies.len());
    }

    /// 把一组 cookie 注入内置浏览器（登录态导入的落点）。
    /// 返回（成功, 失败）计数。
    pub async fn import_cookies(
        &self,
        entries: &[crate::cookies::CookieEntry],
    ) -> Result<(usize, usize), HarnessError> {
        // 先置位：ensure 的 inject-once 让位给这次显式注入，避免同一份灌两遍。
        self.jar_injected
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.ensure().await?;
        let mut ok = 0usize;
        let mut fail = 0usize;
        for c in entries {
            match self.cmd("Network.setCookie", c.to_set_cookie_params()).await {
                Ok(v) if v.get("success").and_then(|s| s.as_bool()).unwrap_or(false) => ok += 1,
                _ => fail += 1,
            }
        }
        Ok((ok, fail))
    }

    /// 清空内置浏览器的全部 cookie（「清除已导入」按钮的落点）。
    /// 浏览器没起时不拉起：没有活着的会话可清（调用方已清空 jar，
    /// 下次启动自然什么都不会注入）。
    pub async fn clear_cookies(&self) -> Result<bool, HarnessError> {
        {
            let cdp = self.cdp.lock().await;
            if cdp.is_none() {
                return Ok(false);
            }
        }
        self.cmd("Network.clearBrowserCookies", json!({})).await?;
        Ok(true)
    }

    /// Ensure a CDP session to a page target. Launches a browser if none is
    /// listening on the debug port.
    async fn ensure(&self) -> Result<(), HarnessError> {
        {
            let cdp = self.cdp.lock().await;
            if cdp.is_some() {
                return Ok(());
            }
        }
        let client = reqwest::Client::new();
        // 只认 Chromium 系(/json/version 的 Browser 字段):防止本机其他进程
        // 恰好占用该端口后被当成受控浏览器接管 CDP。
        if !self.chromium_listening(&client).await {
            self.launch_browser().await?;
        }
        let r = self.connect_target(&client, None).await;
        if r.is_ok() {
            // 浏览器起来后回放持久登录态（每次启动一次）。
            self.inject_jar_once().await;
        }
        r
    }

    /// 连接到一个页面 target 的 WS。`prefer` 为 Some 时优先绑回该 target
    /// （切过标签页后断线重连的场景）；不存在则回落第一个页面。
    async fn connect_target(
        &self,
        client: &reqwest::Client,
        prefer: Option<&str>,
    ) -> Result<(), HarnessError> {
        let list: Value = client
            .get(format!("http://127.0.0.1:{}/json", self.port))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| HarnessError::Tool(format!("CDP 发现失败: {e}")))?
            .json()
            .await
            .map_err(|e| HarnessError::Tool(format!("CDP 发现响应异常: {e}")))?;
        let pages: Vec<&Value> = list
            .as_array()
            .map(|a| a.iter().filter(|t| t["type"] == "page").collect())
            .unwrap_or_default();
        let chosen = pages
            .iter()
            .copied()
            .find(|t| {
                prefer
                    .map(|p| t["id"].as_str() == Some(p))
                    .unwrap_or(false)
            })
            .or_else(|| pages.first().copied())
            .ok_or_else(|| HarnessError::Tool("没有可用的浏览器页面目标".into()))?;
        let ws_url = chosen["webSocketDebuggerUrl"]
            .as_str()
            .ok_or_else(|| HarnessError::Tool("页面目标缺少调试 WS 地址".into()))?
            .to_string();
        let tid = chosen["id"].as_str().unwrap_or("").to_string();
        let mut cdp = Cdp::connect(&ws_url).await?;
        // 首次连接：占标题（可见模式的窗口一眼可辨）；无头时改写 UA，
        // 防站点按 HeadlessChrome 拦截（bot 检测）。
        if self.titled_once.set(true).is_ok() {
            let _ = cdp
                .cmd(
                    "Runtime.evaluate",
                    json!({
                        "expression": "document.title = 'Parable Browser'",
                        "returnByValue": true,
                    }),
                )
                .await;
            if self.is_headless() {
                if let Ok(r) = cdp
                    .cmd(
                        "Runtime.evaluate",
                        json!({"expression": "navigator.userAgent", "returnByValue": true}),
                    )
                    .await
                {
                    let ua = r
                        .pointer("/result/value")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if ua.contains("Headless") {
                        let fixed = ua.replace("HeadlessChrome", "Chrome");
                        let _ = cdp
                            .cmd("Network.setUserAgentOverride", json!({"userAgent": fixed}))
                            .await;
                    }
                }
            }
        }
        *self.cdp.lock().await = Some(cdp);
        *self.target_id.lock().await = if tid.is_empty() { None } else { Some(tid) };
        Ok(())
    }

    /// 端口上是否有一个我们认识的 Chromium 浏览器在监听:GET /json/version,
    /// 校验 `Browser` 字段包含 Chrome/Chromium/Edge/Headless。
    async fn chromium_listening(&self, client: &reqwest::Client) -> bool {
        let Ok(resp) = client
            .get(format!("http://127.0.0.1:{}/json/version", self.port))
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
        else {
            return false;
        };
        let Ok(v) = resp.json::<Value>().await else {
            return false;
        };
        browser_is_chromium(&v)
    }

    async fn launch_browser(&self) -> Result<(), HarnessError> {
        let candidates = [
            std::env::var("ProgramFiles(x86)").unwrap_or_default()
                + "\\Microsoft\\Edge\\Application\\msedge.exe",
            std::env::var("ProgramFiles").unwrap_or_default()
                + "\\Microsoft\\Edge\\Application\\msedge.exe",
            std::env::var("ProgramFiles").unwrap_or_default()
                + "\\Google\\Chrome\\Application\\chrome.exe",
            std::env::var("LocalAppData").unwrap_or_default()
                + "\\Google\\Chrome\\Application\\chrome.exe",
        ];
        let exe = candidates
            .iter()
            .find(|p| std::path::Path::new(p).is_file())
            .ok_or_else(|| HarnessError::Tool(
                "没有找到 Edge 或 Chrome，无法启动 computer use 浏览器".into(),
            ))?;
        // profile 目录按端口唯一：多个桥实例（或上次进程残留）不再互抢
        // SingletonLock——共用目录时第二个实例会静默失联（端口永远起不来）。
        let dir = std::env::temp_dir().join(format!("parable-cua-{}", self.port));
        let _ = std::fs::create_dir_all(&dir);
        let mut cmd = tokio::process::Command::new(exe);
        cmd.arg(format!("--remote-debugging-port={}", self.port))
            .arg(format!("--user-data-dir={}", dir.display()));
        if self.is_headless() {
            // 无头（new headless = 完整 Chromium）：桌面零窗口，侧栏实况
            // 截图即视图。--hide-scrollbars 让截图更干净。
            cmd.arg(format!(
                "--app={}",
                self.boot_url.read().map(|g| g.clone()).unwrap_or_default()
            ))
            .arg("--headless=new")
            .arg("--hide-scrollbars");
        } else {
            // 可见模式：app 窗口（无地址栏/书签/标签栏），可围观 agent 操作
            cmd.arg(format!(
                "--app={}",
                self.boot_url.read().map(|g| g.clone()).unwrap_or_default()
            ));
        }
        let child = cmd
            .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--force-device-scale-factor=1",
            "--restore-last-session=false",
            "--window-size=1440,900",
        ])
        .spawn()
        .map_err(|e| HarnessError::Tool(format!("启动浏览器失败: {e}")))?;
        *self.child.lock().await = Some(child);
        // 轮询调试端口就绪:必须等到 /json/version 报出一个 Chromium 系 Browser
        // 才算数,避免被同端口的其他进程糊弄过去。
        let client = reqwest::Client::new();
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            if let Ok(resp) = client
                .get(format!("http://127.0.0.1:{}/json/version", self.port))
                .timeout(std::time::Duration::from_millis(800))
                .send()
                .await
            {
                if let Ok(v) = resp.json::<Value>().await {
                    if browser_is_chromium(&v) {
                        return Ok(());
                    }
                }
            }
        }
        Err(HarnessError::Tool("浏览器调试端口未就绪".into()))
    }

    async fn cmd(&self, method: &str, params: Value) -> Result<Value, HarnessError> {
        self.ensure().await?;
        let first = {
            let mut cdp = self.cdp.lock().await;
            match cdp.as_mut() {
                Some(c) => c.cmd(method, params.clone()).await,
                None => Err(HarnessError::Tool("CDP 未连接".into())),
            }
        };
        match first {
            Ok(v) => return Ok(v),
            Err(e) => {
                // 一次失败不等于命令非法：浏览器闲置后会掐掉 WS，僵死连接会
                // 一直等到超时才报错。丢弃会话重连（优先回到原标签页），重试
                // 一次；若真是命令本身的问题，重试会给出同样的错误。
                let _ = e;
            }
        }
        *self.cdp.lock().await = None;
        self.ensure().await?;
        let mut cdp = self.cdp.lock().await;
        match cdp.as_mut() {
            Some(c) => c.cmd(method, params).await,
            None => Err(HarnessError::Tool("CDP 未连接".into())),
        }
    }

    // ---------------------------------------------------------------
    // Observation: screenshot + accessibility tree
    // ---------------------------------------------------------------

    /// 视觉：截图并以 base64 PNG 返回。`full_page` 时连滚动区一起截；
    /// 给 `clip` 时按 2× 放大截取该区域（zoom：密集小目标看清细节）。
    async fn screenshot_raw(&self) -> Result<String, HarnessError> {
        self.screenshot_opts(false, None).await
    }

    async fn screenshot_opts(&self, full_page: bool, clip: Option<[f64; 4]>) -> Result<String, HarnessError> {
        let mut params = json!({"format": "png"});
        if let Some([x, y, w, h]) = clip {
            params["clip"] = json!({"x": x, "y": y, "width": w, "height": h, "scale": 2.0});
        } else if full_page {
            params["captureBeyondViewport"] = json!(true);
        }
        let r = self.cmd("Page.captureScreenshot", params).await?;
        let data = r
            .get("data")
            .and_then(|d| d.as_str())
            .ok_or_else(|| HarnessError::Tool("截图响应缺少 data".into()))?
            .to_string();
        self.store_image("image/png", &data).await;
        Ok(data)
    }

    /// 无障碍：拉取全树，分配 ref，抓边界框，序列化为紧凑文本。
    /// 返回（文本, 抓到的 ref 数）。
    async fn ax_tree(&self) -> Result<(String, usize), HarnessError> {
        let r = self
            .cmd("Accessibility.getFullAXTree", json!({}))
            .await?;
        let nodes = r
            .get("nodes")
            .and_then(|n| n.as_array())
            .ok_or_else(|| HarnessError::Tool("AX 树响应缺少 nodes".into()))?;

        // ---- 解析：id → (role, name, value, backend, focused)，id → childIds
        struct Raw {
            role: String,
            name: String,
            value: String,
            backend: Option<i64>,
            focused: bool,
            kids: Vec<i64>,
        }
        let mut raw: HashMap<i64, Raw> = HashMap::new();
        let mut referenced: std::collections::HashSet<i64> = std::collections::HashSet::new();
        for n in nodes {
            let id = n
                .get("nodeId")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<i64>().ok())
                .or_else(|| n.get("nodeId").and_then(|v| v.as_i64()));
            let Some(id) = id else { continue };
            // childIds 必须先于 ignored 剪枝读走：ignored 节点的子树要保留，
            // 否则整段 UI 会从树里凭空消失（旧实现的真实 bug）。
            let kids: Vec<i64> = n
                .get("childIds")
                .and_then(|c| c.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|k| {
                            k.as_str().and_then(|s| s.parse::<i64>().ok())
                        })
                        .collect()
                })
                .unwrap_or_default();
            for k in &kids {
                referenced.insert(*k);
            }
            let role = n
                .pointer("/role/value")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = n
                .pointer("/name/value")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let value = n
                .pointer("/value/value")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let backend = n.get("backendDOMNodeId").and_then(|v| v.as_i64());
            let focused = n
                .pointer("/focused/value")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            raw.insert(
                id,
                Raw { role, name, value, backend, focused, kids },
            );
        }
        let roots: Vec<i64> = raw
            .keys()
            .copied()
            .filter(|id| !referenced.contains(id))
            .collect();

        // ---- 分配 ref：只给「模型可操作」的节点，DFS 顺序 = 阅读顺序
        const INTERACTIVE: [&str; 19] = [
            "button", "link", "textbox", "searchbox", "combobox", "listbox", "option",
            "menuitem", "menuitemcheckbox", "menuitemradio", "checkbox", "radio", "slider",
            "tab", "switch", "treeitem", "menu", "menubar", "tablist",
        ];
        const NOISE: [&str; 6] = ["generic", "none", "InlineTextBox", "StaticText", "group", "WebArea"];
        let mut ref_of: HashMap<i64, i64> = HashMap::new();
        let mut order: Vec<i64> = Vec::new(); // ref 顺序里的节点 id
        let mut next_ref = 1i64;
        {
            let mut stack: Vec<(i64, usize)> = roots.iter().rev().map(|id| (*id, 0)).collect();
            while let Some((id, depth)) = stack.pop() {
                if order.len() >= 350 || depth > 14 {
                    continue;
                }
                let Some(node) = raw.get(&id) else { continue };
                let interesting = INTERACTIVE.contains(&node.role.as_str())
                    || (!node.name.is_empty() && !NOISE.contains(&node.role.as_str()));
                if interesting {
                    ref_of.insert(id, next_ref);
                    order.push(id);
                    next_ref += 1;
                }
                for kid in node.kids.iter().rev() {
                    stack.push((*kid, depth + if interesting { 1 } else { 0 }));
                }
            }
        }

        // ---- 抓边界框：对每个 ref 实时取盒模型（也顺便校验 backend 存活）
        let mut map: HashMap<i64, AxNode> = HashMap::new();
        let mut boxed = 0usize;
        for id in &order {
            let node = raw.get(id).expect("order id from raw");
            let rect = match node.backend {
                Some(b) => self.backend_rect(b).await.ok().flatten(),
                None => None,
            };
            if rect.is_some() {
                boxed += 1;
            }
            map.insert(
                ref_of[id],
                AxNode {
                    role: node.role.clone(),
                    name: node.name.clone(),
                    value: node.value.clone(),
                    backend: node.backend,
                    box_: rect.map(|r| [r.x, r.y, r.w, r.h]),
                    focused: node.focused,
                },
            );
        }
        *self.ax_nodes.lock().await = map.clone();

        // ---- 序列化：树形缩进 + [rN] + box，非交互文本节点保留原文（截断）
        let (url, title) = self
            .page_meta()
            .await
            .unwrap_or_else(|| (String::new(), String::new()));
        let mut out = String::new();
        if !title.is_empty() {
            out.push_str(&format!("page: {title}\n"));
        }
        if !url.is_empty() {
            out.push_str(&format!("url: {url}\n"));
        }
        out.push_str("无障碍树（[rN] 引用 + 视口坐标；操作优先用 ref，坐标与截图对齐）:\n");
        let mut printed = 0usize;
        let mut stack: Vec<(i64, usize)> = roots.iter().rev().map(|id| (*id, 0)).collect();
        while let Some((id, depth)) = stack.pop() {
            if printed >= 350 || depth > 14 {
                continue;
            }
            let Some(node) = raw.get(&id) else { continue };
            let Some(rno) = ref_of.get(&id) else {
                // 非交互节点：有名字的（标题/段落）保留给模型当路标
                if !node.name.is_empty()
                    && !NOISE.contains(&node.role.as_str())
                    && matches!(node.role.as_str(), "heading" | "text" | "para" | "paragraph" | "article" | "region" | "navigation" | "banner" | "contentinfo" | "main" | "form" | "table" | "cell" | "columnheader" | "row" | "listitem" | "term" | "definition" | "status" | "alert" | "dialog" | "figure" | "caption")
                {
                    let nm: String = node.name.chars().take(90).collect();
                    out.push_str(&format!(
                        "{}- {} \"{}\"\n",
                        "  ".repeat(depth.min(10)),
                        node.role,
                        nm
                    ));
                    printed += 1;
                }
                for kid in node.kids.iter().rev() {
                    stack.push((*kid, depth + 1));
                }
                continue;
            };
            let a = &map[rno];
            let nm: String = if a.name.is_empty() {
                format!("({})", a.role)
            } else {
                a.name.chars().take(90).collect()
            };
            let pos = match a.box_ {
                Some([x, y, w, h]) => format!(" @({x:.0},{y:.0} {w:.0}×{h:.0})"),
                None => " @(不可见)".to_string(),
            };
            let focus = if a.focused { " [焦点]" } else { "" };
            let val = if a.value.is_empty() {
                String::new()
            } else {
                let v: String = a.value.chars().take(60).collect();
                format!(" value={v:?}")
            };
            out.push_str(&format!(
                "{}- [r{rno}] {} \"{nm}\"{pos}{focus}{val}\n",
                "  ".repeat(depth.min(10)),
                a.role
            ));
            printed += 1;
            if let Some(kids) = raw.get(&id).map(|n| n.kids.clone()) {
                for kid in kids.iter().rev() {
                    stack.push((*kid, depth + 1));
                }
            }
        }
        if printed == 0 {
            out.push_str("(空——页面可能还在加载，稍等后重试)\n");
        }
        Ok((out, boxed))
    }

    // ---------------------------------------------------------------
    // Ref resolution & actions
    // ---------------------------------------------------------------

    /// 动作期实时解析 ref：先查快照映射拿 backendNodeId，再现场取盒模型。
    /// 旧 ref（页面已变）在这里明确报错，绝不猜坐标。
    async fn resolve_ref(&self, r: i64) -> Result<Rect, HarnessError> {
        let backend = self
            .ax_nodes
            .lock()
            .await
            .get(&r)
            .map(|n| (n.role.clone(), n.name.clone(), n.backend))
            .ok_or_else(|| {
                HarnessError::Tool(format!(
                    "ref r{r} 不在最近一次快照里——先调 computer_snapshot 再操作"
                ))
            })?;
        let Some(backend) = backend.2 else {
            return Err(HarnessError::Tool(format!(
                "ref r{r}（{}:\"{}\"）没有对应 DOM 节点，无法定位",
                backend.0, backend.1
            )));
        };
        self.backend_rect(backend).await?.ok_or_else(|| {
            HarnessError::Tool(format!(
                "ref r{r} 已失效（元素不在文档里或不可见）——页面可能已变化，请重新 computer_snapshot"
            ))
        })
    }

    /// backendNodeId → 当前视口矩形。先滚进视口再取盒模型，保证中心点
    /// 落在可见区域内。元素不存在/零尺寸时返回 None（不报错，由调用方定性）。
    async fn backend_rect(&self, backend: i64) -> Result<Option<Rect>, HarnessError> {
        let _ = self
            .cmd(
                "DOM.scrollIntoViewIfNeeded",
                json!({"backendNodeId": backend}),
            )
            .await;
        let r = match self
            .cmd("DOM.getBoxModel", json!({"backendNodeId": backend}))
            .await
        {
            Ok(r) => r,
            Err(_) => return Ok(None), // 节点已不在文档里
        };
        let quad = r
            .pointer("/model/content")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();
        let nums: Vec<f64> = quad.iter().filter_map(|v| v.as_f64()).collect();
        if nums.len() < 8 {
            return Ok(None);
        }
        let xs = [nums[0], nums[2], nums[4], nums[6]];
        let ys = [nums[1], nums[3], nums[5], nums[7]];
        let (minx, maxx) = (xs.iter().cloned().fold(f64::MAX, f64::min), xs.iter().cloned().fold(f64::MIN, f64::max));
        let (miny, maxy) = (ys.iter().cloned().fold(f64::MAX, f64::min), ys.iter().cloned().fold(f64::MIN, f64::max));
        let (w, h) = (maxx - minx, maxy - miny);
        if w <= 0.0 || h <= 0.0 {
            return Ok(None);
        }
        Ok(Some(Rect { x: minx, y: miny, w, h }))
    }

    /// ref 或坐标 → 视口坐标。ref 优先。
    async fn target_point(&self, a: &Value) -> Result<(f64, f64, String), HarnessError> {
        if let Some(r) = a.get("ref").and_then(|v| v.as_i64()) {
            let rect = self.resolve_ref(r).await?;
            Ok((rect.x + rect.w / 2.0, rect.y + rect.h / 2.0, format!("r{r}")))
        } else {
            let x = a
                .get("x")
                .and_then(|v| v.as_f64())
                .ok_or_else(|| HarnessError::Tool("`x`/`y` 或 `ref` 必填".into()))?;
            let y = a
                .get("y")
                .and_then(|v| v.as_f64())
                .ok_or_else(|| HarnessError::Tool("`x`/`y` 或 `ref` 必填".into()))?;
            Ok((x, y, format!("({x:.0},{y:.0})")))
        }
    }

    async fn click_xy(&self, x: f64, y: f64, button: &str, clicks: i64) -> Result<(), HarnessError> {
        let presses = clicks.clamp(1, 3);
        // 先把鼠标移过去：hover 菜单、焦点样式都需要真实的移动事件
        self.cmd(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseMoved", "x": x, "y": y, "button": "none"}),
        )
        .await?;
        for i in 1..=presses {
            self.cmd(
                "Input.dispatchMouseEvent",
                json!({"type": "mousePressed", "x": x, "y": y, "button": button,
                       "clickCount": i, "buttons": mouse_button_mask(button)}),
            )
            .await?;
            self.cmd(
                "Input.dispatchMouseEvent",
                json!({"type": "mouseReleased", "x": x, "y": y, "button": button,
                       "clickCount": i, "buttons": mouse_button_mask(button)}),
            )
            .await?;
        }
        Ok(())
    }

    /// 按 ref 选择 <select> 的选项：resolveNode + callFunctionOn 设值并派发
    /// input/change 事件（原生下拉的 option 不接受合成点击）。
    async fn select_option(&self, r: i64, wanted: &str) -> Result<String, HarnessError> {
        let backend = self
            .ax_nodes
            .lock()
            .await
            .get(&r)
            .and_then(|n| n.backend)
            .ok_or_else(|| {
                HarnessError::Tool(format!("ref r{r} 不在最近一次快照里——先 computer_snapshot"))
            })?;
        let node = self
            .cmd("DOM.resolveNode", json!({"backendNodeId": backend}))
            .await?;
        let object_id = node
            .pointer("/object/objectId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| HarnessError::Tool(format!("ref r{r} 无法解析为 DOM 对象（元素可能已消失）")))?;
        let js = r#"
function (wanted) {
  if (!(this instanceof HTMLSelectElement)) return 'err:not-a-select';
  const opts = [...this.options];
  const hit = opts.find(o => o.text.trim() === wanted || o.value === wanted)
    || opts.find(o => o.text.trim().includes(wanted));
  if (!hit) return 'err:no-option:' + opts.map(o => o.text.trim()).slice(0, 20).join(' | ');
  this.value = hit.value;
  this.dispatchEvent(new Event('input', {bubbles: true}));
  this.dispatchEvent(new Event('change', {bubbles: true}));
  return 'ok:' + hit.text.trim();
}
"#;
        let r2 = self
            .cmd(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration": js,
                    "arguments": [{"value": wanted}],
                    "returnByValue": true,
                }),
            )
            .await?;
        let result = r2
            .pointer("/result/value")
            .and_then(|v| v.as_str())
            .unwrap_or("err:no-result")
            .to_string();
        Ok(result)
    }

    /// 列出标签页；(target_id, index, title, url)。
    async fn list_tabs(&self) -> Result<Vec<(String, usize, String, String, bool)>, HarnessError> {
        self.ensure().await?;
        let client = reqwest::Client::new();
        let list: Value = client
            .get(format!("http://127.0.0.1:{}/json", self.port))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| HarnessError::Tool(format!("CDP 发现失败: {e}")))?
            .json()
            .await
            .map_err(|e| HarnessError::Tool(format!("CDP 发现响应异常: {e}")))?;
        let cur = self.target_id.lock().await.clone();
        let mut out = Vec::new();
        let mut i = 0usize;
        for t in list.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            if t["type"] != "page" {
                continue;
            }
            let id = t["id"].as_str().unwrap_or("").to_string();
            let is_cur = cur.as_deref() == Some(id.as_str());
            out.push((
                id,
                i,
                t["title"].as_str().unwrap_or("").to_string(),
                t["url"].as_str().unwrap_or("").to_string(),
                is_cur,
            ));
            i += 1;
        }
        Ok(out)
    }

    /// 切换到指定 target：断开当前 WS，连到目标页面的 WS。
    async fn switch_tab(&self, target_id: &str) -> Result<(), HarnessError> {
        {
            let mut cdp = self.cdp.lock().await;
            if let Some(c) = cdp.as_mut() {
                let _ = c.ws.close(None).await;
            }
            *cdp = None;
        }
        let client = reqwest::Client::new();
        self.connect_target(&client, Some(target_id)).await
    }

    /// 打开新标签页并切过去。
    async fn new_tab(&self, url: &str) -> Result<String, HarnessError> {
        self.ensure().await?;
        let client = reqwest::Client::new();
        let url = normalize_url(url)?;
        let resp = client
            .put(format!("http://127.0.0.1:{}/json/new?{}", self.port, url))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| HarnessError::Tool(format!("新建标签页失败: {e}")))?;
        let v: Value = resp
            .json()
            .await
            .map_err(|e| HarnessError::Tool(format!("新建标签页响应异常: {e}")))?;
        let id = v["id"].as_str().unwrap_or("").to_string();
        if id.is_empty() {
            return Err(HarnessError::Tool("新建标签页未返回 target id".into()));
        }
        self.switch_tab(&id).await?;
        Ok(id)
    }

    /// 关闭标签页；如果关的是当前页，自动切到剩下的第一个。
    async fn close_tab(&self, target_id: &str) -> Result<String, HarnessError> {
        self.ensure().await?;
        let client = reqwest::Client::new();
        let _ = client
            .get(format!("http://127.0.0.1:{}/json/close/{}", self.port, target_id))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;
        let was_cur = self.target_id.lock().await.as_deref() == Some(target_id);
        if was_cur {
            let mut cdp = self.cdp.lock().await;
            if let Some(c) = cdp.as_mut() {
                let _ = c.ws.close(None).await;
            }
            *cdp = None;
            *self.target_id.lock().await = None;
            drop(cdp);
            self.connect_target(&client, None).await?;
            return Ok("closed(active), switched to another tab".into());
        }
        Ok("closed".into())
    }
}

fn mouse_button_mask(button: &str) -> i64 {
    match button {
        "middle" => 4,
        "right" => 2,
        _ => 1,
    }
}

/// bind 127.0.0.1:0 拿一个空闲端口后立即释放,供浏览器稍后自己绑定
/// (存在极小的 TOCTOU 窗口,可接受;失败返回 0,后续启动会明确报错)。
fn pick_free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(0)
}

/// 校验 /json/version 的 `Browser` 字段,只认 Chromium 系(Chrome/Chromium/
/// Edge(含 UA 形式 `Edg/`)/Headless)。防止本机其他进程恰好占住端口后被当成
/// 受控浏览器。
fn browser_is_chromium(v: &Value) -> bool {
    const KNOWN: [&str; 5] = ["Chrome", "Chromium", "Edge", "Edg/", "Headless"];
    v.get("Browser")
        .and_then(|b| b.as_str())
        .map(|s| KNOWN.iter().any(|k| s.contains(k)))
        .unwrap_or(false)
}

/// http(s) 白名单；`about:blank` 用于新标签页初始页。
fn normalize_url(url: &str) -> Result<String, HarnessError> {
    let url = url.trim();
    if url == "about:blank" {
        return Ok(url.to_string());
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(HarnessError::Tool(
            "url 必须以 http(s):// 开头（file:// 与 javascript: 被禁止）".into(),
        ));
    }
    Ok(url.to_string())
}

// ------------------------------------------------------------------ tools

fn need_str(a: &Value, key: &str) -> Result<String, HarnessError> {
    a.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| HarnessError::Tool(format!("`{key}` is required")))
}

fn bridge_of(ctx: &super::tools::ToolCtx) -> Result<Arc<ComputerBridge>, HarnessError> {
    ctx.computer
        .clone()
        .ok_or_else(|| HarnessError::Tool("computer use 未挂载".into()))
}

/// computer_snapshot：一次拿到无障碍树 + 截图 + 页面元信息（主力观察工具）。
pub async fn snapshot(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    ctx.authorize("computer_snapshot", "screen", crate::permission::Risk::Shell).await?;
    let bridge = bridge_of(ctx)?;
    let full = a.get("full_page").and_then(|v| v.as_bool()).unwrap_or(false);
    let (tree, refs) = bridge.ax_tree().await?;
    bridge.screenshot_opts(full, None).await?;
    Ok(super::tools::ToolOutcome {
        ok: true,
        title: format!("Snapshot · {refs} interactive refs"),
        display: last_image_display(&bridge).await,
        for_model: format!(
            "{tree}\n最新截图已附在本条工具结果之后（视口像素 == 树中坐标）。ref 在两次快照之间可能失效，动作报失效就重新快照。"
        ),
    })
}

/// computer_axtree：只取无障碍树（省 token 的观察路径）。
pub async fn axtree(_a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    ctx.authorize("computer_axtree", "screen", crate::permission::Risk::Shell).await?;
    let bridge = bridge_of(ctx)?;
    let (tree, refs) = bridge.ax_tree().await?;
    Ok(super::tools::ToolOutcome::text(
        true,
        format!("AX tree · {refs} refs"),
        tree,
    ))
}

/// computer_screenshot：只取截图（canvas / 视觉布局场景）。
/// 给 x/y/width/height 时按 2× 放大截取该区域（zoom，看清密集小目标）。
pub async fn screenshot(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    ctx.authorize("computer_screenshot", "screen", crate::permission::Risk::Shell).await?;
    let bridge = bridge_of(ctx)?;
    let num = |k: &str| a.get(k).and_then(|v| v.as_f64());
    let clip = (num("x"), num("y"), num("width"), num("height"));
    let (full_page, clip, title) = match clip {
        (Some(x), Some(y), Some(w), Some(h)) => {
            if w <= 0.0 || h <= 0.0 {
                return Err(HarnessError::Tool("width/height 必须为正".into()));
            }
            (false, Some([x, y, w, h]), format!("Screenshot zoom ({x:.0},{y:.0} {w:.0}×{h:.0} @2x)"))
        }
        (None, None, None, None) => (
            a.get("full_page").and_then(|v| v.as_bool()).unwrap_or(false),
            None,
            "Captured screenshot".into(),
        ),
        _ => return Err(HarnessError::Tool("zoom 需要 x/y/width/height 四个参数齐全".into())),
    };
    bridge.screenshot_opts(full_page, clip).await?;
    Ok(super::tools::ToolOutcome {
        ok: true,
        title,
        display: last_image_display(&bridge).await,
        for_model: "截图已捕获，图像已附在本条工具结果之后。先观察，再用坐标或 ref 操作。".into(),
    })
}

/// computer_extract：URL + 标题 + 可见文本，读正文最省 token 的方式。
pub async fn extract(_a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    ctx.authorize("computer_extract", "screen", crate::permission::Risk::Shell).await?;
    let bridge = bridge_of(ctx)?;
    let (url, title) = bridge.page_meta().await.unwrap_or_default();
    let mut text = bridge.page_text().await?;
    let truncated = text.len() > 12000;
    if truncated {
        // 按字符边界截，避免把多字节字符切成乱码
        let mut cut = 12000;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    Ok(super::tools::ToolOutcome::text(
        true,
        format!("Extract · {title}"),
        format!("url: {url}\ntitle: {title}\n\n{text}{}", if truncated { "\n\n…(文本过长已截断，可结合 computer_scroll 观察)" } else { "" }),
    ))
}

/// computer_click：按 ref（无障碍优先）或坐标（视觉兜底）点击。
pub async fn click(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let bridge = bridge_of(ctx)?;
    let button = a.get("button").and_then(|v| v.as_str()).unwrap_or("left").to_string();
    let clicks = a.get("clicks").and_then(|v| v.as_i64()).unwrap_or(1).clamp(1, 3);
    // 先授权（只描述意图，不做任何 CDP 副作用），再解析目标
    let desc = if let Some(r) = a.get("ref").and_then(|v| v.as_i64()) {
        format!("click r{r} ×{clicks} {button}")
    } else {
        let x = a.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let y = a.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
        format!("click ({x:.0},{y:.0}) ×{clicks} {button}")
    };
    ctx.authorize("computer_click", &desc, crate::permission::Risk::Shell).await?;
    let (x, y, acted) = bridge.target_point(a).await?;
    bridge.click_xy(x, y, &button, clicks).await?;
    post_action(&bridge, "Clicked", &acted).await
}

/// computer_hover：悬停（hover 菜单/tooltip）。
pub async fn hover(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let bridge = bridge_of(ctx)?;
    let desc = if let Some(r) = a.get("ref").and_then(|v| v.as_i64()) {
        format!("hover r{r}")
    } else {
        let x = a.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let y = a.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
        format!("hover ({x:.0},{y:.0})")
    };
    ctx.authorize("computer_hover", &desc, crate::permission::Risk::Shell).await?;
    let (x, y, acted) = bridge.target_point(a).await?;
    bridge
        .cmd(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseMoved", "x": x, "y": y, "button": "none"}),
        )
        .await?;
    post_action(&bridge, "Hovered", &acted).await
}

/// computer_drag：从起点按住拖到终点（滑块、画布、排序）。
pub async fn drag(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let bridge = bridge_of(ctx)?;
    let desc = format!(
        "drag {} → {}",
        drag_point_desc(&point_arg(a, "from")?),
        drag_point_desc(&point_arg(a, "to")?)
    );
    ctx.authorize("computer_drag", &desc, crate::permission::Risk::Shell).await?;
    let (x1, y1, from) = bridge.target_point(&point_arg(a, "from")?).await?;
    let (x2, y2, to) = bridge.target_point(&point_arg(a, "to")?).await?;
    bridge
        .cmd(
            "Input.dispatchMouseEvent",
            json!({"type": "mousePressed", "x": x1, "y": y1, "button": "left", "clickCount": 1, "buttons": 1}),
        )
        .await?;
    let steps = 8;
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        bridge
            .cmd(
                "Input.dispatchMouseEvent",
                json!({"type": "mouseMoved",
                       "x": x1 + (x2 - x1) * t, "y": y1 + (y2 - y1) * t, "buttons": 1}),
            )
            .await?;
    }
    bridge
        .cmd(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseReleased", "x": x2, "y": y2, "button": "left", "clickCount": 1, "buttons": 1}),
        )
        .await?;
    post_action(&bridge, "Dragged", &format!("{from} → {to}")).await
}

/// drag 的 from/to 参数兼容 {x,y} 与 {ref}；直接平铺写法也接受。
fn point_arg(a: &Value, key: &str) -> Result<Value, HarnessError> {
    if let Some(p) = a.get(key) {
        if p.is_object() {
            return Ok(p.clone());
        }
    }
    // 平铺：x1/y1 或 from_ref/to_ref
    let (px, py, pr) = match key {
        "from" => ("x1", "y1", "from_ref"),
        _ => ("x2", "y2", "to_ref"),
    };
    let mut obj = serde_json::Map::new();
    if let Some(r) = a.get(pr).and_then(|v| v.as_i64()) {
        obj.insert("ref".into(), json!(r));
    } else {
        let x = a.get(px).and_then(|v| v.as_f64())
            .ok_or_else(|| HarnessError::Tool(format!("`{key}` 需要 {{ref}} 或 {{x,y}}（平铺 {px}/{py}）")))?;
        let y = a.get(py).and_then(|v| v.as_f64())
            .ok_or_else(|| HarnessError::Tool(format!("`{key}` 需要 {{ref}} 或 {{x,y}}（平铺 {px}/{py}）")))?;
        obj.insert("x".into(), json!(x));
        obj.insert("y".into(), json!(y));
    }
    Ok(Value::Object(obj))
}

/// 授权面板里 drag 端点的简短描述（不做任何 CDP 调用）。
fn drag_point_desc(p: &Value) -> String {
    if let Some(r) = p.get("ref").and_then(|v| v.as_i64()) {
        format!("r{r}")
    } else {
        let x = p.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let y = p.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
        format!("({x:.0},{y:.0})")
    }
}

/// computer_type：文本输入。可先按 ref 点进输入框；clear=true 先清空。
pub async fn type_text(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let text = need_str(a, "text")?;
    let head: String = text.chars().take(40).collect();
    let bridge = bridge_of(ctx)?;
    // 先授权（只描述意图），再做点击聚焦等 CDP 副作用
    let desc = if let Some(r) = a.get("ref").and_then(|v| v.as_i64()) {
        format!("type into r{r}: {head}")
    } else {
        format!("type: {head}")
    };
    ctx.authorize("computer_type", &desc, crate::permission::Risk::Shell).await?;
    // 可选：先把焦点放进指定输入框
    if let Some(r) = a.get("ref").and_then(|v| v.as_i64()) {
        let rect = bridge.resolve_ref(r).await?;
        bridge.click_xy(rect.x + rect.w / 2.0, rect.y + rect.h / 2.0, "left", 1).await?;
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }
    if a.get("clear").and_then(|v| v.as_bool()).unwrap_or(false) {
        // Control+A + Delete：覆盖 Windows/macOS 之外的主流场景（受控浏览器是桌面 Chromium）
        clear_focused(&bridge).await?;
    }
    bridge
        .cmd("Input.insertText", json!({"text": text}))
        .await?;
    post_action(&bridge, "Typed", &format!("{} 字", text.chars().count())).await
}

async fn clear_focused(bridge: &Arc<ComputerBridge>) -> Result<(), HarnessError> {
    for (t, k, code, vk) in [
        ("rawKeyDown", "a", "KeyA", 65i64),
        ("keyUp", "a", "KeyA", 65i64),
    ] {
        bridge
            .cmd("Input.dispatchKeyEvent", json!({
                "type": t, "key": k, "code": code,
                "windowsVirtualKeyCode": vk, "modifiers": 2
            }))
            .await?;
    }
    bridge
        .cmd("Input.dispatchKeyEvent", json!({
            "type": "keyDown", "key": "Delete", "code": "Delete", "windowsVirtualKeyCode": 46
        }))
        .await?;
    bridge
        .cmd("Input.dispatchKeyEvent", json!({
            "type": "keyUp", "key": "Delete", "code": "Delete", "windowsVirtualKeyCode": 46
        }))
        .await?;
    Ok(())
}

/// computer_key：单键或组合键（如 Enter、Control+a）。
pub async fn key(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let key = need_str(a, "key")?;
    ctx.authorize("computer_key", &key, crate::permission::Risk::Shell).await?;
    let bridge = bridge_of(ctx)?;
    let parts: Vec<&str> = key.split('+').map(|s| s.trim()).collect();
    let (mods, main) = parts.split_at(parts.len().saturating_sub(1));
    let main = main.first().copied().unwrap_or("");
    let Some((k, code, vk)) = key_map(main) else {
        return Err(HarnessError::Tool(format!(
            "未知按键 `{main}`。支持: Enter Tab Escape Backspace Delete Space Home End PageUp PageDown ArrowUp/Down/Left/Right 及 Control/Alt/Shift/Meta 组合"
        )));
    };
    let mut mask = 0i64;
    for m in mods {
        mask |= match m.to_ascii_lowercase().as_str() {
            "control" | "ctrl" => 2,
            "alt" => 1,
            "shift" => 8,
            "meta" | "win" | "cmd" => 4,
            other => {
                return Err(HarnessError::Tool(format!("未知修饰键 `{other}`")));
            }
        };
    }
    // 修饰键按住
    for m in mods {
        let (mk, mcode, mvk) = key_map(m.trim()).unwrap_or((m.to_string(), m.to_string(), 0));
        bridge
            .cmd("Input.dispatchKeyEvent", json!({
                "type": "rawKeyDown", "key": mk, "code": mcode,
                "windowsVirtualKeyCode": mvk, "modifiers": mask
            }))
            .await?;
    }
    bridge
        .cmd("Input.dispatchKeyEvent", json!({
            "type": "keyDown", "key": k, "code": code,
            "windowsVirtualKeyCode": vk, "modifiers": mask,
        }))
        .await?;
    bridge
        .cmd("Input.dispatchKeyEvent", json!({
            "type": "keyUp", "key": k, "code": code,
            "windowsVirtualKeyCode": vk, "modifiers": mask,
        }))
        .await?;
    for m in mods.iter().rev() {
        let (mk, mcode, mvk) = key_map(m.trim()).unwrap_or((m.to_string(), m.to_string(), 0));
        bridge
            .cmd("Input.dispatchKeyEvent", json!({
                "type": "keyUp", "key": mk, "code": mcode,
                "windowsVirtualKeyCode": mvk, "modifiers": mask
            }))
            .await?;
    }
    post_action(&bridge, "Pressed", &key).await
}

/// computer_scroll：滚轮。
pub async fn scroll(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let dir = a.get("direction").and_then(|v| v.as_str()).unwrap_or("down").to_lowercase();
    let pages = a.get("pages").and_then(|v| v.as_f64()).unwrap_or(1.0).max(0.25);
    ctx.authorize("computer_scroll", &dir, crate::permission::Risk::Shell).await?;
    let bridge = bridge_of(ctx)?;
    // 默认滚视口中心；给 ref/x,y 就滚那个元素上
    let (x, y) = if a.get("ref").is_some() || a.get("x").is_some() {
        let (px, py, _) = bridge.target_point(a).await?;
        (px, py)
    } else {
        (400.0, 400.0)
    };
    let (dx, dy) = match dir.as_str() {
        "up" | "u" => (0.0, -600.0 * pages),
        "down" | "d" => (0.0, 600.0 * pages),
        "left" | "l" => (-600.0 * pages, 0.0),
        "right" | "r" => (600.0 * pages, 0.0),
        other => return Err(HarnessError::Tool(format!("方向 `{other}` 无效 (up/down/left/right)"))),
    };
    bridge
        .cmd("Input.dispatchMouseEvent", json!({
            "type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy
        }))
        .await?;
    post_action(&bridge, "Scrolled", &dir).await
}

/// computer_select：给 <select> 选选项（ref + 文本/值匹配）。
pub async fn select_option(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let r = a.get("ref").and_then(|v| v.as_i64())
        .ok_or_else(|| HarnessError::Tool("`ref` is required（下拉选择按元素操作，不支持坐标）".into()))?;
    let wanted = need_str(a, "option")?;
    ctx.authorize("computer_select", &format!("r{r}: {wanted}"), crate::permission::Risk::Shell).await?;
    let bridge = bridge_of(ctx)?;
    let result = bridge.select_option(r, &wanted).await?;
    if result.starts_with("err:") {
        let msg = result.trim_start_matches("err:").to_string();
        return Err(HarnessError::Tool(if let Some(list) = msg.strip_prefix("no-option:") {
            format!("没有匹配的选项 `{wanted}`。现有选项：{list}")
        } else {
            format!("ref r{r} 不是 <select> 元素——下拉菜单（非原生 select）请用 computer_click + computer_click 逐项点选")
        }));
    }
    let chosen = result.trim_start_matches("ok:").to_string();
    post_action(&bridge, "Selected", &format!("r{r} → {chosen}")).await
}

/// computer_navigate：打开网址 / 后退 / 前进 / 刷新，等加载稳定后观察。
pub async fn navigate(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let bridge = bridge_of(ctx)?;
    let action = a.get("action").and_then(|v| v.as_str()).unwrap_or("open");
    let (desc, what) = if action == "back" || action == "forward" || action == "reload" {
        (format!("{action} (history)"), action.to_string())
    } else {
        let url = need_str(a, "url")?;
        (normalize_url(&url)?, url.clone())
    };
    ctx.authorize("computer_navigate", &desc, crate::permission::Risk::Network).await?;
    match action {
        // 后退/前进用组合键实现：navigateToHistoryEntry 需要 entryId，
        // 而 /json 端点拿不到历史列表——Alt+Left/Right 是等价且最稳的路径。
        "back" => {
            history_key(&bridge, "Alt", "ArrowLeft").await?;
        }
        "forward" => {
            history_key(&bridge, "Alt", "ArrowRight").await?;
        }
        "reload" => {
            bridge.cmd("Page.reload", json!({})).await?;
        }
        _ => {
            let url = normalize_url(&what)?;
            bridge.cmd("Page.navigate", json!({"url": url})).await?;
        }
    }
    bridge.wait_ready(8000).await;
    let (url, title) = bridge.page_meta().await.unwrap_or_default();
    let what = if title.is_empty() { url.clone() } else { title.clone() };
    post_action_meta(&bridge, "Navigated", &what, Some((url, title))).await
}

async fn history_key(bridge: &Arc<ComputerBridge>, mod_: &str, k: &str) -> Result<(), HarnessError> {
    let (mk, mcode, mvk) = key_map(mod_).unwrap_or(("Alt".into(), "AltLeft".into(), 18));
    let (kk, kcode, kvk) = key_map(k).unwrap_or((k.into(), k.into(), 0));
    bridge
        .cmd("Input.dispatchKeyEvent", json!({
            "type": "rawKeyDown", "key": mk, "code": mcode, "windowsVirtualKeyCode": mvk, "modifiers": 1
        }))
        .await?;
    bridge
        .cmd("Input.dispatchKeyEvent", json!({
            "type": "keyDown", "key": kk, "code": kcode, "windowsVirtualKeyCode": kvk, "modifiers": 1
        }))
        .await?;
    bridge
        .cmd("Input.dispatchKeyEvent", json!({
            "type": "keyUp", "key": kk, "code": kcode, "windowsVirtualKeyCode": kvk, "modifiers": 1
        }))
        .await?;
    bridge
        .cmd("Input.dispatchKeyEvent", json!({
            "type": "keyUp", "key": mk, "code": mcode, "windowsVirtualKeyCode": mvk, "modifiers": 1
        }))
        .await?;
    Ok(())
}

/// computer_tabs：多标签页管理（list/new/switch/close）。
pub async fn tabs(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let bridge = bridge_of(ctx)?;
    let action = a.get("action").and_then(|v| v.as_str()).unwrap_or("list");
    match action {
        "list" => {
            ctx.authorize("computer_tabs", "list", crate::permission::Risk::Shell).await?;
            let tabs = bridge.list_tabs().await?;
            let mut out = String::from("标签页（* = 当前）:\n");
            for (id, i, title, url, cur) in &tabs {
                let t: String = title.chars().take(50).collect();
                out.push_str(&format!(
                    "{}[{i}] {} {t} — {url}\n",
                    if *cur { "* " } else { "  " },
                    &id[..id.len().min(8)]
                ));
            }
            Ok(super::tools::ToolOutcome::text(true, format!("Tabs · {} open", tabs.len()), out))
        }
        "new" => {
            let url = a.get("url").and_then(|v| v.as_str()).unwrap_or("about:blank");
            ctx.authorize("computer_tabs", &format!("new: {url}"), crate::permission::Risk::Network).await?;
            let id = bridge.new_tab(url).await?;
            tokio::time::sleep(std::time::Duration::from_millis(600)).await;
            post_action(&bridge, "Opened tab", &id[..id.len().min(8)]).await
        }
        "switch" => {
            let idx = a.get("index").and_then(|v| v.as_i64());
            let tid = a.get("target_id").and_then(|v| v.as_str()).map(|s| s.to_string());
            let tabs = bridge.list_tabs().await?;
            let hit = match (idx, tid) {
                (Some(i), _) => tabs.iter().find(|(_, n, ..)| *n == i as usize),
                (_, Some(t)) => tabs.iter().find(|(id, ..)| id.starts_with(t.as_str())),
                _ => None,
            };
            let Some((id, _, title, url, _)) = hit else {
                return Err(HarnessError::Tool("找不到目标标签页——先 computer_tabs action=list 看看有哪些".into()));
            };
            ctx.authorize("computer_tabs", &format!("switch: {title}"), crate::permission::Risk::Shell).await?;
            bridge.switch_tab(id).await?;
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            post_action(&bridge, "Switched to", &format!("[{title}] {url}")).await
        }
        "close" => {
            let idx = a.get("index").and_then(|v| v.as_i64());
            let tid = a.get("target_id").and_then(|v| v.as_str()).map(|s| s.to_string());
            let tabs = bridge.list_tabs().await?;
            let hit = match (idx, tid) {
                (Some(i), _) => tabs.iter().find(|(_, n, ..)| *n == i as usize),
                (_, Some(t)) => tabs.iter().find(|(id, ..)| id.starts_with(t.as_str())),
                _ => None,
            };
            let Some((id, _, title, _, _)) = hit else {
                return Err(HarnessError::Tool("找不到目标标签页——先 computer_tabs action=list".into()));
            };
            if tabs.len() <= 1 {
                return Err(HarnessError::Tool("只剩最后一个标签页，不能关闭".into()));
            }
            ctx.authorize("computer_tabs", &format!("close: {title}"), crate::permission::Risk::Shell).await?;
            let msg = bridge.close_tab(id).await?;
            post_action(&bridge, "Closed tab", &msg).await
        }
        other => Err(HarnessError::Tool(format!(
            "未知 action `{other}`（list / new / switch / close）"
        ))),
    }
}

/// computer_wait：等页面稳定（动画/懒加载/跳转）后再观察。
/// `until_text` 出现可提前结束；文本没等到也不报错（截图反映现状）。
pub async fn wait_ms(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let secs = a.get("seconds").and_then(|v| v.as_f64()).unwrap_or(1.0).clamp(0.1, 10.0);
    let until = a.get("until_text").and_then(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let label = match &until {
        Some(t) => format!("{secs}s until \"{t}\""),
        None => format!("{secs}s"),
    };
    ctx.authorize("computer_wait", &label, crate::permission::Risk::Shell).await?;
    let bridge = bridge_of(ctx)?;
    let step = std::time::Duration::from_millis(300);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis((secs * 1000.0) as u64);
    let mut found = false;
    loop {
        if let Some(t) = &until {
            let body = bridge.page_text().await.unwrap_or_default();
            if body.contains(t.as_str()) {
                found = true;
                break;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(step).await;
    }
    bridge.wait_ready(1500).await;
    let note = match (&until, found) {
        (Some(t), true) => format!("text \"{t}\" appeared"),
        (Some(t), false) => format!("waited {secs}s but \"{t}\" did NOT appear — page may have failed; check the screenshot"),
        _ => format!("waited {secs}s"),
    };
    post_action(&bridge, "Waited", &note).await
}

/// 动作后的统一观察：截一张新图，让模型立刻看到效果（observe-act 循环）。
async fn post_action(
    bridge: &Arc<ComputerBridge>,
    verb: &str,
    what: &str,
) -> Result<super::tools::ToolOutcome, HarnessError> {
    post_action_meta(bridge, verb, what, None).await
}

async fn post_action_meta(
    bridge: &Arc<ComputerBridge>,
    verb: &str,
    what: &str,
    meta: Option<(String, String)>,
) -> Result<super::tools::ToolOutcome, HarnessError> {
    // 动作到渲染之间留一点时间，再等 readyState 稳定（导航场景已等过，这里很快）
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    bridge.wait_ready(1500).await;
    bridge.screenshot_raw().await?;
    let meta_line = {
        let (u, t) = match meta {
            Some((u, t)) => (u, t),
            None => bridge.page_meta().await.unwrap_or_default(),
        };
        if u.is_empty() {
            String::new()
        } else {
            format!("\nurl: {u}\npage: {t}")
        }
    };
    Ok(super::tools::ToolOutcome {
        ok: true,
        title: format!("{verb} {what}"),
        display: last_image_display(bridge).await,
        for_model: format!(
            "{verb} {what} 完成。最新截图已附在本条工具结果之后——先观察再决定下一步。{meta_line}"
        ),
    })
}

/// 从桥里【只读】最新的待发截图作为展示载荷——消费留给 agent 折叠，
/// 否则 UI 拿到图、模型上下文却丢了图。
async fn last_image_display(bridge: &Arc<ComputerBridge>) -> super::tools::DisplayKind {
    let img = bridge.peek_image().await;
    match img {
        Some(p) => super::tools::DisplayKind::Image {
            media_type: p.media_type,
            data: p.data,
        },
        None => super::tools::DisplayKind::Plain("(无截图)".into()),
    }
}

fn key_map(name: &str) -> Option<(String, String, i64)> {
    let lower = name.to_ascii_lowercase();
    let (k, code, vk) = match lower.as_str() {
        "enter" | "return" => ("Enter", "Enter", 13),
        "tab" => ("Tab", "Tab", 9),
        "escape" | "esc" => ("Escape", "Escape", 27),
        "backspace" => ("Backspace", "Backspace", 8),
        "delete" | "del" => ("Delete", "Delete", 46),
        "space" => (" ", "Space", 32),
        "pageup" => ("PageUp", "PageUp", 33),
        "pagedown" => ("PageDown", "PageDown", 34),
        "home" => ("Home", "Home", 36),
        "end" => ("End", "End", 35),
        "arrowleft" | "left" => ("ArrowLeft", "ArrowLeft", 37),
        "arrowup" | "up" => ("ArrowUp", "ArrowUp", 38),
        "arrowright" | "right" => ("ArrowRight", "ArrowRight", 39),
        "arrowdown" | "down" => ("ArrowDown", "ArrowDown", 40),
        "control" | "ctrl" => ("Control", "ControlLeft", 17),
        "alt" => ("Alt", "AltLeft", 18),
        "shift" => ("Shift", "ShiftLeft", 16),
        "meta" | "win" | "cmd" => ("Meta", "MetaLeft", 91),
        _ => return None,
    };
    Some((k.into(), code.into(), vk))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_map_covers_basics() {
        assert_eq!(key_map("Enter").unwrap().2, 13);
        assert_eq!(key_map("ctrl").unwrap().0, "Control");
        assert!(key_map("F1").is_none());
    }

    #[test]
    fn browser_field_gate() {
        // /json/version 的 Browser 字段必须像 Chromium 系才接受
        assert!(browser_is_chromium(&json!({"Browser": "Chrome/120.0.0.0"})));
        assert!(browser_is_chromium(&json!({"Browser": "Edg/121.0"})));
        assert!(browser_is_chromium(&json!({"Browser": "HeadlessChrome/121.0"})));
        assert!(!browser_is_chromium(&json!({"Browser": "SomeProxy/1.0"})));
        assert!(!browser_is_chromium(&json!({})));
    }

    #[test]
    fn url_whitelist() {
        assert!(normalize_url("https://example.com").is_ok());
        assert!(normalize_url(" about:blank ").is_ok());
        assert!(normalize_url("file:///C:/Windows").is_err());
        assert!(normalize_url("javascript:alert(1)").is_err());
        assert!(normalize_url("data:text/html,x").is_err());
    }

    #[test]
    fn mouse_mask() {
        assert_eq!(mouse_button_mask("left"), 1);
        assert_eq!(mouse_button_mask("right"), 2);
        assert_eq!(mouse_button_mask("middle"), 4);
    }

    #[test]
    fn quad_to_rect_uses_corners() {
        // 四角乱序也能取到包围盒
        let quad = json!([30.0, 20.0, 10.0, 20.0, 30.0, 60.0, 10.0, 60.0]);
        let nums: Vec<f64> = quad.as_array().unwrap().iter().filter_map(|v| v.as_f64()).collect();
        let xs = [nums[0], nums[2], nums[4], nums[6]];
        let ys = [nums[1], nums[3], nums[5], nums[7]];
        assert_eq!(xs.iter().cloned().fold(f64::MAX, f64::min), 10.0);
        assert_eq!(ys.iter().cloned().fold(f64::MAX, f64::min), 20.0);
    }

    #[test]
    fn point_arg_flat_and_nested() {
        let a = json!({"from": {"ref": 7}, "x2": 1.0, "y2": 2.0});
        let f = point_arg(&a, "from").unwrap();
        assert_eq!(f["ref"], json!(7));
        let t = point_arg(&a, "to").unwrap();
        assert_eq!(t["x"], json!(1.0));
        let flat = json!({"x1": 3.0, "y1": 4.0, "to_ref": 9});
        let f2 = point_arg(&flat, "from").unwrap();
        assert_eq!(f2["x"], json!(3.0));
        let t2 = point_arg(&flat, "to").unwrap();
        assert_eq!(t2["ref"], json!(9));
    }
}
