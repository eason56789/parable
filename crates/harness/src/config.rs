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

    pub fn load() -> Self {
        let path = Self::config_path();
        match std::fs::read_to_string(&path) {
            Ok(s) => match toml::from_str(&s) {
                Ok(c) => c,
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
