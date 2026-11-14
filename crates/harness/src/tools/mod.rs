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
        /// `None` means the question was never answered (timeout/interrupt);
        /// `Some("")` is a real but empty response, which is still "answered".
        answer: Option<String>,
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
            DisplayKind::Ask { question, options, answer } => json!({
                "kind":"ask","question": question,"options": options,
                "answered": answer.is_some(),"answer": answer
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
    /// Session store — the team board lives with the root session.
    pub store: crate::session::SharedStore,
    /// Agent runtime, so `spawn_agent` can start a child turn.
    pub runtime: Option<Arc<dyn TurnStarter>>,
    /// DeepSeek Search API key; empty disables `deepseek_search`.
    pub search_key: String,
    /// User's preferred search source: "auto" | "deepseek" | "anysearch" | "multi".
    pub search_pref: String,
    /// Computer-use bridge (CDP); present when the runtime mounts it.
    pub computer: Option<Arc<crate::computer::ComputerBridge>>,
    /// OS 级桌面桥（截屏 + UIA + SendInput；仅 Windows）。
    pub desktop: Option<Arc<crate::desktop::DesktopBridge>>,
    /// ask_user 自动裁决超时（毫秒；0 = 关闭，一直等用户）。
    pub ask_timeout_ms: u64,
    /// 生命周期 hooks（本 turn 快照）。
    pub hooks: Arc<Vec<crate::hooks::HookConfig>>,
    /// 文件检查点存储（写入前快照旧内容）。
    pub checkpoint: Option<Arc<crate::checkpoint::CheckpointStore>>,
    /// 当前 turn id（检查点分组键）。
    pub turn_id: String,
    /// MCP 管理器（mcp_* 工具的执行后端）。
    pub mcp: Option<Arc<crate::mcp::McpManager>>,
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
                    | "read_skill"
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
                &self.session_id,
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

/// Process-wide memo for the tool schema list. Keyed by everything the
/// schemas depend on (search preference, key presence, plugin count).
fn specs_cache() -> &'static std::sync::Mutex<
    Option<((String, bool, usize, u64), std::sync::Arc<Vec<crate::provider::ToolSpec>>)>,
> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<Option<((String, bool, usize, u64), std::sync::Arc<Vec<crate::provider::ToolSpec>>)>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

/// MCP 工具缓存：run_turn 每轮从 McpManager 拉一次写进这里，specs() 同步读。
/// 世代号进缓存键——工具集变化即失配重建。
static MCP_TOOLS: std::sync::RwLock<Vec<crate::provider::ToolSpec>> =
    std::sync::RwLock::new(Vec::new());
static MCP_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// run_turn 调用：刷新 MCP 工具缓存（并 bump 世代）。
pub fn mcp_set_tools(specs: Vec<crate::provider::ToolSpec>) {
    if let Ok(mut g) = MCP_TOOLS.write() {
        *g = specs;
    }
    MCP_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

fn mcp_gen() -> u64 {
    MCP_GEN.load(std::sync::atomic::Ordering::Relaxed)
}

pub struct Registry {
    pub workspace: PathBuf,
    /// External-command tools contributed by enabled plugins. Interior mutability
    /// lets a `plugin_create` in the middle of a turn reload them immediately.
    pub plugin_tools: std::sync::RwLock<Vec<crate::plugin::PluginTool>>,
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
            plugin_tools: std::sync::RwLock::new(plugin_tools),
            search_pref: String::new(),
            search_key: String::new(),
        }
    }

    /// Re-read enabled plugins from disk and drop the schema memo. Called after
    /// any plugin mutation so an agent-authored tool becomes callable in the
    /// same turn instead of waiting for the next one.
    pub fn reload_plugins(&self) {
        let fresh = crate::plugin::PluginStore::new().runnable_tools();
        if let Ok(mut guard) = self.plugin_tools.write() {
            *guard = fresh;
        }
        let mut cache = specs_cache().lock().unwrap_or_else(|e| e.into_inner());
        *cache = None;
    }

    /// Attach the search preference + key so `specs()` can tailor the search
    /// tools to what the user picked in Settings.
    pub fn with_search(mut self, pref: &str, key: &str) -> Self {
        self.search_pref = pref.to_string();
        self.search_key = key.to_string();
        self
    }

    /// JSON Schema list handed to the model.
    ///
    /// Building the 19 built-in schemas costs ~1ms and this runs once per
    /// agent *step* (the list is re-serialised into every request), so a
    /// 100-step turn would burn 100ms on schema building alone. The result
    /// only depends on the search preference, key presence and plugin set,
    /// so it is memoised process-wide under that key and handed out as an
    /// `Arc` — no clone per step.
    pub fn specs(&self) -> std::sync::Arc<Vec<crate::provider::ToolSpec>> {
        let cache_key = (
            self.search_pref.clone(),
            !self.search_key.trim().is_empty(),
            self.plugin_tools
                .read()
                .map(|g| g.len())
                .unwrap_or(0),
            mcp_gen(),
        );
        {
            let cache = specs_cache().lock().unwrap_or_else(|e| e.into_inner());
            if let Some((k, specs)) = cache.as_ref() {
                if *k == cache_key {
                    return std::sync::Arc::clone(specs);
                }
            }
        }
        let mut specs = self.build_specs();
        // MCP 工具（缓存由 run_turn 刷新）：每个暴露名为独立工具
        if let Ok(mcp) = MCP_TOOLS.read() {
            specs.extend(mcp.iter().cloned());
        }
        let specs = std::sync::Arc::new(specs);
        let mut cache = specs_cache().lock().unwrap_or_else(|e| e.into_inner());
        *cache = Some((cache_key, std::sync::Arc::clone(&specs)));
        specs
    }

    fn build_specs(&self) -> Vec<crate::provider::ToolSpec> {
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
                description: "Ask the user a question and wait for the answer. Use when you are blocked on a decision only the user can make. Provide 2-8 short options when the choice is bounded; the user can also type a custom answer.".into(),
                input_schema: obj(json!({
                    "question": str_prop("One clear question"),
                    "options": json!({"type":"array","items": str_prop("Suggested answer"),"description":"Optional suggested answers (2-8 short choices)"}),
                }), &["question"]),
            },
            ToolSpec {
                name: "web_search".into(),
                description: if self.search_pref == "deepseek" {
                    "Search the web across several engines at once (Bing, Sogou, 360, DuckDuckGo, Baidu). FALLBACK ONLY — the user prefers deepseek_search; use that first and reach for this only when it errors or is unavailable.".into()
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
                name: "read_skill".into(),
                description: "Load the full text of a skill (a reusable procedure the user installed). \
The system prompt lists installed skills by id and description; call this with that id when a \
skill matches the current request, then follow the steps it contains. Read-only.".into(),
                input_schema: obj(json!({
                    "id": str_prop("Skill id as listed in the Skills section of the system prompt"),
                    "scope": str_prop("Optional: \"user\" or \"project\". Omit to search both."),
                }), &["id"]),
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
                description: "Delegate a task to an isolated child agent with its own context. Use for independent research or parallel work. Returns immediately; call agent_wait to collect the child's result.".into(),
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
                description: "List every child agent in your team tree, running or finished, with its result.".into(),
                input_schema: obj(json!({}), &[]),
            },
            ToolSpec {
                name: "agent_wait".into(),
                description: "Wait for a child agent to finish and return its result. Blocks until the child settles or the timeout elapses. Use this to collect a spawned agent's answer.".into(),
                input_schema: obj(json!({
                    "agent": str_prop("Child agent id, name, or session id from agent_list"),
                    "timeout_seconds": int_prop("How long to wait, in seconds (default 300, max 1800)"),
                }), &["agent"]),
            },
            ToolSpec {
                name: "agent_interrupt".into(),
                description: "Stop a running child agent by id.".into(),
                input_schema: obj(json!({"id": str_prop("Child agent id from agent_list")}), &["id"]),
            },
            // ---- agent teams (reference: DeepSeek Harness `tool-agent-team`) ----
            ToolSpec {
                name: "team_task_create".into(),
                description: "Create one unowned pending task on the shared team task board. Use it to split work across teammates.".into(),
                input_schema: obj(json!({
                    "subject": str_prop("Concise task title"),
                    "description": str_prop("Complete task details and acceptance criteria"),
                    "blocked_by": json!({"type":"array","items":{"type":"string"},"description":"Task ids that must complete first"}),
                    "write_scopes": json!({"type":"array","items":{"type":"string"},"description":"Advisory workspace-relative path prefixes this task expects to modify"}),
                }), &["subject","description"]),
            },
            ToolSpec {
                name: "team_task_list".into(),
                description: "List shared team tasks, including readiness, owner, revision, blockers, and write-scope warnings.".into(),
                input_schema: obj(json!({
                    "status": str_prop("Optional exact status filter: pending | in_progress | completed"),
                    "owner": str_prop("Optional owner agent name filter"),
                    "unowned": bool_prop("Filter tasks by whether they have no owner"),
                    "ready": bool_prop("Optional readiness filter"),
                }), &[]),
            },
            ToolSpec {
                name: "team_task_get".into(),
                description: "Read the complete latest value of one shared team task before changing or executing it.".into(),
                input_schema: obj(json!({"task_id": str_prop("Shared task id")}), &["task_id"]),
            },
            ToolSpec {
                name: "team_task_update".into(),
                description: "Compare-and-set a shared team task action using the latest revision from team_task_get or team_task_list.".into(),
                input_schema: obj(json!({
                    "task_id": str_prop("Shared task id"),
                    "expected_revision": int_prop("Current task revision used as the compare-and-set precondition"),
                    "action": str_prop("claim | release | edit | set_dependencies | complete | reopen | reassign | delete"),
                    "subject": str_prop("Replacement title for edit"),
                    "description": str_prop("Replacement details for edit"),
                    "blocked_by": json!({"type":"array","items":{"type":"string"},"description":"Complete blocker list for set_dependencies"}),
                    "write_scopes": json!({"type":"array","items":{"type":"string"},"description":"Replacement advisory write scopes for edit"}),
                    "owner": str_prop("Agent name of the root or any teammate for reassign; omit to unassign"),
                }), &["task_id","expected_revision","action"]),
            },
            // ---- computer use (reference: ZCode CUA / Anthropic computer tool / Playwright ariaSnapshot) ----
            // 观察主流：computer_snapshot 一次返回 a11y 树（带稳定 ref + 视口坐标）
            // + 截图；动作优先按 ref（动作期实时解析，失效明确报错），坐标兜底。
            ToolSpec {
                name: "computer_snapshot".into(),
                description: "PRIMARY observation: one call returns the page's accessibility tree (interactive elements carry stable [rN] refs + viewport coordinates, textbox values, focused mark) AND a screenshot AND url/title. ref coordinates align with screenshot pixels (DPR=1). Re-snapshot whenever the page changed (refs are only guaranteed against the latest snapshot).".into(),
                input_schema: obj(json!({
                    "full_page": json!({"type":"boolean","description":"Capture beyond the viewport too (default false)"})
                }), &[]),
            },
            ToolSpec {
                name: "computer_axtree".into(),
                description: "Read ONLY the accessibility tree (no screenshot — cheaper). Elements carry [rN] refs + viewport coordinates. Prefer computer_snapshot which also attaches a screenshot; use this when you only need structure.".into(),
                input_schema: obj(json!({}), &[]),
            },
            ToolSpec {
                name: "computer_screenshot".into(),
                description: "Capture ONLY a screenshot (no tree). Use for canvas, visual layout, or verifying how something looks. Act by coordinates afterwards, or take a snapshot to get refs. With x/y/width/height, captures that region magnified 2x (zoom — read small dense UI, verify tiny targets).".into(),
                input_schema: obj(json!({
                    "full_page": json!({"type":"boolean","description":"Capture beyond the viewport too (default false)"}),
                    "x": int_prop("Zoom region left (with y/width/height)"),
                    "y": int_prop("Zoom region top"),
                    "width": int_prop("Zoom region width"),
                    "height": int_prop("Zoom region height"),
                }), &[]),
            },
            ToolSpec {
                name: "computer_extract".into(),
                description: "Read the page's url + title + visible text as plain text (cheapest way to read article/content; no screenshot).".into(),
                input_schema: obj(json!({}), &[]),
            },
            ToolSpec {
                name: "computer_click".into(),
                description: "Click an element by ref from the latest snapshot (preferred) or at viewport coordinates. mouseMoved is sent first so hover menus open.".into(),
                input_schema: obj(json!({
                    "ref": int_prop("Element ref rN from the latest computer_snapshot (preferred)"),
                    "x": int_prop("X coordinate (fallback when no ref)"),
                    "y": int_prop("Y coordinate"),
                    "button": str_prop("left | right | middle (default left)"),
                    "clicks": int_prop("1 = single, 2 = double, 3 = triple (default 1)"),
                }), &[]),
            },
            ToolSpec {
                name: "computer_type".into(),
                description: "Type text into the focused element (unicode-safe, e.g. Chinese). Pass ref to click the field first; clear=true empties it before typing (Ctrl+A, Delete).".into(),
                input_schema: obj(json!({
                    "text": str_prop("Text to type"),
                    "ref": int_prop("Click this ref first to focus the field"),
                    "clear": json!({"type":"boolean","description":"Clear existing content first (default false)"}),
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
                description: "Scroll the page or an element. direction: up/down/left/right; pages: how many screens (default 1). Wheel at x/y or at an element's ref position.".into(),
                input_schema: obj(json!({
                    "direction": str_prop("up | down | left | right"),
                    "pages": int_prop("Screens to scroll (default 1)"),
                    "ref": int_prop("Scroll at this element's position"),
                    "x": int_prop("Wheel X (default 400)"),
                    "y": int_prop("Wheel Y (default 400)"),
                }), &["direction"]),
            },
            ToolSpec {
                name: "computer_hover".into(),
                description: "Hover the mouse over an element (ref) or position — opens hover menus, shows tooltips.".into(),
                input_schema: obj(json!({
                    "ref": int_prop("Element ref (preferred)"),
                    "x": int_prop("X coordinate"),
                    "y": int_prop("Y coordinate"),
                }), &[]),
            },
            ToolSpec {
                name: "computer_drag".into(),
                description: "Press at one point, drag with interpolated mouse moves, release at the other (sliders, canvas drawing, sortable lists).".into(),
                input_schema: obj(json!({
                    "from": json!({"type":"object","description":"Start: {ref} or {x,y}"}),
                    "to": json!({"type":"object","description":"End: {ref} or {x,y}"}),
                    "from_ref": int_prop("Flat form: start ref (alternative to from)"),
                    "to_ref": int_prop("Flat form: end ref (alternative to to)"),
                    "x1": int_prop("Flat form: start X"),
                    "y1": int_prop("Flat form: start Y"),
                    "x2": int_prop("Flat form: end X"),
                    "y2": int_prop("Flat form: end Y"),
                }), &[]),
            },
            ToolSpec {
                name: "computer_select".into(),
                description: "Select an option on a native <select> element by ref (matches option text or value, substring fallback). For custom dropdowns built with divs, use computer_click instead.".into(),
                input_schema: obj(json!({
                    "ref": int_prop("ref of the <select> element"),
                    "option": str_prop("Option text or value to select"),
                }), &["ref", "option"]),
            },
            ToolSpec {
                name: "computer_navigate".into(),
                description: "Navigate the controlled browser: to a URL (waits for load to settle) or back/forward/reload in history. Returns url/title and a fresh screenshot.".into(),
                input_schema: obj(json!({
                    "url": str_prop("http(s) URL to open (action=open, the default)"),
                    "action": str_prop("open | back | forward | reload"),
                }), &[]),
            },
            ToolSpec {
                name: "computer_tabs".into(),
                description: "Manage browser tabs: list (numbered, current marked), new (url), switch (index or target_id prefix), close (index or target_id prefix).".into(),
                input_schema: obj(json!({
                    "action": str_prop("list | new | switch | close"),
                    "url": str_prop("URL for action=new"),
                    "index": int_prop("Tab index for switch/close (see action=list)"),
                    "target_id": str_prop("Target id prefix for switch/close"),
                }), &[]),
            },
            ToolSpec {
                name: "computer_wait".into(),
                description: "Wait for the page to settle (animations, lazy loading, redirects), optionally until `until_text` appears anywhere in the visible text, then observe.".into(),
                input_schema: obj(json!({
                    "seconds": int_prop("Seconds to wait, 0.1-10 (default 1)"),
                    "until_text": str_prop("Keep waiting (within seconds) until this text appears"),
                }), &[]),
            },
            // ---- desktop use（OS 级：真实鼠标键盘 + UIA 无障碍树；仅 Windows）----
            ToolSpec {
                name: "desktop_snapshot".into(),
                description: "PRIMARY desktop observation: the foreground window's UIA accessibility tree (interactive elements carry stable [rN] refs + physical screen coordinates, edit values, focused/disabled marks) AND a full-screen screenshot in ONE call. Coordinates align with screenshot pixels unless a scale note is present. Re-snapshot whenever the screen changed.".into(),
                input_schema: obj(json!({
                    "scope": str_prop("focused (default) | all — all walks every visible window"),
                }), &[]),
            },
            ToolSpec {
                name: "desktop_screenshot".into(),
                description: "Capture ONLY a desktop screenshot (full virtual screen, or a region with x/y/width/height in physical pixels). Screens wider than 1568px are downscaled with a scale note — use tree coordinates for actions.".into(),
                input_schema: obj(json!({
                    "x": int_prop("Region left (with y/width/height)"),
                    "y": int_prop("Region top"),
                    "width": int_prop("Region width"),
                    "height": int_prop("Region height"),
                }), &[]),
            },
            ToolSpec {
                name: "desktop_extract".into(),
                description: "Read the focused window's textual content (document/edit/text nodes + input values) as plain text — cheapest way to read what an app shows.".into(),
                input_schema: obj(json!({
                    "hwnd": int_prop("Optional window handle (default: foreground window)"),
                }), &[]),
            },
            ToolSpec {
                name: "desktop_click".into(),
                description: "Click with the REAL mouse: by ref from the latest desktop_snapshot (preferred) or physical screen coordinates. Sends a mouse-move first. button: left/right/middle; clicks: 1-3.".into(),
                input_schema: obj(json!({
                    "ref": int_prop("Element ref rN from the latest desktop_snapshot (preferred)"),
                    "x": int_prop("Physical screen X (fallback when no ref)"),
                    "y": int_prop("Physical screen Y"),
                    "button": str_prop("left | right | middle (default left)"),
                    "clicks": int_prop("1 = single, 2 = double, 3 = triple (default 1)"),
                }), &[]),
            },
            ToolSpec {
                name: "desktop_type".into(),
                description: "Type text with the REAL keyboard into whatever has focus (unicode-safe incl. Chinese; newline = Enter). desktop_click the field first.".into(),
                input_schema: obj(json!({
                    "text": str_prop("Text to type"),
                }), &["text"]),
            },
            ToolSpec {
                name: "desktop_key".into(),
                description: "Press a real key or combo: Enter, Tab, Escape, Backspace, Delete, Home, End, PageUp, PageDown, arrows, F1-F24, letters/digits, or combos like Control+c, Alt+Tab, Meta (Win key).".into(),
                input_schema: obj(json!({
                    "key": str_prop("Key name or modifier combo"),
                }), &["key"]),
            },
            ToolSpec {
                name: "desktop_scroll".into(),
                description: "Real mouse wheel at the cursor, at x/y, or at a ref's position. direction: up/down/left/right; notches: wheel clicks (default 1).".into(),
                input_schema: obj(json!({
                    "direction": str_prop("up | down | left | right"),
                    "notches": int_prop("Wheel notches (default 1)"),
                    "ref": int_prop("Scroll at this element's position"),
                    "x": int_prop("Physical screen X"),
                    "y": int_prop("Physical screen Y"),
                }), &["direction"]),
            },
            ToolSpec {
                name: "desktop_drag".into(),
                description: "Press, drag with interpolated real mouse moves, release (sliders, canvas, window moving). from/to each take {ref} or {x,y} (flat x1/y1/x2/y2 also accepted).".into(),
                input_schema: obj(json!({
                    "from": json!({"type":"object","description":"Start: {ref} or {x,y}"}),
                    "to": json!({"type":"object","description":"End: {ref} or {x,y}"}),
                }), &[]),
            },
            // ---- 文档能力（读/写/编辑常用办公与文档格式）----
            ToolSpec {
                name: "doc_read".into(),
                description: "Read a document's content as text. Auto-detects by extension: pdf (text per page), docx (paragraphs & tables), pptx (text per slide), xlsx/xls (all sheets as rows), csv, md/txt. doc/ppt (legacy) auto-converts via LibreOffice when installed.".into(),
                input_schema: obj(json!({
                    "path": str_prop("Document path (workspace-relative or absolute)"),
                    "sheet": str_prop("xlsx: read only this sheet name (default: all sheets)"),
                    "max_chars": int_prop("Truncate output at N chars (default 40000)"),
                }), &["path"]),
            },
            ToolSpec {
                name: "doc_create".into(),
                description: "Create a document from a structured spec (by extension): `.docx` doc={blocks:[{type:h1|h2|h3|p|bullet|table, text?, rows?}]}; `.xlsx` doc={sheets:[{name, rows:[[cell...]]}]}; `.pptx` doc={slides:[{title, bullets:[]}]}; `.pdf` doc={blocks:[{type:h1|h2|p, text}]}; `.md`/`.csv` doc={text|rows}. Overwriting an existing file needs confirmation.".into(),
                input_schema: obj(json!({
                    "path": str_prop("Target path; the extension picks the format"),
                    "doc": json!({"type":["object","string"],"description":"Format-specific spec, see description"}),
                }), &["path", "doc"]),
            },
            ToolSpec {
                name: "doc_edit".into(),
                description: "Edit an EXISTING document in place (keeps formatting where possible). ops by type: xlsx [{op:set_cells, sheet, cells:{A1:value}}, {op:add_sheet, name, rows}]; docx [{op:append_blocks, blocks:[...]}, {op:replace_text, find, replace}]; pptx [{op:add_slide, title, bullets}]; pdf [{op:merge, paths:[...]}] (merge into `path`). For .md just use write_file/edit_file.".into(),
                input_schema: obj(json!({
                    "path": str_prop("Document to edit"),
                    "ops": json!({"type":"array","description":"Ops list, see description"}),
                }), &["path", "ops"]),
            },
            ToolSpec {
                name: "doc_convert".into(),
                description: "Convert between document formats via LibreOffice (headless): doc→docx, ppt→pptx, docx→pdf, xlsx→pdf, etc. Requires LibreOffice installed; error message says where to get it when missing.".into(),
                input_schema: obj(json!({
                    "path": str_prop("Source document"),
                    "to": str_prop("Target extension (docx | pdf | pptx | xlsx | html ...)"),
                }), &["path", "to"]),
            },
            ToolSpec {
                name: "desktop_window".into(),
                description: "Manage real windows: list (visible windows with hwnd, * = foreground), focus, minimize, maximize, close (graceful WM_CLOSE — the app decides whether to exit).".into(),
                input_schema: obj(json!({
                    "action": str_prop("list | focus | minimize | maximize | close"),
                    "hwnd": int_prop("Window handle (from action=list)"),
                }), &[]),
            },
            // ---- plugin management & authoring (the agent maintains its own plugins) ----
            ToolSpec {
                name: "plugin_list".into(),
                description: "List installed plugins: id, name, source (manifest/dsh), enabled state, and their commands/tools.".into(),
                input_schema: obj(json!({}), &[]),
            },
            ToolSpec {
                name: "plugin_create".into(),
                description: "Create a plugin that extends your own capabilities. Three parts, all optional but at least one required: (a) `commands` — slash commands with a prompt `template` containing {{input}}; (b) `tools` — callable model tools. Each tool needs a JSON-Schema `parameters` and either a shell `command` template with {{args.<param>}} placeholders, or inline `code` + `language` (python/powershell/node). Inline code is written to the plugin's scripts/ dir and run with the arguments JSON in the PARABLE_TOOL_ARGS env var; it receives PARABLE_TOOL_WORKSPACE too. (c) `prompt` — text appended to the system prompt while enabled. Saved plugins reload immediately, so a tool you create can be called in your next step of the SAME turn.".into(),
                input_schema: obj(json!({
                    "id": str_prop("Plugin id: 1-64 chars, lowercase a-z0-9_- , must start with a letter"),
                    "name": str_prop("Display name (optional, defaults to id)"),
                    "description": str_prop("What the plugin is for"),
                    "prompt": str_prop("System-prompt section text while the plugin is enabled (optional)"),
                    "commands": json!({"type":"array","description":"Slash commands","items":{"type":"object","properties":{"name":{"type":"string"},"description":{"type":"string"},"template":{"type":"string"}},"required":["name","template"]}}),
                    "tools": json!({"type":"array","description":"Callable tools: shell command template, or inline code script","items":{"type":"object","properties":{"name":{"type":"string","description":"lowercase a-z0-9_"},"description":{"type":"string"},"parameters":{"type":"object","description":"JSON Schema for arguments"},"command":{"type":"string","description":"Shell template with {{args.<param>}} placeholders (alternative to code)"},"code":{"type":"string","description":"Inline script source (alternative to command)"},"language":{"type":"string","enum":["python","powershell","node"],"description":"Interpreter for code; default python"}},"required":["name"]}})
                }), &["id"]),
            },
            ToolSpec {
                name: "plugin_edit".into(),
                description: "Replace an existing plugin's manifest with the given fields (same shape as plugin_create; omitted arrays are cleared). Inline `code` tools are re-materialized immediately.".into(),
                input_schema: obj(json!({
                    "id": str_prop("Existing plugin id"),
                    "name": str_prop("Display name"),
                    "description": str_prop("Description"),
                    "prompt": str_prop("System-prompt section text"),
                    "commands": json!({"type":"array","items":{"type":"object","properties":{"name":{"type":"string"},"description":{"type":"string"},"template":{"type":"string"}}}}),
                    "tools": json!({"type":"array","items":{"type":"object","properties":{"name":{"type":"string"},"description":{"type":"string"},"parameters":{"type":"object"},"command":{"type":"string"},"code":{"type":"string"},"language":{"type":"string","enum":["python","powershell","node"]}}}})
                }), &["id"]),
            },
            ToolSpec {
                name: "plugin_toggle".into(),
                description: "Enable or disable a plugin (toggles current state). Reloads the tool list immediately.".into(),
                input_schema: obj(json!({
                    "id": str_prop("Plugin id"),
                }), &["id"]),
            },
            ToolSpec {
                name: "plugin_delete".into(),
                description: "Permanently delete a plugin (removes its directory and generated scripts). Reloads the tool list immediately.".into(),
                input_schema: obj(json!({
                    "id": str_prop("Plugin id"),
                }), &["id"]),
            },
        ];
        let plugin_tools = self
            .plugin_tools
            .read()
            .map(|g| g.clone())
            .unwrap_or_default();
        for t in &plugin_tools {
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
        // PreToolUse hook：exit 2 / deny JSON = 阻断本次调用
        if !crate::hooks::is_empty(&ctx.hooks, crate::hooks::HookEvent::PreToolUse) {
            let payload = json!({"event":"pre_tool_use","tool":name,"args":args,
                "session":ctx.session_id,"turn":ctx.turn_id});
            if let Err(reason) = crate::hooks::run_pre_tool(&ctx.hooks, name, args, payload).await {
                return Ok(ToolOutcome {
                    ok: false,
                    title: format!("{name} · hook 阻断"),
                    display: DisplayKind::Plain(reason.to_string()),
                    for_model: format!("工具调用被 hook 阻断：{reason}。不要重试同一调用；如确需执行，请告知用户调整 hooks 配置。"),
                });
            }
        }
        let res: Result<ToolOutcome, HarnessError> = self.execute_inner(name, args, ctx).await;
        // PostToolUse hook：通知型（失败只记日志）
        if !crate::hooks::is_empty(&ctx.hooks, crate::hooks::HookEvent::PostToolUse) {
            let payload = json!({"event":"post_tool_use","tool":name,
                "ok": res.as_ref().map(|o| o.ok).unwrap_or(false),
                "title": res.as_ref().ok().map(|o| o.title.clone()).unwrap_or_default(),
                "session":ctx.session_id,"turn":ctx.turn_id});
            crate::hooks::run_post_tool(&ctx.hooks, name, payload).await;
        }
        // 执行审计:无论成败都记一条(tool、判定来源 auto/requested、耗时)。
        let source = ctx.broker.decision_of(&ctx.call_id).await;
        let ok = res.as_ref().map(|o| o.ok).unwrap_or(false);
        ctx.broker
            .audit_execution(&ctx.call_id, name, source, ok, t0.elapsed().as_millis())
            .await;
        res
    }

    async fn execute_inner(&self, name: &str, args: &Value, ctx: &ToolCtx) -> Result<ToolOutcome, HarnessError> {
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
            "read_skill" => self.read_skill(args).await,
            "web_search" => web::multi_search(args, ctx).await,
            "deepseek_search" => web::deepseek_search(args, ctx).await,
            "computer_snapshot" => crate::computer::snapshot(args, ctx).await,
            "computer_screenshot" => crate::computer::screenshot(args, ctx).await,
            "computer_axtree" => crate::computer::axtree(args, ctx).await,
            "computer_extract" => crate::computer::extract(args, ctx).await,
            "computer_click" => crate::computer::click(args, ctx).await,
            "computer_type" => crate::computer::type_text(args, ctx).await,
            "computer_key" => crate::computer::key(args, ctx).await,
            "computer_scroll" => crate::computer::scroll(args, ctx).await,
            "computer_hover" => crate::computer::hover(args, ctx).await,
            "computer_drag" => crate::computer::drag(args, ctx).await,
            "computer_select" => crate::computer::select_option(args, ctx).await,
            "computer_navigate" => crate::computer::navigate(args, ctx).await,
            "computer_tabs" => crate::computer::tabs(args, ctx).await,
            "computer_wait" => crate::computer::wait_ms(args, ctx).await,
            "desktop_snapshot" => crate::desktop::snapshot(args, ctx).await,
            "desktop_screenshot" => crate::desktop::screenshot(args, ctx).await,
            "desktop_extract" => crate::desktop::extract(args, ctx).await,
            "desktop_click" => crate::desktop::click(args, ctx).await,
            "desktop_type" => crate::desktop::type_text(args, ctx).await,
            "desktop_key" => crate::desktop::key(args, ctx).await,
            "desktop_scroll" => crate::desktop::scroll(args, ctx).await,
            "desktop_drag" => crate::desktop::drag(args, ctx).await,
            "desktop_window" => crate::desktop::window(args, ctx).await,
            "doc_read" => crate::docs::doc_read(args, ctx).await,
            "doc_create" => crate::docs::doc_create(args, ctx).await,
            "doc_edit" => crate::docs::doc_edit(args, ctx).await,
            "doc_convert" => crate::docs::doc_convert(args, ctx).await,
            "plugin_list" | "plugin_create" | "plugin_edit" | "plugin_toggle" | "plugin_delete" => {
                let r = crate::tools::plugin_admin(name, args, ctx).await;
                if r.is_ok()
                    && matches!(
                        name,
                        "plugin_create" | "plugin_edit" | "plugin_toggle" | "plugin_delete"
                    )
                {
                    self.reload_plugins();
                }
                r
            }
            "schedule_create" | "schedule_list" | "schedule_view" | "schedule_edit" => {
                self.schedule_tool(name, args, ctx).await
            }
            "spawn_agent" | "agent_list" | "agent_wait" | "agent_interrupt" => {
                self.agent_tool(name, args, ctx).await
            }
            "team_task_create" | "team_task_list" | "team_task_get" | "team_task_update" => {
                self.team_tool(name, args, ctx).await
            }
            other if other.starts_with("mcp_") => {
                ctx.authorize(other, "mcp", Risk::Shell).await?;
                let m = ctx.mcp.as_ref().ok_or_else(|| {
                    HarnessError::Tool("MCP 未挂载".into())
                })?;
                let text = m.call_tool(other, args.clone()).await?;
                Ok(ToolOutcome::text(true, format!("MCP {other}"), text))
            }
            other => {
                let hit = self
                    .plugin_tools
                    .read()
                    .ok()
                    .and_then(|g| g.iter().find(|t| t.name == other).cloned());
                match hit {
                    Some(pt) => crate::plugin::run_plugin_tool(&pt, args, ctx).await,
                    None => Err(HarnessError::Tool(format!("unknown tool: {other}"))),
                }
            }
        };
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
        // 文件检查点：写前快照旧内容；新建文件记 None，回退时应删除。
        // 此前只有 edit_file 记检查点，新建的文件一直无法回退。
        {
            let cur = std::fs::read(&path).ok();
            if let Some(cp) = &ctx.checkpoint {
                let _ = cp
                    .record(&ctx.session_id, &ctx.turn_id, &path, cur.as_deref())
                    .await;
            }
        }
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
        // 文件检查点：改前快照旧内容（同 turn 同路径保留最早状态）
        {
            let cur = std::fs::read(&path).ok();
            if let Some(cp) = &ctx.checkpoint {
                let _ = cp
                    .record(&ctx.session_id, &ctx.turn_id, &path, cur.as_deref())
                    .await;
            }
        }
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
        shell::bash(
            command,
            &cwd,
            timeout,
            &ctx.hub,
            &ctx.session_id,
            &ctx.call_id,
        )
        .await
    }

    /// Load a skill's SKILL.md body by id.
    ///
    /// The system prompt only carries the menu, so this is how the model reads
    /// the actual steps. Read-only, so it is allowed in plan mode too.
    async fn read_skill(&self, a: &Value) -> Result<ToolOutcome, HarnessError> {
        let id = a["id"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| HarnessError::Tool("`id` is required".into()))?;
        let wanted_scope = a["scope"].as_str().map(|s| s.trim()).unwrap_or("");
        let scopes: Vec<&str> = match wanted_scope {
            "user" => vec!["user"],
            "project" => vec!["project"],
            _ => vec!["user", "project"],
        };
        for scope in scopes {
            if let Some(sk) = crate::skills::read_body(&self.workspace, scope, id) {
                let mut out = format!(
                    "# Skill: {}\nScope: {}\nPath: {}\n\n{}",
                    sk.name, sk.scope, sk.path, sk.body
                );
                if !sk.warnings.is_empty() {
                    out.push_str("\n\n(warnings: ");
                    out.push_str(&sk.warnings.join("; "));
                    out.push(')');
                }
                return Ok(ToolOutcome::text(
                    true,
                    format!("Loaded skill {}", sk.id),
                    out,
                ));
            }
        }
        // Give the model the menu so it can correct a typo instead of retrying
        // the same wrong id.
        let listing = crate::skills::list(&self.workspace, None);
        let available: Vec<String> = listing
            .skills
            .iter()
            .filter(|s| s.enabled)
            .map(|s| format!("{} — {}", s.id, s.description))
            .collect();
        Err(HarnessError::Tool(if available.is_empty() {
            format!("no skill named `{id}`; no skills are installed")
        } else {
            format!(
                "no skill named `{id}`. Installed skills:\n{}",
                available.join("\n")
            )
        }))
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
        let question = a["question"].as_str().unwrap_or("").trim().to_string();
        if question.is_empty() {
            return Err(HarnessError::Tool("question must not be empty".into()));
        }
        // Keep the UI bounded and deterministic: trim, drop blanks/duplicates,
        // and cap the number of suggested choices.
        let mut options: Vec<String> = Vec::new();
        if let Some(raw) = a["options"].as_array() {
            for o in raw.iter().filter_map(|x| x.as_str()) {
                let v = o.trim();
                if v.is_empty() || options.iter().any(|x| x == v) {
                    continue;
                }
                options.push(v.to_string());
                if options.len() >= 8 {
                    break;
                }
            }
        }

        ctx.hub.publish(
            &ctx.session_id,
            "question",
            json!({"callId": ctx.call_id, "question": question, "options": options.clone()}),
        );

        // ask_timeout_ms > 0（设置 → Agent 行为 → 提问自动裁决）：超时自动选
        // 第一项；没有选项就用空串（仍算"已回答"，走既有 None/Some 语义）。
        let answer = if ctx.ask_timeout_ms > 0 {
            match tokio::time::timeout(
                std::time::Duration::from_millis(ctx.ask_timeout_ms),
                ctx.questions.ask(&ctx.call_id),
            )
            .await
            {
                Ok(a) => a,
                Err(_) => {
                    let auto = options.first().cloned().unwrap_or_default();
                    ctx.hub.publish(
                        &ctx.session_id,
                        "question.resolved",
                        json!({"callId": ctx.call_id, "answer": auto, "auto": true}),
                    );
                    Some(auto)
                }
            }
        } else {
            ctx.questions.ask(&ctx.call_id).await
        };
        // `None` = timed out / interrupted; `Some("")` = the user submitted
        // an empty answer, which is still an explicit answer.
        let answered = answer.is_some();
        let answer = answer.map(|s| s.trim().to_string());
        let for_model = match &answer {
            Some(a) if !a.is_empty() => format!("The user answered: {a}"),
            Some(_) => "The user answered with an empty response.".to_string(),
            None => "The user did not answer the question (it timed out or the turn was interrupted).".to_string(),
        };

        Ok(ToolOutcome {
            ok: true,
            title: if answered {
                format!("Asked the user: {}", truncate(&question, 60))
            } else {
                format!("Asked the user (no answer): {}", truncate(&question, 60))
            },
            display: DisplayKind::Ask {
                question,
                options,
                answer,
            },
            for_model,
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

    /// The root session of the tree the caller belongs to.
    ///
    /// A caller that is itself a child continues its parent's tree; anything
    /// else is the root of its own tree. This is the same rule the subagent
    /// service uses when it spawns, so both agree on the team's identity.
    async fn team_root(&self, sub: &crate::subagent::SharedSubagents, session: &str) -> String {
        match sub.catalog.find_by_session(session).await {
            Some(child) => child.root_session,
            None => session.to_string(),
        }
    }

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
                    format!(
                        "Spawned child agent {id} (id `{id}`). It runs in the background; call agent_wait with this id to collect its result."
                    ),
                ))
            }

            "agent_list" => {
                // 只列自己这棵树里的成员——进程级的全局目录会把别的会话的
                // 子代理也倒给模型，既噪音又是信息泄漏。
                let root = self.team_root(&sub, &ctx.session_id).await;
                let kids = sub.tree(&root).await;
                let mut body = format!("{} child agent(s) in this team:\n", kids.len());
                for k in &kids {
                    body.push_str(&format!(
                        "  [{}] {} ({}) — depth {} {:?}{}\n",
                        k.id,
                        k.name,
                        k.label,
                        k.depth,
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

            "agent_wait" => {
                let target = a["agent"].as_str().unwrap_or("").trim();
                if target.is_empty() {
                    return Err(HarnessError::Tool("`agent` must not be empty".into()));
                }
                let root = self.team_root(&sub, &ctx.session_id).await;
                let child = sub.resolve_child(&root, target).await.ok_or_else(|| {
                    HarnessError::Tool(format!(
                        "no agent `{target}` in this team; use agent_list to see the members"
                    ))
                })?;
                // 等自己会死锁：调用方就是那个子代理。
                if child.session_id == ctx.session_id {
                    return Err(HarnessError::Tool(
                        "an agent cannot wait for itself".into(),
                    ));
                }
                let secs = a["timeout_seconds"].as_u64().unwrap_or(300).clamp(1, 1800);
                let outcome = sub
                    .wait_for(&child.id, std::time::Duration::from_secs(secs))
                    .await;
                let body = match outcome {
                    Some(o) => match o.status {
                        crate::subagent::ChildStatus::Done => {
                            format!("Agent `{}` finished:\n\n{}", child.name, o.result)
                        }
                        crate::subagent::ChildStatus::Failed => format!(
                            "Agent `{}` failed: {}",
                            child.name,
                            o.error.unwrap_or_else(|| "unknown error".into())
                        ),
                        crate::subagent::ChildStatus::Cancelled => {
                            format!("Agent `{}` was cancelled.", child.name)
                        }
                        crate::subagent::ChildStatus::Running => {
                            format!("Agent `{}` is still running.", child.name)
                        }
                    },
                    None => format!(
                        "Agent `{}` did not finish within {secs}s; it may still be running. Check agent_list.",
                        child.name
                    ),
                };
                Ok(ToolOutcome {
                    ok: true,
                    title: format!("Waited for {}", child.name),
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

    // ---------------------------------------------------------------- team board

    /// The shared team board of the tree the caller belongs to.
    ///
    /// Reads through to the root session's own board: children never keep a
    /// second copy, so the whole team sees one state.
    async fn team_board(
        &self,
        sub: &crate::subagent::SharedSubagents,
        ctx: &ToolCtx,
    ) -> Result<(String, crate::team::TeamState), HarnessError> {
        let root = self.team_root(sub, &ctx.session_id).await;
        let state = ctx
            .store
            .get_team(&root)
            .await
            .ok_or_else(|| HarnessError::Other("agent teams are not enabled for this session".into()))?;
        Ok((root, state))
    }

    /// Who is asking, in what capacity, for the team board.
    async fn team_caller(
        &self,
        sub: &crate::subagent::SharedSubagents,
        ctx: &ToolCtx,
        root: &str,
    ) -> crate::team::Caller {
        // A member's name comes from the catalogue; the root has no entry.
        let name = sub
            .catalog
            .find_by_session(&ctx.session_id)
            .await
            .map(|c| c.name)
            .unwrap_or_else(|| "lead".to_string());
        crate::team::Caller {
            session_id: ctx.session_id.clone(),
            name,
            root_id: root.to_string(),
            is_lead: ctx.session_id == root,
        }
    }

    async fn team_tool(
        &self,
        name: &str,
        a: &Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutcome, HarnessError> {
        let Some(sub) = ctx.subagents.clone() else {
            return Err(HarnessError::Tool("subagents are not mounted".into()));
        };
        match name {
            "team_task_create" => {
                let (root, mut state) = self.team_board(&sub, ctx).await?;
                ctx.authorize("team_task_create", &root, Risk::Write).await?;
                let caller = self.team_caller(&sub, ctx, &root).await;
                let req = crate::team::CreateTask {
                    subject: a["subject"].as_str().unwrap_or("").to_string(),
                    description: a["description"].as_str().unwrap_or("").to_string(),
                    blocked_by: string_array(a, "blocked_by"),
                    write_scopes: string_array(a, "write_scopes"),
                };
                let view = crate::team::create_task(&mut state, &caller, &req)?;
                ctx.store.set_team(&root, Some(state)).await?;
                self.publish_team_event(&ctx.hub, &root, &view);
                Ok(ToolOutcome {
                    ok: true,
                    title: format!("Created task {}", view.id),
                    display: DisplayKind::Plain(format!("{}", view.subject)),
                    for_model: format!(
                        "Created task {} (revision {}): {}",
                        view.id, view.revision, view.subject
                    ),
                })
            }

            "team_task_list" => {
                let (_root, state) = self.team_board(&sub, ctx).await?;
                let owner = a["owner"].as_str();
                let tasks: Vec<crate::team::TaskView> = crate::team::board_view(&state)
                    .into_iter()
                    .filter(|t| {
                        a["status"].as_str().map(|s| t.status.as_str() == s).unwrap_or(true)
                            && owner
                                .map(|o| {
                                    t.owner_name.as_deref() == Some(o)
                                        || t.owner_id.as_deref() == Some(o)
                                })
                                .unwrap_or(true)
                            && a["unowned"]
                                .as_bool()
                                .map(|u| t.owner_id.is_none() == u)
                                .unwrap_or(true)
                            && a["ready"].as_bool().map(|r| t.ready == r).unwrap_or(true)
                    })
                    .collect();
                let body = if tasks.is_empty() {
                    "No shared tasks.".to_string()
                } else {
                    tasks
                        .iter()
                        .map(|t| {
                            format!(
                                "{} [{}] rev{} {} — owner: {}{}{}",
                                t.id,
                                t.status.as_str(),
                                t.revision,
                                t.subject,
                                t.owner_name
                                    .as_deref()
                                    .or(t.owner_id.as_deref())
                                    .unwrap_or("unowned"),
                                if t.ready { ", ready" } else { "" },
                                if t.blocked_by.is_empty() {
                                    String::new()
                                } else {
                                    format!(", blocked by {}", t.blocked_by.join(", "))
                                }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                Ok(ToolOutcome {
                    ok: true,
                    title: format!("Listed {} team task(s)", tasks.len()),
                    display: DisplayKind::Plain(body.clone()),
                    for_model: body,
                })
            }

            "team_task_get" => {
                let (_root, state) = self.team_board(&sub, ctx).await?;
                let id = a["task_id"].as_str().unwrap_or("");
                let task = state
                    .find(id)
                    .ok_or_else(|| HarnessError::NotFound(format!("team task `{id}`")))?;
                let view = crate::team::project_task_view(&state, task);
                let blockers = if view.blocked_by.is_empty() {
                    "-".to_string()
                } else {
                    view.blocked_by.join(", ")
                };
                let scopes = if view.write_scopes.is_empty() {
                    "-".to_string()
                } else {
                    view.write_scopes.join(", ")
                };
                let body = format!(
                    "{} (rev {}) [{}] {}\n{}\nowner: {}\nblocked by: {}\nwrite scopes: {}\nready: {}{}",
                    view.id,
                    view.revision,
                    view.status.as_str(),
                    view.subject,
                    view.description,
                    view.owner_name
                        .as_deref()
                        .or(view.owner_id.as_deref())
                        .unwrap_or("unowned"),
                    blockers,
                    scopes,
                    view.ready,
                    if view.write_scope_warnings.is_empty() {
                        String::new()
                    } else {
                        format!("\nwarnings: {}", view.write_scope_warnings.join("; "))
                    }
                );
                Ok(ToolOutcome {
                    ok: true,
                    title: format!("Read task {}", view.id),
                    display: DisplayKind::Plain(body.clone()),
                    for_model: body,
                })
            }

            "team_task_update" => {
                let (root, mut state) = self.team_board(&sub, ctx).await?;
                ctx.authorize("team_task_update", &root, Risk::Write).await?;
                let caller = self.team_caller(&sub, ctx, &root).await;
                let action = a["action"].as_str().unwrap_or("");
                let action = crate::team::TaskAction::parse(action).ok_or_else(|| {
                    HarnessError::Tool(format!(
                        "unknown task action `{action}`; expected claim | release | edit | set_dependencies | complete | reopen | reassign | delete"
                    ))
                })?;
                // `owner` is a name; resolve it to a session id in this tree.
                let owner = match a["owner"].as_str() {
                    Some(name) if !name.is_empty() => {
                        let child = sub
                            .tree(&root)
                            .await
                            .into_iter()
                            .find(|c| c.name == name || c.id == name)
                            .ok_or_else(|| {
                                HarnessError::Tool(format!(
                                    "no agent named `{name}` in this team; use agent_list to see the members"
                                ))
                            })?;
                        Some((child.session_id.clone(), child.name.clone()))
                    }
                    _ => None,
                };
                let req = crate::team::UpdateTask {
                    task_id: a["task_id"].as_str().unwrap_or("").to_string(),
                    expected_revision: a["expected_revision"].as_u64().unwrap_or(0),
                    action,
                    subject: a["subject"].as_str().map(String::from),
                    description: a["description"].as_str().map(String::from),
                    blocked_by: a.get("blocked_by").and_then(|v| v.as_array()).map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    }),
                    write_scopes: a.get("write_scopes").and_then(|v| v.as_array()).map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    }),
                    owner: owner.as_ref().map(|(id, _)| id.clone()),
                    owner_name: owner.map(|(_, name)| name),
                };
                let view = crate::team::update_task(&mut state, &caller, &req)?;
                ctx.store.set_team(&root, Some(state)).await?;
                self.publish_team_event(&ctx.hub, &root, &view);
                Ok(ToolOutcome {
                    ok: true,
                    title: format!("{} task {}", req.action.as_str(), view.id),
                    display: DisplayKind::Plain(format!("{}", view.subject)),
                    for_model: format!(
                        "Task {} is now {} (revision {}).",
                        view.id,
                        view.status.as_str(),
                        view.revision
                    ),
                })
            }

            other => Err(HarnessError::Tool(format!("unknown team tool: {other}"))),
        }
    }

    /// Tell the UI the board changed. Published to the root session so the
    /// team panel sees it while that conversation is open.
    fn publish_team_event(
        &self,
        hub: &crate::event::EventHub,
        root: &str,
        view: &crate::team::TaskView,
    ) {
        hub.publish(
            root,
            "team.task",
            serde_json::json!({
                "taskId": view.id,
                "status": view.status.as_str(),
                "revision": view.revision,
                "subject": view.subject,
                "owner": view.owner_name.clone().or(view.owner_id.clone()),
            }),
        );
    }
}

/// Read a JSON array of strings, ignoring non-string entries.
fn string_array(a: &Value, key: &str) -> Vec<String> {
    a.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
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
                let code = t
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let language = t
                    .get("language")
                    .and_then(|v| v.as_str())
                    .unwrap_or("python")
                    .trim()
                    .to_ascii_lowercase();
                if tname.is_empty() {
                    return Err(HarnessError::Tool("tool `name` 不能为空".into()));
                }
                if command.is_empty() && code.trim().is_empty() {
                    return Err(HarnessError::Tool(format!(
                        "tool `{tname}` 需要 `command` 或 `code` 之一"
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
                    code,
                    language,
                    script: String::new(),
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
            let mut m = PluginManifest {
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
            let mut scripted = 0usize;
            for t in &mut m.tools {
                if !t.code.trim().is_empty() {
                    let (cmd, script) =
                        store.write_script(&id, &t.name, &t.language, &t.code)?;
                    t.command = cmd;
                    t.script = script;
                    scripted += 1;
                }
            }
            store.save_manifest(&id, &m)?;
            let mut note = format!(
                "插件 `{}` ({} 个命令, {} 个工具{}) 已{}。工具表已重载，本 turn 下一步即可调用；新增命令与提示词段下一次 turn 生效。",
                id,
                m.commands.len(),
                m.tools.len(),
                if scripted > 0 {
                    format!("，其中 {scripted} 个由内联代码生成")
                } else {
                    String::new()
                },
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
                    "插件 `{}` 已{}。工具表已重载，立即生效。",
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
                format!("插件 `{}` 已删除（目录、脚本与启用标记一并移除）。工具表已重载。", id),
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

    #[test]
    fn team_tools_are_declared() {
        let specs = Registry::new(std::path::PathBuf::from(".")).specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        for want in [
            "team_task_create",
            "team_task_list",
            "team_task_get",
            "team_task_update",
            "agent_wait",
        ] {
            assert!(names.contains(&want), "{want} must be declared");
        }
    }

    #[test]
    fn string_array_ignores_non_strings() {
        let v = serde_json::json!(["a", 1, "b", null]);
        assert_eq!(string_array(&v, "x").len(), 0, "not an array at that key");
        assert_eq!(
            string_array(&serde_json::json!({"k": ["a", 1, "b"]}), "k"),
            vec!["a".to_string(), "b".to_string()]
        );
    }
}
