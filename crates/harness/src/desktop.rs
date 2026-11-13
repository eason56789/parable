//! Desktop use — OS 级屏幕观察 + 真实输入注入（Windows）。
//!
//! 与浏览器 computer use 同一套设计语言，扩展到整个桌面：
//! - **观察**：GDI `BitBlt` 抓屏（物理像素，DPI aware，多显示器虚拟屏）+ UIA
//!   （Windows UI Automation）ControlView 树 → 带 `[rN]` 稳定引用 + 屏幕坐标
//!   的紧凑树。
//! - **动作**：`SendInput` 注入真实鼠标/键盘（受控浏览器之外的一切窗口）；
//!   Unicode 文本走 `KEYEVENTF_UNICODE`（中日韩直输，不碰剪贴板）。
//! - **ref 语义**：与浏览器版一致——快照分配 ref，动作发生时才实时校验
//!   （缓存矩形中心 + 「点上的根窗口 == 缓存根窗口」双验证）；失效明确报错
//!   引导重新快照，绝不猜。
//!
//! 线程模型：UIA 是 STA COM，专设一个工作线程独占所有 COM 调用（初始化一次
//! CUIAutomation，命令循环），异步侧只通过 channel + oneshot 往返，绝不阻塞
//! tokio 运行时。
//!
//! 纯逻辑（坐标变换、键位映射、控件类型命名、树渲染）与 COM 胶水分离，前者
//! 有单元测试覆盖。

use crate::error::HarnessError;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

#[cfg(windows)]
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
#[cfg(windows)]
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
#[cfg(windows)]
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, CAPTUREBLT, DIB_RGB_COLORS, SRCCOPY,
};
#[cfg(windows)]
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
#[cfg(windows)]
use windows::Win32::UI::Accessibility::{
    IUIAutomation, IUIAutomation2, IUIAutomationElement, IUIAutomationTreeWalker,
    IUIAutomationValuePattern,
    CUIAutomation, UIA_ButtonControlTypeId, UIA_CalendarControlTypeId, UIA_CheckBoxControlTypeId,
    UIA_ComboBoxControlTypeId, UIA_DataItemControlTypeId,
    UIA_DocumentControlTypeId, UIA_EditControlTypeId, UIA_GroupControlTypeId,
    UIA_HeaderControlTypeId, UIA_HyperlinkControlTypeId, UIA_ImageControlTypeId,
    UIA_ListItemControlTypeId, UIA_ListControlTypeId, UIA_MenuBarControlTypeId,
    UIA_MenuControlTypeId, UIA_MenuItemControlTypeId, UIA_PaneControlTypeId,
    UIA_ProgressBarControlTypeId, UIA_RadioButtonControlTypeId, UIA_ScrollBarControlTypeId,
    UIA_SeparatorControlTypeId, UIA_SliderControlTypeId,
    UIA_SpinnerControlTypeId, UIA_SplitButtonControlTypeId, UIA_StatusBarControlTypeId,
    UIA_TableControlTypeId, UIA_TabControlTypeId, UIA_TabItemControlTypeId, UIA_TextControlTypeId,
    UIA_TitleBarControlTypeId, UIA_ToolBarControlTypeId, UIA_ToolTipControlTypeId,
    UIA_TreeControlTypeId, UIA_TreeItemControlTypeId, UIA_ValuePatternId, UIA_WindowControlTypeId,
    UIA_CONTROLTYPE_ID,
};
#[cfg(windows)]
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSE_EVENT_FLAGS,
    MOUSEINPUT, VIRTUAL_KEY, VK_MENU, VK_RETURN, VK_TAB,
};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetCursorPos, GetForegroundWindow, GetSystemMetrics,
    GetWindowTextW, GetWindowRect, IsIconic, IsWindow, IsWindowVisible,
    SendMessageTimeoutW, SetForegroundWindow, ShowWindow, WindowFromPoint, GA_ROOT,
    SMTO_ABORTIFHUNG, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, WHEEL_DELTA, WM_CLOSE,
};

// ============================================================ bridge facade

/// One captured screenshot (base64 PNG) waiting for the model context.
#[derive(Debug, Clone)]
pub struct PendingImage {
    pub media_type: String,
    pub data: String,
}

/// 一个快照节点的纯数据形态（渲染与测试都不依赖 COM）。
/// rect = (left, top, width, height)，物理屏幕像素。
#[derive(Debug, Clone)]
pub struct SnapNode {
    pub role: String,
    pub name: String,
    pub value: String,
    pub rect: Option<(i32, i32, i32, i32)>,
    pub focused: bool,
    pub disabled: bool,
    pub depth: usize,
    pub ref_no: Option<i64>,
}

/// 快照元信息的纯数据形态。
#[derive(Debug, Clone)]
pub struct SnapMeta {
    pub screen_w: i32,
    pub screen_h: i32,
    pub scale_pct: i32,
    pub scope: String,
}

/// 枚举窗口的一条记录。rect = (left, top, width, height)。
#[derive(Debug, Clone)]
pub struct WinInfo {
    pub hwnd: isize,
    pub title: String,
    pub rect: (i32, i32, i32, i32),
    pub foreground: bool,
}

pub enum Cmd {
    Snapshot { scope: Option<String> },
    Screenshot { region: Option<[i32; 4]>, zoom: bool },
    Extract { hwnd: Option<isize> },
    Click { target: RefOrPoint, button: String, clicks: i32 },
    Type { text: String },
    Key { key: String },
    Scroll { direction: String, notches: f64, target: Option<RefOrPoint> },
    Drag { from: RefOrPoint, to: RefOrPoint },
    WindowList,
    WindowShow { hwnd: isize, action: &'static str },
    WindowClose { hwnd: isize },
}

#[derive(Debug, Clone)]
pub enum RefOrPoint {
    Ref(i64),
    Point { x: f64, y: f64 },
}

enum JobReply {
    Text(String),
    Tree { text: String, image: PendingImage },
    Image { image: PendingImage, title: String },
    Windows(Vec<WinInfo>),
}

struct Job {
    cmd: Cmd,
    reply: tokio::sync::oneshot::Sender<Result<JobReply, HarnessError>>,
}

/// 桌面桥：异步侧句柄。所有实际工作在专用 STA 线程上。
pub struct DesktopBridge {
    tx: Sender<Job>,
}

impl DesktopBridge {
    pub fn new() -> Self {
        dpi_aware_once();
        let (tx, rx) = channel::<Job>();
        std::thread::Builder::new()
            .name("parable-desktop".into())
            .spawn(move || worker_main(rx))
            .expect("spawn desktop worker");
        Self { tx }
    }

    async fn call(&self, cmd: Cmd, timeout_ms: u64) -> Result<JobReply, HarnessError> {
        let (rtx, rrx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Job { cmd, reply: rtx })
            .map_err(|_| HarnessError::Tool("desktop 工作线程已退出".into()))?;
        match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), rrx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(HarnessError::Tool("desktop 工作线程丢弃了任务".into())),
            Err(_) => Err(HarnessError::Tool(
                "desktop 操作超时（UIA 可能被目标窗口阻塞）".into(),
            )),
        }
    }

    pub async fn snapshot_text(&self, scope: Option<String>) -> Result<String, HarnessError> {
        match self.call(Cmd::Snapshot { scope }, 40_000).await? {
            JobReply::Tree { text, .. } => Ok(text),
            _ => Err(HarnessError::Tool("desktop snapshot 返回异常".into())),
        }
    }

    pub async fn screenshot_png(&self) -> Result<String, HarnessError> {
        match self.call(Cmd::Screenshot { region: None, zoom: false }, 20_000).await? {
            JobReply::Image { image, .. } => Ok(image.data),
            _ => Err(HarnessError::Tool("desktop screenshot 返回异常".into())),
        }
    }
}

static DPI_ONCE: std::sync::Once = std::sync::Once::new();

/// 进程级 Per-Monitor V2 DPI awareness：让 GetSystemMetrics / GetWindowRect /
/// WindowFromPoint / SendInput 坐标全部处于物理像素空间（UIA 本来就是）。
/// 若进程早已设置（Tauri 壳），失败被忽略——两种情况下坐标空间都一致。
fn dpi_aware_once() {
    DPI_ONCE.call_once(|| {
        #[cfg(windows)]
        unsafe {
            use windows::Win32::UI::HiDpi::*;
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
    });
}

// ============================================================ worker thread

#[cfg(windows)]
struct Worker {
    uia: Option<IUIAutomation>,
    walker: Option<IUIAutomationTreeWalker>,
    refs: HashMap<i64, DRef>,
    next_ref: i64,
}

#[cfg(not(windows))]
struct Worker;

/// 一个 ref 的解析依据：快照时的控件身份 + 根窗口 + 屏幕矩形 (l,t,w,h)。
#[derive(Debug, Clone)]
struct DRef {
    hwnd: isize,
    ctrl: String,
    name: String,
    rect: (i32, i32, i32, i32),
}

fn worker_main(rx: Receiver<Job>) {
    #[cfg(windows)]
    {
        // COM STA：UIA 要求单线程单元
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let uia: windows::core::Result<IUIAutomation> =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) };
        // UIA 熔断：病理 provider（Electron/自绘框架）可能把单次调用拖到十几秒。
        // 连接 2s / 事务 5s，超时返回错误而不是挂死工作线程（Windows-MCP 实测经验）。
        let (uia, walker) = match uia {
            Ok(u) => {
                if let Ok(u2) = u.cast::<IUIAutomation2>() {
                    unsafe {
                        let _ = u2.SetConnectionTimeout(2000);
                        let _ = u2.SetTransactionTimeout(5000);
                    }
                }
                let w = unsafe { u.ControlViewWalker() }.ok();
                (Some(u), w)
            }
            Err(_) => (None, None),
        };
        let mut worker = Worker { uia, walker, refs: HashMap::new(), next_ref: 1 };
        for job in rx {
            let result = worker.run(job.cmd);
            let _ = job.reply.send(result);
        }
    }
    #[cfg(not(windows))]
    {
        let mut worker = Worker;
        for job in rx {
            let _ = job.reply.send(worker.run(job.cmd));
        }
    }
}

impl Worker {
    #[cfg(windows)]
    fn run(&mut self, cmd: Cmd) -> Result<JobReply, HarnessError> {
        match cmd {
            Cmd::Snapshot { scope } => self.snapshot(scope),
            Cmd::Screenshot { region, zoom } => self.screenshot(region, zoom),
            Cmd::Extract { hwnd } => self.extract(hwnd),
            Cmd::Click { target, button, clicks } => self.click(target, &button, clicks),
            Cmd::Type { text } => self.type_text(&text),
            Cmd::Key { key } => self.key(&key),
            Cmd::Scroll { direction, notches, target } => self.scroll(&direction, notches, target),
            Cmd::Drag { from, to } => self.drag(from, to),
            Cmd::WindowList => self.window_list(),
            Cmd::WindowShow { hwnd, action } => self.window_show(hwnd, action),
            Cmd::WindowClose { hwnd } => self.window_close(hwnd),
        }
    }

    #[cfg(not(windows))]
    fn run(&mut self, _cmd: Cmd) -> Result<JobReply, HarnessError> {
        Err(HarnessError::Tool("desktop use 仅支持 Windows".into()))
    }
}

// ============================================================ 纯逻辑（可测）

/// ControlType id → 名称（无法识别时给 custom）。
#[cfg(windows)]
fn ctrl_type_name(id: i32) -> &'static str {
    match id {
        x if x == UIA_ButtonControlTypeId.0 => "button",
        x if x == UIA_CalendarControlTypeId.0 => "calendar",
        x if x == UIA_CheckBoxControlTypeId.0 => "checkbox",
        x if x == UIA_ComboBoxControlTypeId.0 => "combobox",
        x if x == UIA_EditControlTypeId.0 => "edit",
        x if x == UIA_HyperlinkControlTypeId.0 => "link",
        x if x == UIA_ImageControlTypeId.0 => "image",
        x if x == UIA_ListItemControlTypeId.0 => "listitem",
        x if x == UIA_ListControlTypeId.0 => "list",
        x if x == UIA_MenuControlTypeId.0 => "menu",
        x if x == UIA_MenuBarControlTypeId.0 => "menubar",
        x if x == UIA_MenuItemControlTypeId.0 => "menuitem",
        x if x == UIA_ProgressBarControlTypeId.0 => "progress",
        x if x == UIA_RadioButtonControlTypeId.0 => "radio",
        x if x == UIA_ScrollBarControlTypeId.0 => "scrollbar",
        x if x == UIA_SliderControlTypeId.0 => "slider",
        x if x == UIA_SpinnerControlTypeId.0 => "spinner",
        x if x == UIA_StatusBarControlTypeId.0 => "statusbar",
        x if x == UIA_TabControlTypeId.0 => "tablist",
        x if x == UIA_TabItemControlTypeId.0 => "tab",
        x if x == UIA_TextControlTypeId.0 => "text",
        x if x == UIA_ToolBarControlTypeId.0 => "toolbar",
        x if x == UIA_ToolTipControlTypeId.0 => "tooltip",
        x if x == UIA_TreeControlTypeId.0 => "tree",
        x if x == UIA_TreeItemControlTypeId.0 => "treeitem",
        x if x == UIA_DocumentControlTypeId.0 => "document",
        x if x == UIA_WindowControlTypeId.0 => "window",
        x if x == UIA_GroupControlTypeId.0 => "group",
        x if x == UIA_PaneControlTypeId.0 => "pane",
        x if x == UIA_HeaderControlTypeId.0 => "header",
        x if x == UIA_DataItemControlTypeId.0 => "dataitem",
        x if x == UIA_TableControlTypeId.0 => "table",
        x if x == UIA_TitleBarControlTypeId.0 => "titlebar",
        x if x == UIA_SplitButtonControlTypeId.0 => "splitbutton",
        x if x == UIA_SeparatorControlTypeId.0 => "separator",
        _ => "custom",
    }
}

/// UIA ControlType → 可交互判定（IsKeyboardFocusable 之外的白名单）。
#[cfg(windows)]
fn is_interactive_ct(id: i32) -> bool {
    id == UIA_ButtonControlTypeId.0
        || id == UIA_CheckBoxControlTypeId.0
        || id == UIA_RadioButtonControlTypeId.0
        || id == UIA_HyperlinkControlTypeId.0
        || id == UIA_MenuItemControlTypeId.0
        || id == UIA_TabItemControlTypeId.0
        || id == UIA_ComboBoxControlTypeId.0
        || id == UIA_SliderControlTypeId.0
        || id == UIA_SplitButtonControlTypeId.0
        || id == UIA_ListItemControlTypeId.0
        || id == UIA_TreeItemControlTypeId.0
        || id == UIA_DataItemControlTypeId.0
        || id == UIA_EditControlTypeId.0
}

/// 有 ValuePattern 值的控件。
#[cfg(windows)]
fn is_valued_ct(id: i32) -> bool {
    id == UIA_EditControlTypeId.0 || id == UIA_DocumentControlTypeId.0 || id == UIA_ComboBoxControlTypeId.0
}

/// extract 关注的内容控件。
#[cfg(windows)]
fn is_content_ct(id: i32) -> bool {
    id == UIA_DocumentControlTypeId.0
        || id == UIA_TextControlTypeId.0
        || id == UIA_EditControlTypeId.0
        || id == UIA_ListItemControlTypeId.0
        || id == UIA_TreeItemControlTypeId.0
        || id == UIA_DataItemControlTypeId.0
        || id == UIA_HeaderControlTypeId.0
        || id == UIA_GroupControlTypeId.0
}

/// 键名 → VK 码（含单字母/数字/F1-F24；组合键的修饰键走 mod_key_map）。
fn desk_key_map(name: &str) -> Option<u16> {
    let lower = name.to_ascii_lowercase();
    let b = lower.as_bytes();
    match lower.as_str() {
        "enter" | "return" => Some(0x0D),
        "tab" => Some(0x09),
        "escape" | "esc" => Some(0x1B),
        "backspace" => Some(0x08),
        "delete" | "del" => Some(0x2E),
        "space" => Some(0x20),
        "pageup" => Some(0x21),
        "pagedown" => Some(0x22),
        "end" => Some(0x23),
        "home" => Some(0x24),
        "insert" => Some(0x2D),
        "printscreen" => Some(0x2C),
        "arrowleft" | "left" => Some(0x25),
        "arrowup" | "up" => Some(0x26),
        "arrowright" | "right" => Some(0x27),
        "arrowdown" | "down" => Some(0x28),
        _ if b.len() == 1 && b[0].is_ascii_lowercase() => Some((b[0] - b'a') as u16 + 0x41),
        _ if b.len() == 1 && b[0].is_ascii_digit() => Some((b[0] - b'0') as u16 + 0x30),
        _ if b.len() >= 2 && b[0] == b'f' && lower[1..].parse::<u16>().is_ok() => {
            let n: u16 = lower[1..].parse().ok()?;
            if (1..=24).contains(&n) {
                Some(0x70 + n - 1) // VK_F1 = 0x70
            } else {
                None
            }
        }
        _ => None,
    }
}

/// 修饰键名 → VK 码。
fn mod_key_map(name: &str) -> Option<u16> {
    match name.to_ascii_lowercase().as_str() {
        "control" | "ctrl" => Some(0x11),
        "alt" => Some(0x12),
        "shift" => Some(0x10),
        "meta" | "win" | "cmd" => Some(0x5B),
        _ => None,
    }
}

/// 物理屏幕坐标 → SendInput 归一化坐标（相对虚拟屏 0..=65535，越界钳制）。
#[cfg(windows)]
fn to_absolute(x: f64, y: f64) -> (i32, i32) {
    let (vx, vy, vw, vh) = virtual_screen();
    let (vw, vh) = (vw.max(1) as f64, vh.max(1) as f64);
    let nx = ((x - vx as f64) / vw * 65535.0).round().clamp(0.0, 65535.0) as i32;
    let ny = ((y - vy as f64) / vh * 65535.0).round().clamp(0.0, 65535.0) as i32;
    (nx, ny)
}

/// 快照树渲染（纯函数）。
fn render_snapshot(meta: &SnapMeta, nodes: &[SnapNode]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "desktop: {}×{} (虚拟屏, 缩放 {}%)\n",
        meta.screen_w, meta.screen_h, meta.scale_pct
    ));
    out.push_str(&format!("范围: {}\n", meta.scope));
    out.push_str("UIA 树（[rN] 引用 + 物理屏幕坐标 (l,t 宽×高)；操作优先用 ref，坐标与截图像素 1:1）:\n");
    for n in nodes {
        let pos = match n.rect {
            Some((l, t, w, h)) => format!(" @({l},{t} {w}×{h})"),
            None => " @(不可见)".to_string(),
        };
        let focus = if n.focused { " [焦点]" } else { "" };
        let dis = if n.disabled { " [禁用]" } else { "" };
        let val = if n.value.is_empty() {
            String::new()
        } else {
            format!(" value={:?}", n.value)
        };
        match n.ref_no {
            Some(r) => out.push_str(&format!(
                "{}- [r{r}] {} \"{}\"{pos}{focus}{dis}{val}\n",
                "  ".repeat(n.depth.min(10)),
                n.role,
                n.name
            )),
            None => out.push_str(&format!(
                "{}- {} \"{}\"{pos}{focus}{dis}{val}\n",
                "  ".repeat(n.depth.min(10)),
                n.role,
                n.name
            )),
        }
    }
    if nodes.is_empty() {
        out.push_str("(空)\n");
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// 标准 base64（无依赖实现）。
fn b64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

fn encode_png(rgba: &[u8], w: u32, h: u32) -> Result<String, HarnessError> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut enc = png::Encoder::new(&mut buf, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc
            .write_header()
            .map_err(|e| HarnessError::Tool(format!("PNG 编码失败: {e}")))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| HarnessError::Tool(format!("PNG 写入失败: {e}")))?;
    }
    Ok(b64_encode(&buf.into_inner()))
}

// ============================================================ Windows 胶水

#[cfg(windows)]
use windows::core::Interface as _;
#[cfg(windows)]
fn uia_err(e: windows::core::Error) -> HarnessError {
    HarnessError::Tool(format!("UIA 调用失败: {e}"))
}

/// 虚拟桌面边界（物理像素）：(x, y, w, h)。
#[cfg(windows)]
fn virtual_screen() -> (i32, i32, i32, i32) {
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);
        (x, y, w, h)
    }
}

#[cfg(windows)]
fn root_hwnd_of(hwnd: isize) -> isize {
    if hwnd == 0 {
        return 0;
    }
    unsafe {
        let root = GetAncestor(HWND(hwnd as *mut _), GA_ROOT);
        if root.0.is_null() {
            hwnd
        } else {
            root.0 as isize
        }
    }
}

#[cfg(windows)]
fn window_title(h: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

#[cfg(windows)]
fn cursor_pos() -> (f64, f64) {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    (p.x as f64, p.y as f64)
}

#[cfg(windows)]
fn mouse_input(ax: i32, ay: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: ax,
                dy: ay,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(windows)]
fn key_input(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// 一对 VK down/up 事件。
#[cfg(windows)]
fn vk_events(vk: u16) -> Vec<INPUT> {
    vec![key_input(vk, false), key_input(vk, true)]
}

/// Unicode 字符的一对注入事件（wScan = 码点，系统级直输，不经过 IME 状态）。
#[cfg(windows)]
fn unicode_events(c: char) -> Vec<INPUT> {
    let scan = c as u16;
    vec![
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: scan,
                    dwFlags: KEYEVENTF_UNICODE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: scan,
                    dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
    ]
}

#[cfg(windows)]
fn send_inputs(evs: &[INPUT]) -> Result<(), HarnessError> {
    if evs.is_empty() {
        return Ok(());
    }
    let sent = unsafe { SendInput(evs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != evs.len() {
        return Err(HarnessError::Tool(format!(
            "SendInput 只注入了 {sent}/{} 个事件（可能被前台高权限窗口挡下）",
            evs.len()
        )));
    }
    Ok(())
}

/// 抓取屏幕区域（物理像素）→ PNG base64。region 缺省 = 整个虚拟桌面。
/// 宽度超过 1568px 时双线性降采样（Claude 视觉上限 1568；超限会让 API 静默
/// 缩放，模型按截图目测的坐标就会系统性偏移——Anthropic 官方点名的头号坑）。
/// 返回 (png, 图像宽, 图像高, 物理宽, 物理高)。
#[cfg(windows)]
fn capture_screen(region: Option<(i32, i32, i32, i32)>) -> Result<(String, i32, i32, i32, i32), HarnessError> {
    let (vx, vy, vw, vh) = virtual_screen();
    let (x, y, w, h) = region.unwrap_or((vx, vy, vw, vh));
    let (w, h) = (w.max(1), h.max(1));
    unsafe {
        let hdc = GetDC(None);
        if hdc.0.is_null() {
            return Err(HarnessError::Tool("GetDC 失败".into()));
        }
        let mem = CreateCompatibleDC(Some(hdc));
        if mem.0.is_null() {
            let _ = ReleaseDC(None, hdc);
            return Err(HarnessError::Tool("CreateCompatibleDC 失败".into()));
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            ..Default::default()
        };
        let mut ppv: *mut std::ffi::c_void = std::ptr::null_mut();
        let hbmp = CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut ppv, None, 0)
            .map_err(|e| HarnessError::Tool(format!("CreateDIBSection 失败: {e}")))?;
        let old = SelectObject(mem, hbmp.into());
        let blit = BitBlt(mem, 0, 0, w, h, Some(hdc), x, y, SRCCOPY | CAPTUREBLT);
        let out = if blit.is_ok() && !ppv.is_null() {
            let len = (w as usize) * (h as usize) * 4;
            let src = std::slice::from_raw_parts(ppv as *const u8, len);
            let mut rgba = src.to_vec();
            for px in rgba.chunks_exact_mut(4) {
                px.swap(0, 2); // BGRA → RGBA
                if px[3] == 0 {
                    px[3] = 255;
                }
            }
            let (iw, ih, data) = maybe_downscale(&rgba, w, h);
            encode_png(&data, iw as u32, ih as u32).map(|s| (s, iw, ih, w, h))
        } else {
            Err(HarnessError::Tool("BitBlt 抓屏失败（可能被安全策略拦截）".into()))
        };
        let _ = SelectObject(mem, old);
        let _ = DeleteObject(hbmp.into());
        let _ = DeleteDC(mem);
        let _ = ReleaseDC(None, hdc);
        out
    }
}

/// RGBA 双线性降采样到 ≤1568 宽（只缩不放）。返回 (宽, 高, 数据)。
fn maybe_downscale(rgba: &[u8], w: i32, h: i32) -> (i32, i32, Vec<u8>) {
    const MAX_W: i32 = 1568;
    if w <= MAX_W {
        return (w, h, rgba.to_vec());
    }
    let nw = MAX_W;
    let nh = ((h as f64 * MAX_W as f64 / w as f64).round() as i32).max(1);
    let (sw, sh) = (w as usize, h as usize);
    let (dw, dh) = (nw as usize, nh as usize);
    let mut out = vec![0u8; dw * dh * 4];
    for dy in 0..dh {
        let fy = (dy as f64 + 0.5) * sh as f64 / dh as f64 - 0.5;
        let y0 = fy.floor().max(0.0) as usize;
        let y1 = (y0 + 1).min(sh - 1);
        let ty = fy - fy.floor();
        for dx in 0..dw {
            let fx = (dx as f64 + 0.5) * sw as f64 / dw as f64 - 0.5;
            let x0 = fx.floor().max(0.0) as usize;
            let x1 = (x0 + 1).min(sw - 1);
            let tx = fx - fx.floor();
            for c in 0..4 {
                let p00 = rgba[(y0 * sw + x0) * 4 + c] as f64;
                let p01 = rgba[(y0 * sw + x1) * 4 + c] as f64;
                let p10 = rgba[(y1 * sw + x0) * 4 + c] as f64;
                let p11 = rgba[(y1 * sw + x1) * 4 + c] as f64;
                let v = p00 * (1.0 - tx) * (1.0 - ty)
                    + p01 * tx * (1.0 - ty)
                    + p10 * (1.0 - tx) * ty
                    + p11 * tx * ty;
                out[(dy * dw + dx) * 4 + c] = v.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    (nw, nh, out)
}

/// 枚举可见顶层窗口（跳过不可见、cloak 的 UWP 宿主与无标题窗口）。
#[cfg(windows)]
fn enum_windows_info() -> Result<Vec<WinInfo>, HarnessError> {
    let mut out: Vec<WinInfo> = Vec::new();
    let fg = unsafe { GetForegroundWindow() };
    unsafe {
        EnumWindows(Some(enum_proc), LPARAM(&mut out as *mut _ as isize))
            .map_err(|e| HarnessError::Tool(format!("EnumWindows 失败: {e}")))?;
    }
    for w in out.iter_mut() {
        w.foreground = w.hwnd == fg.0 as isize;
    }
    return Ok(out);

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        use windows::core::BOOL;
        let out = unsafe { &mut *(lparam.0 as *mut Vec<WinInfo>) };
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        // cloak 过的 UWP 宿主窗口（开始菜单/通知中心等）跳过
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        );
        if cloaked != 0 {
            return BOOL(1);
        }
        let title = window_title(hwnd);
        if title.trim().is_empty() {
            return BOOL(1);
        }
        let mut r = RECT::default();
        if GetWindowRect(hwnd, &mut r).is_err() {
            return BOOL(1);
        }
        if r.right - r.left <= 0 || r.bottom - r.top <= 0 {
            return BOOL(1);
        }
        out.push(WinInfo {
            hwnd: hwnd.0 as isize,
            title: truncate(&title, 80),
            rect: (r.left, r.top, r.right - r.left, r.bottom - r.top),
            foreground: false,
        });
        BOOL(1)
    }
}

// ============================================================ COM 侧实现

#[cfg(windows)]
impl Worker {
    fn ensure_uia(&self) -> Result<(IUIAutomation, IUIAutomationTreeWalker), HarnessError> {
        match (&self.uia, &self.walker) {
            (Some(u), Some(w)) => Ok((u.clone(), w.clone())),
            _ => Err(HarnessError::Tool("UIA 初始化失败（COM 不可用？）".into())),
        }
    }

    // ------------------------------------------------------- 观察：快照

    fn snapshot(&mut self, scope: Option<String>) -> Result<JobReply, HarnessError> {
        let (uia, walker) = self.ensure_uia()?;
        unsafe {
            let (_, _, vw, vh) = virtual_screen();
            let dpi = windows::Win32::UI::HiDpi::GetDpiForSystem();
            let scope = scope.unwrap_or_else(|| "focused".into());

            // 起始元素：focused = 前台窗口；all = 整个桌面
            let (start, scope_label, hwnd_hint) = if scope == "all" {
                (uia.GetRootElement().map_err(uia_err)?, "all windows".to_string(), 0isize)
            } else {
                let hwnd = GetForegroundWindow();
                if hwnd.0.is_null() {
                    return Err(HarnessError::Tool("没有前台窗口".into()));
                }
                let title = window_title(hwnd);
                (
                    uia.ElementFromHandle(hwnd).map_err(uia_err)?,
                    format!("window: {}", truncate(&title, 80)),
                    hwnd.0 as isize,
                )
            };

            self.refs.clear();
            self.next_ref = 1;
            let mut nodes: Vec<SnapNode> = Vec::new();
            let mut budget = Budget { nodes: 350, refs: 0 };
            self.walk(&walker, &start, 0, hwnd_hint, &mut nodes, &mut budget);

            // 截屏：整个虚拟桌面（与树坐标同一物理像素空间；>1568 宽会降采样）
            let (image, img_w, img_h, phys_w, phys_h) = capture_screen(None).unwrap_or_else(|_| {
                (
                    String::new(),
                    vw,
                    vh,
                    vw,
                    vh,
                )
            });

            let mut text = render_snapshot(
                &SnapMeta {
                    screen_w: vw,
                    screen_h: vh,
                    scale_pct: (dpi * 100 / 96) as i32,
                    scope: scope_label,
                },
                &nodes,
            );
            if img_w != phys_w {
                let k = phys_w as f64 / img_w as f64;
                text.push_str(&format!(
                    "注意：截图已缩放至 {img_w}×{img_h}（物理 {phys_w}×{phys_h}，缩放 {k:.3}）。\
                     树中坐标是【物理像素】；若按截图目测坐标操作，请先乘以 {k:.3}（优先用 ref）。\n"
                ));
            }
            Ok(JobReply::Tree {
                text,
                image: PendingImage { media_type: "image/png".into(), data: image },
            })
        }
    }

    /// UIA ControlView DFS。交互节点分配 ref；有名字的内容节点保留为路标。
    fn walk(
        &mut self,
        walker: &IUIAutomationTreeWalker,
        elem: &IUIAutomationElement,
        depth: usize,
        hwnd_hint: isize,
        out: &mut Vec<SnapNode>,
        budget: &mut Budget,
    ) {
        if out.len() >= 350 || depth > 10 || budget.nodes == 0 {
            return;
        }
        unsafe {
            let name = elem.CurrentName().map(|s| s.to_string()).unwrap_or_default();
            let ct = elem.CurrentControlType().unwrap_or(UIA_CONTROLTYPE_ID(0));
            let role = ctrl_type_name(ct.0);
            let rect = elem
                .CurrentBoundingRectangle()
                .ok()
                .map(|r: RECT| (r.left, r.top, r.right - r.left, r.bottom - r.top))
                .filter(|r| r.2 > 0 && r.3 > 0);
            let focused = elem.CurrentHasKeyboardFocus().map(|b| b.as_bool()).unwrap_or(false);
            let offscreen = elem.CurrentIsOffscreen().map(|b| b.as_bool()).unwrap_or(false);
            let focusable = elem.CurrentIsKeyboardFocusable().map(|b| b.as_bool()).unwrap_or(false);
            let enabled = elem.CurrentIsEnabled().map(|b| b.as_bool()).unwrap_or(true);

            // 窗口归属：自己的 NativeWindowHandle，否则继承上层窗口提示
            let own_hwnd = elem
                .CurrentNativeWindowHandle()
                .ok()
                .map(|h| h.0 as isize)
                .filter(|h| *h != 0);
            let hwnd = root_hwnd_of(own_hwnd.unwrap_or(hwnd_hint));

            let interactive = focusable || is_interactive_ct(ct.0);
            // 禁用控件不分配 ref（点了也没用），但保留在树里作上下文
            let ref_no = if interactive && enabled && budget.refs < 150 && rect.is_some() {
                let r = self.next_ref;
                self.next_ref += 1;
                budget.refs += 1;
                self.refs.insert(
                    r,
                    DRef {
                        hwnd,
                        ctrl: role.to_string(),
                        name: name.clone(),
                        rect: rect.unwrap(),
                    },
                );
                Some(r)
            } else {
                None
            };

            // value（Edit/Document/ComboBox 走 ValuePattern）
            let value = if is_valued_ct(ct.0) {
                elem.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                    .ok()
                    .and_then(|p| p.CurrentValue().ok())
                    .map(|s| s.to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            };

            out.push(SnapNode {
                role: role.to_string(),
                name: truncate(&name, 80),
                value: truncate(&value, 60),
                rect: if offscreen { None } else { rect },
                focused,
                disabled: !enabled,
                depth,
                ref_no,
            });

            budget.nodes -= 1;
            let mut child = walker.GetFirstChildElement(elem).ok();
            let mut guard = 0;
            while let Some(c) = child {
                guard += 1;
                if guard > 120 {
                    break;
                }
                self.walk(walker, &c, depth + 1, hwnd, out, budget);
                child = walker.GetNextSiblingElement(&c).ok();
            }
        }
    }

    // ------------------------------------------------------- 观察：提取

    /// 只读文本提取：窗口里有名字的内容节点 + 输入框值，按序输出。
    fn extract(&mut self, hwnd: Option<isize>) -> Result<JobReply, HarnessError> {
        let (uia, walker) = self.ensure_uia()?;
        unsafe {
            let elem = match hwnd.filter(|h| *h != 0) {
                Some(h) => uia.ElementFromHandle(HWND(h as *mut _)).map_err(uia_err)?,
                None => {
                    let fg = GetForegroundWindow();
                    if fg.0.is_null() {
                        return Err(HarnessError::Tool("没有前台窗口".into()));
                    }
                    uia.ElementFromHandle(fg).map_err(uia_err)?
                }
            };
            let title = elem.CurrentName().map(|s| s.to_string()).unwrap_or_default();
            let mut lines: Vec<String> = Vec::new();
            let mut budget = Budget { nodes: 400, refs: 0 };
            self.collect_text(&walker, &elem, &mut lines, &mut budget, 0);
            let mut out = format!("窗口内容提取（{}）:\n", truncate(&title, 80));
            out.push_str(&lines.join("\n"));
            Ok(JobReply::Text(out))
        }
    }

    fn collect_text(
        &mut self,
        walker: &IUIAutomationTreeWalker,
        elem: &IUIAutomationElement,
        lines: &mut Vec<String>,
        budget: &mut Budget,
        depth: usize,
    ) {
        if lines.len() >= 300 || depth > 12 || budget.nodes == 0 {
            return;
        }
        unsafe {
            let name = elem.CurrentName().map(|s| s.to_string()).unwrap_or_default();
            let ct = elem.CurrentControlType().unwrap_or(UIA_CONTROLTYPE_ID(0));
            let value = elem
                .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                .ok()
                .and_then(|p| p.CurrentValue().ok())
                .map(|s| s.to_string())
                .unwrap_or_default();
            if !name.is_empty() && is_content_ct(ct.0) {
                lines.push(truncate(&name, 160));
            }
            if !value.is_empty() && (ct.0 == UIA_EditControlTypeId.0 || ct.0 == UIA_DocumentControlTypeId.0) {
                lines.push(truncate(&value, 400));
            }
            budget.nodes -= 1;
            let mut child = walker.GetFirstChildElement(elem).ok();
            let mut guard = 0;
            while let Some(c) = child {
                guard += 1;
                if guard > 120 {
                    break;
                }
                self.collect_text(walker, &c, lines, budget, depth + 1);
                child = walker.GetNextSiblingElement(&c).ok();
            }
        }
    }

    // ------------------------------------------------------- 观察：截屏

    fn screenshot(&mut self, region: Option<[i32; 4]>, zoom: bool) -> Result<JobReply, HarnessError> {
        let reg = region.map(|[x, y, w, h]| (x, y, w, h));
        let (data, iw, ih, pw, ph) = capture_screen(reg)?;
        let title = if zoom {
            format!("Desktop zoom ({iw}×{ih} of {pw}×{ph} @2x)")
        } else if iw != pw {
            format!("Desktop screenshot ({iw}×{ih}, 物理 {pw}×{ph})")
        } else {
            format!("Desktop screenshot ({iw}×{ih})")
        };
        Ok(JobReply::Image {
            image: PendingImage { media_type: "image/png".into(), data },
            title,
        })
    }

    // ------------------------------------------------------- 动作：ref 解析

    /// ref / 坐标 → 物理屏幕坐标。ref 先验证后返回（双验证见 verify_ref）。
    fn resolve_target(&self, t: &RefOrPoint) -> Result<(f64, f64, String), HarnessError> {
        match t {
            RefOrPoint::Point { x, y } => Ok((*x, *y, format!("({x:.0},{y:.0})"))),
            RefOrPoint::Ref(r) => {
                let d = self.refs.get(r).cloned().ok_or_else(|| {
                    HarnessError::Tool(format!(
                        "ref r{r} 不在最近一次 desktop_snapshot 里——先快照再操作"
                    ))
                })?;
                let cx = d.rect.0 as f64 + d.rect.2 as f64 / 2.0;
                let cy = d.rect.1 as f64 + d.rect.3 as f64 / 2.0;
                self.verify_ref(cx, cy, &d, *r)?;
                Ok((cx, cy, format!("r{r} {}", d.ctrl)))
            }
        }
    }

    /// ref 双验证：① 中心点仍在缓存矩形内；② 点上的根窗口 == 缓存根窗口
    /// （缓存窗口未知时退化为只查 ①）。任一不满足都报「ref 已失效」。
    fn verify_ref(&self, x: f64, y: f64, d: &DRef, r: i64) -> Result<(), HarnessError> {
        let (left, top, w, h) = d.rect;
        let inside = x >= left as f64
            && x <= (left + w) as f64
            && y >= top as f64
            && y <= (top + h) as f64;
        if !inside {
            return Err(HarnessError::Tool(format!(
                "ref r{r}（{}:\"{}\"）已失效：缓存位置 ({left},{top}) 已不覆盖该点——界面可能变了，请重新 desktop_snapshot",
                d.ctrl, d.name
            )));
        }
        if d.hwnd != 0 {
            unsafe {
                let at = WindowFromPoint(POINT { x: x as i32, y: y as i32 });
                let root = GetAncestor(at, GA_ROOT);
                let root_id = if root.0.is_null() { 0 } else { root.0 as isize };
                if root_id != 0 && root_id != d.hwnd {
                    return Err(HarnessError::Tool(format!(
                        "ref r{r}（{}:\"{}\"）已失效：该位置现在是别的窗口——请重新 desktop_snapshot",
                        d.ctrl, d.name
                    )));
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------- 动作：鼠标

    /// 紧急停止 failsafe：光标停在虚拟屏左上角 10px 内时拒绝注入
    /// （PyAutoGUI 惯例——失控时人把鼠标甩到角落即可夺回控制权）。
    fn failsafe_check(&self) -> Result<(), HarnessError> {
        let (x, y) = cursor_pos();
        let (vx, vy, _, _) = virtual_screen();
        if x < vx as f64 + 10.0 && y < vy as f64 + 10.0 {
            return Err(HarnessError::Tool(
                "紧急停止触发：光标停在屏幕左上角。请移动鼠标后重试。".into(),
            ));
        }
        Ok(())
    }

    fn mouse_move_abs(&self, x: f64, y: f64) -> Result<(), HarnessError> {
        let (ax, ay) = to_absolute(x, y);
        // VIRTUALDESK：绝对坐标按整个虚拟屏（多显示器）归一化，而不是主屏
        send_inputs(&[mouse_input(
            ax,
            ay,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )])
    }

    fn click(&mut self, target: RefOrPoint, button: &str, clicks: i32) -> Result<JobReply, HarnessError> {
        self.failsafe_check()?;
        let (x, y, what) = self.resolve_target(&target)?;
        self.mouse_move_abs(x, y)?;
        let presses = clicks.clamp(1, 3);
        let (down, up) = match button {
            "right" => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
            "middle" => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
            _ => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        };
        let mut evs = Vec::new();
        for _ in 0..presses {
            evs.push(mouse_input(0, 0, 0, down));
            evs.push(mouse_input(0, 0, 0, up));
        }
        send_inputs(&evs)?;
        Ok(JobReply::Text(format!("Clicked {what} at ({x:.0},{y:.0})")))
    }

    fn drag(&mut self, from: RefOrPoint, to: RefOrPoint) -> Result<JobReply, HarnessError> {
        self.failsafe_check()?;
        let (x1, y1, f) = self.resolve_target(&from)?;
        let (x2, y2, t) = self.resolve_target(&to)?;
        self.mouse_move_abs(x1, y1)?;
        send_inputs(&[mouse_input(0, 0, 0, MOUSEEVENTF_LEFTDOWN)])?;
        let steps = 10;
        let mut evs = Vec::new();
        for i in 1..=steps {
            let k = i as f64 / steps as f64;
            let (ax, ay) = to_absolute(x1 + (x2 - x1) * k, y1 + (y2 - y1) * k);
            evs.push(mouse_input(ax, ay, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK));
        }
        send_inputs(&evs)?;
        send_inputs(&[mouse_input(0, 0, 0, MOUSEEVENTF_LEFTUP)])?;
        Ok(JobReply::Text(format!("Dragged {f} → {t}")))
    }

    fn scroll(
        &mut self,
        direction: &str,
        notches: f64,
        target: Option<RefOrPoint>,
    ) -> Result<JobReply, HarnessError> {
        self.failsafe_check()?;
        let (x, y) = match target {
            Some(t) => {
                let (x, y, _) = self.resolve_target(&t)?;
                (x, y)
            }
            None => cursor_pos(),
        };
        self.mouse_move_abs(x, y)?;
        let n = (notches.max(0.25) * WHEEL_DELTA as f64) as i32;
        let (flag, amount) = match direction {
            "up" => (MOUSEEVENTF_WHEEL, n),
            "down" => (MOUSEEVENTF_WHEEL, -n),
            "left" => (MOUSEEVENTF_HWHEEL, -n),
            "right" => (MOUSEEVENTF_HWHEEL, n),
            other => {
                return Err(HarnessError::Tool(format!(
                    "方向 `{other}` 无效 (up/down/left/right)"
                )))
            }
        };
        send_inputs(&[mouse_input(0, 0, amount as u32, flag)])?;
        Ok(JobReply::Text(format!(
            "Scrolled {direction} {notches} notches at ({x:.0},{y:.0})"
        )))
    }

    // ------------------------------------------------------- 动作：键盘

    fn type_text(&mut self, text: &str) -> Result<JobReply, HarnessError> {
        self.failsafe_check()?;
        if text.is_empty() {
            return Err(HarnessError::Tool("`text` 不能为空".into()));
        }
        let mut evs: Vec<INPUT> = Vec::new();
        for ch in text.chars() {
            match ch {
                '\n' => evs.extend(vk_events(VK_RETURN.0)),
                '\t' => evs.extend(vk_events(VK_TAB.0)),
                '\r' => {}
                c => evs.extend(unicode_events(c)),
            }
        }
        // 分批注入：小批次 + 间隔，规避慢速应用丢键（Windows-MCP 实测经验）
        for batch in evs.chunks(40) {
            send_inputs(batch)?;
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        Ok(JobReply::Text(format!("Typed {} chars", text.chars().count())))
    }

    fn key(&mut self, key: &str) -> Result<JobReply, HarnessError> {
        self.failsafe_check()?;
        let parts: Vec<&str> = key.split('+').map(|s| s.trim()).collect();
        let (mods, main) = parts.split_at(parts.len().saturating_sub(1));
        let main = main.first().copied().unwrap_or("");
        let vk = desk_key_map(main).ok_or_else(|| {
            HarnessError::Tool(format!(
                "未知按键 `{main}`。支持: Enter Tab Escape Backspace Delete Space Home End PageUp \
                 PageDown ArrowUp/Down/Left/Right F1-F24 字母/数字，及 Control/Alt/Shift/Meta 组合"
            ))
        })?;
        let mut evs = Vec::new();
        for m in mods {
            let mvk = mod_key_map(m)
                .ok_or_else(|| HarnessError::Tool(format!("未知修饰键 `{m}`")))?;
            evs.push(key_input(mvk, false));
        }
        evs.extend(vk_events(vk));
        for m in mods.iter().rev() {
            let mvk = mod_key_map(m)
                .ok_or_else(|| HarnessError::Tool(format!("未知修饰键 `{m}`")))?;
            evs.push(key_input(mvk, true));
        }
        send_inputs(&evs)?;
        Ok(JobReply::Text(format!("Pressed {key}")))
    }

    // ------------------------------------------------------- 窗口管理

    fn window_list(&mut self) -> Result<JobReply, HarnessError> {
        Ok(JobReply::Windows(enum_windows_info()?))
    }

    fn window_show(&mut self, hwnd: isize, action: &str) -> Result<JobReply, HarnessError> {
        let h = HWND(hwnd as *mut _);
        unsafe {
            if !IsWindow(Some(h)).as_bool() {
                return Err(HarnessError::Tool("窗口不存在（可能已关闭）".into()));
            }
            let title = window_title(h);
            match action {
                "minimize" => {
                    let _ = ShowWindow(h, SW_MINIMIZE);
                    Ok(JobReply::Text(format!("Minimized \"{title}\"")))
                }
                "maximize" => {
                    let _ = ShowWindow(h, SW_MAXIMIZE);
                    Ok(JobReply::Text(format!("Maximized \"{title}\"")))
                }
                _ => {
                    if IsIconic(h).as_bool() {
                        let _ = ShowWindow(h, SW_RESTORE);
                    }
                    // Windows 限制后台进程抢焦点：先按一下 ALT 解锁 SetForegroundWindow
                    send_inputs(&[key_input(VK_MENU.0, false), key_input(VK_MENU.0, true)])?;
                    let _ = SetForegroundWindow(h);
                    let fg = GetForegroundWindow();
                    if fg.0 != h.0 {
                        return Err(HarnessError::Tool(format!(
                            "SetForegroundWindow 被系统拒绝（Windows 防抢焦点策略）——窗口 \"{title}\" 未能前置"
                        )));
                    }
                    Ok(JobReply::Text(format!("Focused \"{title}\"")))
                }
            }
        }
    }

    fn window_close(&mut self, hwnd: isize) -> Result<JobReply, HarnessError> {
        let h = HWND(hwnd as *mut _);
        unsafe {
            if !IsWindow(Some(h)).as_bool() {
                return Err(HarnessError::Tool("窗口不存在（可能已关闭）".into()));
            }
            let title = window_title(h);
            let mut ret = 0usize;
            let sent = SendMessageTimeoutW(
                h,
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                3000,
                Some(&mut ret),
            );
            if sent.0 == 0 {
                return Err(HarnessError::Tool(format!("关闭 \"{title}\" 失败（窗口可能挂起）")));
            }
            Ok(JobReply::Text(format!(
                "Close requested for \"{title}\"（程序自己决定是否退出）"
            )))
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Budget {
    nodes: usize,
    refs: usize,
}

// ============================================================ tools

fn need_str(a: &Value, key: &str) -> Result<String, HarnessError> {
    a.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| HarnessError::Tool(format!("`{key}` is required")))
}

fn bridge_of(ctx: &super::tools::ToolCtx) -> Result<Arc<DesktopBridge>, HarnessError> {
    ctx.desktop
        .clone()
        .ok_or_else(|| HarnessError::Tool("desktop use 未挂载".into()))
}

/// JSON 参数 → RefOrPoint（ref 优先；x/y 兜底；都缺返回 None）。
fn parse_target(a: &Value) -> Result<Option<RefOrPoint>, HarnessError> {
    if let Some(r) = a.get("ref").and_then(|v| v.as_i64()) {
        return Ok(Some(RefOrPoint::Ref(r)));
    }
    match (a.get("x").and_then(|v| v.as_f64()), a.get("y").and_then(|v| v.as_f64())) {
        (Some(x), Some(y)) => Ok(Some(RefOrPoint::Point { x, y })),
        (None, None) => Ok(None),
        _ => Err(HarnessError::Tool("`x`/`y` 必须成对出现，或改用 `ref`".into())),
    }
}

fn require_target(a: &Value) -> Result<RefOrPoint, HarnessError> {
    parse_target(a)?.ok_or_else(|| HarnessError::Tool("`ref` 或 `x`/`y` 必填其一".into()))
}

/// 动作后统一观察：抓一张全屏让模型立刻看到效果（observe-act 循环）。
async fn post_action(
    bridge: &Arc<DesktopBridge>,
    verb: &str,
    what: &str,
) -> Result<super::tools::ToolOutcome, HarnessError> {
    let img = bridge.screenshot_png().await?;
    Ok(super::tools::ToolOutcome {
        ok: true,
        title: format!("{verb} {what}"),
        display: super::tools::DisplayKind::Image {
            media_type: "image/png".into(),
            data: img,
        },
        for_model: format!(
            "{verb} {what} 完成。最新桌面截图已附在本条工具结果之后——先观察再决定下一步。"
        ),
    })
}

/// desktop_snapshot：桌面 UIA 树 + 全屏截图（主力观察工具）。
pub async fn snapshot(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    ctx.authorize("desktop_snapshot", "screen", crate::permission::Risk::Shell).await?;
    let bridge = bridge_of(ctx)?;
    let scope = a.get("scope").and_then(|v| v.as_str()).map(|s| s.to_string());
    let text = bridge.snapshot_text(scope).await?;
    let img = bridge.screenshot_png().await?;
    Ok(super::tools::ToolOutcome {
        ok: true,
        title: format!("Desktop snapshot · {} lines", text.lines().count()),
        display: super::tools::DisplayKind::Image {
            media_type: "image/png".into(),
            data: img,
        },
        for_model: text,
    })
}

/// desktop_screenshot：只截屏。给 x/y/width/height 时抓该区域（物理像素）。
pub async fn screenshot(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    ctx.authorize("desktop_screenshot", "screen", crate::permission::Risk::Shell).await?;
    let bridge = bridge_of(ctx)?;
    let num = |k: &str| a.get(k).and_then(|v| v.as_f64());
    let region = match (num("x"), num("y"), num("width"), num("height")) {
        (Some(x), Some(y), Some(w), Some(h)) if w > 0.0 && h > 0.0 => {
            Some([x as i32, y as i32, w as i32, h as i32])
        }
        (None, None, None, None) => None,
        _ => return Err(HarnessError::Tool("区域需要 x/y/width/height 四个参数齐全".into())),
    };
    match bridge.call(Cmd::Screenshot { region, zoom: region.is_some() }, 20_000).await? {
        JobReply::Image { image, title } => Ok(super::tools::ToolOutcome {
            ok: true,
            title,
            display: super::tools::DisplayKind::Image {
                media_type: image.media_type,
                data: image.data,
            },
            for_model: "桌面截图已捕获，图像已附在本条工具结果之后。先观察，再用坐标或 ref 操作。".into(),
        }),
        _ => Err(HarnessError::Tool("desktop screenshot 返回异常".into())),
    }
}

/// desktop_extract：窗口文本内容提取（读内容最省 token 的方式）。
pub async fn extract(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    ctx.authorize("desktop_extract", "screen", crate::permission::Risk::Shell).await?;
    let bridge = bridge_of(ctx)?;
    let hwnd = a.get("hwnd").and_then(|v| v.as_i64()).map(|v| v as isize);
    match bridge.call(Cmd::Extract { hwnd }, 40_000).await? {
        JobReply::Text(text) => Ok(super::tools::ToolOutcome::text(
            true,
            format!("Desktop extract · {} lines", text.lines().count()),
            text,
        )),
        _ => Err(HarnessError::Tool("desktop extract 返回异常".into())),
    }
}

/// desktop_click：按 ref / 坐标点击（真实鼠标）。
pub async fn click(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let bridge = bridge_of(ctx)?;
    let button = a.get("button").and_then(|v| v.as_str()).unwrap_or("left").to_string();
    let clicks = a.get("clicks").and_then(|v| v.as_i64()).unwrap_or(1).clamp(1, 3) as i32;
    let desc = if let Some(r) = a.get("ref").and_then(|v| v.as_i64()) {
        format!("click r{r} ×{clicks} {button}")
    } else {
        let x = a.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let y = a.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
        format!("click ({x:.0},{y:.0}) ×{clicks} {button}")
    };
    ctx.authorize("desktop_click", &desc, crate::permission::Risk::Shell).await?;
    let target = require_target(a)?;
    match bridge.call(Cmd::Click { target, button, clicks }, 20_000).await? {
        JobReply::Text(msg) => post_action(&bridge, "Clicked", &msg).await,
        _ => Err(HarnessError::Tool("desktop click 返回异常".into())),
    }
}

/// desktop_type：向焦点窗口键入文本（Unicode 直输）。
pub async fn type_text(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let text = need_str(a, "text")?;
    let head: String = text.chars().take(40).collect();
    ctx.authorize("desktop_type", &format!("type: {head}"), crate::permission::Risk::Shell).await?;
    let bridge = bridge_of(ctx)?;
    match bridge.call(Cmd::Type { text }, 60_000).await? {
        JobReply::Text(msg) => post_action(&bridge, "Typed", &msg).await,
        _ => Err(HarnessError::Tool("desktop type 返回异常".into())),
    }
}

/// desktop_key：按键/组合键（如 Enter、Control+c）。
pub async fn key(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let key = need_str(a, "key")?;
    ctx.authorize("desktop_key", &key, crate::permission::Risk::Shell).await?;
    let bridge = bridge_of(ctx)?;
    match bridge.call(Cmd::Key { key }, 20_000).await? {
        JobReply::Text(msg) => post_action(&bridge, "Pressed", &msg).await,
        _ => Err(HarnessError::Tool("desktop key 返回异常".into())),
    }
}

/// desktop_scroll：真实滚轮。
pub async fn scroll(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let direction = a.get("direction").and_then(|v| v.as_str()).unwrap_or("down").to_lowercase();
    let notches = a.get("notches").and_then(|v| v.as_f64()).unwrap_or(1.0);
    ctx.authorize("desktop_scroll", &direction, crate::permission::Risk::Shell).await?;
    let bridge = bridge_of(ctx)?;
    let target = parse_target(a)?;
    match bridge.call(Cmd::Scroll { direction, notches, target }, 20_000).await? {
        JobReply::Text(msg) => post_action(&bridge, "Scrolled", &msg).await,
        _ => Err(HarnessError::Tool("desktop scroll 返回异常".into())),
    }
}

/// desktop_drag：真实鼠标拖拽。
pub async fn drag(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let bridge = bridge_of(ctx)?;
    let pt = |k: &str| -> Result<RefOrPoint, HarnessError> {
        let obj = a.get(k).filter(|v| v.is_object()).cloned();
        match obj {
            Some(o) => {
                if let Some(r) = o.get("ref").and_then(|v| v.as_i64()) {
                    Ok(RefOrPoint::Ref(r))
                } else {
                    match (o.get("x").and_then(|v| v.as_f64()), o.get("y").and_then(|v| v.as_f64())) {
                        (Some(x), Some(y)) => Ok(RefOrPoint::Point { x, y }),
                        _ => Err(HarnessError::Tool(format!("`{k}` 需要 {{ref}} 或 {{x,y}}"))),
                    }
                }
            }
            None => {
                let (px, py, pr) = match k {
                    "from" => ("x1", "y1", "from_ref"),
                    _ => ("x2", "y2", "to_ref"),
                };
                if let Some(r) = a.get(pr).and_then(|v| v.as_i64()) {
                    Ok(RefOrPoint::Ref(r))
                } else {
                    match (a.get(px).and_then(|v| v.as_f64()), a.get(py).and_then(|v| v.as_f64())) {
                        (Some(x), Some(y)) => Ok(RefOrPoint::Point { x, y }),
                        _ => Err(HarnessError::Tool(format!("`{k}` 需要 {{ref}} 或 {{x,y}}（平铺 {px}/{py}）"))),
                    }
                }
            }
        }
    };
    let from = pt("from")?;
    let to = pt("to")?;
    let desc = match (&from, &to) {
        (RefOrPoint::Ref(a), RefOrPoint::Ref(b)) => format!("drag r{a} → r{b}"),
        (RefOrPoint::Ref(a), RefOrPoint::Point { x, y }) => format!("drag r{a} → ({x:.0},{y:.0})"),
        (RefOrPoint::Point { x, y }, RefOrPoint::Ref(b)) => format!("drag ({x:.0},{y:.0}) → r{b}"),
        (RefOrPoint::Point { x, y }, RefOrPoint::Point { x: x2, y: y2 }) => {
            format!("drag ({x:.0},{y:.0}) → ({x2:.0},{y2:.0})")
        }
    };
    ctx.authorize("desktop_drag", &desc, crate::permission::Risk::Shell).await?;
    match bridge.call(Cmd::Drag { from, to }, 30_000).await? {
        JobReply::Text(msg) => post_action(&bridge, "Dragged", &msg).await,
        _ => Err(HarnessError::Tool("desktop drag 返回异常".into())),
    }
}

/// desktop_window：窗口管理（list / focus / minimize / maximize / close）。
pub async fn window(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let bridge = bridge_of(ctx)?;
    let action = a.get("action").and_then(|v| v.as_str()).unwrap_or("list").to_string();
    let hwnd = a.get("hwnd").and_then(|v| v.as_i64()).map(|v| v as isize);
    match action.as_str() {
        "list" => {
            ctx.authorize("desktop_window", "list", crate::permission::Risk::Shell).await?;
            match bridge.call(Cmd::WindowList, 20_000).await? {
                JobReply::Windows(list) => {
                    let mut out = String::from("窗口（* = 前台；hwnd 供 focus/close 使用）:\n");
                    for w in &list {
                        out.push_str(&format!(
                            "{}hwnd={} \"{}\" @({},{},{},{})\n",
                            if w.foreground { "* " } else { "  " },
                            w.hwnd,
                            w.title,
                            w.rect.0,
                            w.rect.1,
                            w.rect.2,
                            w.rect.3
                        ));
                    }
                    if list.is_empty() {
                        out.push_str("(没有可见窗口)\n");
                    }
                    Ok(super::tools::ToolOutcome::text(
                        true,
                        format!("Windows · {} visible", list.len()),
                        out,
                    ))
                }
                _ => Err(HarnessError::Tool("desktop window list 返回异常".into())),
            }
        }
        _ => {
            let hwnd = hwnd.ok_or_else(|| {
                HarnessError::Tool("focus/minimize/maximize/close 需要 `hwnd`（先 action=list 查看）".into())
            })?;
            ctx.authorize("desktop_window", &format!("{action} hwnd={hwnd}"), crate::permission::Risk::Shell).await?;
            let verb = match action.as_str() {
                "minimize" => "minimize",
                "maximize" => "maximize",
                "close" => "close",
                _ => "focus",
            };
            let cmd = match verb {
                "minimize" => Cmd::WindowShow { hwnd, action: "minimize" },
                "maximize" => Cmd::WindowShow { hwnd, action: "maximize" },
                "close" => Cmd::WindowClose { hwnd },
                _ => Cmd::WindowShow { hwnd, action: "focus" },
            };
            match bridge.call(cmd, 20_000).await? {
                JobReply::Text(msg) => {
                    if verb == "close" {
                        Ok(super::tools::ToolOutcome::text(true, msg.clone(), msg))
                    } else {
                        post_action(&bridge, &msg, "").await
                    }
                }
                _ => Err(HarnessError::Tool("desktop window 返回异常".into())),
            }
        }
    }
}

// ============================================================ tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_key_map_basics() {
        assert_eq!(desk_key_map("Enter"), Some(0x0D));
        assert_eq!(desk_key_map("a"), Some(0x41));
        assert_eq!(desk_key_map("5"), Some(0x35));
        assert_eq!(desk_key_map("F1"), Some(0x70));
        assert_eq!(desk_key_map("F12"), Some(0x7B));
        assert_eq!(desk_key_map("F24"), Some(0x87));
        assert_eq!(desk_key_map("arrowleft"), Some(0x25));
        assert_eq!(desk_key_map("?"), None);
        assert_eq!(mod_key_map("ctrl"), Some(0x11));
        assert_eq!(mod_key_map("win"), Some(0x5B));
        assert_eq!(mod_key_map("hyper"), None);
    }

    #[test]
    fn snapshot_render_shape() {
        let meta = SnapMeta {
            screen_w: 1920,
            screen_h: 1080,
            scale_pct: 150,
            scope: "window: 测试".into(),
        };
        let nodes = vec![
            SnapNode {
                role: "button".into(),
                name: "发送".into(),
                value: String::new(),
                rect: Some((1180, 760, 64, 32)),
                focused: false,
                disabled: false,
                depth: 0,
                ref_no: Some(1),
            },
            SnapNode {
                role: "edit".into(),
                name: "搜索".into(),
                value: "hello".into(),
                rect: None,
                focused: true,
                disabled: false,
                depth: 1,
                ref_no: Some(2),
            },
            SnapNode {
                role: "pane".into(),
                name: "侧栏".into(),
                value: String::new(),
                rect: Some((0, 0, 200, 900)),
                focused: false,
                disabled: false,
                depth: 1,
                ref_no: None,
            },
        ];
        let s = render_snapshot(&meta, &nodes);
        assert!(s.contains("desktop: 1920×1080"));
        assert!(s.contains("缩放 150%"));
        assert!(s.contains("- [r1] button \"发送\" @(1180,760 64×32)"));
        assert!(s.contains("- [r2] edit \"搜索\" @(不可见) [焦点] value=\"hello\""));
        assert!(s.contains("  - pane \"侧栏\"")); // 非交互无 ref，带缩进
    }

    #[test]
    fn b64_known_vectors() {
        assert_eq!(b64_encode(b""), "");
        assert_eq!(b64_encode(b"f"), "Zg==");
        assert_eq!(b64_encode(b"fo"), "Zm8=");
        assert_eq!(b64_encode(b"foo"), "Zm9v");
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn truncate_respects_chars() {
        assert_eq!(truncate("你好世界", 2), "你好");
        assert_eq!(truncate("abc", 10), "abc");
    }
}
