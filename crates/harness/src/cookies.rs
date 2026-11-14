//! Cookies — one-click import of browser cookies / login state into the
//! built-in browser.
//!
//! The built-in browser runs with a throwaway profile (`temp/parable-cua-*`),
//! so a fresh launch is never logged in anywhere. This module closes the gap:
//! read the cookies out of the browsers already on this machine and inject
//! them into the built-in browser over CDP. A persistent jar
//! (`~/.parable/browser-cookies.json`) keeps the login state across restarts
//! and re-injects on browser launch.
//!
//! Extraction paths, both read-only against the user's own browsers:
//!   - Chromium family (Chrome / Edge / Brave / Vivaldi): cookie values are
//!     encrypted at rest (DPAPI / app-bound), so we copy `Local State` plus
//!     the profile's `Network\Cookies` into a throwaway profile and let the
//!     REAL browser binary decrypt them for us: launch it headless on a
//!     random debug port, pull everything over CDP, kill it. The real profile
//!     is never touched.
//!   - Firefox: `cookies.sqlite` is a plain SQLite database — read it
//!     directly (copied first so a running Firefox never sees us).
//!
//! Security note: the jar holds session secrets in plaintext under the user's
//! home — the same trust level as Parable's own `config.toml` keys.

use crate::error::HarnessError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// One cookie as far as CDP and the jar care about it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CookieEntry {
    pub name: String,
    pub value: String,
    pub domain: String,
    #[serde(default = "default_path")]
    pub path: String,
    /// Unix seconds; `None` = session cookie.
    #[serde(default)]
    pub expires: Option<i64>,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub http_only: bool,
    /// "Strict" | "Lax" | "None"; `None` field = unspecified.
    #[serde(default)]
    pub same_site: Option<String>,
    /// True when the cookie is host-only (no leading dot on domain).
    #[serde(default)]
    pub host_only: bool,
}

fn default_path() -> String {
    "/".into()
}

/// Dedupe key: later imports overwrite earlier ones for the same slot.
impl CookieEntry {
    fn key(&self) -> (String, String, String) {
        (self.domain.clone(), self.path.clone(), self.name.clone())
    }

    /// Is this cookie expired right now (session cookies never are)?
    pub fn is_expired(&self) -> bool {
        match self.expires {
            Some(ts) => ts > 0 && ts < now_unix(),
            None => false,
        }
    }

    /// CDP `Network.setCookie` parameters.
    pub fn to_set_cookie_params(&self) -> Value {
        let mut p = json!({"name": self.name, "value": self.value, "path": self.path});
        // url 总是带上（从 domain/path 推导）；domain cookie 追加带点 domain。
        let bare = self.domain.trim_start_matches('.');
        let scheme = if self.secure { "https" } else { "http" };
        p["url"] = json!(format!("{scheme}://{bare}{}", if self.path.is_empty() { "/" } else { &self.path }));
        if !self.host_only && !self.domain.is_empty() {
            p["domain"] = json!(&self.domain);
        }
        if let Some(ts) = self.expires {
            if ts > 0 {
                p["expires"] = json!(ts as f64);
            }
        }
        if self.secure {
            p["secure"] = json!(true);
        }
        if self.http_only {
            p["httpOnly"] = json!(true);
        }
        // SameSite=None 必须配 Secure，否则 setCookie 直接失败；不满足时丢弃同站属性。
        match self.same_site.as_deref() {
            Some("None") if self.secure => {
                p["sameSite"] = json!("None");
            }
            Some("Lax") => {
                p["sameSite"] = json!("Lax");
            }
            Some("Strict") => {
                p["sameSite"] = json!("Strict");
            }
            _ => {}
        }
        p
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- jar

/// Persistent cookie jar: one JSON file under `~/.parable`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CookieJar {
    #[serde(default)]
    pub cookies: Vec<CookieEntry>,
    #[serde(default)]
    pub updated_at: String,
}

pub fn jar_path() -> PathBuf {
    crate::config::Config::home_dir().join("browser-cookies.json")
}

impl CookieJar {
    pub fn load() -> Self {
        let p = jar_path();
        let Ok(text) = std::fs::read_to_string(&p) else {
            return Self::default();
        };
        let mut jar: Self = serde_json::from_str(&text).unwrap_or_default();
        let before = jar.cookies.len();
        jar.cookies.retain(|c| !c.is_expired());
        if jar.cookies.len() != before {
            let _ = jar.save();
        }
        jar
    }

    pub fn save(&self) -> std::io::Result<()> {
        let body = serde_json::to_vec_pretty(self).unwrap_or_default();
        crate::atomic_write(&jar_path(), &body)
    }

    /// Merge `incoming` over the jar (same domain+path+name wins), then prune
    /// expired entries. An expired cookie from a source browser deletes the
    /// jar's copy — the source is the truth about what is still logged in.
    /// Returns the merged total.
    pub fn merge(&mut self, incoming: Vec<CookieEntry>) -> usize {
        for c in incoming {
            let k = c.key();
            if let Some(slot) = self.cookies.iter_mut().find(|e| e.key() == k) {
                *slot = c;
            } else {
                self.cookies.push(c);
            }
        }
        self.cookies.retain(|c| !c.is_expired());
        self.updated_at = chrono::Utc::now().to_rfc3339();
        self.cookies.len()
    }

    pub fn clear(&mut self) {
        self.cookies.clear();
        self.updated_at = chrono::Utc::now().to_rfc3339();
    }
}

// ---------------------------------------------------------------- sources

/// One browser profile that can be imported from.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CookieSource {
    /// Stable id used by the import call: `kind:profileDirName`, e.g.
    /// `edge:Default`, `chrome:Profile 1`, `firefox:abc1.default-release`.
    pub id: String,
    /// Human label: `Microsoft Edge — 默认`.
    pub label: String,
    pub kind: String,
    /// Browser binary (chromium kinds only) — used to decrypt via headless dump.
    pub exe: String,
    /// Chromium: User Data root; Firefox: the profile dir.
    pub root: String,
    /// Chromium: profile dir name (`Default` / `Profile 1`); Firefox: same as root dir name.
    pub profile: String,
    /// Best-effort cookie count from the source DB; null when unreadable.
    pub count: Option<u64>,
    /// Cookie 库被浏览器排他锁占用（运行中的 Chromium 系会锁库，ABE
    /// 防窃取设计）——导入此类来源需要先关闭浏览器。
    pub locked: bool,
}

/// 一次抽取的结果：拿到的 cookie，以及是否动过源浏览器的进程。
#[derive(Debug, Clone, Default)]
pub struct ExtractReport {
    pub cookies: Vec<CookieEntry>,
    /// 抽取前关掉了源浏览器（它的库被排他锁占用）。
    pub closed: bool,
    /// 抽取后自动重新打开了源浏览器（--restore-last-session 恢复标签页）。
    pub reopened: bool,
}

/// Browser definitions: display name + User Data root + exe candidates.
struct ChromiumKind {
    key: &'static str,
    label: &'static str,
    root: fn() -> PathBuf,
    exes: fn() -> Vec<PathBuf>,
}

fn env_dir(var: &str) -> PathBuf {
    PathBuf::from(std::env::var(var).unwrap_or_default())
}

fn chromium_kinds() -> [ChromiumKind; 4] {
    [
        ChromiumKind {
            key: "chrome",
            label: "Google Chrome",
            root: || env_dir("LOCALAPPDATA").join("Google").join("Chrome").join("User Data"),
            exes: || {
                vec![
                    PathBuf::from(std::env::var("ProgramFiles").unwrap_or_default())
                        .join("Google\\Chrome\\Application\\chrome.exe"),
                    PathBuf::from(std::env::var("ProgramFiles(x86)").unwrap_or_default())
                        .join("Google\\Chrome\\Application\\chrome.exe"),
                    env_dir("LOCALAPPDATA").join("Google\\Chrome\\Application\\chrome.exe"),
                ]
            },
        },
        ChromiumKind {
            key: "edge",
            label: "Microsoft Edge",
            root: || env_dir("LOCALAPPDATA").join("Microsoft").join("Edge").join("User Data"),
            exes: || {
                vec![
                    PathBuf::from(std::env::var("ProgramFiles(x86)").unwrap_or_default())
                        .join("Microsoft\\Edge\\Application\\msedge.exe"),
                    PathBuf::from(std::env::var("ProgramFiles").unwrap_or_default())
                        .join("Microsoft\\Edge\\Application\\msedge.exe"),
                ]
            },
        },
        ChromiumKind {
            key: "brave",
            label: "Brave",
            root: || {
                env_dir("LOCALAPPDATA")
                    .join("BraveSoftware")
                    .join("Brave-Browser")
                    .join("User Data")
            },
            exes: || {
                vec![
                    PathBuf::from(std::env::var("ProgramFiles").unwrap_or_default())
                        .join("BraveSoftware\\Brave-Browser\\Application\\brave.exe"),
                    PathBuf::from(std::env::var("ProgramFiles(x86)").unwrap_or_default())
                        .join("BraveSoftware\\Brave-Browser\\Application\\brave.exe"),
                    env_dir("LOCALAPPDATA")
                        .join("BraveSoftware\\Brave-Browser\\Application\\brave.exe"),
                ]
            },
        },
        ChromiumKind {
            key: "vivaldi",
            label: "Vivaldi",
            root: || env_dir("LOCALAPPDATA").join("Vivaldi").join("User Data"),
            exes: || {
                vec![
                    env_dir("LOCALAPPDATA").join("Vivaldi\\Application\\vivaldi.exe"),
                    PathBuf::from(std::env::var("ProgramFiles").unwrap_or_default())
                        .join("Vivaldi\\Application\\vivaldi.exe"),
                ]
            },
        },
    ]
}

/// Detect installed browsers and their profiles. `local` / `roaming` are the
/// `%LOCALAPPDATA%` / `%APPDATA%` roots (parameterised for tests).
pub fn detect_sources_at(local: &Path, roaming: &Path) -> Vec<CookieSource> {
    let mut out = Vec::new();
    for kind in chromium_kinds() {
        // 测试用：按传入的 local root 重放同样布局。
        let root = match kind.key {
            "chrome" => local.join("Google").join("Chrome").join("User Data"),
            "edge" => local.join("Microsoft").join("Edge").join("User Data"),
            "brave" => local
                .join("BraveSoftware")
                .join("Brave-Browser")
                .join("User Data"),
            _ => local.join("Vivaldi").join("User Data"),
        };
        if !root.join("Local State").is_file() {
            continue;
        }
        let exe = (kind.exes)()
            .into_iter()
            .find(|p| p.is_file())
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut profiles: Vec<String> = rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| {
                (n == "Default" || n.starts_with("Profile "))
                    && cookie_db_of(&root.join(n)).is_some()
            })
            .collect();
        profiles.sort();
        for profile in profiles {
            let db = cookie_db_of(&root.join(&profile)).unwrap();
            out.push(CookieSource {
                id: format!("{}:{}", kind.key, profile),
                label: format!("{} — {}", kind.label, profile),
                kind: kind.key.into(),
                exe: exe.clone(),
                root: root.display().to_string(),
                profile: profile.clone(),
                count: count_rows(&db),
                locked: source_locked(&db),
            });
        }
    }
    // Firefox：Profiles 目录下带 cookies.sqlite 的即为配置。
    let ff = roaming.join("Mozilla").join("Firefox").join("Profiles");
    if let Ok(rd) = std::fs::read_dir(&ff) {
        let mut profiles: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && p.join("cookies.sqlite").is_file())
            .collect();
        profiles.sort();
        for profile in profiles {
            let name = profile
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            out.push(CookieSource {
                id: format!("firefox:{name}"),
                label: format!("Firefox — {name}"),
                kind: "firefox".into(),
                exe: String::new(),
                root: profile.display().to_string(),
                profile: name,
                count: count_rows(&profile.join("cookies.sqlite")),
                // Firefox 的库允许共享读，运行中也不锁。
                locked: false,
            });
        }
    }
    out
}

/// Live detection against this machine's real profile roots.
pub fn detect_sources() -> Vec<CookieSource> {
    detect_sources_at(&env_dir("LOCALAPPDATA"), &env_dir("APPDATA"))
}

/// The `cookies.sqlite` path of one Chromium profile dir (new + legacy layout).
fn cookie_db_of(profile_dir: &Path) -> Option<PathBuf> {
    let new = profile_dir.join("Network").join("Cookies");
    if new.is_file() {
        return Some(new);
    }
    let legacy = profile_dir.join("Cookies");
    if legacy.is_file() {
        return Some(legacy);
    }
    None
}

/// Best-effort cookie count (read-only, never blocks). 运行中浏览器的库
/// 以拒绝读共享打开，先用备份语义复制出来再数。
fn count_rows(db: &Path) -> Option<u64> {
    let tmp = temp_work_dir("cnt");
    let _ = std::fs::create_dir_all(&tmp);
    let copy = tmp.join("c.sqlite");
    copy_db_tolerant(db, &copy).ok()?;
    let n = (|| {
        let conn = rusqlite::Connection::open_with_flags(
            &copy,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .ok()?;
        conn.query_row("SELECT COUNT(*) FROM cookies", [], |r| r.get::<_, u64>(0))
            .ok()
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    n
}

/// 手动粘贴导入：解析 `document.cookie` 风格的 `a=1; b=2` 字符串。
///
/// ABE（App-Bound Encryption，Edge/Chrome 127+ 默认）让大部分加密 cookie
/// 无法从复制档案中还原，这条路径是用户 100% 可靠的兜底：在已登录的
/// 浏览器控制台跑 `document.cookie`，或从 EditThisCookie 之类的扩展导出
/// JSON，粘贴进来即可。`domain` 决定 cookie 归属（`host_only` 为 false 时
/// 以点前缀的域 cookie 注入，使子域共享）。
pub fn parse_pasted(text: &str, domain: &str) -> Result<Vec<CookieEntry>, HarnessError> {
    let dom = domain.trim();
    if dom.is_empty() {
        return Err(HarnessError::Config("domain 不能为空".into()));
    }
    let t = text.trim();
    if t.is_empty() {
        return Err(HarnessError::Config("没有可导入的内容".into()));
    }
    // JSON 数组：EditThisCookie / Cookie-Editor 的导出格式。
    if t.starts_with('[') || t.starts_with('{') {
        let v: Value = serde_json::from_str(t)
            .map_err(|e| HarnessError::Config(format!("JSON 解析失败: {e}")))?;
        let arr = match v {
            Value::Array(a) => a,
            other => vec![other],
        };
        let mut out = Vec::new();
        for item in arr {
            let name = item
                .get("name")
                .or_else(|| item.get("Name"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                continue;
            }
            let host_only_dom = item
                .get("domain")
                .and_then(|x| x.as_str())
                .map(String::from)
                .unwrap_or_else(|| dom.to_string());
            let exp = item
                .get("expirationDate")
                .or_else(|| item.get("expires"))
                .or_else(|| item.get("expiry"))
                .and_then(|x| x.as_f64());
            out.push(CookieEntry {
                name,
                value: item
                    .get("value")
                    .or_else(|| item.get("Value"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                host_only: !host_only_dom.starts_with('.'),
                domain: host_only_dom,
                path: item
                    .get("path")
                    .and_then(|x| x.as_str())
                    .unwrap_or("/")
                    .to_string(),
                expires: exp.filter(|e| *e > 0.0).map(|e| e as i64),
                secure: item.get("secure").and_then(|x| x.as_bool()).unwrap_or(true),
                http_only: item
                    .get("httpOnly")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false),
                same_site: item
                    .get("sameSite")
                    .and_then(|x| x.as_str())
                    .and_then(|s| match s {
                        "Strict" | "strict" => Some("Strict".to_string()),
                        "Lax" | "lax" => Some("Lax".to_string()),
                        "None" | "no_restriction" => Some("None".to_string()),
                        _ => None,
                    }),
            });
        }
        if out.is_empty() {
            return Err(HarnessError::Config("JSON 里没有解析出任何 cookie".into()));
        }
        return Ok(out);
    }
    // `a=1; b=2` 形式（document.cookie 原文）。
    let mut out = Vec::new();
    for pair in t.split(';') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let name = k.trim().to_string();
        if name.is_empty() {
            continue;
        }
        out.push(CookieEntry {
            name,
            value: v.trim().to_string(),
            domain: dom.to_string(),
            path: "/".into(),
            expires: None,
            secure: true,
            http_only: false,
            same_site: None,
            host_only: !dom.starts_with('.'),
        });
    }
    if out.is_empty() {
        return Err(HarnessError::Config(
            "没有解析出 cookie：请粘贴 `a=1; b=2` 或导出 JSON".into(),
        ));
    }
    Ok(out)
}

// ---------------------------------------------------------------- extraction

/// Resolve a source id (`kind:profile`) against the live machine and extract.
pub async fn extract_source(id: &str) -> Result<Vec<CookieEntry>, HarnessError> {
    Ok(extract_source_ex(id, false).await?.cookies)
}

/// 同上，但 `allow_close` 允许「先关掉运行中的源浏览器再抽取」——
/// Chromium 系运行时对 cookie 库持排他锁（ABE 防窃取设计），任何外部
/// 进程都读不了，只能请它让开。抽完自动 `--restore-last-session` 重开。
pub async fn extract_source_ex(
    id: &str,
    allow_close: bool,
) -> Result<ExtractReport, HarnessError> {
    let (kind, profile) = id.split_once(':').ok_or_else(|| {
        HarnessError::Tool(format!("无效的来源 id: {id}（应形如 edge:Default）"))
    })?;
    match kind {
        "firefox" => {
            let dir = env_dir("APPDATA")
                .join("Mozilla")
                .join("Firefox")
                .join("Profiles")
                .join(profile);
            let db = dir.join("cookies.sqlite");
            if !db.is_file() {
                return Err(HarnessError::Tool(format!("找不到 Firefox 配置 {id}")));
            }
            Ok(ExtractReport {
                cookies: extract_firefox(&db)?,
                ..Default::default()
            })
        }
        "chrome" | "edge" | "brave" | "vivaldi" => {
            let root: PathBuf = match kind {
                "chrome" => env_dir("LOCALAPPDATA").join("Google\\Chrome\\User Data"),
                "edge" => env_dir("LOCALAPPDATA").join("Microsoft\\Edge\\User Data"),
                "brave" => {
                    env_dir("LOCALAPPDATA").join("BraveSoftware\\Brave-Browser\\User Data")
                }
                _ => env_dir("LOCALAPPDATA").join("Vivaldi\\User Data"),
            };
            let exe = chromium_kinds()
                .into_iter()
                .find(|k| k.key == kind)
                .and_then(|k| (k.exes)().into_iter().find(|p| p.is_file()))
                .ok_or_else(|| HarnessError::Tool(format!("{kind} 的浏览器程序未找到")))?;
            let db = cookie_db_of(&root.join(profile))
                .ok_or_else(|| HarnessError::Tool(format!("找不到 {profile} 的 cookie 库")))?;

            let mut closed = false;
            if source_locked(&db) {
                if !allow_close {
                    return Err(HarnessError::Tool(format!(
                        "{id} 的浏览器正在运行，Cookie 库被排他锁占用。\
                         请先关闭浏览器，或在导入时允许自动关闭（结束后会自动重新打开并恢复标签页）。"
                    )));
                }
                close_browser_image(image_name(kind));
                // 等锁放开（进程优雅退出需要一点时间）。
                for _ in 0..30 {
                    if !source_locked(&db) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                }
                if source_locked(&db) {
                    // 优雅关不掉（后台进程无窗口）：强杀兜底。
                    force_close_browser_image(image_name(kind));
                    for _ in 0..15 {
                        if !source_locked(&db) {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                    }
                }
                if source_locked(&db) {
                    return Err(HarnessError::Tool(format!(
                        "关闭浏览器后 {id} 的 cookie 库仍被占用"
                    )));
                }
                closed = true;
            }

            let cookies = extract_chromium(&exe, &root, profile).await?;
            let mut reopened = false;
            if closed {
                // 用真实浏览器进程把它请回来，恢复用户之前的标签页。
                reopened = reopen_browser(&exe);
            }
            Ok(ExtractReport {
                cookies,
                closed,
                reopened,
            })
        }
        other => Err(HarnessError::Tool(format!("不支持的浏览器: {other}"))),
    }
}

/// 进程镜像名（taskkill /IM 用）。
fn image_name(kind: &str) -> &'static str {
    match kind {
        "chrome" => "chrome.exe",
        "edge" => "msedge.exe",
        "brave" => "brave.exe",
        "vivaldi" => "vivaldi.exe",
        _ => "browser.exe",
    }
}

/// 温柔关闭：taskkill 不带 /F 先走 WM_CLOSE（给窗口一个保存会话的机会）。
fn close_browser_image(image: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/IM", image])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = image;
    }
}

/// 强杀兜底（无窗口的后台进程温柔关不掉）。
fn force_close_browser_image(image: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/IM", image, "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = image;
    }
}

/// 重新打开源浏览器并恢复标签页（尽力而为，失败不致命）。
fn reopen_browser(exe: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS：脱离我们的进程树独立存活。
        std::process::Command::new(exe)
            .arg("--restore-last-session")
            .creation_flags(0x0000_0008)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
    }
    #[cfg(not(windows))]
    {
        let _ = exe;
        false
    }
}

/// Cookie 库是否被浏览器排他锁占用（运行中的 Chromium 系会锁库）。
pub fn source_locked(db: &Path) -> bool {
    std::fs::File::open(db).is_err()
}

/// Firefox cookies are plain SQLite; copy first so a running Firefox never
/// sees a reader on its live DB.
pub fn extract_firefox(db: &Path) -> Result<Vec<CookieEntry>, HarnessError> {
    let tmp = temp_work_dir("ff");
    let copy = tmp.join("cookies.sqlite");
    std::fs::create_dir_all(&tmp).ok();
    copy_db_tolerant(db, &copy)
        .map_err(|e| HarnessError::Tool(format!("复制 Firefox cookie 库失败: {e}")))?;
    let out = extract_firefox_db(&copy);
    let _ = std::fs::remove_dir_all(&tmp);
    out
}

fn extract_firefox_db(db: &Path) -> Result<Vec<CookieEntry>, HarnessError> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| HarnessError::Tool(format!("打开 Firefox cookie 库失败: {e}")))?;
    // 旧版 Firefox 没有 sameSite 列：失败再退基础列。
    let sql_full = "SELECT host, name, value, path, expiry, isSecure, isHttpOnly, sameSite FROM cookies";
    let sql_min = "SELECT host, name, value, path, expiry, isSecure, isHttpOnly FROM cookies";
    let rows = conn
        .prepare(sql_full)
        .and_then(|mut st| {
            let mapped = st.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                ))
            })?;
            mapped.collect::<Result<Vec<_>, _>>()
        })
        .or_else(|_| {
            let mut st = conn.prepare(sql_min)?;
            let mapped = st.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    None,
                ))
            })?;
            mapped.collect::<Result<Vec<_>, _>>()
        })
        .map_err(|e| HarnessError::Tool(format!("读取 Firefox cookie 失败: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|(host, name, value, path, expiry, secure, http_only, same_site)| {
            let host_only = !host.starts_with('.');
            CookieEntry {
                name,
                value,
                domain: host,
                path: if path.is_empty() { "/".into() } else { path },
                // 0 = 会话 cookie；远古值（> 253402300799，year 9999）也是会话语义。
                expires: if expiry <= 0 || expiry > 253_402_300_799 {
                    None
                } else {
                    Some(expiry)
                },
                secure: secure != 0,
                http_only: http_only != 0,
                same_site: match same_site {
                    Some(2) => Some("Strict".into()),
                    Some(1) => Some("Lax".into()),
                    Some(0) => Some("None".into()),
                    _ => None,
                },
                host_only,
            }
        })
        .collect())
}

/// Chromium cookies are encrypted at rest (DPAPI / app-bound). Copy just
/// `Local State` + the cookie DB into a throwaway profile and let the REAL
/// browser binary decrypt for us: headless + CDP dump, then kill. The user's
/// real profile is never opened for writing.
pub async fn extract_chromium(
    exe: &Path,
    user_data: &Path,
    profile: &str,
) -> Result<Vec<CookieEntry>, HarnessError> {
    let db = cookie_db_of(&user_data.join(profile))
        .ok_or_else(|| HarnessError::Tool(format!("找不到 {profile} 的 cookie 库")))?;
    let tmp = temp_work_dir("cr");
    // 最小解密档案：Local State（密钥）+ cookie 库（密文）。
    let _ = std::fs::create_dir_all(tmp.join(profile).join("Network"));
    copy_file_tolerant(&user_data.join("Local State"), &tmp.join("Local State"))
        .map_err(|e| HarnessError::Tool(format!("复制 Local State 失败: {e}")))?;
    let target_db = tmp.join(profile).join("Network").join("Cookies");
    copy_db_tolerant(&db, &target_db)
        .map_err(|e| HarnessError::Tool(format!("复制 cookie 库失败: {e}")))?;

    let port = free_port()?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", tmp.display()))
        .args([
            "--headless=new",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--disable-sync",
            "--disable-background-networking",
            "--hide-scrollbars",
            "about:blank",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| HarnessError::Tool(format!("启动浏览器抽取 cookie 失败: {e}")))?;

    let result = cdp_dump(port).await;
    // 不管成败都收尾：先礼（Browser.close）后兵（kill）。
    let _ = child.kill().await;
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

/// CDP dump against the throwaway headless instance.
async fn cdp_dump(port: u16) -> Result<Vec<CookieEntry>, HarnessError> {
    let client = reqwest::Client::new();
    let mut ws_url = String::new();
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        if let Ok(resp) = client
            .get(format!("http://127.0.0.1:{port}/json/version"))
            .timeout(std::time::Duration::from_millis(800))
            .send()
            .await
        {
            if let Ok(v) = resp.json::<Value>().await {
                if v.get("webSocketDebuggerUrl").and_then(|u| u.as_str()).is_some() {
                    ws_url = v["webSocketDebuggerUrl"].as_str().unwrap_or_default().to_string();
                    break;
                }
            }
        }
    }
    if ws_url.is_empty() {
        return Err(HarnessError::Tool("抽取浏览器调试端口未就绪".into()));
    }
    let mut cdp = CdpRaw::connect(&ws_url).await?;
    // 首选浏览器级 Storage.getCookies（一把全拿）；不行就建页签走
    // Network.getAllCookies。两者都拿的是解密后的明文。
    let cookies = match cdp.call("Storage.getCookies", json!({})).await {
        Ok(v) => v
            .get("cookies")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default(),
        Err(_) => {
            let target = cdp.call("Target.createTarget", json!({"url": "about:blank"})).await?;
            let tid = target["targetId"].as_str().unwrap_or_default().to_string();
            let att = cdp
                .call("Target.attachToTarget", json!({"targetId": tid, "flatten": true}))
                .await?;
            let sid = att["sessionId"].as_str().unwrap_or_default().to_string();
            let r = cdp
                .call_on(&sid, "Network.getAllCookies", json!({}))
                .await?;
            r.get("cookies").and_then(|c| c.as_array()).cloned().unwrap_or_default()
        }
    };
    let _ = cdp.call("Browser.close", json!({})).await;
    Ok(cookies.iter().filter_map(cookie_from_cdp).collect())
}

/// Map one CDP cookie object to a jar entry (null = unparseable).
pub fn cookie_from_cdp(v: &Value) -> Option<CookieEntry> {
    let name = v.get("name")?.as_str()?.to_string();
    let domain = v.get("domain").and_then(|d| d.as_str()).unwrap_or("").to_string();
    let value = v.get("value").and_then(|d| d.as_str()).unwrap_or("").to_string();
    if name.is_empty() {
        return None;
    }
    let session = v.get("session").and_then(|s| s.as_bool()).unwrap_or(false);
    let exp = v.get("expires").and_then(|e| e.as_f64()).unwrap_or(-1.0);
    let secure = v.get("secure").and_then(|s| s.as_bool()).unwrap_or(false);
    Some(CookieEntry {
        name,
        value,
        host_only: !domain.starts_with('.'),
        domain,
        path: v.get("path").and_then(|p| p.as_str()).unwrap_or("/").to_string(),
        expires: if session || exp <= 0.0 { None } else { Some(exp as i64) },
        secure,
        http_only: v.get("httpOnly").and_then(|h| h.as_bool()).unwrap_or(false),
        same_site: v
            .get("sameSite")
            .and_then(|s| s.as_str())
            .and_then(|s| match s {
                "Strict" => Some("Strict"),
                "Lax" => Some("Lax"),
                "None" => Some("None"),
                _ => None,
            })
            .map(String::from),
    })
}

// ---------------------------------------------------------------- helpers

/// 复制（带几次重试）：WAL 检查点之类会短暂影响读。
fn copy_file_tolerant(src: &Path, dst: &Path) -> std::io::Result<()> {
    let mut last = None;
    for _ in 0..3 {
        match std::fs::copy(src, dst) {
            Ok(_) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("copy failed")))
}

/// 库 + WAL/journal 侧车一起搬（缺了也无妨，主库内容仍在）。
fn copy_db_tolerant(db: &Path, dst: &Path) -> std::io::Result<()> {
    copy_file_tolerant(db, dst)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let side = PathBuf::from(format!("{}{suffix}", db.display()));
        if side.is_file() {
            let _ = copy_file_tolerant(&side, &PathBuf::from(format!("{}{suffix}", dst.display())));
        }
    }
    Ok(())
}

fn temp_work_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "parable-ck-{tag}-{}-{}",
        std::process::id(),
        crate::event::now_ms()
    ))
}

fn free_port() -> Result<u16, HarnessError> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| HarnessError::Tool(format!("分配调试端口失败: {e}")))?;
    let port = l.local_addr().map(|a| a.port()).unwrap_or(0);
    drop(l);
    if port == 0 {
        return Err(HarnessError::Tool("分配调试端口失败".into()));
    }
    Ok(port)
}

/// Minimal CDP WebSocket client (one command/response pair at a time).
struct CdpRaw {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next_id: u64,
}

impl CdpRaw {
    async fn connect(url: &str) -> Result<Self, HarnessError> {
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| HarnessError::Tool(format!("CDP 连接失败: {e}")))?;
        Ok(Self { ws, next_id: 1 })
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value, HarnessError> {
        self.roundtrip(method, params, None).await
    }

    async fn call_on(
        &mut self,
        session: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, HarnessError> {
        self.roundtrip(method, params, Some(session)).await
    }

    async fn roundtrip(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, HarnessError> {
        use futures::{SinkExt, StreamExt};
        let id = self.next_id;
        self.next_id += 1;
        let mut req = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            req["sessionId"] = json!(s);
        }
        self.ws
            .send(tokio_tungstenite::tungstenite::Message::Text(req.to_string().into()))
            .await
            .map_err(|e| HarnessError::Tool(format!("CDP 发送失败: {e}")))?;
        loop {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(10), self.ws.next())
                .await
                .map_err(|_| HarnessError::Tool("CDP 响应超时".into()))?
                .ok_or_else(|| HarnessError::Tool("CDP 连接关闭".into()))?
                .map_err(|e| HarnessError::Tool(format!("CDP 接收失败: {e}")))?;
            let tokio_tungstenite::tungstenite::Message::Text(text) = msg else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else {
                continue;
            };
            if v.get("id").and_then(|i| i.as_u64()) != Some(id) {
                continue; // event or unrelated response
            }
            if let Some(err) = v.get("error") {
                return Err(HarnessError::Tool(format!(
                    "CDP {method} 错误: {}",
                    err.get("message").and_then(|m| m.as_str()).unwrap_or("?")
                )));
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firefox_parse_maps_columns() {
        let dir = std::env::temp_dir().join(format!("parable-ck-test-{}-{}", std::process::id(), crate::event::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("cookies.sqlite");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE cookies(
                    id INTEGER PRIMARY KEY, originAttributes TEXT DEFAULT '',
                    name TEXT, value TEXT, host TEXT, path TEXT,
                    expiry INTEGER, lastAccessed INTEGER DEFAULT 0,
                    creationTime INTEGER DEFAULT 0, isSecure INTEGER,
                    isHttpOnly INTEGER, inBrowserElement INTEGER DEFAULT 0,
                    sameSite INTEGER, rawSameSite INTEGER DEFAULT 0);
                 INSERT INTO cookies(name,value,host,path,expiry,isSecure,isHttpOnly,sameSite)
                    VALUES ('sid','abc','.example.com','/',1700000000,1,1,2);
                 INSERT INTO cookies(name,value,host,path,expiry,isSecure,isHttpOnly,sameSite)
                    VALUES ('sess','xyz','exact.example.com','/app',0,0,0,1);",
            )
            .unwrap();
        }
        let out = extract_firefox_db(&db).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(out.len(), 2);
        let d = out.iter().find(|c| c.name == "sid").unwrap();
        assert_eq!(d.domain, ".example.com");
        assert!(!d.host_only, "leading dot = domain cookie");
        assert_eq!(d.expires, Some(1700000000));
        assert!(d.secure && d.http_only);
        assert_eq!(d.same_site.as_deref(), Some("Strict"));
        let s = out.iter().find(|c| c.name == "sess").unwrap();
        assert!(s.host_only);
        assert_eq!(s.expires, None, "expiry 0 = session cookie");
        assert_eq!(s.same_site.as_deref(), Some("Lax"));
    }

    #[test]
    fn pasted_cookie_string_and_json() {
        // document.cookie 原文
        let out = parse_pasted("SESSDATA=abc123; bili_jct=def456;;", ".bilibili.com").unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "SESSDATA");
        assert_eq!(out[0].value, "abc123");
        assert_eq!(out[0].domain, ".bilibili.com");
        assert!(!out[0].host_only, "leading dot = domain cookie");
        // 域名不带点时是 host-only
        let h = parse_pasted("a=1", "example.com").unwrap();
        assert!(h[0].host_only);

        // EditThisCookie JSON
        let json_text = r#"[
          {"name":"token","value":"xyz","domain":".deepseek.com","path":"/",
           "expirationDate":1893456000,"secure":true,"httpOnly":true,"sameSite":"lax"},
          {"name":"uid","value":"42","domain":"chat.deepseek.com","path":"/"}
        ]"#;
        let out = parse_pasted(json_text, ".ignored.com").unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].expires, Some(1893456000));
        assert!(out[0].http_only);
        assert_eq!(out[0].same_site.as_deref(), Some("Lax"));
        assert_eq!(out[1].domain, "chat.deepseek.com");
        assert!(out[1].host_only);

        // 空输入与空域名被拒
        assert!(parse_pasted("   ", ".x.com").is_err());
        assert!(parse_pasted("a=1", "  ").is_err());
        assert!(parse_pasted("[]", ".x.com").is_err());
    }

    #[test]
    fn jar_merge_dedupes_and_prunes() {
        let mut jar = CookieJar::default();
        let mk = |n: &str, dom: &str, exp: Option<i64>| CookieEntry {
            name: n.into(),
            value: "v".into(),
            domain: dom.into(),
            path: "/".into(),
            expires: exp,
            secure: false,
            http_only: false,
            same_site: None,
            host_only: false,
        };
        jar.merge(vec![mk("a", ".x.com", None), mk("b", ".x.com", Some(now_unix() + 100))]);
        assert_eq!(jar.cookies.len(), 2);
        // 同槽覆盖：值变、不重复
        let total = jar.merge(vec![CookieEntry { value: "v2".into(), ..mk("a", ".x.com", None) }]);
        assert_eq!(total, 2);
        assert_eq!(
            jar.cookies.iter().find(|c| c.name == "a").unwrap().value,
            "v2"
        );
        // 过期的被剪掉（merge 前 prune + 不收新过期）
        jar.merge(vec![mk("c", ".x.com", Some(now_unix() - 10)), mk("b", ".x.com", Some(now_unix() - 10))]);
        assert!(!jar.cookies.iter().any(|c| c.name == "c"));
        assert!(!jar.cookies.iter().any(|c| c.name == "b"), "expired incoming overwrites then prunes");
    }

    #[test]
    fn detect_sources_walks_both_layouts() {
        let local = std::env::temp_dir().join(format!("parable-ck-detect-{}-{}", std::process::id(), crate::event::now_ms()));
        let roaming = local.join("roaming");
        // Edge: Local State + Default/Network/Cookies
        let edge = local.join("Microsoft").join("Edge").join("User Data");
        std::fs::create_dir_all(edge.join("Default").join("Network")).unwrap();
        std::fs::write(edge.join("Local State"), "{}").unwrap();
        {
            let conn = rusqlite::Connection::open(edge.join("Default").join("Network").join("Cookies")).unwrap();
            conn.execute_batch("CREATE TABLE cookies(host TEXT, name TEXT, value TEXT, path TEXT, expiry INTEGER, isSecure INTEGER, isHttpOnly INTEGER); INSERT INTO cookies VALUES('.a.com','n','v','/',0,0,0);").unwrap();
        }
        // Chrome: 只有 Profile 1（没有 Default）
        let chrome = local.join("Google").join("Chrome").join("User Data");
        std::fs::create_dir_all(chrome.join("Profile 1").join("Network")).unwrap();
        std::fs::write(chrome.join("Local State"), "{}").unwrap();
        std::fs::write(chrome.join("Profile 1").join("Network").join("Cookies"), "").unwrap();
        // Firefox
        let ff = roaming.join("Mozilla").join("Firefox").join("Profiles").join("z.default-release");
        std::fs::create_dir_all(&ff).unwrap();
        std::fs::write(ff.join("cookies.sqlite"), "").unwrap();

        let srcs = detect_sources_at(&local, &roaming);
        let _ = std::fs::remove_dir_all(&local);
        let ids: Vec<&str> = srcs.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"edge:Default"), "{ids:?}");
        assert!(ids.contains(&"chrome:Profile 1"), "{ids:?}");
        assert!(ids.contains(&"firefox:z.default-release"), "{ids:?}");
        assert_eq!(
            srcs.iter().find(|s| s.id == "edge:Default").unwrap().count,
            Some(1)
        );
        assert!(
            srcs.iter().all(|s| !s.locked),
            "test files are not locked by any browser"
        );
    }

    /// Windows：share_mode(0) 的独占句柄 = 运行中 Chromium 锁库的模拟。
    #[cfg(windows)]
    #[test]
    fn locked_probe_sees_exclusive_open() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = std::env::temp_dir().join(format!(
            "parable-ck-lock-{}-{}",
            std::process::id(),
            crate::event::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("c.sqlite");
        std::fs::write(&db, b"x").unwrap();
        assert!(!source_locked(&db));
        let exclusive = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&db)
            .unwrap();
        assert!(source_locked(&db), "a share_mode(0) open must look locked");
        drop(exclusive);
        assert!(!source_locked(&db), "the lock must clear on close");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cdp_cookie_mapping() {
        let v = json!({
            "name": "sid", "value": "tok", "domain": ".ex.com", "path": "/x",
            "expires": 1700000000.0, "session": false,
            "secure": true, "httpOnly": true, "sameSite": "Lax"
        });
        let c = cookie_from_cdp(&v).unwrap();
        assert_eq!(c.expires, Some(1700000000));
        assert!(!c.host_only);
        assert_eq!(c.same_site.as_deref(), Some("Lax"));
        let s = json!({"name": "s", "value": "", "domain": "h.com", "path": "/", "expires": -1, "session": true});
        let c2 = cookie_from_cdp(&s).unwrap();
        assert_eq!(c2.expires, None);
        assert!(c2.host_only);
    }

    #[test]
    fn set_cookie_params_shape() {
        let c = CookieEntry {
            name: "n".into(),
            value: "v".into(),
            domain: ".ex.com".into(),
            path: "/".into(),
            expires: Some(1700000000),
            secure: true,
            http_only: true,
            same_site: Some("None".into()),
            host_only: false,
        };
        let p = c.to_set_cookie_params();
        assert_eq!(p["url"], "https://ex.com/");
        assert_eq!(p["domain"], ".ex.com");
        assert_eq!(p["sameSite"], "None");
        // None 必须带 Secure 才合法
        let mut c2 = c.clone();
        c2.secure = false;
        let p2 = c2.to_set_cookie_params();
        assert!(p2.get("sameSite").is_none(), "None without Secure must be dropped");
    }
}
