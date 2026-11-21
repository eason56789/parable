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

/// manifest 文件最大字节数（PL-06）：超出拒绝解析，防恶意插件塞满内存。
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
/// 内联脚本源码最大字节数（PL-06）。
const MAX_SCRIPT_BYTES: usize = 256 * 1024;

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
    /// Materialized from `code` when the plugin is created with inline source.
    #[serde(default)]
    pub command: String,
    /// Inline source written to `<plugin>/scripts/` on create. Optional; a
    /// manifest may instead ship a ready-made `command`.
    #[serde(default)]
    pub code: String,
    /// Interpreter for `code`: python | powershell | node (default python).
    #[serde(default)]
    pub language: String,
    /// Absolute path of the materialized script (set together with `code`).
    /// When present, the runner invokes the interpreter directly instead of
    /// going through `cmd /C`, which avoids Windows quote mangling.
    #[serde(default)]
    pub script: String,
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
        let root = crate::config::Config::plugins_dir();
        let _ = std::fs::create_dir_all(&root);
        Self { root }
    }

    fn enabled_path(&self) -> std::path::PathBuf {
        self.root.join("enabled.json")
    }

    fn enabled_map(&self) -> HashMap<String, bool> {
        match std::fs::read_to_string(self.enabled_path()) {
            Ok(t) => match serde_json::from_str::<HashMap<String, bool>>(&t) {
                Ok(m) => m,
                Err(e) => {
                    // PL-01：损坏的 enabled.json 回退空表并留痕，不当成全启用。
                    tracing::warn!(error = %e, "enabled.json 解析失败，按空表处理");
                    HashMap::new()
                }
            },
            Err(_) => HashMap::new(),
        }
    }

    pub fn toggle(&self, id: &str) -> Result<bool, HarnessError> {
        let mut m = self.enabled_map();
        let next = !m.get(id).copied().unwrap_or(false);
        m.insert(id.to_string(), next);
        // PL-01：原子写盘，避免断电/磁盘满留下半截 JSON。
        let bytes = serde_json::to_string_pretty(&m).unwrap_or_else(|_| "{}".into());
        crate::atomic_write(&self.enabled_path(), bytes.as_bytes())
            .map_err(|e| HarnessError::Tool(format!("写入 enabled.json 失败: {e}")))?;
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
            // PL-05：未在 enabled.json 显式登记的插件默认禁用，不随目录扫描自动启用。
            p.enabled = enabled.get(&id).copied().unwrap_or(false);
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
        // PL-02：命令/工具名只允许字母数字 - _ .，拒绝空格与 shell 元字符，
        // 避免后续拼命令行时被注入。
        for c in &m.commands {
            if !valid_decl_name(&c.name) {
                return Err(HarnessError::Tool(format!(
                    "命令名 `{}` 非法：只允许字母数字、`-`、`_`、`.`",
                    c.name
                )));
            }
        }
        for t in &m.tools {
            if !valid_decl_name(&t.name) {
                return Err(HarnessError::Tool(format!(
                    "工具名 `{}` 非法：只允许字母数字、`-`、`_`、`.`",
                    t.name
                )));
            }
        }
        let dir = self.root.join(id);
        std::fs::create_dir_all(&dir)
            .map_err(|e| HarnessError::Tool(format!("无法创建插件目录 {}: {e}", dir.display())))?;
        let json = serde_json::to_string_pretty(m)
            .map_err(|e| HarnessError::Tool(format!("manifest 序列化失败: {e}")))?;
        crate::atomic_write(&dir.join("plugin.json"), json.as_bytes())
            .map_err(|e| HarnessError::Tool(format!("写入 plugin.json 失败: {e}")))?;
        Ok(dir)
    }

    /// Materialize an inline-code tool into `<plugin>/scripts/<tool>.<ext>` and
    /// return the command that runs it. Arguments are delivered through the
    /// `PARABLE_TOOL_ARGS` environment variable (JSON), so no shell quoting of
    /// user data is involved.
    pub fn write_script(
        &self,
        id: &str,
        tool: &str,
        language: &str,
        code: &str,
    ) -> Result<(String, String), HarnessError> {
        Self::check_id(id)?;
        // PL-02：脚本文件名同样只允许安全字符，防路径穿越。
        if !valid_decl_name(tool) {
            return Err(HarnessError::Tool(format!(
                "工具名 `{tool}` 非法：只允许字母数字、`-`、`_`、`.`"
            )));
        }
        let lang = if language.trim().is_empty() {
            "python".to_string()
        } else {
            language.trim().to_ascii_lowercase()
        };
        let (ext, runner) = match lang.as_str() {
            "python" => (
                "py",
                if cfg!(target_os = "windows") {
                    "python".to_string()
                } else {
                    "python3".to_string()
                },
            ),
            "powershell" | "pwsh" => (
                "ps1",
                if cfg!(target_os = "windows") {
                    "powershell -NoProfile -ExecutionPolicy Bypass -File".to_string()
                } else {
                    "pwsh -NoProfile -File".to_string()
                },
            ),
            "node" | "javascript" | "js" => ("js", "node".to_string()),
            other => {
                return Err(HarnessError::Tool(format!(
                    "不支持的内联语言 `{other}`，可用 python / powershell / node"
                )))
            }
        };
        let dir = self.root.join(id).join("scripts");
        std::fs::create_dir_all(&dir)
            .map_err(|e| HarnessError::Tool(format!("无法创建脚本目录 {}: {e}", dir.display())))?;
        // PL-06：内联脚本源码带上限，防超大代码块写盘/执行。
        if code.len() > MAX_SCRIPT_BYTES {
            return Err(HarnessError::Tool(format!(
                "脚本源码超过 {MAX_SCRIPT_BYTES} 字节，已拒绝写入"
            )));
        }
        let path = dir.join(format!("{tool}.{ext}"));
        // Windows PowerShell 5.1 对无 BOM 的 UTF-8 按 ANSI 解析：中文注释/字符串
        // 会乱码甚至打破语法。ps1 统一补 UTF-8 BOM（python/node 不需要）。
        let bytes = if ext == "ps1" && !code.starts_with('\u{FEFF}') {
            let mut b = vec![0xEF, 0xBB, 0xBF];
            b.extend_from_slice(code.as_bytes());
            b
        } else {
            code.as_bytes().to_vec()
        };
        std::fs::write(&path, bytes)
            .map_err(|e| HarnessError::Tool(format!("写入脚本 {} 失败: {e}", path.display())))?;
        let script_path = path.to_string_lossy().to_string();
        Ok((format!("{runner} \"{}\"", path.display()), script_path))
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
            let bytes = serde_json::to_string_pretty(&m).unwrap_or_else(|_| "{}".into());
            let _ = crate::atomic_write(&self.enabled_path(), bytes.as_bytes());
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

/// 命令/工具声明名合法性（PL-02）：非空、≤64 字符，只含字母数字 `-_.`。
fn valid_decl_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// 读文本文件并带上限（PL-06）：超限或读取失败返回 None。
fn read_capped(path: &Path, max: usize) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() as usize > max {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

fn parse_manifest(id: &str, dir: &Path) -> Plugin {
    let raw = read_capped(&dir.join("plugin.json"), MAX_MANIFEST_BYTES).unwrap_or_default();
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
            code: String::new(),
            language: String::new(),
            script: String::new(),
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
    // 先渲染出真正要执行的命令,再进权限链:面板能看到原样命令(而不再只是
    // description),Always 规则也按渲染后的命令精确匹配。
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
    if !t.script.is_empty() {
        // 内联代码工具:权限面板展示真正会被执行的源码,而不是只有一句路径命令。
        let lang = if t.language.trim().is_empty() {
            "python"
        } else {
            t.language.trim()
        };
        ctx.authorize_with_reason(
            &t.name,
            &t.description,
            crate::permission::Risk::Shell,
            &format!("run inline {lang} tool script {}", t.script),
            Some(&t.code),
        )
        .await?;
    } else {
        // Windows 限制:cmd.exe 在双引号内仍会展开 %VAR%,参数携带的 %VAR% 是一个
        // 环境变量注入/泄露面。宁可拒绝也不悄悄展开(POSIX 半没有这个问题)。
        if cfg!(target_os = "windows") && cmd.contains('%') {
            return Err(HarnessError::Tool(
                "plugin command contains `%`: Windows 会展开 %VAR%,存在注入/泄露风险,已拒绝执行(请改写命令避免 %)".into(),
            ));
        }
        ctx.authorize_shell(&t.name, &t.description, crate::permission::Risk::Shell, &cmd)
            .await?;
    }
    // 参数同时以 JSON 环境变量交给脚本，避免内联代码工具在引号/转义上翻车；
    // 传统 command 模板仍可用 {{args.x}}。
    let args_json = serde_json::to_string(args).unwrap_or_else(|_| "{}".into());
    let workspace = ctx.workspace.to_string_lossy().to_string();
    let mut command = if !t.script.is_empty() {
        // 内联代码工具:解释器直接拿脚本路径作为独立 argv,不经 cmd/sh 拼字符串,
        // 避免 Windows `cmd /C` 在路径引号上二次转义。
        let lang = t.language.trim().to_ascii_lowercase();
        let c = if lang == "powershell" || lang == "pwsh" {
            let exe = if cfg!(target_os = "windows") {
                "powershell"
            } else {
                "pwsh"
            };
            let mut c = tokio::process::Command::new(exe);
            if cfg!(target_os = "windows") {
                c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
                    .arg(&t.script);
            } else {
                c.args(["-NoProfile", "-File"]).arg(&t.script);
            }
            c
        } else if lang == "node" || lang == "javascript" || lang == "js" {
            let mut c = tokio::process::Command::new("node");
            c.arg(&t.script);
            c
        } else {
            let exe = if cfg!(target_os = "windows") {
                "python"
            } else {
                "python3"
            };
            let mut c = tokio::process::Command::new(exe);
            c.arg(&t.script);
            c
        };
        c
    } else if cfg!(target_os = "windows") {
        let mut c = tokio::process::Command::new("cmd");
        // PL-03：raw_arg 原样透传整条命令，避免 MSVCRT 引号转义被 cmd.exe 弄坏。
        c.raw_arg("/C");
        c.raw_arg(&cmd);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(&cmd);
        c
    };
    // SEC-01：清空继承环境，只回填运行必需的系统变量；PARABLE_* 显式项随后注入。
    command.env_clear();
    for key in crate::CHILD_ENV_WHITELIST {
        if let Ok(v) = std::env::var(key) {
            command.env(key, v);
        }
    }
    command
        .env("PARABLE_TOOL_ARGS", &args_json)
        .env("PARABLE_TOOL_NAME", &t.name)
        .env("PARABLE_TOOL_WORKSPACE", &workspace)
        .kill_on_drop(true);
    let out = match tokio::time::timeout(std::time::Duration::from_secs(120), command.output()).await
    {
        Ok(r) => r,
        Err(_) => {
            return Err(HarnessError::Tool(format!(
                "plugin tool `{}` timed out after 120s",
                t.name
            )))
        }
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

    #[test]
    fn writes_inline_tool_script() {
        let tmp = std::env::temp_dir().join(format!(
            "parable-script-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = PluginStore { root: tmp.clone() };
        let (cmd, script) = store
            .write_script("agent-tools", "text_stats", "python", "print('ok')")
            .unwrap();
        assert!(Path::new(&script).exists(), "script not written: {script}");
        assert!(script.ends_with("text_stats.py"));
        assert!(cmd.contains("text_stats.py"));
        assert_eq!(std::fs::read_to_string(&script).unwrap(), "print('ok')");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
