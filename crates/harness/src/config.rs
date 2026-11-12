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

pub fn mask_key(key: &str) -> String {
    if key.is_empty() {
        return String::new();
    }
    let n = key.len();
    if n <= 8 {
        "•".repeat(n)
    } else {
        format!("{}…{}", &key[..4], &key[n - 4..])
    }
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
}

fn default_search_pref() -> String {
    "auto".into()
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
                enabled: true,
            },
            ModelConfig {
                id: "deepseek-reasoner".into(),
                name: "deepseek-reasoner".into(),
                context: 128_000,
                vision: false,
                tools: true,
                thinking: true,
                enabled: true,
            },
        ],
    }]
}

impl Config {
    pub fn home_dir() -> PathBuf {
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
        match std::fs::read_to_string(Self::config_path()) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                tracing::warn!("config parse failed ({e}); using defaults");
                Config::default()
            }),
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
        std::fs::write(&p, s)?;
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
