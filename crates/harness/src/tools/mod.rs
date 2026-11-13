//! The tool set the agent can call. Every tool returns a [`ToolOutcome`] which
//! carries three things at once: a collapsed-card title, a structured payload the
//! frontend renders, and the text the model actually reads.

pub mod fs;
pub mod shell;
pub mod search;
pub mod web;

use crate::config::Config;
use crate::error::HarnessError;
use crate::event::EventHub;
use crate::permission::{PermissionBroker, Risk};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ------------------------------------------------------------------ outputs

#[derive(Debug, Clone, Serialize)]
pub struct DiffLine {
    /// ` ` context, `+` added, `-` removed
    pub sign: char,
    pub old_no: Option<u64>,
    pub new_no: Option<u64>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: String,
}

#[derive(Debug, Clone)]
pub enum DisplayKind {
    Plain(String),
    FileRead {
        path: String,
        lines: Vec<(u64, String)>,
        total: u64,
    },
    FileDiff {
        path: String,
        lines: Vec<DiffLine>,
        added: usize,
        removed: usize,
    },
    Shell {
        command: String,
        stdout: String,
        stderr: String,
        exit: i32,
    },
    Ask {
        question: String,
        options: Vec<String>,
    },
    Todo(Vec<TodoItem>),
    /// Screenshot / computer-use image, base64 inline.
    Image {
        media_type: String,
        data: String,
    },
}

impl DisplayKind {
    /// Serialize into the shape the frontend `renderToolCard` expects.
    pub fn to_payload(&self) -> Value {
        match self {
            DisplayKind::Plain(s) => json!({"kind":"plain","text": s}),
            DisplayKind::FileRead { path, lines, total } => json!({
                "kind":"read","path": path,"total": total,
                "lines": lines.iter().map(|(n, t)| json!({"n": n, "text": t})).collect::<Vec<_>>()
            }),
            DisplayKind::FileDiff { path, lines, added, removed } => json!({
                "kind":"diff","path": path,"added": added,"removed": removed,
                "lines": lines.iter().map(|l| json!({"sign": l.sign.to_string(), "old": l.old_no, "new": l.new_no, "text": l.text})).collect::<Vec<_>>()
            }),
            DisplayKind::Shell { command, stdout, stderr, exit } => json!({
                "kind":"shell","command": command,"stdout": stdout,"stderr": stderr,"exit": exit
            }),
            DisplayKind::Ask { question, options } => json!({
                "kind":"ask","question": question,"options": options
            }),
            DisplayKind::Todo(items) => json!({
                "kind":"todo","todos": items
            }),
            DisplayKind::Image { media_type, data } => json!({
                "kind":"image","media_type": media_type,"data": data
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub ok: bool,
    /// Short text for the collapsed card header, e.g. `Read src/main.rs · 214 lines`.
    pub title: String,
    pub display: DisplayKind,
    /// Text handed back to the model.
    pub for_model: String,
}

impl ToolOutcome {
    pub fn text(ok: bool, title: impl Into<String>, for_model: impl Into<String>) -> Self {
        let fm = for_model.into();
        Self {
            ok,
            title: title.into(),
            display: DisplayKind::Plain(fm.clone()),
            for_model: fm,
        }
    }
}

// ------------------------------------------------------------------ context

/// Type-erased handle used by `spawn_agent` to start a child turn.
///
/// This must be a trait object, not a concrete `AgentRuntime`: `AgentRuntime`'s
/// async methods return opaque futures whose type would otherwise close a cycle
/// through `ToolCtx`. Erasing the type here is what breaks it.
#[async_trait::async_trait]
pub trait TurnStarter: Send + Sync {
    async fn start_child(
        &self,
        session_id: &str,
        prompt: &str,
        model: &str,
    ) -> Result<String, HarnessError>;

    /// 带 TurnOptions 启动子轮（子代理继承父轮权限/思考档位用）。
    /// 同样必须经由 trait 对象调用：保持 future 类型擦除，避免
    /// `start_turn` 的 opaque future 经 tokio::spawn 的 Send 检查
    /// 递归套回自身（E0391 cycle）。
    async fn start_child_opts(
        &self,
        session_id: &str,
        prompt: &str,
        opts: crate::agent::TurnOptions,
    ) -> Result<String, HarnessError> {
        // 默认兜底：退化为无 opts 的路径（读全局配置）。
        self.start_child(session_id, prompt, &opts.model_sel).await
    }
}

pub struct ToolCtx {
    pub workspace: PathBuf,
    pub permission_mode: String,
    pub plan_mode: bool,
    pub broker: Arc<PermissionBroker>,
    pub questions: Arc<crate::agent::QuestionBroker>,
    pub hub: EventHub,
    pub session_id: String,
    pub call_id: String,
    /// Present when scheduled tasks are mounted; `schedule_*` tools need it.
    pub schedule: Option<Arc<crate::schedule::ScheduleService>>,
    /// Present when subagents are mounted; `agent_*` tools need it.
    pub subagents: Option<Arc<crate::subagent::SubagentService>>,
    /// Agent runtime, so `spawn_agent` can start a child turn.
    pub runtime: Option<Arc<dyn TurnStarter>>,
    /// DeepSeek Search API key; empty disables `deepseek_search`.
    pub search_key: String,
    /// User's preferred search source: "auto" | "deepseek" | "anysearch" | "multi".
    pub search_pref: String,
    /// Computer-use bridge (CDP); present when the runtime mounts it.
    pub computer: Option<Arc<crate::computer::ComputerBridge>>,
}

impl ToolCtx {
    /// 所有权限入口的统一闸门:plan 模式白名单 → ~/.parable 硬拒 → 规则/模式
    /// 判定(auto_allow)→ 挂起等用户决策。保证每条路径都不漏检查与审计。
    async fn gate(
        &self,
        tool: &str,
        path: &str,
        risk: Risk,
        command: Option<&str>,
        reason: &str,
        preview: Option<&str>,
    ) -> Result<(), HarnessError> {
        if self.plan_mode
            && !matches!(
                tool,
                "read_file" | "glob" | "grep" | "list_dir" | "todo_write" | "ask_user"
            )
        {
            return Err(HarnessError::PermissionDenied(
                "plan mode is on: only read-only tools and planning are allowed".into(),
            ));
        }
        // 只读工具一律不碰 ~/.parable 内部(config/permissions/sessions/logs/
        // uploads):直接报错,不弹确认面板。
        if matches!(tool, "read_file" | "glob" | "grep" | "list_dir")
            && crate::permission::is_parable_internal(Path::new(path))
        {
            return Err(HarnessError::PermissionDenied(
                "refusing to touch ~/.parable internals".into(),
            ));
        }
        if self
            .broker
            .auto_allow(
                &self.permission_mode,
                tool,
                path,
                command,
                risk,
                &self.workspace,
            )
            .await
        {
            // 规则/模式放行同样留痕,decision 标 "auto"。
            self.broker.audit_auto(&self.call_id, tool, path, risk).await;
            self.broker.note_decision(&self.call_id, "auto").await;
            return Ok(());
        }
        let d = self
            .broker
            .request(
                &self.call_id,
                tool,
                path,
                risk,
                reason,
                preview.map(|s| s.to_string()),
                command,
            )
            .await?;
        if d.allowed() {
            Ok(())
        } else {
            Err(HarnessError::PermissionDenied(format!(
                "user declined {tool} on {path}"
            )))
        }
    }

    pub async fn authorize(&self, tool: &str, path: &str, risk: Risk) -> Result<(), HarnessError> {
        self.gate(tool, path, risk, None, "", None).await
    }

    /// Like [`authorize`] but carries a diff preview into the permission panel.
    pub async fn authorize_with_preview(
        &self,
        tool: &str,
        path: &str,
        risk: Risk,
        preview: &str,
    ) -> Result<(), HarnessError> {
        self.gate(tool, path, risk, None, "", Some(preview)).await
    }

    /// shell 类工具(bash / 插件命令):渲染后的命令原文进入权限链——面板的
    /// reason 里能看到真正要执行的命令,Always 规则也按命令精确匹配。
    pub async fn authorize_shell(
        &self,
        tool: &str,
        path: &str,
        risk: Risk,
        command: &str,
    ) -> Result<(), HarnessError> {
        self.gate(
            tool,
            path,
            risk,
            Some(command),
            &format!("command: {command}"),
            None,
        )
        .await
    }

    /// 自定义 reason / 预览内容的权限请求(如插件 manifest 全文)。
    pub async fn authorize_with_reason(
        &self,
        tool: &str,
        path: &str,
        risk: Risk,
        reason: &str,
        preview: Option<&str>,
    ) -> Result<(), HarnessError> {
        self.gate(tool, path, risk, None, reason, preview).await
    }
}

// ------------------------------------------------------------------ schema helpers

fn obj(props: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false
    })
}

fn str_prop(desc: &str) -> Value {
    json!({"type":"string","description": desc})
}

fn int_prop(desc: &str) -> Value {
    json!({"type":"integer","description": desc})
}

fn bool_prop(desc: &str) -> Value {
    json!({"type":"boolean","description": desc})
}

// ------------------------------------------------------------------ images

/// 扩展名 → 图片 MIME。只认常见的截图/网页图片格式。
fn image_mime(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

/// 魔数校验：扩展名说它是图，文件头也得像。防呆，不做完整解码。
fn magic_matches(bytes: &[u8], mime: &str) -> bool {
    match mime {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        "image/bmp" => bytes.starts_with(b"BM"),
        _ => false,
    }
}

/// 零依赖 base64（标准字母表 + padding），与项目"不引入编解码依赖"的取向一致。
fn base64_encode(data: &[u8]) -> String {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TBL[(n >> 18) as usize & 63] as char);
        out.push(TBL[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TBL[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TBL[n as usize & 63] as char } else { '=' });
    }
    out
}

/// 读取一张图片：base64 后以图像块回传，视觉模型直接看图。
/// 前端工具卡与视觉管线（与 computer_screenshot 相同）都已就绪。
fn read_image(path: &Path, mime: &str) -> Result<ToolOutcome, HarnessError> {
    const MAX_IMAGE: u64 = 5 * 1024 * 1024;
    let meta = std::fs::metadata(path)
        .map_err(|e| HarnessError::Tool(format!("cannot read {}: {e}", path.display())))?;
    if meta.len() > MAX_IMAGE {
        return Err(HarnessError::Tool(format!(
            "image too large: {} bytes (limit 5 MB); downscale or crop it first",
            meta.len()
        )));
    }
    let bytes = std::fs::read(path)
        .map_err(|e| HarnessError::Tool(format!("cannot read {}: {e}", path.display())))?;
    if !magic_matches(&bytes, mime) {
        return Err(HarnessError::Tool(format!(
            "{} does not look like a valid {} file (magic bytes mismatch)",
            path.display(),
            mime
        )));
    }
    let data = base64_encode(&bytes);
    let kb = (meta.len() + 1023) / 1024;
    Ok(ToolOutcome {
        ok: true,
        title: format!("Read image {} · {} KB", path.display(), kb),
        display: DisplayKind::Image { media_type: mime.to_string(), data: data.clone() },
        for_model: format!(
            "[image attached: {} ({}), {} KB — the picture is embedded in this tool result; describe or analyze it directly]",
            path.display(),
            mime,
            kb
        ),
    })
}

// ------------------------------------------------------------------ registry

pub struct Registry {
    pub workspace: PathBuf,
    /// External-command tools contributed by enabled plugins.
    pub plugin_tools: Vec<crate::plugin::PluginTool>,
    /// User's preferred search source: "auto" | "deepseek" | "anysearch" | "multi".
    /// Empty string behaves like "auto".
    pub search_pref: String,
    /// DeepSeek Search API key; empty means the `deepseek_search` tool is off.
    pub search_key: String,
}

impl Registry {
    pub fn new(workspace: PathBuf) -> Self {
        let plugin_tools = crate::plugin::PluginStore::new().runnable_tools();
        Self {
            workspace,
            plugin_tools,
            search_pref: String::new(),
            search_key: String::new(),
        }
    }

    /// Attach the search preference + key so `specs()` can tailor the search
    /// tools to what the user picked in Settings.
    pub fn with_search(mut self, pref: &str, key: &str) -> Self {
        self.search_pref = pref.to_string();
        self.search_key = key.to_string();
        self
    }

    /// JSON Schema list handed to the model.
    pub fn specs(&self) -> Vec<crate::provider::ToolSpec> {
        use crate::provider::ToolSpec;
        let mut specs = vec![
            ToolSpec {
                name: "read_file".into(),
                description: "Read a file. Text files return numbered lines (use offset/limit for long files); image files (png/jpg/gif/webp/bmp) are detected by extension and returned as a picture you can look at directly.".into(),
                input_schema: obj(json!({
                    "path": str_prop("Absolute or workspace-relative file path"),
                    "offset": int_prop("1-based start line (optional, text files only)"),
                    "limit": int_prop("Max lines to return (optional, default 2000, text files only)"),
                }), &["path"]),
            },
            ToolSpec {
                name: "write_file".into(),
                description: "Create or overwrite a file with the given content. Creates parent directories.".into(),
                input_schema: obj(json!({
                    "path": str_prop("Absolute or workspace-relative file path"),
                    "content": str_prop("Full file content"),
                }), &["path", "content"]),
            },
            ToolSpec {
                name: "edit_file".into(),
                description: "Replace an exact substring in a file. old_string must appear exactly once unless replace_all is true.".into(),
                input_schema: obj(json!({
                    "path": str_prop("Absolute or workspace-relative file path"),
                    "old_string": str_prop("Exact text to find"),
                    "new_string": str_prop("Replacement text"),
                    "replace_all": bool_prop("Replace every occurrence (default false)"),
                }), &["path", "old_string", "new_string"]),
            },
            ToolSpec {
                name: "list_dir".into(),
                description: "List a directory (non-recursive) with file sizes and types.".into(),
                input_schema: obj(json!({
                    "path": str_prop("Directory path (defaults to workspace root)"),
                }), &[]),
            },
            ToolSpec {
                name: "glob".into(),
                description: "Find files matching a glob pattern, e.g. **/*.rs. Returns matching paths.".into(),
                input_schema: obj(json!({
                    "pattern": str_prop("Glob pattern"),
                    "path": str_prop("Directory to search in (defaults to workspace root)"),
                }), &["pattern"]),
            },
            ToolSpec {
                name: "grep".into(),
                description: "Search file contents with a regular expression. Returns file:line:content matches. Respects .gitignore.".into(),
                input_schema: obj(json!({
                    "pattern": str_prop("Regular expression"),
                    "path": str_prop("Directory to search (defaults to workspace root)"),
                    "glob": str_prop("Only search files matching this glob, e.g. **/*.rs"),
                    "ignore_case": bool_prop("Case-insensitive (default false)"),
                    "max_results": int_prop("Cap on matches (default 200)"),
                }), &["pattern"]),
            },
            ToolSpec {
                name: "bash".into(),
                description: "Run a shell command in the workspace and return stdout/stderr/exit code. Output is streamed and truncated if very long.".into(),
                input_schema: obj(json!({
                    "command": str_prop("Command line to run"),
                    "cwd": str_prop("Working directory (defaults to workspace root)"),
                    "timeout_ms": int_prop("Timeout in milliseconds (default 120000)"),
                }), &["command"]),
            },
            ToolSpec {
                name: "todo_write".into(),
                description: "Create or update the task plan shown to the user. Use it to track multi-step work.".into(),
                input_schema: obj(json!({
                    "todos": {
                        "type":"array",
                        "items": obj(json!({
                            "id": str_prop("Stable id like \"1\""),
                            "content": str_prop("What to do"),
                            "status": str_prop("pending | in_progress | completed"),
                        }), &["id","content","status"])
                    }
                }), &["todos"]),
            },
            ToolSpec {
                name: "ask_user".into(),
                description: "Ask the user a question and wait for the answer. Use when you are blocked on a decision only the user can make.".into(),
                input_schema: obj(json!({
                    "question": str_prop("The question"),
                    "options": json!({"type":"array","items": str_prop("A choice"),"description":"Optional suggested answers"}),
                }), &["question"]),
            },
            ToolSpec {
                name: "web_search".into(),
                description: if self.search_pref == "deepseek" {
                    "Search the web across several engines at once (Bing, Sogou, 360, DuckDuckGo, Baidu). FALLBACK ONLY — the user prefers deepseek_search; use that first and reach for this only when it errors or is unavailable.".into()
                } else if self.search_pref == "anysearch" {
                    "Search the web via the AnySearch aggregated API. The user prefers AnySearch — this tool queries it exclusively.".into()
                } else if self.search_pref == "multi" {
                    "Search the web across several engines at once (Bing, Sogou, 360, DuckDuckGo, Baidu) and return each platform's results. The user prefers the multi-engine comparison view — use this for web searches.".into()
                } else {
                    "Search the web across several engines at once (Bing, Sogou, 360, DuckDuckGo, Baidu) and return each platform's results. Use this whenever you need current or factual information from the internet — prefer it over guessing.".into()
                },
                input_schema: obj(json!({
                    "query": str_prop("Search keywords"),
                    "max_bytes": int_prop("Max characters per platform (default 3500)"),
                }), &["query"]),
            },
            ToolSpec {
                name: "web_fetch".into(),
                description: "Fetch a URL and return its main text content as Markdown. Read-only network access.".into(),
                input_schema: obj(json!({
                    "url": str_prop("http(s) URL to fetch"),
                    "max_bytes": int_prop("Truncate output to this many bytes (default 20000)"),
                }), &["url"]),
            },
            ToolSpec {
                name: "deepseek_search".into(),
                // auto 模式下没配 key 就不把工具递给模型，免得模型空跑一次报错。
                // 显式偏好 deepseek 但 key 缺失时仍然暴露——错误信息会提示去设置。
                description: if self.search_pref == "deepseek" {
                    "Search the web via DeepSeek's in-house search engine (POST /v1/tools/search). Returns ranked results with relevance scores, snippets and model-ready summaries; supports site:/-site:/quoted-phrase operators. The user's PREFERRED search — use this before web_search.".into()
                } else {
                    "Search the web via DeepSeek's in-house search engine (POST /v1/tools/search). Returns ranked results with relevance scores, snippets and model-ready summaries; supports site:/-site:/quoted-phrase operators. Higher quality than web_search's engine scraping — prefer it when available.".into()
                },
                input_schema: obj(json!({
                    "query": str_prop("Search keywords, 1-128 chars. Supports site:, -site:, and quoted phrases"),
                    "count": int_prop("Number of results, 1-50 (default 8)"),
                    "need_content": json!({"type":"boolean","description":"Also return full page content per result (heavier, default false)"}),
                    "ori_question": str_prop("The user's original question (optional, aids ranking)"),
                }), &["query"]),
            },
            // ---- scheduled tasks (reference: DeepSeek Harness `schedule`) ----
            ToolSpec {
                name: "schedule_create".into(),
                description: "Create a persistent scheduled task that runs a prompt on a recurrence. Survives restarts. Timing kinds: after | at | every | daily | weekly | cron.".into(),
                input_schema: obj(json!({
                    "name": str_prop("Short human label, e.g. \"daily build check\""),
                    "prompt": str_prop("The prompt to deliver when the task fires"),
                    "timing": {
                        "type":"object",
                        "description":"One of {kind:'after',after_seconds}, {kind:'at',at:RFC3339}, {kind:'every',every_seconds>=60}, {kind:'daily',time:'HH:MM',timeZone}, {kind:'weekly',time,timeZone,weekdays:[1-7]}, {kind:'cron',expression:'5-field',timeZone}",
                        "additionalProperties": true
                    },
                    "target": str_prop("existing | reuse | new  — where the prompt is delivered"),
                    "session_id": str_prop("Session id, only when target is `existing`"),
                }), &["name","prompt","timing","target"]),
            },
            ToolSpec {
                name: "schedule_list".into(),
                description: "List every scheduled task with its status and next run time.".into(),
                input_schema: obj(json!({}), &[]),
            },
            ToolSpec {
                name: "schedule_view".into(),
                description: "Show one scheduled task in full, including its recurrence rule and history.".into(),
                input_schema: obj(json!({"id": str_prop("Task id from schedule_list")}), &["id"]),
            },
            ToolSpec {
                name: "schedule_edit".into(),
                description: "Edit a scheduled task: rename, change its prompt or recurrence, pause/resume, or delete it.".into(),
                input_schema: obj(json!({
                    "id": str_prop("Task id"),
                    "name": str_prop("New name (optional)"),
                    "prompt": str_prop("New prompt (optional)"),
                    "timing": json!({"type":"object","description":"Replacement recurrence (optional)","additionalProperties":true}),
                    "action": str_prop("pause | resume | delete (optional)"),
                }), &["id"]),
            },
            // ---- subagents (reference: DeepSeek Harness `subagent`) ----
            ToolSpec {
                name: "spawn_agent".into(),
                description: "Delegate a task to an isolated child agent with its own context. Use for independent research or parallel work. Returns immediately; the result arrives as a message.".into(),
                input_schema: obj(json!({
                    "label": str_prop("Short task label for the catalogue"),
                    "task": str_prop("The task the child should carry out"),
                    "name": str_prop("Optional stable name: 1-32 of [a-z0-9-], starts with a letter"),
                    "cwd": str_prop("Initial working directory (defaults to the workspace)"),
                    "seed_history": bool_prop("Start the child with this conversation as context"),
                    "model": str_prop("Model selector for the child (defaults to the parent's)"),
                }), &["label","task"]),
            },
            ToolSpec {
                name: "agent_list".into(),
                description: "List every child agent this conversation created, running or finished, with its result.".into(),
                input_schema: obj(json!({}), &[]),
            },
            ToolSpec {
                name: "agent_interrupt".into(),
                description: "Stop a running child agent by id.".into(),
                input_schema: obj(json!({"id": str_prop("Child agent id from agent_list")}), &["id"]),
            },
            // ---- computer use (reference: ZCode CUA — a11y first, vision fallback) ----
            ToolSpec {
                name: "computer_axtree".into(),
                description: "Read the browser's accessibility tree. Returns a compact tree where every interactive element carries an [ref] id. PREFERRED over computer_screenshot: act on elements by ref (computer_click ref=N) — semantic, precise, works without pixel guessing.".into(),
                input_schema: obj(json!({}), &[]),
            },
            ToolSpec {
                name: "computer_screenshot".into(),
                description: "Capture a screenshot of the current browser page. The image is attached to the tool result so you can look at it. Use when the accessibility tree cannot describe what you need (canvas, visual layout). Act by coordinates afterwards.".into(),
                input_schema: obj(json!({}), &[]),
            },
            ToolSpec {
                name: "computer_click".into(),
                description: "Click an element by accessibility ref (preferred) or at screen coordinates (x/y). Optionally choose button and click count.".into(),
                input_schema: obj(json!({
                    "ref": int_prop("Element ref from computer_axtree (preferred)"),
                    "x": int_prop("X coordinate (fallback when no ref)"),
                    "y": int_prop("Y coordinate"),
                    "button": str_prop("left | right | middle (default left)"),
                    "clicks": int_prop("1 = single, 2 = double (default 1)"),
                }), &[]),
            },
            ToolSpec {
                name: "computer_type".into(),
                description: "Type text into the focused element (unicode-safe, e.g. Chinese). Focus the field first (click its ref).".into(),
                input_schema: obj(json!({
                    "text": str_prop("Text to type"),
                }), &["text"]),
            },
            ToolSpec {
                name: "computer_key".into(),
                description: "Press a key or combo: Enter, Tab, Escape, ArrowDown, Home, End, PageUp, PageDown, or combos like Control+a, Control+Shift+Tab.".into(),
                input_schema: obj(json!({
                    "key": str_prop("Key name or modifier combo"),
                }), &["key"]),
            },
            ToolSpec {
                name: "computer_scroll".into(),
                description: "Scroll the page or an element. direction: up/down/left/right; pages: how many screens (default 1).".into(),
                input_schema: obj(json!({
                    "direction": str_prop("up | down | left | right"),
                    "pages": int_prop("Screens to scroll (default 1)"),
                    "x": int_prop("Wheel X (default 400)"),
                    "y": int_prop("Wheel Y (default 400)"),
                }), &["direction"]),
            },
            ToolSpec {
                name: "computer_navigate".into(),
                description: "Navigate the controlled browser to a URL, then observe the result.".into(),
                input_schema: obj(json!({
                    "url": str_prop("http(s) URL"),
                }), &["url"]),
            },
            // ---- plugin management & authoring (the agent maintains its own plugins) ----
            ToolSpec {
                name: "plugin_list".into(),
                description: "List installed plugins: id, name, source (manifest/dsh), enabled state, and their commands/tools.".into(),
                input_schema: obj(json!({}), &[]),
            },
            ToolSpec {
                name: "plugin_create".into(),
                description: "Create a declarative plugin from JSON. A plugin can have: (a) slash `commands` — each with a prompt `template` containing {{input}}; (b) `tools` — each backed by a shell `command` with {{args.<param>}} placeholders and a JSON-Schema `parameters`; (c) a `prompt` section appended to the system prompt while enabled. Takes effect on the next turn.".into(),
                input_schema: obj(json!({
                    "id": str_prop("Plugin id: 1-64 chars, lowercase a-z0-9_- , must start with a letter"),
                    "name": str_prop("Display name (optional, defaults to id)"),
                    "description": str_prop("What the plugin is for"),
                    "prompt": str_prop("System-prompt section text while the plugin is enabled (optional)"),
                    "commands": json!({"type":"array","description":"Slash commands","items":{"type":"object","properties":{"name":{"type":"string"},"description":{"type":"string"},"template":{"type":"string"}},"required":["name","template"]}}),
                    "tools": json!({"type":"array","description":"Model tools backed by shell commands","items":{"type":"object","properties":{"name":{"type":"string"},"description":{"type":"string"},"command":{"type":"string"},"parameters":{"type":"object"}},"required":["name","command"]}})
                }), &["id"]),
            },
            ToolSpec {
                name: "plugin_edit".into(),
                description: "Replace an existing plugin's manifest with the given fields (same shape as plugin_create; omitted arrays are cleared).".into(),
                input_schema: obj(json!({
                    "id": str_prop("Existing plugin id"),
                    "name": str_prop("Display name"),
                    "description": str_prop("Description"),
                    "prompt": str_prop("System-prompt section text"),
                    "commands": json!({"type":"array","items":{"type":"object","properties":{"name":{"type":"string"},"description":{"type":"string"},"template":{"type":"string"}}}}),
                    "tools": json!({"type":"array","items":{"type":"object","properties":{"name":{"type":"string"},"description":{"type":"string"},"command":{"type":"string"},"parameters":{"type":"object"}}}})
                }), &["id"]),
            },
            ToolSpec {
                name: "plugin_toggle".into(),
                description: "Enable or disable a plugin (toggles current state). Takes effect next turn.".into(),
                input_schema: obj(json!({
                    "id": str_prop("Plugin id"),
                }), &["id"]),
            },
            ToolSpec {
                name: "plugin_delete".into(),
                description: "Permanently delete a manifest plugin (removes its directory). dsh packages can be deleted too.".into(),
                input_schema: obj(json!({
                    "id": str_prop("Plugin id"),
                }), &["id"]),
            },
        ];
        for t in &self.plugin_tools {
            specs.push(ToolSpec {
                name: t.name.clone(),
                description: if t.description.is_empty() {
                    format!("Plugin tool {}", t.name)
                } else {
                    t.description.clone()
                },
                input_schema: if t.parameters.get("type").is_some() {
                    t.parameters.clone()
                } else {
                    obj(json!({}), &[])
                },
            });
        }
        // 用户在设置里挑选的搜索来源决定工具暴露：
        // - auto：web_search 始终可用；deepseek_search 仅在配了 key 时暴露
        // - deepseek：两个都暴露，描述里写明优先级（key 缺失时调用会报配置提示）
        // - anysearch / multi：只要 web_search，且 multi_search 按偏好裁剪查询源
        if self.search_pref != "deepseek" && self.search_key.trim().is_empty() {
            specs.retain(|s| s.name != "deepseek_search");
        }
        if self.search_pref == "deepseek" && self.search_key.trim().is_empty() {
            specs.retain(|s| s.name != "web_search");
        }
        specs
    }

    pub async fn execute(&self, name: &str, args: &Value, ctx: &ToolCtx) -> Result<ToolOutcome, HarnessError> {
        let t0 = std::time::Instant::now();
        let res: Result<ToolOutcome, HarnessError> = match name {
            "read_file" => self.read_file(args, ctx).await,
            "write_file" => self.write_file(args, ctx).await,
            "edit_file" => self.edit_file(args, ctx).await,
            "list_dir" => self.list_dir(args, ctx).await,
            "glob" => self.glob(args, ctx).await,
            "grep" => self.grep(args, ctx).await,
            "bash" => self.bash(args, ctx).await,
            "todo_write" => self.todo_write(args, ctx).await,
            "ask_user" => self.ask_user(args, ctx).await,
            "web_fetch" => web::fetch(args, ctx).await,
            "web_search" => web::multi_search(args, ctx).await,
            "deepseek_search" => web::deepseek_search(args, ctx).await,
            "computer_screenshot" => crate::computer::screenshot(args, ctx).await,
            "computer_axtree" => crate::computer::axtree(args, ctx).await,
            "computer_click" => crate::computer::click(args, ctx).await,
            "computer_type" => crate::computer::type_text(args, ctx).await,
            "computer_key" => crate::computer::key(args, ctx).await,
            "computer_scroll" => crate::computer::scroll(args, ctx).await,
            "computer_navigate" => crate::computer::navigate(args, ctx).await,
            "plugin_list" | "plugin_create" | "plugin_edit" | "plugin_toggle" | "plugin_delete" => {
                crate::tools::plugin_admin(name, args, ctx).await
            }
            "schedule_create" | "schedule_list" | "schedule_view" | "schedule_edit" => {
                self.schedule_tool(name, args, ctx).await
            }
            "spawn_agent" | "agent_list" | "agent_interrupt" => {
                self.agent_tool(name, args, ctx).await
            }
            other => match self.plugin_tools.iter().find(|t| t.name == other) {
                Some(pt) => crate::plugin::run_plugin_tool(pt, args, ctx).await,
                None => Err(HarnessError::Tool(format!("unknown tool: {other}"))),
            },
        };
        // 执行审计:无论成败都记一条(tool、判定来源 auto/requested、耗时)。
        let source = ctx.broker.decision_of(&ctx.call_id).await;
        let ok = res.as_ref().map(|o| o.ok).unwrap_or(false);
        ctx.broker
            .audit_execution(&ctx.call_id, name, source, ok, t0.elapsed().as_millis())
            .await;
        res
    }

    // ---------------------------------------------------------------- fs helpers

    /// Resolve a caller-supplied path against the workspace and reject escapes.
    pub fn resolve(&self, raw: &str) -> Result<PathBuf, HarnessError> {
        fs::resolve_within(&self.workspace, raw)
    }
}

// Re-export the concrete implementations from the submodules.
use fs as fs_impl;
use search as search_impl;

impl Registry {
    async fn read_file(&self, a: &Value, ctx: &ToolCtx) -> Result<ToolOutcome, HarnessError> {
        let path = fs_impl::resolve_readonly(&self.workspace, a["path"].as_str().unwrap_or(""))?;
        ctx.authorize("read_file", &path.to_string_lossy(), Risk::ReadOnly)
            .await?;
        // 图片按扩展名识别：base64 后作为图像块回传，视觉模型直接看图。
        if let Some(mime) = image_mime(&path) {
            return read_image(&path, mime);
        }
        let offset = a["offset"].as_u64().unwrap_or(1).max(1);
        // limit=0 会被当成"从头读全部"之外的三不像,归一成默认值
        let limit = a["limit"]
            .as_u64()
            .filter(|l| *l > 0)
            .map(|l| l.min(20000))
            .unwrap_or(2000);
        fs_impl::read_file(&path, offset, limit)
    }

    async fn write_file(&self, a: &Value, ctx: &ToolCtx) -> Result<ToolOutcome, HarnessError> {
        let path = self.resolve(a["path"].as_str().unwrap_or(""))?;
        let content = a["content"].as_str().ok_or_else(|| {
            HarnessError::Tool("`content` is required and must be a string".into())
        })?;
        // Show a diff preview in the permission panel when the file already exists.
        let preview = std::fs::read_to_string(&path)
            .ok()
            .map(|old| crate::tools::fs::preview_diff(&old, content, 200))
            .unwrap_or_default();
        // 覆盖已有文件是不可逆的破坏性操作:ws/ask 模式下必须确认(auto_allow
        // 对 Destructive 一律送确认),只有新建文件才是普通 Write。
        let risk = if path.exists() { Risk::Destructive } else { Risk::Write };
        ctx.authorize_with_preview(
            "write_file",
            &path.to_string_lossy(),
            risk,
            &preview,
        )
        .await?;
        fs_impl::write_file(&path, content)
    }

    async fn edit_file(&self, a: &Value, ctx: &ToolCtx) -> Result<ToolOutcome, HarnessError> {
        let path = self.resolve(a["path"].as_str().unwrap_or(""))?;
        let old = a["old_string"].as_str().ok_or_else(|| {
            HarnessError::Tool("`old_string` is required and must be a string".into())
        })?;
        let new = a["new_string"].as_str().ok_or_else(|| {
            HarnessError::Tool("`new_string` is required and must be a string".into())
        })?;
        let replace_all = a["replace_all"].as_bool().unwrap_or(false);
        if old.is_empty() {
            // 空串在 `text.matches("")` 里处处命中,等于全文替换——参数错误直接拦
            return Err(HarnessError::Tool(
                "`old_string` must not be empty; use write_file to replace a whole file".into(),
            ));
        }
        let preview = fs_impl::preview_replace(&path, old, new, replace_all)?;
        ctx.authorize_with_preview("edit_file", &path.to_string_lossy(), Risk::Write, &preview)
            .await?;
        fs_impl::edit_file(&path, old, new, replace_all)
    }

    async fn list_dir(&self, a: &Value, ctx: &ToolCtx) -> Result<ToolOutcome, HarnessError> {
        let raw = a["path"].as_str().unwrap_or(".");
        let path = if raw.is_empty() || raw == "." {
            self.workspace.clone()
        } else {
            fs_impl::resolve_readonly(&self.workspace, raw)?
        };
        ctx.authorize("list_dir", &path.to_string_lossy(), Risk::ReadOnly)
            .await?;
        fs_impl::list_dir(&path)
    }

    async fn glob(&self, a: &Value, ctx: &ToolCtx) -> Result<ToolOutcome, HarnessError> {
        let pattern = a["pattern"].as_str().ok_or_else(|| {
            HarnessError::Tool("`pattern` is required".into())
        })?;
        let root = match a["path"].as_str() {
            Some(p) if !p.is_empty() && p != "." => {
                fs_impl::resolve_readonly(&self.workspace, p)?
            }
            _ => self.workspace.clone(),
        };
        ctx.authorize("glob", &root.to_string_lossy(), Risk::ReadOnly)
            .await?;
        search_impl::glob(&root, pattern)
    }

    async fn grep(&self, a: &Value, ctx: &ToolCtx) -> Result<ToolOutcome, HarnessError> {
        let pattern = a["pattern"].as_str().ok_or_else(|| {
            HarnessError::Tool("`pattern` is required".into())
        })?;
        let root = match a["path"].as_str() {
            Some(p) if !p.is_empty() && p != "." => {
                fs_impl::resolve_readonly(&self.workspace, p)?
            }
            _ => self.workspace.clone(),
        };
        ctx.authorize("grep", &root.to_string_lossy(), Risk::ReadOnly)
            .await?;
        let glob_filter = a["glob"].as_str();
        let icase = a["ignore_case"].as_bool().unwrap_or(false);
        let max = a["max_results"].as_u64().unwrap_or(200) as usize;
        search_impl::grep(&root, pattern, glob_filter, icase, max)
    }

    async fn bash(&self, a: &Value, ctx: &ToolCtx) -> Result<ToolOutcome, HarnessError> {
        let command = a["command"].as_str().ok_or_else(|| {
            HarnessError::Tool("`command` is required".into())
        })?;
        let cwd = match a["cwd"].as_str() {
            Some(c) if !c.is_empty() && c != "." => self.resolve(c)?,
            _ => self.workspace.clone(),
        };
        let timeout = a["timeout_ms"].as_u64().unwrap_or(120_000);
        // 命令原文进权限链:面板上能看到真正要执行的命令,Always 规则按命令精确匹配
        ctx.authorize_shell("bash", &cwd.to_string_lossy(), Risk::Shell, command)
            .await?;
        shell::bash(command, &cwd, timeout, ctx).await
    }

    async fn todo_write(&self, a: &Value, _ctx: &ToolCtx) -> Result<ToolOutcome, HarnessError> {
        // 参数校验:todos 必须是数组(空数组=清空计划,允许);status 必须是
        // 三态之一,非法直接报工具错误,别让脏状态进前端。
        let arr = a["todos"].as_array().ok_or_else(|| {
            HarnessError::Tool("`todos` must be an array of {id, content, status}".into())
        })?;
        let mut items: Vec<TodoItem> = Vec::with_capacity(arr.len());
        for t in arr {
            let status = t["status"].as_str().unwrap_or("pending");
            if !matches!(status, "pending" | "in_progress" | "completed") {
                return Err(HarnessError::Tool(format!(
                    "invalid todo status `{status}`: must be pending | in_progress | completed"
                )));
            }
            items.push(TodoItem {
                id: t["id"].as_str().unwrap_or("").to_string(),
                content: t["content"].as_str().unwrap_or("").to_string(),
                status: status.to_string(),
            });
        }
        let n = items.len();
        Ok(ToolOutcome {
            ok: true,
            title: format!("Updated plan · {n} items"),
            display: DisplayKind::Todo(items.clone()),
            for_model: format!(
                "Plan updated:\n{}",
                items
                    .iter()
                    .map(|i| format!("[{}] {}", i.status, i.content))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        })
    }

    async fn ask_user(&self, a: &Value, ctx: &ToolCtx) -> Result<ToolOutcome, HarnessError> {
        let question = a["question"].as_str().unwrap_or("").to_string();
        let options: Vec<String> = a["options"]
            .as_array()
            .map(|v| v.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();

        ctx.hub.publish(
            &ctx.session_id,
            "question",
            json!({"callId": ctx.call_id, "question": question, "options": options}),
        );

        let answer = ctx.questions.ask(&ctx.call_id).await;
        let answer = answer.unwrap_or_else(|| "(the user did not answer)".to_string());

        Ok(ToolOutcome {
            ok: true,
            title: format!("Asked the user: {}", truncate(&question, 60)),
            display: DisplayKind::Ask {
                question: question.clone(),
                options,
            },
            for_model: format!("The user answered: {answer}"),
        })
    }

    // ---------------------------------------------------------------- schedule

    async fn schedule_tool(
        &self,
        name: &str,
        a: &Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutcome, HarnessError> {
        let Some(svc) = ctx.schedule.clone() else {
            return Err(HarnessError::Tool("scheduled tasks are not mounted".into()));
        };
        match name {
            "schedule_create" => {
                let timing: crate::schedule::Timing = serde_json::from_value(
                    a["timing"].clone(),
                )
                .map_err(|e| HarnessError::Tool(format!("invalid `timing`: {e}")))?;
                let target = match a["target"].as_str().unwrap_or("reuse") {
                    "existing" => crate::schedule::Target::Existing {
                        session_id: a["session_id"]
                            .as_str()
                            .unwrap_or(&ctx.session_id)
                            .to_string(),
                    },
                    "new" => crate::schedule::Target::NewEachTime,
                    _ => crate::schedule::Target::Reuse,
                };
                let label = a["name"].as_str().unwrap_or("task");
                // 定时任务会长期反复向会话投递 prompt:写入级权限,plan 模式下被拒
                ctx.authorize("schedule_create", label, Risk::Write).await?;
                let t = svc
                    .create(
                        a["name"].as_str().unwrap_or("task").to_string(),
                        a["prompt"].as_str().unwrap_or("").to_string(),
                        timing,
                        target,
                    )
                    .await?;
                Ok(ToolOutcome {
                    ok: true,
                    title: format!("Scheduled `{}`", t.name),
                    display: DisplayKind::Plain(format!(
                        "task {} `{}`\nrule: {}\nnext: {}\nstatus: {:?}",
                        t.id,
                        t.name,
                        t.timing.label(),
                        t.scheduled_at.as_deref().unwrap_or("(none)"),
                        t.status
                    )),
                    for_model: format!(
                        "Created scheduled task `{}` (id {}), rule {}, next run {}.",
                        t.name,
                        t.id,
                        t.timing.label(),
                        t.scheduled_at.as_deref().unwrap_or("(none)")
                    ),
                })
            }

            "schedule_list" => {
                let tasks = svc.store().list().await;
                let mut body = format!("{} scheduled task(s):\n", tasks.len());
                for t in &tasks {
                    body.push_str(&format!(
                        "  [{}] {} — rule {} — next {} — {:?} — run {} time(s)\n",
                        t.id, t.name, t.timing.label(),
                        t.scheduled_at.as_deref().unwrap_or("(none)"),
                        t.status, t.run_count
                    ));
                }
                if tasks.is_empty() {
                    body = "No scheduled tasks yet.".into();
                }
                Ok(ToolOutcome {
                    ok: true,
                    title: format!("Listed {} task(s)", tasks.len()),
                    display: DisplayKind::Plain(body.clone()),
                    for_model: body,
                })
            }

            "schedule_view" => {
                let id = a["id"].as_str().unwrap_or("");
                let t = svc
                    .store()
                    .get(id)
                    .await
                    .ok_or_else(|| HarnessError::NotFound(format!("task {id}")))?;
                let body = format!(
                    "id: {}\nname: {}\nrule: {}\ntiming: {}\ntarget: {:?}\nstatus: {:?}\nnext: {}\nlast run: {}\nrun count: {}\nprompt:\n{}",
                    t.id, t.name, t.timing.label(),
                    serde_json::to_string_pretty(&t.timing).unwrap_or_default(),
                    t.target, t.status,
                    t.scheduled_at.as_deref().unwrap_or("(none)"),
                    t.last_run_at.as_deref().unwrap_or("(never)"),
                    t.run_count, t.prompt
                );
                Ok(ToolOutcome {
                    ok: true,
                    title: format!("Task `{}`", t.name),
                    display: DisplayKind::Plain(body.clone()),
                    for_model: body,
                })
            }

            "schedule_edit" => {
                let id = a["id"].as_str().unwrap_or("");
                let action = a["action"].as_str().unwrap_or("");
                if action == "delete" {
                    // 删除是持久任务的一次性毁灭,按破坏性对待
                    ctx.authorize("schedule_edit", id, Risk::Destructive).await?;
                    svc.store().delete(id).await?;
                    return Ok(ToolOutcome::text(
                        true,
                        format!("Deleted task {id}"),
                        format!("Deleted scheduled task {id}."),
                    ));
                }
                ctx.authorize("schedule_edit", id, Risk::Write).await?;
                let t = svc
                    .store()
                    .update(id, |t| {
                        if let Some(n) = a["name"].as_str() {
                            if !n.is_empty() {
                                t.name = n.to_string();
                            }
                        }
                        if let Some(p) = a["prompt"].as_str() {
                            if !p.is_empty() {
                                t.prompt = p.to_string();
                            }
                        }
                        if !a["timing"].is_null() {
                            if let Ok(tm) =
                                serde_json::from_value::<crate::schedule::Timing>(a["timing"].clone())
                            {
                                t.timing = tm;
                                t.scheduled_at = None; // recompute on next tick
                            }
                        }
                        match action {
                            "pause" => t.status = crate::schedule::TaskStatus::Paused,
                            "resume" => {
                                t.status = crate::schedule::TaskStatus::Active;
                                t.scheduled_at = None;
                            }
                            _ => {}
                        }
                    })
                    .await?;
                Ok(ToolOutcome::text(
                    true,
                    format!("Updated `{}`", t.name),
                    format!(
                        "Updated scheduled task {} (`{}`), status {:?}, rule {}.",
                        t.id, t.name, t.status, t.timing.label()
                    ),
                ))
            }

            other => Err(HarnessError::Tool(format!("unknown schedule tool: {other}"))),
        }
    }

    // ---------------------------------------------------------------- subagents

    async fn agent_tool(
        &self,
        name: &str,
        a: &Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutcome, HarnessError> {
        let Some(sub) = ctx.subagents.clone() else {
            return Err(HarnessError::Tool("subagents are not mounted".into()));
        };
        match name {
            "spawn_agent" => {
                let runtime = ctx.runtime.clone().ok_or_else(|| {
                    HarnessError::Tool("agent runtime not available".into())
                })?;
                let label = a["label"].as_str().unwrap_or("task");
                // 拉起子 agent 等于委托一批不可预知的操作:按 shell 级送确认
                // (plan 模式下也会被 authorize 拒掉)。
                ctx.authorize("spawn_agent", label, Risk::Shell).await?;
                let req = crate::subagent::SpawnRequest {
                    label: a["label"].as_str().unwrap_or("task").to_string(),
                    name: a["name"].as_str().map(String::from),
                    task: a["task"].as_str().unwrap_or("").to_string(),
                    cwd: a["cwd"].as_str().map(String::from),
                    seed_history: a["seed_history"].as_bool().unwrap_or(false),
                    model: a["model"].as_str().unwrap_or("").to_string(),
                    delivery: Some(crate::subagent::Delivery::Parent),
                };
                let id = sub
                    .spawn(runtime, &ctx.session_id, &self.workspace.to_string_lossy(), req)
                    .await?;
                Ok(ToolOutcome::text(
                    true,
                    format!("Spawned agent {id}"),
                    format!("Spawned child agent {id}. Its result will arrive as a message."),
                ))
            }

            "agent_list" => {
                let kids = sub.list().await;
                let mut body = format!("{} child agent(s):\n", kids.len());
                for k in &kids {
                    body.push_str(&format!(
                        "  [{}] {} ({}) — {:?}{}\n",
                        k.id,
                        k.name,
                        k.label,
                        k.status,
                        k.result
                            .as_ref()
                            .map(|r| format!(" — {}", truncate(r, 80)))
                            .unwrap_or_default()
                    ));
                }
                if kids.is_empty() {
                    body = "No child agents yet.".into();
                }
                Ok(ToolOutcome {
                    ok: true,
                    title: format!("Listed {} child agent(s)", kids.len()),
                    display: DisplayKind::Plain(body.clone()),
                    for_model: body,
                })
            }

            "agent_interrupt" => {
                let id = a["id"].as_str().unwrap_or("");
                let ok = sub.interrupt(id).await;
                Ok(ToolOutcome::text(
                    ok,
                    if ok { format!("Stopped {id}") } else { format!("{id} is not running") },
                    if ok {
                        format!("Stopped child agent {id}.")
                    } else {
                        format!("Child agent {id} is not running.")
                    },
                ))
            }

            other => Err(HarnessError::Tool(format!("unknown agent tool: {other}"))),
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= n {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

#[allow(unused_imports)]
use Config as _Config;

/// Agent-facing plugin management: list / create / edit / toggle / delete.
/// 插件是声明式的（参考 dsh 的 manifest 半）：slash 命令模板 + shell 工具 +
/// 系统提示词段；JS 代码半不执行。
pub async fn plugin_admin(
    name: &str,
    args: &Value,
    ctx: &ToolCtx,
) -> Result<ToolOutcome, HarnessError> {
    use crate::plugin::{PluginCommand, PluginManifest, PluginStore, PluginTool};
    let store = PluginStore::new();
    match name {
        "plugin_list" => {
            ctx.authorize(name, "list plugins", Risk::ReadOnly).await?;
            let list = store.scan();
            if list.is_empty() {
                return Ok(ToolOutcome::text(
                    true,
                    "No plugins",
                    "没有已安装的插件。用 plugin_create 创建一个（manifest 形态）。",
                ));
            }
            let mut out = String::from("已安装插件:\n");
            for p in &list {
                out.push_str(&format!(
                    "- {} (id: {}, 来源: {}, {})\n  命令: {} | 工具: {}{}\n",
                    p.name,
                    p.id,
                    p.source,
                    if p.enabled { "已启用" } else { "已禁用" },
                    p.commands
                        .iter()
                        .map(|c| format!("/{}", c.name))
                        .collect::<Vec<_>>()
                        .join(", "),
                    p.tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>().join(", "),
                    if p.source == "dsh" && !p.extractable {
                        " | 注意: dsh 包的 JS 不执行，仅声明可见"
                    } else {
                        ""
                    },
                ));
            }
            Ok(ToolOutcome::text(true, format!("{} plugins", list.len()), out))
        }
        "plugin_create" | "plugin_edit" => {
            // 授权放在 manifest 组装完成之后:面板能展示完整 manifest JSON,
            // 用户看清要装/改的命令与 prompt 段再放行。参数非法时直接报错,不弹面板。
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if id.is_empty() {
                return Err(HarnessError::Tool("`id` is required".into()));
            }
            if name == "plugin_edit" && !store.scan().iter().any(|p| p.id == id) {
                return Err(HarnessError::NotFound(format!(
                    "plugin {id} 不存在——用 plugin_create 创建"
                )));
            }
            let parse_arr = |key: &str| -> Vec<Value> {
                args.get(key)
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
            };
            let mut commands = Vec::new();
            for c in parse_arr("commands") {
                let cname = c
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let template = c
                    .get("template")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if cname.is_empty() || template.is_empty() {
                    return Err(HarnessError::Tool(format!(
                        "command `{cname}` 缺少 name 或 template"
                    )));
                }
                commands.push(PluginCommand {
                    name: cname,
                    description: c
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .into(),
                    template,
                });
            }
            let mut tools = Vec::new();
            for t in parse_arr("tools") {
                let tname = t
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let command = t
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if tname.is_empty() || command.is_empty() {
                    return Err(HarnessError::Tool(format!(
                        "tool `{tname}` 缺少 name 或 command"
                    )));
                }
                if !tname
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                {
                    return Err(HarnessError::Tool(format!(
                        "tool 名 `{tname}` 只能含小写字母、数字、下划线"
                    )));
                }
                tools.push(PluginTool {
                    name: tname,
                    description: t
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .into(),
                    parameters: t.get("parameters").cloned().unwrap_or(json!({})),
                    command,
                });
            }
            let prompt = args
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if commands.is_empty() && tools.is_empty() && prompt.is_empty() {
                return Err(HarnessError::Tool(
                    "插件至少要有 commands、tools 或 prompt 之一".into(),
                ));
            }
            let m = PluginManifest {
                name: args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&id)
                    .to_string(),
                version: args
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or("0.1.0")
                    .to_string(),
                description: args
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                commands,
                tools: tools.clone(),
                prompt,
            };
            // 与内置工具重名会被内置遮蔽，提前告知
            let builtin: Vec<String> = Registry::new(std::path::PathBuf::from("."))
                .specs()
                .iter()
                .map(|t| t.name.clone())
                .collect();
            let shadowed: Vec<&str> = tools
                .iter()
                .filter(|t| builtin.iter().any(|b| *b == t.name))
                .map(|t| t.name.as_str())
                .collect();
            // 完整 manifest JSON 进权限面板(diffPreview/reason 双通道展示)
            let manifest_json = serde_json::to_string_pretty(&m)
                .unwrap_or_else(|_| "(manifest 序列化失败)".into());
            ctx.authorize_with_reason(
                name,
                &id,
                Risk::Write,
                &format!(
                    "{} plugin manifest `{id}` (commands: {}, tools: {})",
                    if name == "plugin_create" { "create" } else { "edit" },
                    m.commands.len(),
                    tools.len()
                ),
                Some(&manifest_json),
            )
            .await?;
            store.save_manifest(&id, &m)?;
            let mut note = format!(
                "插件 `{}` ({} 个命令, {} 个工具) 已{}。下一次 turn 生效。",
                id,
                m.commands.len(),
                tools.len(),
                if name == "plugin_create" { "创建" } else { "更新" }
            );
            if !shadowed.is_empty() {
                note.push_str(&format!(
                    " 警告: 工具 {} 与内置工具重名，内置优先。",
                    shadowed.join(", ")
                ));
            }
            Ok(ToolOutcome::text(
                true,
                format!(
                    "{} {}",
                    if name == "plugin_create" { "Created" } else { "Updated" },
                    id
                ),
                note,
            ))
        }
        "plugin_toggle" => {
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if id.is_empty() {
                return Err(HarnessError::Tool("`id` is required".into()));
            }
            ctx.authorize(name, &id, Risk::Write).await?;
            if !store.scan().iter().any(|p| p.id == id) {
                return Err(HarnessError::NotFound(format!(
                    "plugin {id} 不存在——用 plugin_list 查看已装插件"
                )));
            }
            let enabled = store.toggle(&id)?;
            Ok(ToolOutcome::text(
                true,
                format!("{} {}", id, if enabled { "enabled" } else { "disabled" }),
                format!(
                    "插件 `{}` 已{}。下一次 turn 生效。",
                    id,
                    if enabled { "启用" } else { "禁用" }
                ),
            ))
        }
        "plugin_delete" => {
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if id.is_empty() {
                return Err(HarnessError::Tool("`id` is required".into()));
            }
            ctx.authorize(name, &id, Risk::Destructive).await?;
            store.delete(&id)?;
            Ok(ToolOutcome::text(
                true,
                format!("Deleted {}", id),
                format!("插件 `{}` 已删除（目录与启用标记一并移除）。下一次 turn 生效。", id),
            ))
        }
        _ => Err(HarnessError::Tool(format!(
            "unknown plugin admin tool: {name}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn image_mime_by_extension() {
        assert_eq!(image_mime(Path::new("a.PNG")), Some("image/png"));
        assert_eq!(image_mime(Path::new("b.Jpg")), Some("image/jpeg"));
        assert_eq!(image_mime(Path::new("c.webp")), Some("image/webp"));
        assert_eq!(image_mime(Path::new("d.txt")), None);
        assert_eq!(image_mime(Path::new("noext")), None);
    }

    #[test]
    fn magic_checks_headers() {
        let png = b"\x89PNG\r\n\x1a\n rest";
        assert!(magic_matches(png, "image/png"));
        assert!(!magic_matches(png, "image/jpeg"));
        assert!(magic_matches(b"\xff\xd8\xff\xe0xx", "image/jpeg"));
        assert!(magic_matches(b"GIF89a...", "image/gif"));
        assert!(magic_matches(b"RIFF1234WEBPVP8 ", "image/webp"));
        assert!(magic_matches(b"BM...", "image/bmp"));
    }

    #[test]
    fn read_image_rejects_bad_magic() {
        let dir = std::env::temp_dir().join("parable-img-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("fake.png");
        std::fs::write(&p, b"definitely not a png").unwrap();
        let err = read_image(&p, "image/png").unwrap_err();
        assert!(err.to_string().contains("magic bytes"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
