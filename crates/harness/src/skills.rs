//! Skills — reusable workflow documents the agent loads on demand.
//!
//! A skill is a directory containing `SKILL.md`:
//!
//! ```text
//! ---
//! name: release-notes
//! description: Turn the git log into grouped Chinese release notes.
//! ---
//!
//! # Steps
//! 1. `git log --oneline <last-tag>..HEAD`
//! …
//! ```
//!
//! Two scopes exist, mirroring how Claude Code and dsh lay them out:
//!   - user:    `~/.parable/skills/<name>/SKILL.md`
//!   - project: `<workspace>/.parable/skills/<name>/SKILL.md`
//!
//! Only the YAML-ish front matter is parsed — we need `name`, `description` and
//! an optional `enabled: false`. The body stays opaque and is handed to the
//! model verbatim when the skill is loaded.
//!
//! Disabled skills are recorded in `<scope>/.disabled.json` as a name list
//! rather than by rewriting every SKILL.md, so the user's files stay untouched.
//!
//! The system prompt only carries each skill's name + description (a "menu");
//! the body is read by the `read_skill` tool when the model decides it needs it.
//! That keeps a big skill library from eating the context window every turn.

use crate::error::HarnessError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Front matter + body of one skill file.
#[derive(Debug, Clone, Serialize)]
pub struct Skill {
    /// Directory name, which is also the skill's stable id.
    pub id: String,
    pub name: String,
    pub description: String,
    /// "user" | "project"
    pub scope: String,
    pub enabled: bool,
    /// Absolute path to the `SKILL.md`.
    pub path: String,
    /// Markdown body after the front matter.
    pub body: String,
    /// Non-fatal problems found while loading (shown in the 诊断 panel).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

/// A file that failed to load at all.
#[derive(Debug, Clone, Serialize)]
pub struct SkillProblem {
    pub path: String,
    pub reason: String,
    /// "user" | "project" — which scope the bad file belongs to.
    pub scope: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillListing {
    pub skills: Vec<Skill>,
    pub problems: Vec<SkillProblem>,
}

/// Body of `POST /api/skills`.
#[derive(Debug, Deserialize)]
pub struct SkillInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub body: String,
    /// "user" | "project"; defaults to user.
    #[serde(default)]
    pub scope: Option<String>,
    /// Renaming: the directory name to replace. Empty when creating.
    #[serde(default)]
    pub rename_from: Option<String>,
}

/// Where the two scopes live for this process.
pub fn user_dir() -> PathBuf {
    crate::config::Config::home_dir().join("skills")
}

pub fn project_dir(workspace: &Path) -> PathBuf {
    workspace.join(".parable").join("skills")
}

fn scope_dir(scope: &str, workspace: &Path) -> PathBuf {
    match scope {
        "project" => project_dir(workspace),
        _ => user_dir(),
    }
}

/// Load every skill in both scopes.
///
/// A malformed file is reported in `problems` instead of aborting the listing —
/// one bad skill must not hide the rest.
pub fn list(workspace: &Path, scope: Option<&str>) -> SkillListing {
    let mut out = SkillListing {
        skills: Vec::new(),
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
    out.skills
        .sort_by(|a, b| a.scope.cmp(&b.scope).then(a.name.cmp(&b.name)));
    out
}

fn scan(dir: &Path, scope: &str, disabled: &[String], out: &mut SkillListing) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let file = p.join("SKILL.md");
        if !file.exists() {
            continue;
        }
        let id = p
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let Ok(text) = std::fs::read_to_string(&file) else {
            out.problems.push(SkillProblem {
                path: file.display().to_string(),
                reason: "could not read the file".into(),
                scope: scope.to_string(),
            });
            continue;
        };
        match parse(&id, scope, &text) {
            Ok(mut skill) => {
                skill.path = file.display().to_string();
                skill.enabled = !disabled.iter().any(|d| d == &skill.id);
                out.skills.push(skill);
            }
            Err(reason) => out.problems.push(SkillProblem {
                path: file.display().to_string(),
                reason,
                scope: scope.to_string(),
            }),
        }
    }
}

/// Split `---` front matter from the body.
fn parse(id: &str, scope: &str, text: &str) -> Result<Skill, String> {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("").trim();
    if first != "---" {
        return Err("SKILL.md must start with a `---` front matter block".into());
    }
    let mut name = String::new();
    let mut description = String::new();
    let mut enabled_flag: Option<bool> = None;
    let mut body_start = 0usize;
    let mut closed = false;
    let mut consumed = first.len() + 1;
    for line in lines.by_ref() {
        consumed += line.len() + 1;
        let t = line.trim();
        if t == "---" {
            closed = true;
            body_start = consumed;
            break;
        }
        let Some((k, v)) = t.split_once(':') else {
            continue;
        };
        let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
        match k.trim() {
            "name" => name = v,
            "description" => description = v,
            "enabled" => enabled_flag = Some(!matches!(v.as_str(), "false" | "no" | "0")),
            _ => {}
        }
    }
    if !closed {
        return Err("front matter is missing its closing `---`".into());
    }
    if description.is_empty() {
        return Err("front matter is missing the required `description` field".into());
    }
    let body = text.get(body_start..).unwrap_or("").trim_start().to_string();
    let mut warnings = Vec::new();
    if name.is_empty() {
        name = id.to_string();
        warnings.push("no `name` in front matter; using the directory name".into());
    }
    Ok(Skill {
        id: id.to_string(),
        name,
        description,
        scope: scope.to_string(),
        // `enabled: false` in the file is respected on top of the sidecar list.
        enabled: enabled_flag.unwrap_or(true),
        path: String::new(),
        body,
        warnings,
    })
}

/// Render a SKILL.md from its parts.
fn render(name: &str, description: &str, body: &str) -> String {
    let mut s = String::from("---\n");
    s.push_str(&format!("name: {name}\n"));
    s.push_str(&format!("description: {description}\n"));
    s.push_str("---\n\n");
    s.push_str(body.trim_start());
    s.push('\n');
    s
}

/// Create or update a skill. `rename_from` moves an existing directory.
pub fn save(
    workspace: &Path,
    input: &SkillInput,
) -> Result<Skill, HarnessError> {
    let slug = slugify(&input.name);
    if slug.is_empty() {
        return Err(HarnessError::Config(
            "skill name must contain lowercase letters, digits or dashes".into(),
        ));
    }
    if input.description.trim().is_empty() {
        return Err(HarnessError::Config("description is required".into()));
    }
    let scope = input.scope.as_deref().unwrap_or("user");
    let dir = scope_dir(scope, workspace);
    let target = dir.join(&slug);
    std::fs::create_dir_all(&target)?;

    // Renaming: drop the old directory once the new one is written.
    let old_slug = input
        .rename_from
        .as_deref()
        .map(slugify)
        .filter(|s| !s.is_empty() && *s != slug);

    let file = target.join("SKILL.md");
    let text = render(&slug, input.description.trim(), &input.body);
    crate::atomic_write(&file, text.as_bytes())?;

    if let Some(old) = old_slug {
        let old_dir = dir.join(&old);
        if old_dir.exists() && old_dir != target {
            let _ = std::fs::remove_dir_all(&old_dir);
        }
        let mut dis = read_disabled(&dir);
        dis.retain(|d| d != &old);
        let _ = write_disabled(&dir, &dis);
    }

    parse(&slug, scope, &text)
        .map(|mut s| {
            s.path = file.display().to_string();
            s
        })
        .map_err(|e| HarnessError::Config(e))
}

/// Delete a skill directory.
pub fn remove(workspace: &Path, scope: &str, id: &str) -> Result<bool, HarnessError> {
    let slug = slugify(id);
    if slug.is_empty() {
        return Err(HarnessError::Config("invalid skill id".into()));
    }
    let dir = scope_dir(scope, workspace).join(&slug);
    // Never delete outside the skills root, even if `id` contained `..`.
    if !dir.starts_with(scope_dir(scope, workspace)) {
        return Err(HarnessError::Config("invalid skill id".into()));
    }
    if !dir.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&dir)?;
    let mut dis = read_disabled(&scope_dir(scope, workspace));
    dis.retain(|d| d != &slug);
    let _ = write_disabled(&scope_dir(scope, workspace), &dis);
    Ok(true)
}

/// Enable or disable a skill without touching its SKILL.md.
pub fn set_enabled(
    workspace: &Path,
    scope: &str,
    id: &str,
    enabled: bool,
) -> Result<bool, HarnessError> {
    let slug = slugify(id);
    if slug.is_empty() {
        return Err(HarnessError::Config("invalid skill id".into()));
    }
    let dir = scope_dir(scope, workspace);
    if !dir.join(&slug).join("SKILL.md").exists() {
        return Err(HarnessError::NotFound(format!("skill {slug}")));
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

/// Load one skill's full body, for the `read_skill` tool.
pub fn read_body(workspace: &Path, scope: &str, id: &str) -> Option<Skill> {
    let slug = slugify(id);
    let file = scope_dir(scope, workspace).join(&slug).join("SKILL.md");
    let text = std::fs::read_to_string(&file).ok()?;
    let mut s = parse(&slug, scope, &text).ok()?;
    s.path = file.display().to_string();
    s.enabled = true;
    Some(s)
}

/// The "menu" appended to the system prompt: one line per enabled skill.
///
/// Deliberately short — the body is fetched with `read_skill` only when the
/// model decides the skill applies.
pub fn prompt_block(workspace: &Path) -> Option<String> {
    let listing = list(workspace, None);
    let enabled: Vec<&Skill> = listing.skills.iter().filter(|s| s.enabled).collect();
    if enabled.is_empty() {
        return None;
    }
    let mut s = String::from(
        "## Skills\n\
         Reusable procedures you can follow. When a request matches one of these, \
         call `read_skill` with its id first and follow the steps in the document \
         instead of improvising.\n\n",
    );
    for sk in enabled {
        s.push_str(&format!("- `{}` — {}\n", sk.id, sk.description));
    }
    Some(s)
}

// ---------------------------------------------------------------- helpers

/// Lowercase letters, digits and dashes only.
fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if (c == '-' || c == '_' || c == ' ') && !out.is_empty() && !dash {
            out.push('-');
            dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
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
    /// on that variable and see each other's skills.
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
                "parable-skills-{tag}-{}-{}",
                std::process::id(),
                crate::event::now_ms()
            ));
            std::fs::create_dir_all(&root).unwrap();
            // Point the user scope at the scratch dir.
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

    fn write_skill(dir: &Path, slug: &str, text: &str) {
        let d = dir.join(slug);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), text).unwrap();
    }

    #[test]
    fn parses_front_matter_and_body() {
        let text = "---\nname: demo\ndescription: does a thing\n---\n\n# Steps\n1. go\n";
        let s = parse("demo", "user", text).unwrap();
        assert_eq!(s.name, "demo");
        assert_eq!(s.description, "does a thing");
        assert!(s.body.starts_with("# Steps"));
        assert!(s.warnings.is_empty());
    }

    #[test]
    fn rejects_a_missing_description() {
        let text = "---\nname: demo\n---\nbody\n";
        let e = parse("demo", "user", text).unwrap_err();
        assert!(e.contains("description"), "got: {e}");
    }

    #[test]
    fn rejects_unterminated_front_matter() {
        let text = "---\nname: demo\ndescription: x\n";
        let e = parse("demo", "user", text).unwrap_err();
        assert!(e.contains("closing"), "got: {e}");
    }

    #[test]
    fn falls_back_to_the_directory_name() {
        let text = "---\ndescription: x\n---\nbody\n";
        let s = parse("dir-name", "user", text).unwrap();
        assert_eq!(s.name, "dir-name");
        assert_eq!(s.warnings.len(), 1);
    }

    #[test]
    fn slugify_normalises() {
        assert_eq!(slugify("Release Notes"), "release-notes");
        assert_eq!(slugify("  a_b__c  "), "a-b-c");
        assert_eq!(slugify("---"), "");
        assert_eq!(slugify("中文"), "");
    }

    #[tokio::test]
    async fn round_trips_through_disk() {
        let sc = Scratch::new("rt");
        let ws = sc.ws();
        let saved = save(
            &ws,
            &SkillInput {
                name: "Release Notes".into(),
                description: "group commits".into(),
                body: "# Steps\n1. git log".into(),
                scope: Some("project".into()),
                rename_from: None,
            },
        )
        .unwrap();
        assert_eq!(saved.id, "release-notes");
        let listing = list(&ws, Some("project"));
        assert_eq!(listing.skills.len(), 1);
        assert!(listing.skills[0].body.contains("git log"));
    }

    #[tokio::test]
    async fn disable_writes_a_sidecar_not_the_skill_file() {
        let sc = Scratch::new("dis");
        let ws = sc.ws();
        save(
            &ws,
            &SkillInput {
                name: "t".into(),
                description: "d".into(),
                body: "b".into(),
                scope: Some("project".into()),
                rename_from: None,
            },
        )
        .unwrap();
        set_enabled(&ws, "project", "t", false).unwrap();
        let listing = list(&ws, Some("project"));
        assert!(!listing.skills[0].enabled);
        // The SKILL.md itself is untouched.
        let text = std::fs::read_to_string(project_dir(&ws).join("t/SKILL.md")).unwrap();
        assert!(!text.contains("enabled"));
        // And the prompt menu skips it.
        assert!(prompt_block(&ws).is_none());
        set_enabled(&ws, "project", "t", true).unwrap();
        assert!(prompt_block(&ws).unwrap().contains("`t` — d"));
    }

    #[tokio::test]
    async fn rename_moves_the_directory() {
        let sc = Scratch::new("ren");
        let ws = sc.ws();
        save(
            &ws,
            &SkillInput {
                name: "old".into(),
                description: "d".into(),
                body: "b".into(),
                scope: Some("user".into()),
                rename_from: None,
            },
        )
        .unwrap();
        save(
            &ws,
            &SkillInput {
                name: "new".into(),
                description: "d2".into(),
                body: "b2".into(),
                scope: Some("user".into()),
                rename_from: Some("old".into()),
            },
        )
        .unwrap();
        let ids: Vec<String> = list(&ws, Some("user")).skills.into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["new".to_string()]);
    }

    #[tokio::test]
    async fn delete_removes_the_directory() {
        let sc = Scratch::new("rm");
        let ws = sc.ws();
        save(
            &ws,
            &SkillInput {
                name: "gone".into(),
                description: "d".into(),
                body: "b".into(),
                scope: Some("project".into()),
                rename_from: None,
            },
        )
        .unwrap();
        assert!(remove(&ws, "project", "gone").unwrap());
        assert!(!remove(&ws, "project", "gone").unwrap());
        assert!(list(&ws, Some("project")).skills.is_empty());
    }

    #[test]
    fn a_broken_file_does_not_hide_the_others() {
        let sc = Scratch::new("broken");
        let ws = sc.ws();
        let dir = project_dir(&ws);
        write_skill(&dir, "good", "---\ndescription: ok\n---\nbody\n");
        write_skill(&dir, "bad", "no front matter\n");
        let listing = list(&ws, Some("project"));
        assert_eq!(listing.skills.len(), 1);
        assert_eq!(listing.skills[0].id, "good");
        assert_eq!(listing.problems.len(), 1);
        assert!(listing.problems[0].reason.contains("front matter"));
    }

    #[test]
    fn rejects_traversal_ids() {
        let sc = Scratch::new("trav");
        let ws = sc.ws();
        assert!(remove(&ws, "project", "../../etc").unwrap_or(false) == false);
    }
}
