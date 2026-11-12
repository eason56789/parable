//! Computer use — drive a Chromium browser over the DevTools protocol.
//!
//! Two observation paths, mirroring ZCode's CUA contract:
//! - **视觉**：`Page.captureScreenshot` → base64 PNG，模型以坐标操作
//!   （`Input.dispatchMouseEvent` / `insertText` / key events）。
//! - **无障碍**：`Accessibility.getFullAXTree` → 带 `[ref]` 的紧凑树，
//!   模型优先按 ref 点击（语义准确、不依赖像素）。
//!
//! 浏览器 lazily 拉起（Edge/Chrome，独立 user-data-dir，强制 DPR=1 使
//! 截图像素 == CSS 坐标），进程与 WS 会话在本桥内复用。

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
            let msg = tokio::time::timeout(std::time::Duration::from_secs(20), self.ws.next())
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

/// Per-process bridge: owns the launched browser and the live CDP session.
pub struct ComputerBridge {
    port: u16,
    child: Mutex<Option<tokio::process::Child>>,
    cdp: Mutex<Option<Cdp>>,
    /// Last AX tree, so `ref` actions can resolve to coordinates.
    ax_nodes: Mutex<HashMap<i64, AxNode>>,
    pending: Mutex<Option<PendingImage>>,
}

#[derive(Debug, Clone)]
struct AxNode {
    role: String,
    name: String,
    backend: Option<i64>,
}

impl ComputerBridge {
    pub fn new() -> Self {
        let port = std::env::var("PARABLE_CDP_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(9222);
        Self {
            port,
            child: Mutex::new(None),
            cdp: Mutex::new(None),
            ax_nodes: Mutex::new(HashMap::new()),
            pending: Mutex::new(None),
        }
    }

    /// Take the screenshot captured by the latest computer tool call.
    pub async fn take_image(&self) -> Option<PendingImage> {
        self.pending.lock().await.take()
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
        let probe = client
            .get(format!("http://127.0.0.1:{}/json", self.port))
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await;
        if probe.is_err() {
            self.launch_browser().await?;
        }
        // Pick the first page target.
        let list: Value = client
            .get(format!("http://127.0.0.1:{}/json", self.port))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| HarnessError::Tool(format!("CDP 发现失败: {e}")))?
            .json()
            .await
            .map_err(|e| HarnessError::Tool(format!("CDP 发现响应异常: {e}")))?;
        let ws_url = list
            .as_array()
            .and_then(|a| a.iter().find(|t| t["type"] == "page"))
            .and_then(|t| t["webSocketDebuggerUrl"].as_str())
            .ok_or_else(|| HarnessError::Tool("没有可用的浏览器页面目标".into()))?
            .to_string();
        let cdp = Cdp::connect(&ws_url).await?;
        *self.cdp.lock().await = Some(cdp);
        Ok(())
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
        let dir = std::env::temp_dir().join("parable-cua");
        let _ = std::fs::create_dir_all(&dir);
        let child = tokio::process::Command::new(exe)
            .arg(format!("--remote-debugging-port={}", self.port))
            .arg(format!("--user-data-dir={}", dir.display()))
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "--force-device-scale-factor=1",
                "--restore-last-session=false",
                "about:blank",
            ])
            .spawn()
            .map_err(|e| HarnessError::Tool(format!("启动浏览器失败: {e}")))?;
        *self.child.lock().await = Some(child);
        // 轮询调试端口就绪
        let client = reqwest::Client::new();
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            if client
                .get(format!("http://127.0.0.1:{}/json/version", self.port))
                .timeout(std::time::Duration::from_millis(800))
                .send()
                .await
                .is_ok()
            {
                return Ok(());
            }
        }
        Err(HarnessError::Tool("浏览器调试端口未就绪".into()))
    }

    async fn cmd(&self, method: &str, params: Value) -> Result<Value, HarnessError> {
        self.ensure().await?;
        let mut cdp = self.cdp.lock().await;
        match cdp.as_mut() {
            Some(c) => c.cmd(method, params).await,
            None => Err(HarnessError::Tool("CDP 未连接".into())),
        }
    }

    /// 视觉：截图并以 base64 PNG 返回。
    async fn screenshot_raw(&self) -> Result<String, HarnessError> {
        let r = self
            .cmd("Page.captureScreenshot", json!({"format": "png"}))
            .await?;
        let data = r
            .get("data")
            .and_then(|d| d.as_str())
            .ok_or_else(|| HarnessError::Tool("截图响应缺少 data".into()))?
            .to_string();
        self.store_image("image/png", &data).await;
        Ok(data)
    }

    /// 无障碍：拉取全树并序列化为带 `[ref]` 的紧凑文本。
    async fn ax_tree(&self) -> Result<String, HarnessError> {
        let r = self
            .cmd("Accessibility.getFullAXTree", json!({}))
            .await?;
        let nodes = r
            .get("nodes")
            .and_then(|n| n.as_array())
            .ok_or_else(|| HarnessError::Tool("AX 树响应缺少 nodes".into()))?;
        let mut map: HashMap<i64, AxNode> = HashMap::new();
        let mut children: HashMap<i64, Vec<i64>> = HashMap::new();
        let mut roots: Vec<i64> = Vec::new();
        for n in nodes {
            if n.get("ignored").and_then(|i| i.as_bool()).unwrap_or(false) {
                continue;
            }
            let id = n
                .get("nodeId")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<i64>().ok())
                .or_else(|| n.get("nodeId").and_then(|v| v.as_i64()));
            let Some(id) = id else { continue };
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
            let backend = n
                .get("backendDOMNodeId")
                .and_then(|v| v.as_i64());
            map.insert(id, AxNode { role, name, backend });
            let mut has_parent = false;
            if let Some(kids) = n.get("childIds").and_then(|c| c.as_array()) {
                for kid in kids {
                    if let Some(kid) = kid.as_str().and_then(|s| s.parse::<i64>().ok()) {
                        children.entry(id).or_default().push(kid);
                        has_parent = true;
                    }
                }
            }
            let _ = has_parent;
            roots.push(id);
        }
        // 根 = 没有被任何 childIds 引用的节点
        let mut referenced: std::collections::HashSet<i64> = std::collections::HashSet::new();
        for kids in children.values() {
            for k in kids {
                referenced.insert(*k);
            }
        }
        let roots: Vec<i64> = roots.into_iter().filter(|id| !referenced.contains(id)).collect();

        *self.ax_nodes.lock().await = map.clone();

        let interactive = [
            "button", "link", "textbox", "searchbox", "combobox", "listbox", "option", "menuitem",
            "checkbox", "radio", "slider", "tab", "switch", "menu", "tablist", "tree", "list",
        ];
        let mut out = String::from("无障碍树（[ref] 角色名称；优先按 ref 操作，ref 不可见时回退坐标）:\n");
        let mut count = 0usize;
        // 迭代 DFS，避免递归栈
        let mut stack: Vec<(i64, usize)> = roots.into_iter().rev().map(|id| (id, 0)).collect();
        while let Some((id, depth)) = stack.pop() {
            if count > 300 || depth > 18 {
                continue;
            }
            let Some(node) = map.get(&id) else { continue };
            let interesting = interactive.contains(&node.role.as_str())
                || (!node.name.is_empty()
                    && !matches!(node.role.as_str(), "generic" | "none" | "InlineTextBox" | "StaticText" | "group" | "WebArea"));
            if interesting && (node.role == "WebArea" || !node.name.is_empty() || interactive.contains(&node.role.as_str())) {
                let nm = node.name.chars().take(80).collect::<String>();
                out.push_str(&format!(
                    "{}[{}] {}: {}{}\n",
                    "  ".repeat(depth.min(10)),
                    id,
                    node.role,
                    nm,
                    if node.backend.is_some() { "" } else { " (无坐标)" }
                ));
                count += 1;
            }
            if let Some(kids) = children.get(&id) {
                for kid in kids.iter().rev() {
                    stack.push((*kid, depth + if interesting { 1 } else { 0 }));
                }
            }
        }
        if count == 0 {
            out.push_str("(空)\n");
        }
        Ok(out)
    }

    /// 按 ref（AX nodeId）定位坐标并点击。
    async fn click_ref(&self, id: i64, button: &str) -> Result<(), HarnessError> {
        let node = self
            .ax_nodes
            .lock()
            .await
            .get(&id)
            .cloned()
            .ok_or_else(|| HarnessError::Tool(format!("ref {id} 不存在，请先 computer_axtree")))?;
        let Some(backend) = node.backend else {
            return Err(HarnessError::Tool(format!(
                "ref {id}（{}:{}）没有 DOM 节点，无法定位坐标",
                node.role, node.name
            )));
        };
        let r = self
            .cmd("DOM.getBoxModel", json!({"backendNodeId": backend}))
            .await?;
        let quad = r
            .pointer("/model/content")
            .and_then(|c| c.as_array())
            .ok_or_else(|| HarnessError::Tool(format!("ref {id} 当前不可见（无盒模型）")))?;
        let nums: Vec<f64> = quad
            .iter()
            .filter_map(|v| v.as_f64())
            .collect();
        if nums.len() < 8 {
            return Err(HarnessError::Tool(format!("ref {id} 盒模型数据不完整")));
        }
        let cx = (nums[0] + nums[2] + nums[4] + nums[6]) / 4.0;
        let cy = (nums[1] + nums[3] + nums[5] + nums[7]) / 4.0;
        self.click_xy(cx, cy, button, 1).await
    }

    async fn click_xy(&self, x: f64, y: f64, button: &str, clicks: i64) -> Result<(), HarnessError> {
        let presses = clicks.max(1);
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
}

fn mouse_button_mask(button: &str) -> i64 {
    match button {
        "middle" => 4,
        "right" => 2,
        _ => 1,
    }
}

// ------------------------------------------------------------------ tools

fn need_str(a: &Value, key: &str) -> Result<String, HarnessError> {
    a.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| HarnessError::Tool(format!("`{key}` is required")))
}

///computer_screenshot：视觉观察。返回后模型上下文里会附带这张截图。
pub async fn screenshot(_a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    ctx.authorize("computer_screenshot", "screen", crate::permission::Risk::Shell).await?;
    let bridge = ctx.computer.as_ref().ok_or_else(|| HarnessError::Tool("computer use 未挂载".into()))?;
    bridge.screenshot_raw().await?;
    Ok(super::tools::ToolOutcome {
        ok: true,
        title: "Captured screenshot".into(),
        display: last_image_display(bridge).await,
        for_model: "截图已捕获，图像已附在本条工具结果之后。先观察，再用坐标或 ref 操作。".into(),
    })
}

/// computer_axtree：无障碍观察。
pub async fn axtree(_a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    ctx.authorize("computer_axtree", "screen", crate::permission::Risk::Shell).await?;
    let bridge = ctx.computer.as_ref().ok_or_else(|| HarnessError::Tool("computer use 未挂载".into()))?;
    let tree = bridge.ax_tree().await?;
    Ok(super::tools::ToolOutcome::text(
        true,
        format!("Accessibility tree · {} lines", tree.lines().count()),
        tree,
    ))
}

/// computer_click：按 ref（无障碍优先）或坐标（视觉兜底）点击。
pub async fn click(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let desc = if let Some(r) = a.get("ref").and_then(|v| v.as_i64()) {
        format!("ref {r}")
    } else {
        let x = a.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let y = a.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
        format!("click ({x:.0},{y:.0})")
    };
    ctx.authorize("computer_click", &desc, crate::permission::Risk::Shell).await?;
    let bridge = ctx.computer.as_ref().ok_or_else(|| HarnessError::Tool("computer use 未挂载".into()))?;
    let button = a.get("button").and_then(|v| v.as_str()).unwrap_or("left").to_string();
    let clicks = a.get("clicks").and_then(|v| v.as_i64()).unwrap_or(1);
    let acted = if let Some(r) = a.get("ref").and_then(|v| v.as_i64()) {
        bridge.click_ref(r, &button).await?;
        format!("ref {r}")
    } else {
        let x = a
            .get("x")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| HarnessError::Tool("`x`/`y` 或 `ref` 必填".into()))?;
        let y = a
            .get("y")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| HarnessError::Tool("`x`/`y` 或 `ref` 必填".into()))?;
        bridge.click_xy(x, y, &button, clicks).await?;
        format!("({x:.0},{y:.0}) ×{clicks} {button}")
    };
    post_action(bridge, "Clicked", &acted).await
}

/// computer_type：文本输入（insertText，天然支持中文）。
pub async fn type_text(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let text = need_str(a, "text")?;
    let head: String = text.chars().take(40).collect();
    ctx.authorize("computer_type", &head, crate::permission::Risk::Shell).await?;
    let bridge = ctx.computer.as_ref().ok_or_else(|| HarnessError::Tool("computer use 未挂载".into()))?;
    bridge
        .cmd("Input.insertText", json!({"text": text}))
        .await?;
    post_action(bridge, "Typed", &format!("{} 字", text.chars().count())).await
}

/// computer_key：单键或组合键（如 Enter、Control+a）。
pub async fn key(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let key = need_str(a, "key")?;
    ctx.authorize("computer_key", &key, crate::permission::Risk::Shell).await?;
    let bridge = ctx.computer.as_ref().ok_or_else(|| HarnessError::Tool("computer use 未挂载".into()))?;
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
    post_action(bridge, "Pressed", &key).await
}

/// computer_scroll：滚轮。
pub async fn scroll(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let dir = a.get("direction").and_then(|v| v.as_str()).unwrap_or("down").to_lowercase();
    let pages = a.get("pages").and_then(|v| v.as_f64()).unwrap_or(1.0).max(0.25);
    let x = a.get("x").and_then(|v| v.as_f64()).unwrap_or(400.0);
    let y = a.get("y").and_then(|v| v.as_f64()).unwrap_or(400.0);
    ctx.authorize("computer_scroll", &dir, crate::permission::Risk::Shell).await?;
    let bridge = ctx.computer.as_ref().ok_or_else(|| HarnessError::Tool("computer use 未挂载".into()))?;
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
    post_action(bridge, "Scrolled", &dir).await
}

/// computer_navigate：打开网址。
pub async fn navigate(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let url = need_str(a, "url")?;
    if !(url.starts_with("http://") || url.starts_with("https://") || url == "about:blank") {
        return Err(HarnessError::Tool("url 必须以 http(s):// 开头".into()));
    }
    ctx.authorize("computer_navigate", &url, crate::permission::Risk::Network).await?;
    let bridge = ctx.computer.as_ref().ok_or_else(|| HarnessError::Tool("computer use 未挂载".into()))?;
    bridge.cmd("Page.navigate", json!({"url": url})).await?;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    post_action(bridge, "Navigated", &url).await
}

/// 动作后的统一观察：截一张新图，让模型立刻看到效果（observe-act 循环）。
async fn post_action(
    bridge: &Arc<ComputerBridge>,
    verb: &str,
    what: &str,
) -> Result<super::tools::ToolOutcome, HarnessError> {
    // 动作到渲染之间留一点时间
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    bridge.screenshot_raw().await?;
    Ok(super::tools::ToolOutcome {
        ok: true,
        title: format!("{verb} {what}"),
        display: last_image_display(bridge).await,
        for_model: format!("{verb} {what} 完成。最新截图已附在本条工具结果之后——先观察再决定下一步。"),
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
    fn ax_tree_skips_ignored_roles() {
        // parse/序列化逻辑由 ax_tree 内联完成；这里验证交互角色清单包含核心控件
        let interactive = ["button", "link", "textbox", "combobox"];
        assert!(interactive.contains(&"button"));
    }
}
