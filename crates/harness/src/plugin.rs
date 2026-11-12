//! Plugin support — the declarative layer of dsh (deepseek-harness) plugins.
//!
//! dsh plugins are Cordis JS packages (`apply(ctx)`); without a JS runtime we
//! draw the line the dsh contract itself suggests: the **manifest half**
//! (commands, tool schemas, prompt sections, external-command tools) is fully
//! supported, while the **code half** is never executed. For dsh packages we
//! statically extract the declarations (the literal arguments of
//! `ctx.commands.register({…})` / `defineTool({…})`) so their names and
//! descriptions still show up.
//!
//! Layout:
//!   ~/.parable/plugins/<id>/plugin.json                    — manifest plugin
//!   ~/.parable/plugins/<id>/package.json + src/index.ts    — dsh package
//!   ~/.parable/plugins/enabled.json                        — {"<id>": bool}

use crate::error::HarnessError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PluginCommand {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Prompt template; `{{input}}` is replaced with the rest of the slash line.
    #[serde(default)]
    pub template: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PluginTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// JSON Schema for the arguments (passed to the model as-is when valid).
    #[serde(default)]
    pub parameters: Value,
    /// Shell command template; `{{args.<param>}}` placeholders are substituted.
    /// Empty on dsh-extracted declarations (their execute half is JS).
    #[serde(default)]
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PluginManifest {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub commands: Vec<PluginCommand>,
    #[serde(default)]
    pub tools: Vec<PluginTool>,
    /// Extra section appended to the system prompt while the plugin is enabled.
    #[serde(default)]
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plugin {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    /// "manifest" | "dsh"
    pub source: String,
    pub enabled: bool,
    pub commands: Vec<PluginCommand>,
    pub tools: Vec<PluginTool>,
    pub prompt: String,
    /// False when the package carries JS code we do not execute — declarations
    /// are shown, but nothing runs.
    pub extractable: bool,
}

pub struct PluginStore {
    root: std::path::PathBuf,
}

impl Default for PluginStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginStore {
    pub fn new() -> Self {
        let root = crate::config::Config::home_dir().join("plugins");
        let _ = std::fs::create_dir_all(&root);
        Self { root }
    }

    fn enabled_path(&self) -> std::path::PathBuf {
        self.root.join("enabled.json")
    }

    fn enabled_map(&self) -> HashMap<String, bool> {
        std::fs::read_to_string(self.enabled_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn toggle(&self, id: &str) -> Result<bool, HarnessError> {
        let mut m = self.enabled_map();
        let next = !m.get(id).copied().unwrap_or(true);
        m.insert(id.to_string(), next);
        let _ = std::fs::write(
            self.enabled_path(),
            serde_json::to_string_pretty(&m).unwrap_or_default(),
        );
        Ok(next)
    }

    pub fn scan(&self) -> Vec<Plugin> {
        let enabled = self.enabled_map();
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return out;
        };
        for e in rd.flatten() {
            let dir = e.path();
            if !dir.is_dir() {
                continue;
            }
            let id = dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let mut p = if dir.join("plugin.json").exists() {
                parse_manifest(&id, &dir)
            } else if dir.join("package.json").exists()
                || dir.join("src/index.ts").exists()
                || dir.join("lib/index.js").exists()
                || dir.join("index.js").exists()
            {
                parse_dsh(&id, &dir)
            } else {
                continue;
            };
            p.enabled = enabled.get(&id).copied().unwrap_or(true);
            out.push(p);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Enabled plugins' system-prompt sections, joined.
    pub fn prompt_sections(&self) -> String {
        self.scan()
            .into_iter()
            .filter(|p| p.enabled && !p.prompt.is_empty())
            .map(|p| format!("### Plugin: {}\n{}", p.name, p.prompt))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Write (create or replace) a manifest plugin's plugin.json.
    /// `id` must be a plain directory name — it becomes the plugin's identity.
    pub fn save_manifest(&self, id: &str, m: &PluginManifest) -> Result<std::path::PathBuf, HarnessError> {
        Self::check_id(id)?;
        let dir = self.root.join(id);
        std::fs::create_dir_all(&dir)
            .map_err(|e| HarnessError::Tool(format!("无法创建插件目录 {}: {e}", dir.display())))?;
        let json = serde_json::to_string_pretty(m)
            .map_err(|e| HarnessError::Tool(format!("manifest 序列化失败: {e}")))?;
        std::fs::write(dir.join("plugin.json"), json)
            .map_err(|e| HarnessError::Tool(format!("写入 plugin.json 失败: {e}")))?;
        Ok(dir)
    }

    /// Remove a manifest plugin's directory and its enabled flag.
    pub fn delete(&self, id: &str) -> Result<(), HarnessError> {
        Self::check_id(id)?;
        let dir = self.root.join(id);
        if !dir.starts_with(&self.root) || !dir.is_dir() {
            return Err(HarnessError::NotFound(format!("plugin {id}")));
        }
        std::fs::remove_dir_all(&dir)
            .map_err(|e| HarnessError::Tool(format!("删除插件目录失败: {e}")))?;
        let mut m = self.enabled_map();
        if m.remove(id).is_some() {
            let _ = std::fs::write(
                self.enabled_path(),
                serde_json::to_string_pretty(&m).unwrap_or_default(),
            );
        }
        Ok(())
    }

    /// Plugin ids are directory names under the plugins root — keep them
    /// plain so a crafted id can't escape the root.
    fn check_id(id: &str) -> Result<(), HarnessError> {
        let ok = !id.is_empty()
            && id.len() <= 64
            && id.chars().next().map(|c| c.is_ascii_lowercase()).unwrap_or(false)
            && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
            && id != "enabled";
        if ok {
            Ok(())
        } else {
            Err(HarnessError::Tool(
                "插件 id 必须是 1-64 位小写字母开头的 [a-z0-9_-]，且不能叫 enabled".into(),
            ))
        }
    }

    /// Enabled manifest tools that have an executable half.
    pub fn runnable_tools(&self) -> Vec<PluginTool> {
        self.scan()
            .into_iter()
            .filter(|p| p.enabled)
            .flat_map(|p| p.tools)
            .filter(|t| !t.command.is_empty())
            .collect()
    }
}

fn parse_manifest(id: &str, dir: &Path) -> Plugin {
    let raw = std::fs::read_to_string(dir.join("plugin.json")).unwrap_or_default();
    let m: PluginManifest = serde_json::from_str(&raw).unwrap_or_default();
    Plugin {
        id: id.into(),
        name: if m.name.is_empty() { id.into() } else { m.name },
        version: m.version,
        description: m.description,
        source: "manifest".into(),
        enabled: true,
        commands: m.commands,
        tools: m.tools,
        prompt: m.prompt,
        extractable: true,
    }
}

/// Best-effort static extraction of a dsh package's declarative half. The JS
/// itself is never executed; only the literal `name` / `description` /
/// `parameters` of register/defineTool calls are pulled out.
fn parse_dsh(id: &str, dir: &Path) -> Plugin {
    let pkg_raw = std::fs::read_to_string(dir.join("package.json")).unwrap_or_default();
    let pkg: Value = serde_json::from_str(&pkg_raw).unwrap_or(Value::Null);
    let name = pkg
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.trim_start_matches('@').to_string())
        .unwrap_or_else(|| id.into());
    let version = pkg
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let description = pkg
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let src = ["src/index.ts", "src/index.js", "lib/index.js", "index.js"]
        .iter()
        .map(|p| dir.join(p))
        .find(|p| p.exists())
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();

    let commands = extract_calls(&src, "commands.register")
        .into_iter()
        .map(|b| PluginCommand {
            name: field_str(&b, "name").unwrap_or_default(),
            description: field_str(&b, "description").unwrap_or_default(),
            template: String::new(),
        })
        .filter(|c| !c.name.is_empty())
        .collect();
    let tools = extract_calls(&src, "defineTool")
        .into_iter()
        .map(|b| PluginTool {
            name: field_str(&b, "name").unwrap_or_default(),
            description: field_str(&b, "description").unwrap_or_default(),
            parameters: field_json(&b, "parameters"),
            command: String::new(),
        })
        .filter(|t| !t.name.is_empty())
        .collect();

    Plugin {
        id: id.into(),
        name,
        version,
        description,
        source: "dsh".into(),
        enabled: true,
        commands,
        tools,
        prompt: String::new(),
        extractable: false,
    }
}

/// Grab the `{…}` block following each `<callee>(` occurrence.
fn extract_calls(src: &str, callee: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(i) = rest.find(callee) {
        let tail = &rest[i + callee.len()..];
        let Some(open) = tail.find('{') else { break };
        let block = &tail[open..];
        let end = block.find("})").map(|e| e + 2).unwrap_or(block.len().min(900));
        out.push(block[..end].to_string());
        rest = &tail[open + end..];
    }
    out
}

fn field_str(block: &str, key: &str) -> Option<String> {
    for q in ['"', '\''] {
        let pat = format!("{key}: {q}");
        if let Some(i) = block.find(&pat) {
            let rest = &block[i + pat.len()..];
            if let Some(e) = rest.find(q) {
                return Some(rest[..e].to_string());
            }
        }
        let pat = format!("{key}:{q}");
        if let Some(i) = block.find(&pat) {
            let rest = &block[i + pat.len()..];
            if let Some(e) = rest.find(q) {
                return Some(rest[..e].to_string());
            }
        }
    }
    None
}

fn field_json(block: &str, key: &str) -> Value {
    let pat = format!("{key}:");
    let Some(i) = block.find(&pat) else {
        return Value::Null;
    };
    let rest = &block[i + pat.len()..];
    let Some(open) = rest.find('{') else {
        return Value::Null;
    };
    let mut depth = 0usize;
    for (e, c) in rest[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let lit = &rest[open..open + e + 1];
                    return serde_json::from_str(&normalize_js_literal(lit))
                        .unwrap_or(Value::Null);
                }
            }
            _ => {}
        }
    }
    Value::Null
}

/// Best-effort JS object literal → JSON: single quotes become double quotes
/// and bare keys get quoted. Enough for the declarative `parameters: {…}`
/// literals found in dsh sources; not a JS parser.
fn normalize_js_literal(s: &str) -> String {
    let re_q = regex::Regex::new(r"'([^'\\]*)'").unwrap();
    let s2 = re_q.replace_all(s, "\"$1\"");
    let re_k = regex::Regex::new(r"([{,]\s*)([A-Za-z_][A-Za-z0-9_]*)\s*:").unwrap();
    re_k.replace_all(&s2, "$1\"$2\":").into_owned()
}

/// Run a manifest tool's external command with `{{args.<param>}}` substitution.
pub async fn run_plugin_tool(
    t: &PluginTool,
    args: &Value,
    ctx: &crate::tools::ToolCtx,
) -> Result<crate::tools::ToolOutcome, HarnessError> {
    use crate::tools::DisplayKind;
    ctx.authorize(&t.name, &t.description, crate::permission::Risk::Shell)
        .await?;
    let mut cmd = t.command.clone();
    if let Some(obj) = args.as_object() {
        for (k, v) in obj {
            let s = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            cmd = cmd.replace(&format!("{{{{args.{k}}}}}"), &shell_quote(&s));
        }
    }
    let out = if cfg!(target_os = "windows") {
        tokio::process::Command::new("cmd")
            .args(["/C", &cmd])
            .output()
            .await
    } else {
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .output()
            .await
    };
    let out =
        out.map_err(|e| HarnessError::Tool(format!("plugin tool failed to start: {e}")))?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        text.push_str("\n[stderr]\n");
        text.push_str(&err);
    }
    if text.len() > 24_000 {
        let mut end = 24_000;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n…[truncated]");
    }
    let ok = out.status.success();
    Ok(crate::tools::ToolOutcome {
        ok,
        title: format!("{} · {}", t.name, if ok { "ok" } else { "failed" }),
        display: DisplayKind::Plain(text.clone()),
        for_model: text,
    })
}

fn shell_quote(s: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_dsh_declarations() {
        let src = r#"
export const name = 'command-compact'
export function apply(ctx) {
  ctx.commands.register({
    definitionId: CommandDefinitionId('@x'),
    name: 'compact',
    description: 'Compact older conversation history',
    handler,
  })
  ctx.tools.register(defineTool({
    name: 'todo_write',
    description: 'Write the todo list',
    parameters: { type: 'object', properties: { items: { type: 'array' } } },
  }))
}
"#;
        let cmds = extract_calls(src, "commands.register");
        assert_eq!(cmds.len(), 1);
        assert_eq!(field_str(&cmds[0], "name").as_deref(), Some("compact"));
        assert!(field_str(&cmds[0], "description")
            .unwrap()
            .contains("Compact"));
        let tools = extract_calls(src, "defineTool");
        assert_eq!(tools.len(), 1);
        assert_eq!(field_str(&tools[0], "name").as_deref(), Some("todo_write"));
        assert!(field_json(&tools[0], "parameters").get("type").is_some());
    }

    #[test]
    fn manifest_round_trips() {
        let raw = r#"{"name":"demo","commands":[{"name":"hi","template":"say {{input}}"}],
            "tools":[{"name":"df","command":"df -h"}],"prompt":"Be nice."}"#;
        let m: PluginManifest = serde_json::from_str(raw).unwrap();
        assert_eq!(m.commands[0].name, "hi");
        assert_eq!(m.tools[0].command, "df -h");
        assert_eq!(m.prompt, "Be nice.");
    }

    #[test]
    fn shell_quote_escapes() {
        if cfg!(target_os = "windows") {
            assert_eq!(shell_quote("a'b"), "\"a'b\"");
            assert_eq!(shell_quote("x\"y"), "\"x\"\"y\"");
        } else {
            assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        }
    }

    #[test]
    fn rejects_bad_plugin_ids() {
        let tmp = std::env::temp_dir().join(format!("parable-plugbad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = PluginStore { root: tmp.clone() };
        let m = PluginManifest::default();
        for bad in ["", "../escape", ".hidden", "UPPER", "has space", "a/../../x", "enabled"] {
            assert!(
                store.save_manifest(bad, &m).is_err() && store.delete(bad).is_err(),
                "{bad} should be rejected"
            );
        }
        assert!(store.save_manifest("my-tool_2", &m).is_ok());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn manifest_save_and_delete_round_trip() {
        let tmp = std::env::temp_dir().join(format!("parable-plugtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = PluginStore { root: tmp.clone() };
        let m = PluginManifest {
            name: "tester".into(),
            version: "0.1.0".into(),
            description: "t".into(),
            commands: vec![PluginCommand {
                name: "hi".into(),
                description: String::new(),
                template: "say {{input}}".into(),
            }],
            tools: vec![],
            prompt: String::new(),
        };
        store.save_manifest("tester", &m).unwrap();
        let scanned = store.scan();
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].id, "tester");
        assert_eq!(scanned[0].commands[0].name, "hi");
        // delete removes both the dir and any enabled.json entry
        store.toggle("tester").unwrap();
        store.delete("tester").unwrap();
        assert!(store.scan().is_empty());
        assert!(store.enabled_map().get("tester").is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
