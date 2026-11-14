//! Agent definitions — reusable subagent profiles stored as Markdown files.
//!
//! One file per profile, YAML-ish front matter plus the system prompt body:
//!
//! ```text
//! ---
//! name: 文档整理师
//! description: 把散落的文档按规范归档并生成索引。
//! color: blue
//! model: provider/model
//! thinking: 2
//! tools:
//!   - read_file
//!   - grep
//! ---
//!
//! 你是文档整理师……
//! ```
//!
//! Two scopes exist, mirroring skills:
//!   - user:    `~/.parable/agents/<slug>.md`
//!   - project: `<workspace>/.parable/agents/<slug>.md`
//!
//! Only the front matter is interpreted — `name` / `description` are required,
//! everything else is optional. The body is the child agent's system prompt
//! (its "persona"), appended to the normal harness prompt when a child is
//! spawned from the profile.
//!
//! Disabled profiles are recorded in `<scope>/.disabled.json` as a slug list
//! rather than by rewriting the user's files, the same sidecar layout skills
//! use. Enabled profiles surface in the parent's system prompt ("Saved
//! agents") and can be spawned with `spawn_agent` + `agent`, or launched
//! directly from the team page.
//!
//! Naming deliberately reuses the subagent roster rules ([`crate::subagent::valid_name`]):
//! a profile's name is the name the child will wear, so it must be a valid
//! roster name — Chinese-first, 1–32 chars, no spaces, not `lead`.

use crate::error::HarnessError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Identity colors, in picker order (mirrors ZCode's palette).
pub const COLORS: [&str; 8] = [
    "red", "blue", "green", "yellow", "purple", "orange", "pink", "cyan",
];

/// One saved agent profile.
#[derive(Debug, Clone, Serialize)]
pub struct AgentDef {
    /// File stem, also the stable id used by the API.
    pub id: String,
    pub name: String,
    pub description: String,
    /// Identity color shown in the UI (see [`COLORS`]); invalid values fall
    /// back to `blue` with a warning.
    pub color: String,
    /// Model selector for children spawned from this profile; empty = inherit.
    pub model: String,
    /// Thinking level for children; none = inherit the parent's.
    pub thinking: Option<u32>,
    /// Tools the child may call; empty = all tools.
    pub tools: Vec<String>,
    /// "user" | "project"
    pub scope: String,
    pub enabled: bool,
    /// Absolute path to the Markdown file.
    pub path: String,
    /// The child's persona — appended to the harness system prompt.
    pub system_prompt: String,
    /// Non-fatal problems found while loading (shown in the UI).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

/// A file that failed to load at all.
#[derive(Debug, Clone, Serialize)]
pub struct AgentProblem {
    pub path: String,
    pub reason: String,
    /// "user" | "project" — which scope the bad file belongs to.
    pub scope: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentListing {
    pub agents: Vec<AgentDef>,
    pub problems: Vec<AgentProblem>,
}

/// Body of `POST /api/agents/defs`.
#[derive(Debug, Deserialize)]
pub struct AgentInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub thinking: Option<u32>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub system_prompt: String,
    /// "user" | "project"; defaults to user.
    #[serde(default)]
    pub scope: Option<String>,
    /// Present on updates (the id being replaced): the target file may be
    /// overwritten in place, and a changed name moves the old file aside.
    /// Absent on creates, which refuse to clobber an existing file.
    #[serde(default)]
    pub rename_from: Option<String>,
}

/// Where the two scopes live for this process.
pub fn user_dir() -> PathBuf {
    crate::config::Config::home_dir().join("agents")
}

pub fn project_dir(workspace: &Path) -> PathBuf {
    workspace.join(".parable").join("agents")
}

fn scope_dir(scope: &str, workspace: &Path) -> PathBuf {
    match scope {
        "project" => project_dir(workspace),
        _ => user_dir(),
    }
}

/// Load every agent profile in both scopes (or one scope when given).
///
/// A malformed file is reported in `problems` instead of aborting the listing —
/// one bad profile must not hide the rest.
pub fn list(workspace: &Path, scope: Option<&str>) -> AgentListing {
    let mut out = AgentListing {
        agents: Vec::new(),
        problems: Vec::new(),
    };
    let scopes: Vec<&str> = match scope {
        Some("user") => vec!["user"],
        Some("project") => vec!["project"],
        _ => vec!["user", "project"],
    };
    for s in scopes {
        let dir = scope_dir(s, workspace);
        let disabled = read_disabled(&dir);
        scan(&dir, s, &disabled, &mut out);
    }
    out.agents
        .sort_by(|a, b| a.scope.cmp(&b.scope).then(a.name.cmp(&b.name)));
    out
}

fn scan(dir: &Path, scope: &str, disabled: &[String], out: &mut AgentListing) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        // 只认第一层的 *.md —— 与 ZCode 一致，子目录不是 profile。
        if !p.is_file() {
            continue;
        }
        let is_md = p
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.eq_ignore_ascii_case("md") || s.eq_ignore_ascii_case("markdown"))
            .unwrap_or(false);
        if !is_md {
            continue;
        }
        let id = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let Ok(text) = std::fs::read_to_string(&p) else {
            out.problems.push(AgentProblem {
                path: p.display().to_string(),
                reason: "could not read the file".into(),
                scope: scope.to_string(),
            });
            continue;
        };
        match parse(&id, scope, &text) {
            Ok(mut def) => {
                def.path = p.display().to_string();
                def.enabled = !disabled.iter().any(|d| d == &def.id);
                out.agents.push(def);
            }
            Err(reason) => out.problems.push(AgentProblem {
                path: p.display().to_string(),
                reason,
                scope: scope.to_string(),
            }),
        }
    }
}

/// Split `---` front matter from the body and interpret the fields.
fn parse(id: &str, scope: &str, text: &str) -> Result<AgentDef, String> {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("").trim();
    if first != "---" {
        return Err("an agent file must start with a `---` front matter block".into());
    }
    let mut name = String::new();
    let mut description = String::new();
    let mut color = String::new();
    let mut model = String::new();
    let mut thinking: Option<u32> = None;
    let mut tools: Vec<String> = Vec::new();
    let mut enabled_flag: Option<bool> = None;
    let mut body_start = 0usize;
    let mut closed = false;
    let mut consumed = first.len() + 1;
    let mut in_tools = false;
    for line in lines.by_ref() {
        consumed += line.len() + 1;
        let t = line.trim();
        if t == "---" {
            closed = true;
            body_start = consumed;
            break;
        }
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        // tools 列表延续行：`- item`（任意缩进）。
        if let Some(item) = t.strip_prefix("- ") {
            if in_tools {
                let v = item.trim().trim_matches('"').trim_matches('\'').to_string();
                if !v.is_empty() {
                    tools.push(v);
                }
                continue;
            }
        }
        let Some((k, v)) = t.split_once(':') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
        in_tools = false;
        match k {
            "name" => name = yaml_unescape(&v),
            "description" => description = yaml_unescape(&v),
            "color" => color = v,
            "model" => model = v,
            "thinking" => thinking = v.parse::<u32>().ok(),
            "enabled" => enabled_flag = Some(!matches!(v.as_str(), "false" | "no" | "0")),
            "tools" => {
                in_tools = true;
                tools.clear();
                // inline 形式 `tools: [a, b]` 也支持。
                if v.starts_with('[') && v.ends_with(']') {
                    for part in v[1..v.len() - 1].split(',') {
                        let p = part.trim().trim_matches('"').trim_matches('\'');
                        if !p.is_empty() {
                            tools.push(p.to_string());
                        }
                    }
                    in_tools = false;
                }
            }
            _ => {}
        }
    }
    if !closed {
        return Err("front matter is missing its closing `---`".into());
    }
    if name.trim().is_empty() {
        return Err("front matter is missing the required `name` field".into());
    }
    if description.trim().is_empty() {
        return Err("front matter is missing the required `description` field".into());
    }
    let body = text.get(body_start..).unwrap_or("").trim().to_string();
    let mut warnings = Vec::new();
    let mut color_v = color.trim().to_string();
    if !color_v.is_empty() && !COLORS.contains(&color_v.as_str()) {
        warnings.push(format!("unknown color `{color_v}`; using blue"));
        color_v = "blue".into();
    }
    if color_v.is_empty() {
        color_v = "blue".into();
    }
    Ok(AgentDef {
        id: id.to_string(),
        name: name.trim().to_string(),
        description: description.trim().to_string(),
        color: color_v,
        model: model.trim().to_string(),
        thinking,
        tools,
        scope: scope.to_string(),
        // `enabled: false` in the file is respected on top of the sidecar list.
        enabled: enabled_flag.unwrap_or(true),
        path: String::new(),
        system_prompt: body,
        warnings,
    })
}

/// Render a profile Markdown file from its parts.
fn render(input: &AgentInput, slug: &str) -> String {
    let _ = slug;
    let mut s = String::from("---\n");
    s.push_str(&format!("name: \"{}\"\n", yaml_str(input.name.trim())));
    s.push_str(&format!(
        "description: \"{}\"\n",
        yaml_str(input.description.trim())
    ));
    if !input.color.trim().is_empty() {
        s.push_str(&format!("color: {}\n", input.color.trim()));
    }
    if !input.model.trim().is_empty() {
        s.push_str(&format!("model: {}\n", input.model.trim()));
    }
    if let Some(t) = input.thinking {
        s.push_str(&format!("thinking: {}\n", t.clamp(0, 3)));
    }
    let tools: Vec<String> = input
        .tools
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect();
    if !tools.is_empty() {
        s.push_str("tools:\n");
        for t in &tools {
            s.push_str(&format!("  - {t}\n"));
        }
    }
    s.push_str("---\n\n");
    s.push_str(input.system_prompt.trim());
    s.push('\n');
    s
}

/// Create or update a profile. `rename_from` moves an existing file.
pub fn save(workspace: &Path, input: &AgentInput) -> Result<AgentDef, HarnessError> {
    let name = input.name.trim();
    if name.is_empty() || name.chars().count() > 32 {
        return Err(HarnessError::Config(
            "name must be 1-32 characters".into(),
        ));
    }
    // 名字就是子代理在名册里的身份：直接复用名册的命名规则（中文优先、
    // 无空格、保留字 lead 不可用）。
    if !crate::subagent::valid_name(name) {
        return Err(HarnessError::Config(
            "name must start with a letter or CJK character and contain only letters, digits, CJK characters or hyphens (and not `lead`)".into(),
        ));
    }
    if input.description.trim().is_empty() {
        return Err(HarnessError::Config("description is required".into()));
    }
    if input.system_prompt.trim().is_empty() {
        return Err(HarnessError::Config("system prompt is required".into()));
    }
    if let Some(t) = input.thinking {
        if t > 3 {
            return Err(HarnessError::Config("thinking must be 0-3".into()));
        }
    }
    let scope = input.scope.as_deref().unwrap_or("user");
    let dir = scope_dir(scope, workspace);
    let slug = slugify(name);
    if slug.is_empty() {
        return Err(HarnessError::Config(
            "name must contain letters, digits or CJK characters".into(),
        ));
    }
    let target = dir.join(format!("{slug}.md"));
    std::fs::create_dir_all(&dir)?;

    // `rename_from` 标记这是一次「更新」：目标文件允许覆盖（原地编辑与改名
    // 都走这里）；缺省则是「创建」，撞车必须拒绝而不是悄悄覆盖陌生文件。
    // 改名时旧文件在新文件写好后删除，禁用记录也随之迁移。
    let old_slug = input
        .rename_from
        .as_deref()
        .map(slugify)
        .filter(|s| !s.is_empty());
    if old_slug.is_none() && target.exists() {
        return Err(HarnessError::Config(format!(
            "an agent file \"{slug}.md\" already exists"
        )));
    }

    let text = render(input, &slug);
    crate::atomic_write(&target, text.as_bytes())?;
    // 先验证即将生效的 Markdown 可解析（ZCode 语义），避免 serializer 回归
    // 时把坏 profile 落盘。
    let mut def = parse(&slug, scope, &text).map_err(HarnessError::Config)?;
    def.path = target.display().to_string();
    let mut disabled = read_disabled(&dir);
    def.enabled = !disabled.iter().any(|d| d == &def.id);

    if let Some(old) = old_slug.filter(|s| *s != slug) {
        let old_file = dir.join(format!("{old}.md"));
        if old_file.exists() {
            let _ = std::fs::remove_file(&old_file);
        }
        // 禁用状态跟人走：改名不把用户关掉的 profile 悄悄变回启用。
        if disabled.iter().any(|d| d == &old) {
            disabled.retain(|d| d != &old);
            disabled.push(slug.clone());
            disabled.sort();
            let _ = write_disabled(&dir, &disabled);
        }
    }

    Ok(def)
}

/// Delete a profile file.
pub fn remove(workspace: &Path, scope: &str, id: &str) -> Result<bool, HarnessError> {
    let slug = slugify(id);
    if slug.is_empty() {
        return Err(HarnessError::Config("invalid agent id".into()));
    }
    let dir = scope_dir(scope, workspace);
    let file = dir.join(format!("{slug}.md"));
    // Never delete outside the agents root, even if `id` contained `..`.
    if !file.starts_with(dir.clone()) {
        return Err(HarnessError::Config("invalid agent id".into()));
    }
    if !file.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&file)?;
    let mut dis = read_disabled(&dir);
    dis.retain(|d| d != &slug);
    let _ = write_disabled(&dir, &dis);
    Ok(true)
}

/// Enable or disable a profile without touching its Markdown file.
pub fn set_enabled(
    workspace: &Path,
    scope: &str,
    id: &str,
    enabled: bool,
) -> Result<bool, HarnessError> {
    let slug = slugify(id);
    if slug.is_empty() {
        return Err(HarnessError::Config("invalid agent id".into()));
    }
    let dir = scope_dir(scope, workspace);
    if !dir.join(format!("{slug}.md")).exists() {
        return Err(HarnessError::NotFound(format!("agent {slug}")));
    }
    let mut dis = read_disabled(&dir);
    let had = dis.iter().any(|d| d == &slug);
    if enabled {
        dis.retain(|d| d != &slug);
    } else if !had {
        dis.push(slug);
    }
    write_disabled(&dir, &dis)?;
    Ok(true)
}

/// Resolve one profile by id or name, project scope first (more specific),
/// enabled profiles only. Used by `spawn_agent` + `agent` and the team page's
/// launch button.
pub fn find(workspace: &Path, key: &str) -> Option<AgentDef> {
    let k = key.trim();
    if k.is_empty() {
        return None;
    }
    for scope in ["project", "user"] {
        for a in list(workspace, Some(scope)).agents {
            if (a.id == k || a.name == k) && a.enabled {
                return Some(a);
            }
        }
    }
    None
}

/// The "Saved agents" menu appended to the parent's system prompt: one line
/// per enabled profile. Delegation happens through `spawn_agent` + `agent`,
/// so this stays deliberately short — no prompt bodies here.
pub fn prompt_block(workspace: &Path) -> Option<String> {
    let listing = list(workspace, None);
    let enabled: Vec<&AgentDef> = listing.agents.iter().filter(|a| a.enabled).collect();
    if enabled.is_empty() {
        return None;
    }
    let mut s = String::from(
        "# Saved agents\n\
         Custom agent profiles you can delegate to. To spawn one, call `spawn_agent` \
         with `agent` set to its name — its system prompt, model and tool allowlist \
         are applied to the child automatically, and its name becomes the child's \
         identity in the roster.\n\n",
    );
    for a in enabled {
        s.push_str(&format!("- `{}` — {}\n", a.name, a.description));
    }
    Some(s)
}

// ---------------------------------------------------------------- helpers

/// Filename slug for a profile: letters, digits, CJK characters and dashes.
/// Unlike the skill slug this must keep CJK names intact — a profile named
/// 文档整理师 spawns a child wearing exactly that name.
fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.trim().chars() {
        if c.is_ascii_alphanumeric() || is_cjk(c) {
            if c.is_ascii_alphabetic() {
                out.push(c.to_ascii_lowercase());
            } else {
                out.push(c);
            }
            dash = false;
        } else if (c == '-' || c == '_' || c == ' ') && !out.is_empty() && !dash {
            out.push('-');
            dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3400..=0x4DBF   // CJK 扩展 A
        | 0x4E00..=0x9FFF // CJK 基本区
        | 0xF900..=0xFAFF // CJK 兼容表意
        | 0x20000..=0x2A6DF) // CJK 扩展 B
}

fn yaml_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Inverse of [`yaml_str`] for the quoted `name`/`description` values.
fn yaml_unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('"') => {
                    out.push('"');
                    chars.next();
                }
                Some('\\') => {
                    out.push('\\');
                    chars.next();
                }
                _ => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn disabled_path(dir: &Path) -> PathBuf {
    dir.join(".disabled.json")
}

fn read_disabled(dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(disabled_path(dir)) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(&text).unwrap_or_default()
}

fn write_disabled(dir: &Path, list: &[String]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let body = serde_json::to_vec_pretty(list).unwrap_or_default();
    crate::atomic_write(&disabled_path(dir), &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Every test here points the *user* scope at its own scratch dir through
    /// `PARABLE_HOME`, which is process-wide. Without this lock the tests race
    /// on that variable and see each other's agents.
    fn env_lock() -> &'static Mutex<()> {
        static L: OnceLock<Mutex<()>> = OnceLock::new();
        L.get_or_init(|| Mutex::new(()))
    }

    struct Scratch {
        root: PathBuf,
        /// What `PARABLE_HOME` was before this test claimed it.
        prev: Option<std::ffi::OsString>,
        _guard: MutexGuard<'static, ()>,
    }

    impl Drop for Scratch {
        /// Restore the process-wide variable so an unrelated test that runs
        /// after this one does not resolve `~/.parable` to a deleted tmp dir.
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var("PARABLE_HOME", v),
                None => std::env::remove_var("PARABLE_HOME"),
            }
        }
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            let guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
            let root = std::env::temp_dir().join(format!(
                "parable-agents-{tag}-{}-{}",
                std::process::id(),
                crate::event::now_ms()
            ));
            std::fs::create_dir_all(&root).unwrap();
            let prev = std::env::var_os("PARABLE_HOME");
            std::env::set_var("PARABLE_HOME", &root);
            Self {
                root,
                prev,
                _guard: guard,
            }
        }
        fn ws(&self) -> PathBuf {
            self.root.join("ws")
        }
    }

    fn input(name: &str) -> AgentInput {
        AgentInput {
            name: name.into(),
            description: "does a thing".into(),
            color: "blue".into(),
            model: String::new(),
            thinking: None,
            tools: vec![],
            system_prompt: "你是测试员。".into(),
            scope: Some("user".into()),
            rename_from: None,
        }
    }

    #[test]
    fn slug_keeps_cjk_and_folds_ascii() {
        assert_eq!(slugify("文档整理师"), "文档整理师");
        assert_eq!(slugify("Research Bot"), "research-bot");
        assert_eq!(slugify("统计 员"), "统计-员");
        assert_eq!(slugify("a/b:c"), "abc");
        assert_eq!(slugify("   "), "");
    }

    #[test]
    fn parses_front_matter_and_body() {
        let text = "---\nname: 文档整理师\ndescription: 归档文档\ncolor: cyan\nmodel: p/m\nthinking: 2\ntools:\n  - read_file\n  - grep\n---\n\n你是文档整理师。\n";
        let d = parse("文档整理师", "user", text).unwrap();
        assert_eq!(d.name, "文档整理师");
        assert_eq!(d.description, "归档文档");
        assert_eq!(d.color, "cyan");
        assert_eq!(d.model, "p/m");
        assert_eq!(d.thinking, Some(2));
        assert_eq!(d.tools, vec!["read_file", "grep"]);
        assert_eq!(d.system_prompt, "你是文档整理师。");
        assert!(d.warnings.is_empty());
    }

    #[test]
    fn parses_inline_tools_and_rejects_missing_name() {
        let d = parse("x", "user", "---\nname: x\ndescription: d\ntools: [bash, web_search]\n---\nbody").unwrap();
        assert_eq!(d.tools, vec!["bash", "web_search"]);
        let e = parse("x", "user", "---\ndescription: d\n---\nbody").unwrap_err();
        assert!(e.contains("name"), "missing name must be reported");
    }

    #[test]
    fn unknown_color_warns_and_falls_back() {
        let d = parse("x", "user", "---\nname: x\ndescription: d\ncolor: magenta\n---\nb").unwrap();
        assert_eq!(d.color, "blue");
        assert_eq!(d.warnings.len(), 1);
    }

    #[test]
    fn save_create_find_roundtrip() {
        let s = Scratch::new("roundtrip");
        let ws = s.ws();
        let def = save(&ws, &input("文档整理师")).unwrap();
        assert_eq!(def.id, "文档整理师");
        assert!(def.enabled);
        assert!(def.path.ends_with("文档整理师.md"));
        // 按 id 与按名都能找到
        assert_eq!(find(&ws, "文档整理师").unwrap().id, "文档整理师");
        // 重名文件：创建被拒
        let e = save(&ws, &input("文档整理师")).unwrap_err();
        assert!(e.to_string().contains("already exists"));
        // 名字不合法：拒绝
        let mut bad = input("bad name");
        bad.name = "has space".into();
        assert!(save(&ws, &bad).is_err());
        bad.name = "lead".into();
        assert!(save(&ws, &bad).is_err());
    }

    #[test]
    fn save_rename_moves_file() {
        let s = Scratch::new("rename");
        let ws = s.ws();
        save(&ws, &input("旧名字")).unwrap();
        let mut inp = input("新名字");
        inp.rename_from = Some("旧名字".into());
        let def = save(&ws, &inp).unwrap();
        assert_eq!(def.id, "新名字");
        assert!(!user_dir().join("旧名字.md").exists());
        assert!(user_dir().join("新名字.md").exists());
        assert!(find(&ws, "旧名字").is_none());
        assert_eq!(find(&ws, "新名字").unwrap().id, "新名字");
    }

    #[test]
    fn save_update_overwrites_and_keeps_disabled() {
        let s = Scratch::new("update");
        let ws = s.ws();
        save(&ws, &input("统计员")).unwrap();
        set_enabled(&ws, "user", "统计员", false).unwrap();
        // 原地编辑：rename_from 指向自己 → 覆盖，不报"已存在"。
        let mut inp = input("统计员");
        inp.description = "改过的描述".into();
        inp.rename_from = Some("统计员".into());
        let def = save(&ws, &inp).unwrap();
        assert_eq!(def.description, "改过的描述");
        assert!(!def.enabled, "an edited profile stays disabled");
        assert_eq!(list(&ws, Some("user")).agents.len(), 1);
        // 改名：禁用状态跟着迁到新 id。
        let mut inp = input("记账员");
        inp.rename_from = Some("统计员".into());
        save(&ws, &inp).unwrap();
        assert!(!find(&ws, "记账员").is_some(), "disabled profiles stay hidden");
        let dis = list(&ws, Some("user"));
        assert_eq!(dis.agents.len(), 1);
        assert!(!dis.agents[0].enabled);
        set_enabled(&ws, "user", "记账员", true).unwrap();
        assert!(find(&ws, "记账员").is_some());
    }

    #[test]
    fn toggle_and_remove() {
        let s = Scratch::new("toggle");
        let ws = s.ws();
        save(&ws, &input("统计员")).unwrap();
        assert!(set_enabled(&ws, "user", "统计员", false).unwrap());
        // find 只回 enabled 的 profile
        assert!(find(&ws, "统计员").is_none());
        assert!(set_enabled(&ws, "user", "统计员", true).unwrap());
        assert!(find(&ws, "统计员").is_some());
        // 未知的 id 报 NotFound
        assert!(matches!(
            set_enabled(&ws, "user", "nobody", true),
            Err(HarnessError::NotFound(_))
        ));
        assert!(remove(&ws, "user", "统计员").unwrap());
        assert!(!remove(&ws, "user", "统计员").unwrap());
        // 目录穿越被挡
        let e = remove(&ws, "user", "..").unwrap_err();
        assert!(e.to_string().contains("invalid agent id"));
    }

    #[test]
    fn project_scope_shadows_user() {
        let s = Scratch::new("scopes");
        let ws = s.ws();
        let mut u = input("评审员");
        u.scope = Some("user".into());
        save(&ws, &u).unwrap();
        assert_eq!(find(&ws, "评审员").unwrap().scope, "user");
        let mut p = input("评审员");
        p.scope = Some("project".into());
        p.system_prompt = "项目版。".into();
        save(&ws, &p).unwrap();
        // 项目作用域更具体，压过用户级同名 profile
        let found = find(&ws, "评审员").unwrap();
        assert_eq!(found.scope, "project");
        assert_eq!(found.system_prompt, "项目版。");
        // 作用域过滤
        assert_eq!(list(&ws, Some("user")).agents.len(), 1);
        assert_eq!(list(&ws, Some("project")).agents.len(), 1);
        assert_eq!(list(&ws, None).agents.len(), 2);
    }

    #[test]
    fn prompt_block_lists_enabled_only() {
        let s = Scratch::new("menu");
        let ws = s.ws();
        assert!(prompt_block(&ws).is_none());
        save(&ws, &input("文档整理师")).unwrap();
        save(&ws, &input("测试跑腿")).unwrap();
        set_enabled(&ws, "user", "测试跑腿", false).unwrap();
        let menu = prompt_block(&ws).unwrap();
        assert!(menu.contains("文档整理师"));
        assert!(!menu.contains("测试跑腿"));
        assert!(menu.contains("spawn_agent"));
    }

    #[test]
    fn render_parse_roundtrip() {
        let s = Scratch::new("render");
        let ws = s.ws();
        let mut inp = input("研究员");
        inp.color = "pink".into();
        inp.model = "zhipu/glm-5".into();
        inp.thinking = Some(3);
        inp.tools = vec!["read_file".into(), "grep".into()];
        inp.description = "带 \"引号\" 的描述".into();
        save(&ws, &inp).unwrap();
        let text = std::fs::read_to_string(user_dir().join("研究员.md")).unwrap();
        let d = parse("研究员", "user", &text).unwrap();
        assert_eq!(d.color, "pink");
        assert_eq!(d.model, "zhipu/glm-5");
        assert_eq!(d.thinking, Some(3));
        assert_eq!(d.tools, vec!["read_file", "grep"]);
        assert_eq!(d.description, "带 \"引号\" 的描述");
        assert_eq!(d.system_prompt, "你是测试员。");
    }
}
