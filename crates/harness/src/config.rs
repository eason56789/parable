use crate::error::HarnessError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const PERMISSION_MODES: [&str; 3] = ["ask", "ws", "full"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub context: u64,
    #[serde(default)]
    pub vision: bool,
    #[serde(default = "default_true")]
    pub tools: bool,
    /// Accepts the provider's `thinking` parameter (extended reasoning).
    #[serde(default)]
    pub thinking: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Max tokens the model may answer with (0 = provider default).
    #[serde(default)]
    pub max_output: u64,
    /// Reasoning effort levels, low → high. The chat UI's thinking level maps
    /// onto these names.
    #[serde(default)]
    pub levels: Vec<String>,
    /// JSON template written into the request body; `{level}` is replaced with
    /// the level name. Empty means "don't send a reasoning parameter".
    #[serde(default)]
    pub mapping: String,
    /// Extra capabilities beyond vision/tools.
    #[serde(default)]
    pub structured: bool,
    #[serde(default)]
    pub native_search: bool,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            context: 0,
            vision: false,
            tools: true,
            thinking: false,
            enabled: true,
            max_output: 0,
            levels: Vec::new(),
            mapping: String::new(),
            structured: false,
            native_search: false,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    pub name: String,
    #[serde(default = "default_color")]
    pub color: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, alias = "baseUrl")]
    pub base_url: String,
    /// `anthropic` | `openai` | `responses`
    #[serde(default = "default_api")]
    pub api: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub models: Vec<ModelConfig>,
}

fn default_color() -> String {
    "#4d6bfe".to_string()
}
fn default_api() -> String {
    "anthropic".to_string()
}

impl ProviderConfig {
    /// Return this provider with the API key masked for display.
    pub fn masked(&self) -> ProviderConfig {
        let mut p = self.clone();
        p.key = mask_key(&self.key);
        p
    }

    pub fn model(&self, id: &str) -> Option<&ModelConfig> {
        self.models.iter().find(|m| m.id == id && m.enabled)
    }
}

/// 按字符边界安全掩码：非 ASCII key 也不能 panic（修复字节下标截断越界）。
pub fn mask_key(key: &str) -> String {
    if key.is_empty() {
        return String::new();
    }
    let n = key.chars().count();
    if n <= 8 {
        "•".repeat(n)
    } else {
        let head: String = key.chars().take(4).collect();
        let tail: String = key.chars().skip(n - 4).collect();
        format!("{head}…{tail}")
    }
}

/// 当前 Unix 时间戳（秒），用于损坏文件备份命名。
fn unix_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_lang")]
    pub lang: String,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_font")]
    pub font: String,
    #[serde(default = "default_size")]
    pub size: u32,
    /// 界面字号（侧栏、设置页、状态栏）。聊天正文字号是 `size`，两者分开，
    /// 调界面字号不会连带把正文放大——ZCode 的 `--ui-font-size` 也是这个分工。
    #[serde(default = "default_ui_size")]
    pub ui_size: u32,
    /// 中文正文的行距倍数（1.3–2.2）。写文章和读代码需要的手感不同。
    #[serde(default = "default_line_height")]
    pub line_height: f32,
    /// 代码块 / diff / 终端字号。
    #[serde(default = "default_code_size")]
    pub code_size: u32,
    /// 消息密度：`airy` | `cozy` | `compact`，控制段落与气泡之间的留白。
    #[serde(default = "default_density")]
    pub density: String,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_accent")]
    pub accent: String,
    #[serde(default = "default_speed")]
    pub speed: u32,
    #[serde(default = "default_cd")]
    pub cd: u32,
    #[serde(default = "default_blur")]
    pub blur: u32,
    #[serde(default = "default_sf")]
    pub sf: f32,
    #[serde(default = "default_anim")]
    pub anim: f32,
    #[serde(default = "default_true")]
    pub stream: bool,
    #[serde(default = "default_true")]
    pub think: bool,
    #[serde(default = "default_true")]
    pub side: bool,
    #[serde(default)]
    pub prefs: String,
    /// `providerId/modelId` of the currently selected model.
    #[serde(default)]
    pub model: String,
    #[serde(default = "default_perm")]
    pub permission_mode: String,
    #[serde(default = "default_lvl")]
    pub thinking_level: u32,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    /// Extra system prompt appended after the built-in one.
    #[serde(default)]
    pub system_prompt: String,
    /// Raw system prompt at the transport level: when non-empty it REPLACES the
    /// entire built-in prompt — the exact string sent as `system` in the request
    /// body to the provider. `{{workspace}}` and `{{permission_mode}}` are
    /// substituted. Plan-mode safety lines are still appended when plan mode is on.
    #[serde(default)]
    pub system_prompt_raw: String,
    /// DeepSeek Search API key (`/v1/tools/search`). Empty disables the
    /// `deepseek_search` tool. Stored locally, masked in the API echo.
    #[serde(default)]
    pub search_key: String,
    /// Which search the user prefers: "auto" | "deepseek" | "anysearch" | "multi".
    /// Steers tool exposure and what `web_search` queries.
    #[serde(default = "default_search_pref")]
    pub search_pref: String,
    /// What Enter does while a turn is running: `queue` (deliver after the
    /// current turn) or `steer` (inject at the next model-step boundary).
    #[serde(default = "default_followup_mode")]
    pub followup_mode: String,

    // ---- Agent 行为 ---------------------------------------------------
    /// 上下文将满时自动压缩历史（Claude autoCompact / Gemini compression
    /// 同款标配）。关闭后超限的轮次直接报错。
    #[serde(default = "default_true")]
    pub compaction_enabled: bool,
    /// 压缩触发阈值：上下文占用百分比（50-95）。
    #[serde(default = "default_compaction_pct")]
    pub compaction_pct: u32,
    /// 单轮最大工具步数（20-500），超过即收尾并说明。
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
    /// ask_user 提问 120 秒无应答时自动选第一项（无人值守/定时任务场景）。
    #[serde(default)]
    pub ask_auto_resolve: bool,
    /// turn 运行期间阻止系统休眠（Windows SetThreadExecutionState）。
    #[serde(default)]
    pub keep_awake: bool,
    /// 上下文用量告警阈值（前端用量条变色点，50-95）。
    #[serde(default = "default_ctx_warn")]
    pub ctx_warn_pct: u32,
    /// 内置浏览器无头运行（桌面零窗口，侧栏实况截图即视图）。
    /// 关闭 = 可见 app 窗口（围观 agent 操作）。
    #[serde(default = "default_true")]
    pub browser_headless: bool,

    // ---- 通知 ---------------------------------------------------------
    /// turn 结束且页面在后台时弹系统通知。
    #[serde(default = "default_true")]
    pub notify: bool,
    /// 通知附带提示音（Web Audio 短哔）。
    #[serde(default)]
    pub notify_sound: bool,

    // ---- 会话管理 -----------------------------------------------------
    /// 自动归档 N 天未活跃的会话（0 = 关闭；3/7/14/30 常用档位）。
    #[serde(default)]
    pub auto_archive_days: u32,

    // ---- 代码块 -------------------------------------------------------
    /// 代码块显示行号。
    #[serde(default = "default_true")]
    pub code_line_numbers: bool,
    /// 代码块长行自动换行（默认横向滚动）。
    #[serde(default)]
    pub code_wrap: bool,

    // ---- 网络与隐私 ---------------------------------------------------
    /// 出站 HTTP 代理（http(s)://host:port），作用于模型请求与 web 工具。
    #[serde(default)]
    pub http_proxy: String,
    /// 不走代理的主机后缀/网段列表，逗号分隔。
    #[serde(default)]
    pub http_no_proxy: String,
    /// 调试：把每次发往模型的请求体（system+tools+history）追加落盘到
    /// logs/model-io.jsonl。只记请求侧（流式响应无完整原文）。
    #[serde(default)]
    pub model_io_log: bool,

    // ---- 输入行为 -----------------------------------------------------
    /// 发送键策略：`enter` = Enter 发送 / Ctrl+Enter 换行；`ctrl` = 反之。
    #[serde(default = "default_enter_mode")]
    pub enter_mode: String,

    // ---- 数据治理 -----------------------------------------------------
    /// 已归档会话保留天数（0 = 永不清理）。超期归档会话在启动时删除。
    #[serde(default)]
    pub session_retention_days: u32,

    // ---- 生命周期 hooks ------------------------------------------------
    /// 生命周期钩子（设置 → Hooks；Claude Code 方言子集）。
    #[serde(default)]
    pub hooks: Vec<crate::hooks::HookConfig>,

    // ---- MCP 服务器 ----------------------------------------------------
    /// stdio MCP 服务器清单（设置 → MCP；工具暴露为 mcp_<server>_<tool>）。
    #[serde(default)]
    pub mcp_servers: Vec<crate::mcp::McpServerConfig>,

    // ---- 多 Profile ----------------------------------------------------
    /// 命名配置束：模型 + 权限 + 思考强度一键切换。
    #[serde(default)]
    pub profiles: Vec<ProfileConfig>,
    /// 当前激活的 Profile id（空 = 未启用）。
    #[serde(default)]
    pub active_profile: String,
}

/// 命名配置束（工作/个人一键切换）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileConfig {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub model: String,
    #[serde(default = "default_perm")]
    pub permission_mode: String,
    #[serde(default = "default_lvl")]
    pub thinking_level: u32,
}

use serde_json::Value;

/// 项目级设置覆盖：工作区 `.parable/settings.json` 里的键在 run_turn 时
/// 压过全局配置（仅限白名单键）。缓存按路径 + mtime。
pub fn project_overlay(workspace: &std::path::Path) -> Option<Value> {
    use std::sync::atomic::{AtomicPtr, Ordering};
    static CACHE: AtomicPtr<(std::path::PathBuf, u64, Option<Value>)> =
        AtomicPtr::new(std::ptr::null_mut());
    let f = workspace.join(".parable").join("settings.json");
    let meta = std::fs::metadata(&f).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    // 读取缓存
    let cached = unsafe { CACHE.load(Ordering::Relaxed).as_ref() }.cloned();
    if let Some((p, m, v)) = cached {
        if p == f && m == mtime {
            return v;
        }
    }
    let v: Option<Value> = std::fs::read_to_string(&f)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let out = v.as_ref().and_then(|v| v.get("overrides")).cloned();
    let boxd = Box::new((f, mtime, out.clone()));
    unsafe { CACHE.store(Box::into_raw(boxd), Ordering::Relaxed) };
    out
}

/// 项目覆盖白名单键：只允许这些键生效（防项目文件越权改 provider/key）。
pub const OVERLAY_KEYS: [&str; 8] = [
    "permission_mode",
    "model",
    "max_steps",
    "compaction_enabled",
    "compaction_pct",
    "system_prompt",
    "plan_mode",
    "notify",
];

fn default_compaction_pct() -> u32 {
    85
}
fn default_max_steps() -> u32 {
    100
}
fn default_ctx_warn() -> u32 {
    80
}
fn default_enter_mode() -> String {
    "enter".into()
}

fn default_search_pref() -> String {
    "auto".into()
}
fn default_followup_mode() -> String {
    "queue".into()
}

fn default_lang() -> String {
    "zh".into()
}
fn default_theme() -> String {
    "auto".into()
}
fn default_font() -> String {
    "serif".into()
}
fn default_size() -> u32 {
    16
}
fn default_ui_size() -> u32 {
    13
}
fn default_line_height() -> f32 {
    1.5
}
fn default_code_size() -> u32 {
    13
}
fn default_density() -> String {
    "cozy".into()
}
fn default_width() -> u32 {
    760
}
fn default_accent() -> String {
    "#d97757".into()
}
fn default_speed() -> u32 {
    22
}
fn default_cd() -> u32 {
    650
}
fn default_blur() -> u32 {
    8
}
fn default_sf() -> f32 {
    1.5
}
fn default_anim() -> f32 {
    1.0
}
fn default_perm() -> String {
    "ask".into()
}
fn default_lvl() -> u32 {
    2
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: String::new(),
            lang: default_lang(),
            theme: default_theme(),
            font: default_font(),
            size: default_size(),
            ui_size: default_ui_size(),
            line_height: default_line_height(),
            code_size: default_code_size(),
            density: default_density(),
            width: default_width(),
            accent: default_accent(),
            speed: default_speed(),
            cd: default_cd(),
            blur: default_blur(),
            sf: default_sf(),
            anim: default_anim(),
            stream: true,
            think: true,
            side: true,
            prefs: String::new(),
            model: String::new(),
            permission_mode: default_perm(),
            thinking_level: default_lvl(),
            workspace: String::new(),
            providers: default_providers(),
            system_prompt: String::new(),
            system_prompt_raw: String::new(),
            search_key: String::new(),
            search_pref: default_search_pref(),
            followup_mode: default_followup_mode(),
            compaction_enabled: true,
            compaction_pct: default_compaction_pct(),
            max_steps: default_max_steps(),
            ask_auto_resolve: false,
            keep_awake: false,
            ctx_warn_pct: default_ctx_warn(),
            browser_headless: true,
            notify: true,
            notify_sound: false,
            auto_archive_days: 0,
            code_line_numbers: true,
            code_wrap: false,
            http_proxy: String::new(),
            http_no_proxy: String::new(),
            model_io_log: false,
            enter_mode: default_enter_mode(),
            session_retention_days: 0,
            hooks: Vec::new(),
            mcp_servers: Vec::new(),
            profiles: Vec::new(),
            active_profile: String::new(),
        }
    }
}

/// A starter provider entry matching the sample shape of the original UI, but with
/// an empty key — the user must paste a real key before anything streams.
pub fn default_providers() -> Vec<ProviderConfig> {
    vec![ProviderConfig {
        id: "deepseek".into(),
        name: "DeepSeek".into(),
        color: "#4d6bfe".into(),
        icon: "S".into(),
        enabled: true,
        base_url: "https://api.deepseek.com/anthropic".into(),
        api: "anthropic".into(),
        key: String::new(),
        models: vec![
            ModelConfig {
                id: "deepseek-chat".into(),
                name: "deepseek-chat".into(),
                context: 128_000,
                vision: false,
                tools: true,
                thinking: false,
                max_output: 0,
                levels: Vec::new(),
                mapping: String::new(),
                structured: false,
                native_search: false,
                enabled: true,
            },
            ModelConfig {
                id: "deepseek-reasoner".into(),
                name: "deepseek-reasoner".into(),
                context: 128_000,
                vision: false,
                tools: true,
                thinking: true,
                max_output: 0,
                levels: Vec::new(),
                mapping: String::new(),
                structured: false,
                native_search: false,
                enabled: true,
            },
        ],
    }]
}

impl Config {
    /// The data directory: config, sessions, permissions, logs, plugins.
    ///
    /// `PARABLE_HOME` overrides it. On Windows `dirs::home_dir()` reads the
    /// OS user profile and ignores `HOME`, so without this switch there is
    /// no way to run a second instance against its own data — or to test
    /// without touching the real profile.
    pub fn home_dir() -> PathBuf {
        if let Some(dir) = std::env::var_os("PARABLE_HOME") {
            let dir = PathBuf::from(dir);
            if !dir.as_os_str().is_empty() {
                return dir;
            }
        }
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".parable")
    }

    pub fn config_path() -> PathBuf {
        Self::home_dir().join("config.toml")
    }

    pub fn permissions_path() -> PathBuf {
        Self::home_dir().join("permissions.toml")
    }

    pub fn logs_dir() -> PathBuf {
        Self::home_dir().join("logs")
    }

    /// 修复历史遗留的非法值：模型 context = 0（旧版 UI 落库）会让压缩判定
    /// 把 8000 当窗口、状态栏却按 128000 显示——两边精神分裂。统一归一为
    /// 128000（运行时兜底同款）。
    pub fn normalize(&mut self) {
        let mut fixed = 0usize;
        for p in &mut self.providers {
            for m in &mut p.models {
                if m.context == 0 {
                    m.context = 128_000;
                    fixed += 1;
                }
            }
        }
        if fixed > 0 {
            tracing::info!("config: normalized {fixed} model(s) with context=0 to 128000");
        }
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        match std::fs::read_to_string(&path) {
            Ok(s) => match toml::from_str::<Config>(&s) {
                Ok(mut c) => {
                    c.normalize();
                    c
                }
                Err(e) => {
                    // 解析失败先把原文件备份走，再回退默认值——
                    // 避免后续 save() 用默认配置覆盖丢失原始数据。
                    let name = path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "config.toml".to_string());
                    let backup = path.with_file_name(format!("{name}.corrupt-{}", unix_ts()));
                    match std::fs::rename(&path, &backup) {
                        Ok(()) => tracing::error!(
                            "config parse failed ({e}); original backed up to {}",
                            backup.display()
                        ),
                        Err(be) => tracing::error!(
                            "config parse failed ({e}); backup rename failed: {be} (original left in place)"
                        ),
                    }
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) -> crate::Result<()> {
        let p = Self::config_path();
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let s = toml::to_string_pretty(self)
            .map_err(|e| HarnessError::Config(e.to_string()))?;
        // 原子写盘（临时文件 + rename），避免写一半崩溃留下损坏的 config.toml。
        crate::atomic_write(&p, s.as_bytes())?;
        restrict_config_acl(&p);
        Ok(())
    }

    /// A provider + model resolved from the `providerId/modelId` selector string.
    pub fn resolve_model(&self, sel: &str) -> Option<(&ProviderConfig, &ModelConfig)> {
        let sel = if sel.is_empty() { self.model.as_str() } else { sel };
        let (pid, mid) = sel.split_once('/')?;
        self.providers
            .iter()
            .find(|p| p.id == pid && p.enabled)
            .and_then(|p| p.model(mid).map(|m| (p, m)))
    }

    /// Currently selected model, falling back to the first enabled one.
    pub fn active_model(&self) -> Option<(&ProviderConfig, &ModelConfig)> {
        self.resolve_model("").or_else(|| {
            self.providers
                .iter()
                .filter(|p| p.enabled && !p.key.is_empty())
                .find_map(|p| {
                    p.models
                        .iter()
                        .find(|m| m.enabled)
                        .map(|m| (p, m))
                })
        })
    }

    pub fn ensure_home() -> std::io::Result<PathBuf> {
        let d = Self::home_dir();
        std::fs::create_dir_all(&d)?;
        std::fs::create_dir_all(Self::logs_dir())?;
        Ok(d)
    }
}

pub fn read_credentials_for(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

// ------------------------------------------------------------------ 文件 ACL

/// 收紧 config 文件的访问权限（内含 API key / search key）。
/// Unix：0o600；Windows：尽力 `icacls /inheritance:r /grant:r` 仅保留当前用户。
/// 失败只告警，不阻塞保存。
fn restrict_config_acl(p: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        if let Err(e) = std::fs::set_permissions(p, perms) {
            tracing::warn!("could not chmod 600 {}: {e}", p.display());
        }
    }
    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME").unwrap_or_default();
        if user.is_empty() {
            tracing::warn!("could not restrict ACL on {}: USERNAME not set", p.display());
            return;
        }
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = std::process::Command::new("icacls")
            .arg(p)
            .arg("/inheritance:r")
            .arg("/grant:r")
            .arg(format!("{user}:F"))
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => tracing::warn!(
                "icacls on {} failed ({}): {}",
                p.display(),
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => tracing::warn!("could not run icacls on {}: {e}", p.display()),
        }
    }
}

// ------------------------------------------------------------------ PATCH 删除

/// PATCH 合并的删除支持：provider 级 `"deleted": true` 删除整个 provider，
/// models 数组内 model 级 `"deleted": true` 删除该模型。
///
/// server.rs 的 `merge_config` 在做字段合并**之前**调用本函数即可
/// （其余 merge 逻辑不用感知删除语义）。
pub fn apply_patch_deletions(cfg: &mut Config, v: &serde_json::Value) {
    let Some(arr) = v.get("providers").and_then(|x| x.as_array()) else {
        return;
    };
    for p in arr {
        let Some(pid) = p.get("id").and_then(|x| x.as_str()) else {
            continue;
        };
        if p.get("deleted").and_then(|x| x.as_bool()) == Some(true) {
            cfg.providers.retain(|x| x.id != pid);
            continue;
        }
        let Some(models) = p.get("models").and_then(|x| x.as_array()) else {
            continue;
        };
        let to_delete: Vec<String> = models
            .iter()
            .filter(|m| m.get("deleted").and_then(|x| x.as_bool()) == Some(true))
            .filter_map(|m| m.get("id").and_then(|x| x.as_str()).map(String::from))
            .collect();
        if to_delete.is_empty() {
            continue;
        }
        if let Some(pp) = cfg.providers.iter_mut().find(|x| x.id == pid) {
            pp.models.retain(|m| !to_delete.contains(&m.id));
        }
    }
}

// ---- 代理感知 HTTP client ------------------------------------------------

static PROXY: std::sync::RwLock<Option<(String, String)>> = std::sync::RwLock::new(None);
static CLIENT_CACHE: std::sync::Mutex<Option<(String, reqwest::Client)>> =
    std::sync::Mutex::new(None);

/// 运行时更新代理设置（启动与每次 PATCH /config 后调用）。
/// 传入 (proxy, no_proxy)；proxy 为空 = 直连。
pub fn set_proxy(proxy: String, no_proxy: String) {
    *PROXY.write().expect("proxy lock") = Some((proxy, no_proxy));
}

/// 代理感知的共享 HTTP client：模型请求、web 工具共用。按当前代理串缓存，
/// 设置变更后自动重建。connect_timeout 20s（与原各处手搓 builder 一致）。
/// 给任意 ClientBuilder 套上当前代理设置（保留调用方的 UA/重定向/超时定制）。
pub fn apply_proxy(b: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    let (proxy, no_proxy) = PROXY
        .read()
        .expect("proxy lock")
        .clone()
        .unwrap_or_default();
    if proxy.is_empty() {
        return b;
    }
    match reqwest::Proxy::all(&proxy) {
        Ok(p) => {
            let p = if no_proxy.is_empty() {
                p
            } else {
                p.no_proxy(reqwest::NoProxy::from_string(&no_proxy))
            };
            b.proxy(p)
        }
        Err(e) => {
            tracing::warn!("代理配置无效({proxy}): {e}");
            b
        }
    }
}

/// 代理感知的共享 HTTP client：简单场景直接用（按当前代理串缓存）。
pub fn http_client() -> reqwest::Client {
    let key = {
        let p = PROXY.read().expect("proxy lock");
        p.clone().unwrap_or_default()
    };
    let sig = format!("{}\u{1f}{}", key.0, key.1);
    let mut cache = CLIENT_CACHE.lock().expect("client cache");
    if let Some((k, c)) = cache.as_ref() {
        if *k == sig {
            return c.clone();
        }
    }
    let client = apply_proxy(
        reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(20)),
    )
    .build()
    .unwrap_or_else(|_| reqwest::Client::new());
    *cache = Some((sig, client.clone()));
    client
}
