//! Session persistence.
//!
//! Deliberate deviation from the original spec: this is a JSON-file store under
//! `~/.parable/sessions/`, not SQLite. rusqlite with the `bundled` feature drags
//! in a C toolchain on Windows for data volumes that are tiny on a local box.
//! The API surface here is what the rest of the code sees, so swapping in SQLite
//! later touches only this module.

use crate::config::Config;
use crate::error::HarnessError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    pub role: String,
    /// Neutral message parts, stored as JSON so any provider can rebuild them.
    pub parts: Value,
    /// Tool-card render payloads, aligned with the tool results in `parts`.
    /// Lets the UI rebuild the exact cards it showed when the turn ran.
    #[serde(default)]
    pub displays: Vec<Value>,
    /// 产生这条消息的 turn（t_xxx）。前端用它把「本轮文件改动摘要条」
    /// 精确放回它所属的回合末尾；旧会话数据没有此字段，按空串兼容。
    #[serde(default)]
    pub turn: String,
    /// 系统注入、不该渲染给用户的消息（子代理结果投递等）。模型历史
    /// 照常包含它；/api/sessions/{id}/messages 对前端过滤掉。
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub created_at: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ToolEdit {
    pub id: String,
    /// 这条工具调用归属的消息下标（assistant 消息），新增时可以指定挂在哪一步。
    pub step: Option<usize>,
    pub name: Option<String>,
    pub args: Option<Value>,
    pub result: Option<String>,
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub workspace: String,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub permission_mode: String,
    #[serde(default)]
    pub thinking_level: u32,
    /// 最近一轮的结局：""（还没跑过）| "ok" | "failed"。finalize_turn 记账，
    /// 侧栏会话状态小球按它上色（失败红 / 完成蓝）。
    #[serde(default)]
    pub outcome: String,
    /// 侧栏状态小球的「未读」位：状态变化（轮次收尾/提问）时置起，进会话
    /// 消费（PATCH dot=false）。彩色球按它显示；转圈是实时态不看它。
    #[serde(default)]
    pub dot: bool,
}

/// A message the user sent while a turn was already running.
///
/// `queue` is delivered at the next turn boundary; `steer` is injected at the
/// next model-step boundary of the turn that is currently running. Stored with
/// the session so a reload (or a late turn end) cannot silently lose the user's
/// text. This mirrors ZCode/dsh's inbox with two delivery modes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingInput {
    pub id: String,
    /// "queue" | "steer"
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<crate::agent::Attachment>,
    #[serde(default)]
    pub opts: crate::agent::TurnOptions,
    #[serde(default)]
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Session {
    #[serde(flatten)]
    pub meta: SessionSummary,
    #[serde(default)]
    pub messages: Vec<StoredMessage>,
    /// Inputs waiting for delivery: queued next turns and steering for the
    /// active turn. Old session files simply load with an empty list.
    #[serde(default)]
    pub pending: Vec<PendingInput>,
    #[serde(default)]
    pub usage: UsageTotals,
    /// Persistent goal for this session (`/goal`). Lives and dies with the
    /// session; the agent loop consults it after every turn.
    #[serde(default)]
    pub goal: Option<GoalState>,
    /// Shared Agent Teams board for the tree rooted at this session.
    /// Lives and dies with the session; children reach it through their root.
    #[serde(default)]
    pub team: Option<crate::team::TeamState>,
}

/// A persistent objective, per dsh/zcode: one goal per session, a small state
/// machine, and a round budget so the auto-continuation cannot run forever.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalState {
    pub objective: String,
    /// "active" | "paused" | "complete"
    pub status: String,
    #[serde(default)]
    pub rounds: u32,
    #[serde(default = "goal_max_rounds")]
    pub max_rounds: u32,
    /// Consecutive judge rounds with the same reason — 3 in a row means stuck.
    #[serde(default)]
    pub stalls: u32,
    #[serde(default)]
    pub last_reason: String,
    #[serde(default)]
    pub updated_at: u64,
}

fn goal_max_rounds() -> u32 {
    32
}

impl GoalState {
    pub fn new(objective: &str) -> Self {
        Self {
            objective: objective.to_string(),
            status: "active".into(),
            rounds: 0,
            max_rounds: goal_max_rounds(),
            stalls: 0,
            last_reason: String::new(),
            updated_at: crate::event::now_ms(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.status == "active"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub turns: u64,
    pub steps: u64,
    pub wall_ms: u64,
    /// 最近一次模型请求的上下文占用（system + tools + messages 的 token 估计）。
    ///
    /// 与 `input_tokens` 的语义不同：后者是整轮所有模型调用**累加**的计费量，
    /// 一轮里十几次工具调用会把它堆到上下文窗口的数倍；这里只保留最后一次
    /// 请求的真实大小，供上下文环/百分比使用。旧会话没有此字段，按 0 兼容。
    #[serde(default)]
    pub ctx_used: u64,
}

impl Default for SessionSummary {
    fn default() -> Self {
        Self {
            id: String::new(),
            title: String::new(),
            workspace: String::new(),
            created_at: crate::event::now_ms(),
            updated_at: crate::event::now_ms(),
            pinned: false,
            archived: false,
            group: String::new(),
            model: String::new(),
            permission_mode: "ask".into(),
            thinking_level: 2,
            outcome: String::new(),
            dot: false,
        }
    }
}

pub struct SessionStore {
    root: PathBuf,
    cache: Mutex<HashMap<String, Session>>,
    /// Session metadata index: id -> (summary, file mtime, size).
    ///
    /// `list()` used to read and parse every session file on every call, but
    /// it only needs the summary. With dozens of long chats that is megabytes
    /// of JSON parsed per request — the dominant cost of `/api/sessions` and
    /// of the message search. This index keeps the summary plus the file
    /// stamp it was derived from, so a file that changed on disk is re-read.
    meta_index: Mutex<HashMap<String, MetaEntry>>,
    /// Per-session searchable text, memoised by file stamp.
    text_index: Mutex<HashMap<String, TextEntry>>,
    /// Per-root team-board locks (T-01)：看板是「读-改-写」整板落盘，两个并发
    /// 请求各读半板再回写会互相覆盖；按根会话一把锁把整段临界区串起来。
    team_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

#[derive(Clone)]
struct MetaEntry {
    summary: SessionSummary,
    /// Seconds + nanoseconds of the file's last modification.
    mtime: (u64, u32),
    len: u64,
    /// When the file was last stamped — bounds how stale the entry may be.
    checked_at: std::time::Instant,
}

/// How long a session-meta index entry is trusted without re-stat'ing the file.
const META_TTL: std::time::Duration = std::time::Duration::from_millis(750);

#[derive(Clone)]
struct TextEntry {
    stamp: ((u64, u32), u64),
    /// Cached text size in bytes — used for the LRU budget.
    bytes: usize,
    /// Last time this entry was served (LRU ordering).
    touched: std::time::Instant,
    items: Vec<(usize, String, String)>,
}

/// Upper bound on the searchable-text cache, in bytes of raw message text.
///
/// The cache holds one copy of every session's message bodies; without a cap
/// it grows with the archive (3.6 MB of session files → ~8 MB of RSS). 4 MB
/// keeps the recent chats hot while bounding the footprint.
const TEXT_CACHE_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Drop least-recently-used entries until the cached text fits the budget.
///
/// The newest entry is never evicted on its own (the caller just inserted it),
/// so the cache always keeps at least the session being searched.
fn evict_text_cache(idx: &mut HashMap<String, TextEntry>, budget: usize) {
    let mut total: usize = idx.values().map(|e| e.bytes).sum();
    while total > budget && idx.len() > 1 {
        let Some(lru) = idx
            .iter()
            .min_by_key(|(_, e)| e.touched)
            .map(|(k, _)| k.clone())
        else {
            break;
        };
        if let Some(e) = idx.remove(&lru) {
            total = total.saturating_sub(e.bytes);
        }
    }
}

impl SessionStore {
    pub fn new() -> Self {
        let root = Config::sessions_dir();
        let _ = std::fs::create_dir_all(&root);
        Self {
            root,
            cache: Mutex::new(HashMap::new()),
            meta_index: Mutex::new(HashMap::new()),
            text_index: Mutex::new(HashMap::new()),
            team_locks: Mutex::new(HashMap::new()),
        }
    }

    /// Store rooted at an explicit directory (tests, alternate data homes).
    pub fn with_root(root: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&root);
        Self {
            root,
            cache: Mutex::new(HashMap::new()),
            meta_index: Mutex::new(HashMap::new()),
            text_index: Mutex::new(HashMap::new()),
            team_locks: Mutex::new(HashMap::new()),
        }
    }

    fn path_for(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    /// 外部传入 id 的入口（load/delete/exists/insert_full…）必须走这里：
    /// id 过不了校验就返回 None，绝不拼路径——否则含 `/`、`\`、`..` 的
    /// 恶意/异常 id 会借 `path_for` 拼出 store 根之外的路径，读写删都越界。
    /// `path_for` 本身保留给内部自生成 id 的调用（persist 用 s.meta.id）。
    fn checked_path_for(&self, id: &str) -> Option<PathBuf> {
        if is_valid_session_id(id) {
            Some(self.path_for(id))
        } else {
            None
        }
    }

    pub async fn create(&self, workspace: &str, title: &str) -> Session {
        let id = format!("s_{}", uuid::Uuid::new_v4().simple().to_string()[..12].to_string());
        let meta = SessionSummary {
            id: id.clone(),
            title: if title.is_empty() {
                "New chat".into()
            } else {
                title.to_string()
            },
            workspace: workspace.to_string(),
            ..Default::default()
        };
        let s = Session {
            meta,
            messages: Vec::new(),
            pending: Vec::new(),
            usage: UsageTotals::default(),
            goal: None,
            team: None,
        };
        if let Err(e) = self.persist(&s).await {
            tracing::error!("session {}: persist failed: {e}", s.meta.id);
        }
        self.cache.lock().await.insert(id.clone(), s.clone());
        s
    }

    /// 复制一个会话（侧聊/辅助对话用）：历史消息全部带过去，模型能当上下文用，
    /// 但侧边面板只显示 fork 之后的新消息。返回 (新会话, 被复制的历史条数)。
    pub async fn fork(&self, src: &str, workspace: &str, title: &str) -> Option<(Session, usize)> {
        let from = self.load(src).await?;
        let mut s = self.create(workspace, title).await;
        let hist = from.messages.len();
        s.messages = from.messages.clone();
        s.meta.model = from.meta.model.clone();
        s.meta.permission_mode = from.meta.permission_mode.clone();
        s.meta.thinking_level = from.meta.thinking_level;
        if let Err(e) = self.persist(&s).await {
            tracing::error!("session {}: fork persist failed: {e}", s.meta.id);
        }
        self.cache.lock().await.insert(s.meta.id.clone(), s.clone());
        Some((s, hist))
    }

    /// 原子落盘：临时文件 + rename（见 `crate::atomic_write`），失败返回错误由
    /// 调用方记录——静默丢写会让重载后的会话回退到旧状态。
    ///
    /// 调用纪律：所有「cache 锁内改内存」的写路径必须在**持有 cache 锁期间**
    /// 调用本方法（tokio Mutex 持锁跨 await 安全，persist 内部不碰 cache），
    /// 把「读-改-写-落盘」整体串行化——否则两个并发写方交错时，后完成内存
    /// 改动的一方可能拿着旧快照落盘，把磁盘上新快照回滚掉。
    async fn persist(&self, s: &Session) -> Result<(), HarnessError> {
        let p = self.path_for(&s.meta.id);
        let json = serde_json::to_string_pretty(s)
            .map_err(|e| HarnessError::Other(format!("session {}: serialize failed: {e}", s.meta.id)))?;
        std::fs::create_dir_all(&self.root)?;
        crate::atomic_write(&p, json.as_bytes())?;
        Ok(())
    }

    pub async fn load(&self, id: &str) -> Option<Session> {
        {
            let cache = self.cache.lock().await;
            if let Some(s) = cache.get(id) {
                return Some(s.clone());
            }
        }
        // 非法 id（含路径分隔符/点等）直接当「不存在」：不碰磁盘，也不会
        // 走到下面的 .corrupt 备份逻辑去动 store 根之外的文件。
        let p = self.checked_path_for(id)?;
        let text = std::fs::read_to_string(&p).ok()?;
        match serde_json::from_str::<Session>(&text) {
            Ok(s) => {
                self.cache.lock().await.insert(id.to_string(), s.clone());
                Some(s)
            }
            Err(e) => {
                // 损坏文件处理：改名备份原始数据（rename 失败就留在原地），
                // 返回 None。这样后续 or_insert_with/重建只会生成空会话，
                // 不会覆盖丢失的原始记录。
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let mut bak = p.clone().into_os_string();
                bak.push(format!(".corrupt-{ts}"));
                let backup = PathBuf::from(bak);
                match std::fs::rename(&p, &backup) {
                    Ok(()) => tracing::error!(
                        "session {id}: corrupt file ({e}); backed up to {}",
                        backup.display()
                    ),
                    Err(be) => tracing::error!(
                        "session {id}: corrupt file ({e}); backup rename failed: {be}, file left in place"
                    ),
                }
                None
            }
        }
    }

    /// 会话是否已存在（内存 cache 或磁盘文件），供 schedule 的
    /// `Target::Existing` 校验，避免把任务投进幽灵会话。
    pub async fn exists(&self, id: &str) -> bool {
        if self.cache.lock().await.contains_key(id) {
            return true;
        }
        self.checked_path_for(id)
            .map(|p| p.exists())
            .unwrap_or(false)
    }

    /// dsh's archived-session gate: cheap check used by start_turn so an
    /// archived session never runs another model step.
    pub async fn is_archived(&self, id: &str) -> bool {
        {
            let cache = self.cache.lock().await;
            if let Some(s) = cache.get(id) {
                return s.meta.archived;
            }
        }
        self.load(id).await.map(|s| s.meta.archived).unwrap_or(false)
    }

    /// List session summaries, optionally filtered by a title substring.
    ///
    /// Uses the metadata index: a session file is parsed only when it is new
    /// or its mtime/size changed. Without this the sidebar (and the message
    /// search that calls `list()`) re-parsed every chat on every request.
    pub async fn list(&self, query: &str) -> Vec<SessionSummary> {
        let mut out = Vec::new();
        let q = query.to_lowercase();
        if let Ok(rd) = std::fs::read_dir(&self.root) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some("json") {
                    continue;
                }
                let Some(summary) = self.summary_for(&p).await else {
                    continue;
                };
                if !q.is_empty() && !summary.title.to_lowercase().contains(&q) {
                    continue;
                }
                out.push(summary);
            }
        }
        out.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then(b.updated_at.cmp(&a.updated_at))
        });
        out
    }

    /// Summary for one session file, memoised by (mtime, size).
    ///
    /// The file stamp check costs one `stat` per session, which on Windows is
    /// not free (64 sessions ≈ 1ms). A short-lived cache skips even that:
    /// within the TTL the index is trusted, after it every file is re-stamped.
    async fn summary_for(&self, path: &Path) -> Option<SessionSummary> {
        let id = path.file_stem()?.to_str()?.to_string();

        // Fast path: index entry still within TTL -> trust it.
        {
            let idx = self.meta_index.lock().await;
            if let Some(e) = idx.get(&id) {
                if e.checked_at.elapsed() < META_TTL {
                    return Some(e.summary.clone());
                }
            }
        }

        let meta = std::fs::metadata(path).ok()?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| (d.as_secs(), d.subsec_nanos()))
            .unwrap_or((0, 0));
        let len = meta.len();

        // Stamped but the file changed since indexing -> re-read.
        {
            let mut idx = self.meta_index.lock().await;
            if let Some(e) = idx.get(&id) {
                if e.mtime == mtime && e.len == len {
                    let mut e = e.clone();
                    e.checked_at = std::time::Instant::now();
                    let out = e.summary.clone();
                    idx.insert(id.clone(), e);
                    return Some(out);
                }
            }
        }

        // Parse only the metadata fields. The file is `#[serde(flatten)]`-ed
        // meta + messages, so we scan the head of the JSON for the summary
        // keys instead of materialising every message.
        let text = std::fs::read_to_string(path).ok()?;
        let summary = match serde_json::from_str::<Session>(&text) {
            Ok(s) => s.meta,
            Err(_) => parse_summary_head(&text)?,
        };
        self.meta_index.lock().await.insert(
            id,
            MetaEntry {
                summary: summary.clone(),
                mtime,
                len,
                checked_at: std::time::Instant::now(),
            },
        );
        Some(summary)
    }

    pub async fn append_message(&self, id: &str, msg: StoredMessage) -> Result<(), HarnessError> {
        // 仅当内存 cache 已有、或磁盘文件存在时才允许追加——拒绝为不存在的
        // 会话凭空创建文件（幽灵会话，schedule 投递错目标时尤其危险）。
        if !self.cache.lock().await.contains_key(id) {
            self.load(id)
                .await
                .ok_or_else(|| HarnessError::NotFound(format!("session {id}")))?;
        }
        let mut cache = self.cache.lock().await;
        let s = cache
            .get_mut(id)
            .ok_or_else(|| HarnessError::NotFound(format!("session {id}")))?;
        s.messages.push(msg);
        s.meta.updated_at = crate::event::now_ms();
        // 持锁落盘：不提前 drop(cache)，把「读-改-写-落盘」整段放进同一把
        // cache 锁里——两个并发写方再怎么交错，最后完成写的快照必然最后
        // 落盘，磁盘不会被旧快照回滚。tokio Mutex 持锁跨 await 是安全的
        // （persist 内部不再碰 cache，无死锁）。persist 失败上抛，与
        // insert_pending 一致；调用方 agent.rs 按 saved.is_ok() 决定是否
        // 发 step.persisted，落盘失败不发事件。
        let snapshot = s.clone();
        self.persist(&snapshot).await?;
        Ok(())
    }

    /// Inputs waiting for delivery in a session. Missing/never-opened sessions
    /// resolve as an empty list, not an error: the queue dock can poll safely.
    pub async fn pending(&self, id: &str) -> Vec<PendingInput> {
        self.load(id)
            .await
            .map(|s| s.pending)
            .unwrap_or_default()
    }

    pub async fn add_pending(&self, id: &str, item: PendingInput) -> Result<(), HarnessError> {
        self.insert_pending(id, item, false).await
    }

    /// Put an item back at the front after a failed queue drain, preserving FIFO.
    pub async fn add_pending_front(&self, id: &str, item: PendingInput) -> Result<(), HarnessError> {
        self.insert_pending(id, item, true).await
    }

    async fn insert_pending(
        &self,
        id: &str,
        item: PendingInput,
        front: bool,
    ) -> Result<(), HarnessError> {
        if !self.cache.lock().await.contains_key(id) {
            self.load(id)
                .await
                .ok_or_else(|| HarnessError::NotFound(format!("session {id}")))?;
        }
        let mut cache = self.cache.lock().await;
        let s = cache
            .get_mut(id)
            .ok_or_else(|| HarnessError::NotFound(format!("session {id}")))?;
        if front {
            s.pending.insert(0, item);
        } else {
            s.pending.push(item);
        }
        s.meta.updated_at = crate::event::now_ms();
        // 持锁落盘（同 append_message 的串行化纪律），失败照旧上抛。
        let snapshot = s.clone();
        self.persist(&snapshot).await?;
        Ok(())
    }

    /// Atomically take up to `limit` pending items of one kind (FIFO).
    pub async fn take_pending(&self, id: &str, kind: &str, limit: usize) -> Vec<PendingInput> {
        if !self.cache.lock().await.contains_key(id) && self.load(id).await.is_none() {
            return Vec::new();
        }
        let mut cache = self.cache.lock().await;
        let Some(s) = cache.get_mut(id) else {
            return Vec::new();
        };
        let mut taken = Vec::new();
        s.pending.retain(|p| {
            if p.kind == kind && taken.len() < limit {
                taken.push(p.clone());
                false
            } else {
                true
            }
        });
        if taken.is_empty() {
            return taken;
        }
        s.meta.updated_at = crate::event::now_ms();
        // 持锁落盘。返回类型是 Vec 无法上抛错误，失败仍记日志——但落盘
        // 已与改内存串行化，不会再被并发写方回滚。
        let snapshot = s.clone();
        if let Err(e) = self.persist(&snapshot).await {
            tracing::error!("session {id}: persist pending take failed: {e}");
        }
        taken
    }

    pub async fn remove_pending(&self, id: &str, pending_id: &str) -> Option<PendingInput> {
        if !self.cache.lock().await.contains_key(id) && self.load(id).await.is_none() {
            return None;
        }
        let mut cache = self.cache.lock().await;
        let s = cache.get_mut(id)?;
        let pos = s.pending.iter().position(|p| p.id == pending_id)?;
        let item = s.pending.remove(pos);
        s.meta.updated_at = crate::event::now_ms();
        // 持锁落盘（返回 Option 无法上抛，失败仍记日志）。
        let snapshot = s.clone();
        if let Err(e) = self.persist(&snapshot).await {
            tracing::error!("session {id}: persist pending removal failed: {e}");
        }
        Some(item)
    }

    pub async fn update_pending_text(
        &self,
        id: &str,
        pending_id: &str,
        text: &str,
    ) -> Option<PendingInput> {
        if !self.cache.lock().await.contains_key(id) && self.load(id).await.is_none() {
            return None;
        }
        let mut cache = self.cache.lock().await;
        let s = cache.get_mut(id)?;
        let item = s.pending.iter_mut().find(|p| p.id == pending_id)?;
        item.text = text.to_string();
        let updated = item.clone();
        s.meta.updated_at = crate::event::now_ms();
        // 持锁落盘（返回 Option 无法上抛，失败仍记日志）。
        let snapshot = s.clone();
        if let Err(e) = self.persist(&snapshot).await {
            tracing::error!("session {id}: persist pending edit failed: {e}");
        }
        Some(updated)
    }

    /// Turn any steering that was never injected into next-turn queue work.
    /// This is the fallback ZCode calls when a text-only turn ends before the
    /// next step boundary: the guidance must not be dropped.
    pub async fn promote_steer_to_queue(&self, id: &str) -> Vec<PendingInput> {
        if !self.cache.lock().await.contains_key(id) && self.load(id).await.is_none() {
            return Vec::new();
        }
        let mut cache = self.cache.lock().await;
        let Some(s) = cache.get_mut(id) else {
            return Vec::new();
        };
        let mut changed = Vec::new();
        for p in &mut s.pending {
            if p.kind == "steer" {
                p.kind = "queue".into();
                changed.push(p.clone());
            }
        }
        if changed.is_empty() {
            return changed;
        }
        s.meta.updated_at = crate::event::now_ms();
        // 持锁落盘（返回 Vec 无法上抛，失败仍记日志）。
        let snapshot = s.clone();
        if let Err(e) = self.persist(&snapshot).await {
            tracing::error!("session {id}: persist steer fallback failed: {e}");
        }
        changed
    }

    /// 整份写入一个会话（导入路径用）。
    ///
    /// 与 `create()` 的区别：ID、标题、消息、用量全部由调用方给全——导入要
    /// 保留原始消息与统计，而不是造一个空壳。调用方负责先解决 ID 冲突；
    /// 这里再做最后一道防线（空 ID、已存在的 ID 一律拒绝），绝不覆盖本地会话。
    pub async fn insert_full(&self, s: Session) -> Result<(), HarnessError> {
        let id = s.meta.id.trim().to_string();
        if id.is_empty() {
            return Err(HarnessError::Other("session id is empty".into()));
        }
        // 导入的 id 是外部输入：过不了路径校验（含分隔符/点等）一律拒绝，
        // 否则 persist 会把整份会话写到 store 根之外。
        if !is_valid_session_id(&id) {
            return Err(HarnessError::Other(format!(
                "session id {id:?} contains characters that are not allowed"
            )));
        }
        if self.path_for(&id).exists() {
            return Err(HarnessError::Other(format!(
                "session {id} already exists; refusing to overwrite"
            )));
        }
        let mut s = s;
        if s.meta.created_at == 0 {
            s.meta.created_at = crate::event::now_ms();
        }
        s.meta.updated_at = crate::event::now_ms();
        self.persist(&s).await?;
        self.cache.lock().await.insert(id, s);
        Ok(())
    }

    pub async fn update_meta(&self, id: &str, f: impl FnOnce(&mut SessionSummary)) -> Result<(), HarnessError> {
        // The cache is populated lazily (load on demand); a session that has
        // never been opened since process start must still be patchable, so
        // pull it in from disk first.
        if !self.cache.lock().await.contains_key(id) {
            self.load(id)
                .await
                .ok_or_else(|| HarnessError::NotFound(format!("session {id}")))?;
        }
        let mut cache = self.cache.lock().await;
        let s = cache
            .get_mut(id)
            .ok_or_else(|| HarnessError::NotFound(format!("session {id}")))?;
        f(&mut s.meta);
        s.meta.updated_at = crate::event::now_ms();
        // 持锁落盘，失败上抛：meta 改动（归档/置顶/未读点等）静默丢失同样
        // 会让重载后的会话回退到旧状态。
        let snapshot = s.clone();
        self.persist(&snapshot).await?;
        Ok(())
    }

    pub async fn add_usage(&self, id: &str, u: &crate::token::Usage, turns: u64, steps: u64, wall_ms: u64, ctx_used: u64) {
        // cache 未命中先回源磁盘（照 update_meta 的兜底模式）。
        if !self.cache.lock().await.contains_key(id) {
            self.load(id).await;
        }
        let mut cache = self.cache.lock().await;
        if let Some(s) = cache.get_mut(id) {
            s.usage.input_tokens += u.input_tokens;
            s.usage.output_tokens += u.output_tokens;
            s.usage.cache_read += u.cache_read_input_tokens;
            s.usage.cache_write += u.cache_creation_input_tokens;
            s.usage.turns += turns;
            s.usage.steps += steps;
            s.usage.wall_ms += wall_ms;
            // 只覆盖不累加：它表示「最近一次请求」的窗口占用。
            if ctx_used > 0 {
                s.usage.ctx_used = ctx_used;
            }
            // 持锁落盘（返回 () 无法上抛，失败仍记日志；内存侧已累计，
            // 下一次成功写会把补上后的总量一并落盘）。
            let snapshot = s.clone();
            if let Err(e) = self.persist(&snapshot).await {
                tracing::error!("session {id}: persist failed: {e}");
            }
        }
    }

    pub async fn delete(&self, id: &str) -> Result<(), HarnessError> {
        self.cache.lock().await.remove(id);
        // 非法 id 不可能对应本 store 里的文件：按「删除不存在的会话」处理
        // 返回 Ok（与现有语义一致），同时杜绝把路径拼到 store 根之外误删。
        let Some(p) = self.checked_path_for(id) else {
            return Ok(());
        };
        if p.exists() {
            std::fs::remove_file(p)?;
        }
        Ok(())
    }

    pub async fn messages(&self, id: &str) -> Vec<StoredMessage> {
        self.load(id).await.map(|s| s.messages).unwrap_or_default()
    }

    /// Replace the text of the message at `idx`, or of the whole group
    /// `[idx, to)` when `to` is given.
    ///
    /// A rendered assistant bubble spans several stored messages (assistant
    /// steps interleaved with merged tool-result messages), and its visible
    /// text is the concatenation of every `Text` part in the group. Editing it
    /// must therefore fold all of that text into one blob — while preserving
    /// thinking / tool_use / tool_result parts, or the provider sees a dangling
    /// tool_result and rejects the replay.
    pub async fn edit_message_text(
        &self,
        id: &str,
        idx: usize,
        to: Option<usize>,
        text: &str,
    ) -> Result<(), HarnessError> {
        // cache 未命中先回源磁盘（照 update_meta 的兜底模式）。
        if !self.cache.lock().await.contains_key(id) {
            self.load(id)
                .await
                .ok_or_else(|| HarnessError::NotFound(format!("session {id}")))?;
        }
        let mut cache = self.cache.lock().await;
        let s = cache
            .get_mut(id)
            .ok_or_else(|| HarnessError::NotFound(format!("session {id}")))?;
        if idx >= s.messages.len() {
            return Err(HarnessError::NotFound(format!("message {idx}")));
        }
        let end = to.unwrap_or(idx + 1).clamp(idx + 1, s.messages.len());
        // Strip every Text part in the range, remember where the new text goes:
        // the last assistant message (fall back to the head of the range).
        let mut target = idx;
        for i in idx..end {
            let m = &mut s.messages[i];
            let key = if m.role == "user" { "User" } else { "Assistant" };
            let arr = m
                .parts
                .get(key)
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let kept = keep_non_text(arr);
            let mut obj = serde_json::Map::new();
            obj.insert(key.to_string(), serde_json::Value::Array(kept));
            m.parts = serde_json::Value::Object(obj);
            if m.role == "assistant" {
                target = i;
            }
        }
        let m = &mut s.messages[target];
        let key = if m.role == "user" { "User" } else { "Assistant" };
        if let Some(arr) = m.parts.get_mut(key).and_then(|v| v.as_array_mut()) {
            arr.push(serde_json::json!({ "Text": text }));
        } else {
            let mut obj = serde_json::Map::new();
            obj.insert(
                key.to_string(),
                serde_json::Value::Array(vec![serde_json::json!({ "Text": text })]),
            );
            m.parts = serde_json::Value::Object(obj);
        }
        // `displays` stays: the tool cards keep their rendered content.
        s.meta.updated_at = crate::event::now_ms();
        // 持锁落盘，失败上抛（编辑是用户显式动作，静默丢失等于丢字）。
        let snapshot = s.clone();
        self.persist(&snapshot).await?;
        Ok(())
    }

    /// 重建一段 Agent 气泡的工具调用结构：按 `step` 分组写回 ToolUse，
    /// 紧跟一条 ToolResult 消息；Thinking / 图片原样保留，正文折叠到最后一条
    /// assistant。没出现在 `tools` 里的调用即被删除，`step` 上也允许新增。
    pub async fn edit_message_parts(
        &self,
        id: &str,
        idx: usize,
        to: Option<usize>,
        text: Option<&str>,
        tools: &[ToolEdit],
    ) -> Result<(), HarnessError> {
        if !self.cache.lock().await.contains_key(id) {
            self.load(id)
                .await
                .ok_or_else(|| HarnessError::NotFound(format!("session {id}")))?;
        }
        let mut cache = self.cache.lock().await;
        let s = cache
            .get_mut(id)
            .ok_or_else(|| HarnessError::NotFound(format!("session {id}")))?;
        if idx >= s.messages.len() {
            return Err(HarnessError::NotFound(format!("message {idx}")));
        }
        let end = to.unwrap_or(idx + 1).clamp(idx + 1, s.messages.len());
        let last_asst = (idx..end)
            .filter(|i| s.messages[*i].role == "assistant")
            .last()
            .unwrap_or(idx);

        let mut groups: HashMap<usize, Vec<&ToolEdit>> = HashMap::new();
        for t in tools {
            let step = t.step.unwrap_or(last_asst).clamp(idx, end - 1);
            groups.entry(step).or_default().push(t);
        }

        let mut rebuilt: Vec<StoredMessage> = Vec::new();
        let mut pending_images: Vec<Value> = Vec::new();
        for old in idx..end {
            let m = &s.messages[old];
            let key = if m.role == "user" { "User" } else { "Assistant" };
            let parts = m
                .parts
                .get(key)
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let created_at = m.created_at;
            if m.role == "assistant" {
                let kept: Vec<Value> = parts
                    .iter()
                    .filter(|p| p.get("Thinking").is_some())
                    .cloned()
                    .collect();
                let mut arr = kept;
                if let Some(list) = groups.get(&old) {
                    for t in list {
                        arr.push(serde_json::json!({"ToolUse": {
                            "id": t.id,
                            "name": t.name.clone().unwrap_or_else(|| "tool".to_string()),
                            "args": t.args.clone().unwrap_or_else(|| serde_json::json!({})),
                        }}));
                    }
                }
                let mut obj = serde_json::Map::new();
                obj.insert("Assistant".to_string(), Value::Array(arr));
                rebuilt.push(StoredMessage {
                    role: "assistant".into(),
                    parts: Value::Object(obj),
                    displays: Vec::new(),
                    // 旧数据迁移合并出的消息：原始 turn 已不可考
                    turn: String::new(),
                    hidden: false,
                    created_at,
                });
                if let Some(list) = groups.get(&old) {
                    let mut rp: Vec<Value> = list
                        .iter()
                        .map(|t| {
                            serde_json::json!({"ToolResult": {
                                "id": t.id,
                                "content": t.result.clone().unwrap_or_default(),
                                "is_error": t.is_error.unwrap_or(false),
                            }})
                        })
                        .collect();
                    if !pending_images.is_empty() {
                        rp.append(&mut pending_images);
                    }
                    let d: Vec<Value> = list
                        .iter()
                        .map(|t| {
                            serde_json::json!({
                                "kind": "plain",
                                "text": t.result.clone().unwrap_or_default(),
                            })
                        })
                        .collect();
                    let mut obj = serde_json::Map::new();
                    obj.insert("User".to_string(), Value::Array(rp));
                    rebuilt.push(StoredMessage {
                        role: "user".into(),
                        parts: Value::Object(obj),
                        displays: d,
                        turn: String::new(),
                        hidden: false,
                        created_at,
                    });
                }
            } else {
                let imgs: Vec<Value> = parts
                    .iter()
                    .filter(|p| p.get("Image").is_some())
                    .cloned()
                    .collect();
                if imgs.is_empty() {
                    continue;
                }
                // 图片并进最近一条工具结果消息，避免留下一条空 user 消息。
                if let Some(last) = rebuilt.last_mut() {
                    if last.role == "user" {
                        if let Some(a) = last.parts.get_mut("User").and_then(|v| v.as_array_mut()) {
                            a.extend(imgs);
                            continue;
                        }
                    }
                }
                pending_images = imgs;
            }
        }

        if let Some(txt) = text {
            if let Some(last) = rebuilt.iter_mut().rev().find(|m| m.role == "assistant") {
                let txt_json = serde_json::json!({ "Text": txt });
                if let Some(arr) = last
                    .parts
                    .get_mut("Assistant")
                    .and_then(|v| v.as_array_mut())
                {
                    arr.push(txt_json);
                } else {
                    let mut obj = serde_json::Map::new();
                    obj.insert("Assistant".to_string(), Value::Array(vec![txt_json]));
                    last.parts = Value::Object(obj);
                }
            }
        }
        if !pending_images.is_empty() {
            let mut obj = serde_json::Map::new();
            obj.insert("User".to_string(), Value::Array(pending_images));
            rebuilt.push(StoredMessage {
                role: "user".into(),
                parts: Value::Object(obj),
                displays: Vec::new(),
                turn: String::new(),
                hidden: false,
                created_at: crate::event::now_ms(),
            });
        }

        s.messages.splice(idx..end, rebuilt);
        s.meta.updated_at = crate::event::now_ms();
        // 持锁落盘，失败上抛（同 edit_message_text）。
        let snapshot = s.clone();
        self.persist(&snapshot).await?;
        Ok(())
    }

    /// Drop the message at `idx`.
    pub async fn delete_message(&self, id: &str, idx: usize) -> Result<(), HarnessError> {
        // cache 未命中先回源磁盘（照 update_meta 的兜底模式）。
        if !self.cache.lock().await.contains_key(id) {
            self.load(id)
                .await
                .ok_or_else(|| HarnessError::NotFound(format!("session {id}")))?;
        }
        let mut cache = self.cache.lock().await;
        let s = cache
            .get_mut(id)
            .ok_or_else(|| HarnessError::NotFound(format!("session {id}")))?;
        if idx >= s.messages.len() {
            return Err(HarnessError::NotFound(format!("message {idx}")));
        }
        drop(cache);
        self.delete_range(id, idx, Some(idx + 1)).await
    }

    /// Drop every message in `[from, to)`. `to = None` means "to the end" —
    /// the truncate-from-here case used when re-running from a point. An empty
    /// or out-of-bounds range is a no-op.
    ///
    /// from/end 先做组边界对齐（见 `align_split`/`align_split_end`）：
    /// 裁剪后的历史永远不出现 dangling tool_use / orphan tool_result。
    pub async fn delete_range(
        &self,
        id: &str,
        from: usize,
        to: Option<usize>,
    ) -> Result<(), HarnessError> {
        // cache 未命中先回源磁盘（照 update_meta 的兜底模式）。
        if !self.cache.lock().await.contains_key(id) {
            self.load(id)
                .await
                .ok_or_else(|| HarnessError::NotFound(format!("session {id}")))?;
        }
        let mut cache = self.cache.lock().await;
        let s = cache
            .get_mut(id)
            .ok_or_else(|| HarnessError::NotFound(format!("session {id}")))?;
        let end = to.unwrap_or(s.messages.len()).min(s.messages.len());
        if from < end {
            // 组对齐：from 回退到组头、end 前扩到组尾，绝不在
            // tool_use/tool_result 组中间落刀。
            let from = align_split(&s.messages, from);
            let end = align_split_end(&s.messages, end);
            s.messages.drain(from..end);
            s.meta.updated_at = crate::event::now_ms();
            // 持锁落盘，失败上抛。
            let snapshot = s.clone();
            self.persist(&snapshot).await?;
        }
        Ok(())
    }

    /// Drop every message from `from` onward — used when re-running from a point.
    pub async fn truncate_from(&self, id: &str, from: usize) -> Result<(), HarnessError> {
        self.delete_range(id, from, None).await
    }

    /// Replace the first `keep_from` messages with one summary message — the
    /// persistence half of context compaction. Returns how many were folded.
    ///
    /// `keep_from` 由调用方按内存里折叠后的中性格式消息算出，而
    /// `neutral_messages` 会跳过解析失败的消息，两边下标本就可能错位；
    /// 落刀前先做组边界对齐（见 `align_split`），保证 tool_use/tool_result
    /// 组不从中切开。返回对齐后真正折进摘要的条数。
    pub async fn compact_store(
        &self,
        id: &str,
        keep_from: usize,
        summary: &str,
    ) -> Result<usize, HarnessError> {
        // cache 未命中先回源磁盘（照 update_meta 的兜底模式）。
        if !self.cache.lock().await.contains_key(id) {
            self.load(id)
                .await
                .ok_or_else(|| HarnessError::NotFound(format!("session {id}")))?;
        }
        let mut cache = self.cache.lock().await;
        let s = cache
            .get_mut(id)
            .ok_or_else(|| HarnessError::NotFound(format!("session {id}")))?;
        let keep_from = keep_from.min(s.messages.len());
        // 组对齐：切分点落在 tool_result 组中间/组尾，或上一条是含 ToolUse
        // 的 assistant 时，回退到组头——整组要么全折进摘要、要么全保留。
        let keep_from = align_split(&s.messages, keep_from);
        if keep_from == 0 {
            return Ok(0);
        }
        s.messages.drain(0..keep_from);
        s.messages.insert(
            0,
            StoredMessage {
                role: "user".into(),
                parts: serde_json::json!({"User": [{"Text": summary}]}),
                displays: Vec::new(),
                // 压缩摘要不属于任何执行回合
                turn: String::new(),
                hidden: false,
                created_at: crate::event::now_ms(),
            },
        );
        s.meta.updated_at = crate::event::now_ms();
        // 持锁落盘，失败上抛。
        let snapshot = s.clone();
        self.persist(&snapshot).await?;
        Ok(keep_from)
    }

    pub async fn get_goal(&self, id: &str) -> Option<GoalState> {
        self.load(id).await.and_then(|s| s.goal)
    }

    /// Persist a goal state (`None` clears the goal).
    pub async fn set_goal(&self, id: &str, goal: Option<GoalState>) -> Result<(), HarnessError> {
        // cache 未命中先回源磁盘（照 update_meta 的兜底模式）。
        if !self.cache.lock().await.contains_key(id) {
            self.load(id)
                .await
                .ok_or_else(|| HarnessError::NotFound(format!("session {id}")))?;
        }
        let mut cache = self.cache.lock().await;
        let s = cache
            .get_mut(id)
            .ok_or_else(|| HarnessError::NotFound(format!("session {id}")))?;
        s.goal = goal;
        s.meta.updated_at = crate::event::now_ms();
        // 持锁落盘，失败上抛：goal 状态机（轮次预算/停滞计数）回退会导致
        // 自动续跑判断失真。
        let snapshot = s.clone();
        self.persist(&snapshot).await?;
        Ok(())
    }

    /// Read the Agent Teams board of the tree rooted at this session.
    pub async fn get_team(&self, id: &str) -> Option<crate::team::TeamState> {
        self.load(id).await.and_then(|s| s.team)
    }

    /// Per-root board lock (T-01)：调用方在整段「读看板 → 改 → 落盘」临界区
    /// 内持有返回的 guard，并发的 create/update 不会各读半板再互相覆盖。
    pub async fn team_lock(&self, id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.team_locks.lock().await;
        locks
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Persist the Agent Teams board of the tree rooted at this session.
    ///
    /// `None` clears the board. Mirrors `set_goal`'s mutate → bump → persist
    /// shape, minus the error swallow: a team board write that silently failed
    /// would leave two agents disagreeing about the state of the board.
    pub async fn set_team(
        &self,
        id: &str,
        team: Option<crate::team::TeamState>,
    ) -> Result<(), HarnessError> {
        // cache 未命中先回源磁盘（照 update_meta 的兜底模式）。
        if !self.cache.lock().await.contains_key(id) {
            self.load(id)
                .await
                .ok_or_else(|| HarnessError::NotFound(format!("session {id}")))?;
        }
        let mut cache = self.cache.lock().await;
        let s = cache
            .get_mut(id)
            .ok_or_else(|| HarnessError::NotFound(format!("session {id}")))?;
        s.team = team;
        s.meta.updated_at = crate::event::now_ms();
        // 持锁落盘（本就上抛，只补上与其它写路径一致的持锁纪律）。
        let snapshot = s.clone();
        self.persist(&snapshot).await?;
        Ok(())
    }

    /// Search every session's message text for `query`.
    ///
    /// Returns one hit per matching message: the session id/title plus a
    /// short excerpt around the first match. Tool payloads are not searched —
    /// only what the user typed and what the model said.
    pub async fn search_messages(&self, query: &str, limit: usize) -> Vec<MessageHit> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<MessageHit> = Vec::new();
        for s in self.list("").await {
            if hits.len() >= limit {
                break;
            }
            // The per-session text index is memoised by file stamp, so a
            // repeated search does not re-parse every chat on disk.
            for (idx, role, text) in self.session_text_index(&s.id).await {
                if hits.len() >= limit {
                    break;
                }
                let Some(ex) = excerpt(&text, &q) else {
                    continue;
                };
                hits.push(MessageHit {
                    session_id: s.id.clone(),
                    session_title: s.title.clone(),
                    role,
                    index: idx,
                    excerpt: ex,
                });
            }
        }
        hits
    }

    /// Per-session searchable text: (message index, role, text).
    ///
    /// Cached by the session file's (mtime, size), so repeated searches only
    /// pay for the files that actually changed. The cache is bounded by total
    /// text size (see `TEXT_CACHE_MAX_BYTES`): a long chat's text is worth
    /// megabytes of RSS, and an unbounded cache would grow with the archive.
    /// When the budget is exceeded the least-recently-used entries are
    /// dropped, so the hot sessions stay hot and the cold ones are re-read.
    async fn session_text_index(&self, id: &str) -> Vec<(usize, String, String)> {
        // id 与 load 同一道校验闸：过不了的 id（异常文件名的 stem 等）
        // 不做 stat、也不进索引。
        let stamp = self
            .checked_path_for(id)
            .and_then(|p| p.metadata().ok())
            .and_then(|m| {
                let t = m.modified().ok()?;
                let d = t.duration_since(std::time::UNIX_EPOCH).ok()?;
                Some(((d.as_secs(), d.subsec_nanos()), m.len()))
            });
        if let Some(st) = stamp {
            let mut idx = self.text_index.lock().await;
            if let Some(hit) = idx.get_mut(id) {
                if hit.stamp == st {
                    hit.touched = std::time::Instant::now();
                    return hit.items.clone();
                }
            }
        }
        let mut items = Vec::new();
        for (idx, m) in self.messages(id).await.iter().enumerate() {
            // hidden 消息不进搜索索引：用户看不见的内容也不该被搜出来。
            if m.hidden {
                continue;
            }
            if let Some(text) = message_text(&m.parts) {
                items.push((idx, m.role.clone(), text));
            }
        }
        if let Some(st) = stamp {
            let bytes: usize = items.iter().map(|(_, _, t)| t.len()).sum();
            let mut idx = self.text_index.lock().await;
            idx.insert(
                id.to_string(),
                TextEntry {
                    stamp: st,
                    bytes,
                    touched: std::time::Instant::now(),
                    items: items.clone(),
                },
            );
            // Evict least-recently-used entries until the budget is met.
            evict_text_cache(&mut idx, TEXT_CACHE_MAX_BYTES);
        }
        items
    }

    /// Rebuild neutral messages from storage.
    ///
    /// 解析失败的消息不再静默丢弃：记 warn 后跳过。注意调用方（如手工
    /// compact）据此计算的切分下标可能与 store 的原始消息下标错位。
    pub async fn neutral_messages(&self, id: &str) -> Vec<crate::provider::Msg> {
        let mut out = Vec::new();
        for m in self.messages(id).await {
            match serde_json::from_value::<crate::provider::Msg>(m.parts) {
                Ok(msg) => out.push(msg),
                Err(e) => {
                    tracing::warn!("session {id}: dropping unparseable message (role {}): {e}", m.role);
                }
            }
        }
        out
    }
}

/// Parse just the metadata header of a session file.
///
/// The file is written as `#[serde(flatten)]`-ed summary fields first, then
/// `messages`, then `usage`. We read only the prefix up to the `messages`
/// key and deserialize that as a `SessionSummary` — a long chat's message
/// array (megabytes) never enters the parser.
///
/// `"messages"` 必须处于对象键位置才认：元数据字符串值里出现字面量
/// `"messages"`（比如标题/工作区路径提到它）不能切——判定办法是向前找
/// 最近一个非空白字符，只有 `{`（首键）或 `,`（后续键）才是键位置的合法
/// 前导；否则跳过这处匹配继续往后找真正的键。
fn parse_summary_head(text: &str) -> Option<SessionSummary> {
    const NEEDLE: &str = "\"messages\"";
    let bytes = text.as_bytes();
    let mut from = 0;
    let cut = loop {
        let pos = text[from..].find(NEEDLE)? + from;
        let prev = bytes[..pos]
            .iter()
            .rev()
            .find(|b| !b.is_ascii_whitespace())?;
        if *prev == b'{' || *prev == b',' {
            break pos;
        }
        // 字符串值里的字面量：跳过它继续找下一处。
        from = pos + NEEDLE.len();
    };
    // Walk back over the comma separating it from the previous field, then
    // close the object we cut in half.
    let head = text[..cut].trim_end().trim_end_matches(',');
    let head = format!("{head}}}");
    serde_json::from_str::<SessionSummary>(&head).ok()
}

/// One message that matched a search.
#[derive(Debug, Clone, Serialize)]
pub struct MessageHit {
    pub session_id: String,
    pub session_title: String,
    pub role: String,
    /// Index of the message inside the session store.
    pub index: usize,
    pub excerpt: String,
}

/// Flatten a neutral message's text parts into one searchable string.
pub fn message_text(parts: &Value) -> Option<String> {
    let arr = parts.get("User").or_else(|| parts.get("Assistant"))?.as_array()?;
    let mut out = String::new();
    for p in arr {
        if let Some(s) = p.get("Text").and_then(|x| x.as_str()) {
            out.push_str(s);
            out.push('\n');
        }
    }
    let t = out.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// A window of `text` around the match, with the query wrapped in markers the
/// frontend turns into highlights.
///
/// Matching is done on lowercased **characters** so the index is a character
/// index — CJK text is never cut mid-character, and the byte length of the
/// lowercased text is irrelevant.
pub(crate) fn excerpt(text: &str, query_lower: &str) -> Option<String> {
    const BEFORE: usize = 40;
    const AFTER: usize = 90;
    let chars: Vec<char> = text.chars().collect();
    let lower: Vec<char> = text.to_lowercase().chars().collect();
    let q: Vec<char> = query_lower.chars().collect();
    if q.is_empty() || q.len() > lower.len() {
        return None;
    }
    let hit_start = (0..=lower.len() - q.len())
        .find(|&i| lower[i..i + q.len()] == q[..])?;
    let hit_end = hit_start + q.len();
    let start = hit_start.saturating_sub(BEFORE);
    let end = (hit_end + AFTER).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(chars[start..hit_start].iter());
    out.push('\u{1}');
    out.extend(chars[hit_start..hit_end].iter());
    out.push('\u{1}');
    out.extend(chars[hit_end..end].iter());
    if end < chars.len() {
        out.push('…');
    }
    Some(out)
}

/// Keep every non-Text part of a message part array, in order.
/// `Text` parts (and bare strings, which the neutral format also allows) are
/// the editable surface; thinking / tool_use / tool_result must survive an edit
/// or the provider sees a dangling tool_result and rejects the replay.
fn keep_non_text(arr: Vec<Value>) -> Vec<Value> {
    arr.into_iter()
        .filter(|p| !(p.is_string() || p.get("Text").is_some()))
        .collect()
}

/// 会话 id 合法性：`^[A-Za-z0-9_-]{1,80}$`。
///
/// 不含路径分隔符与点——把 `path_for` 的拼接面锁死在 store 根目录内的
/// 一个普通文件名上，`../`、`..\`、URL 编码等穿越写法全都过不了这道闸。
/// 全 ASCII 判定，多字节字符（中文 id 等）一律拒绝。
fn is_valid_session_id(id: &str) -> bool {
    (1..=80).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// assistant 消息里是否带着未收尾的 tool_use（中性格式：ToolUse 部件）。
fn has_tool_use(m: &StoredMessage) -> bool {
    m.role == "assistant"
        && m.parts
            .get("Assistant")
            .and_then(|v| v.as_array())
            .map_or(false, |arr| arr.iter().any(|p| p.get("ToolUse").is_some()))
}

/// user 消息里是否带着 tool_result（中性格式：ToolResult 部件）。
fn has_tool_result(m: &StoredMessage) -> bool {
    m.role == "user"
        && m.parts
            .get("User")
            .and_then(|v| v.as_array())
            .map_or(false, |arr| arr.iter().any(|p| p.get("ToolResult").is_some()))
}

/// 切分点对齐：把拟切分下标向回挪，直到边界不会把
/// 「assistant(tool_use) + 紧随其后的全部 tool_result 用户消息」组切开——
/// 组要么整体落在切分点之后（保留侧），要么整体落在之前（裁掉侧）。
///
/// 与 agent.rs 的 `align_compact_split` 同一语义（那边作用于内存里的
/// provider Msg，这边作用于 store 的 StoredMessage）。两步走：
/// 1. 边界右侧以 tool_result 开头 → 一路回退越过整段结果消息；
/// 2. 上一条是含 ToolUse 的 assistant → 再回退一步把组头并进同一侧。
fn align_split(messages: &[StoredMessage], mut idx: usize) -> usize {
    while idx < messages.len() && idx > 0 && has_tool_result(&messages[idx]) {
        idx -= 1;
    }
    if idx < messages.len() && idx > 0 && has_tool_use(&messages[idx - 1]) {
        idx -= 1;
    }
    idx
}

/// 删除区间终点对齐：终点落在组中间时向后扩到组尾——
/// 区间末尾是含 ToolUse 的 assistant（其结果组紧跟其后）或下一条是
/// tool_result 用户消息（其组头在区间内）时都继续扩，保证裁剪后
/// 不出现 dangling tool_use / orphan tool_result。
fn align_split_end(messages: &[StoredMessage], mut end: usize) -> usize {
    while end < messages.len() {
        if end > 0 && has_tool_use(&messages[end - 1]) {
            end += 1;
        } else if has_tool_result(&messages[end]) {
            end += 1;
        } else {
            break;
        }
    }
    end
}

pub type SharedStore = Arc<SessionStore>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TestHome;

    /// 列出目录下全部 `.json` 文件名（排序），用于快照比对真实 home 是否被写入。
    fn session_file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// PARABLE_HOME 隔离：`SessionStore::new()` 的根必须跟随环境变量，
    /// 新建会话落到临时目录，且不写进真实 `~/.parable/sessions`。
    #[tokio::test]
    async fn new_store_follows_parable_home() {
        let home = TestHome::new("isolated");
        let store = SessionStore::new();
        let expected_root = home.sessions_dir();
        assert_eq!(store.root, expected_root);

        let s = store.create(".", "isolated-stub").await;
        let stub = expected_root.join(format!("{}.json", s.meta.id));
        assert!(stub.exists(), "会话 stub 必须落在 PARABLE_HOME 下：{}", stub.display());

        // 直接取真实用户主目录（绕过 PARABLE_HOME），确认没有在那边落 stub。
        if let Some(real) = dirs::home_dir() {
            let real_sessions = real.join(".parable").join("sessions");
            assert!(
                !real_sessions.join(format!("{}.json", s.meta.id)).exists(),
                "会话不得绕过 PARABLE_HOME 写进真实 home"
            );
        }
    }

    /// 回归断言：在隔离 home 下建会话，真实 `~/.parable/sessions` 的文件集合必须不变。
    /// 历史上有测试未注入隔离根，每跑一次套件就往用户真实目录写空 stub。
    #[tokio::test]
    async fn isolated_create_does_not_touch_real_home() {
        let real = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".parable")
            .join("sessions");
        let before = session_file_names(&real);

        let home = TestHome::new("real-home-guard");
        let store = SessionStore::new();
        let created = store.create(".", "real-home-guard").await;
        // 隔离会话必须落在临时目录，而不是真实 home。
        assert!(
            home.sessions_dir()
                .join(format!("{}.json", created.meta.id))
                .exists(),
            "隔离会话必须落在 PARABLE_HOME 下"
        );
        drop(home);

        let after = session_file_names(&real);
        assert_eq!(
            before, after,
            "在隔离 home 建会话不得在真实 ~/.parable/sessions 新增文件"
        );
    }

    /// 导入必须保留消息与统计，且拒绝覆盖同 ID 的本地会话。
    #[tokio::test]
    async fn insert_full_keeps_messages_and_refuses_overwrite() {
        let dir = std::env::temp_dir().join("parable-test-insert-full");
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::with_root(dir.clone());

        let msg = |t: &str| StoredMessage {
            role: "user".into(),
            parts: serde_json::json!({"User": [{"Text": t}]}),
            displays: Vec::new(),
            turn: String::new(),
            hidden: false,
            created_at: 1,
        };
        let s = Session {
            meta: SessionSummary { id: "s_import1".into(), title: "old chat".into(), ..Default::default() },
            messages: vec![msg("hello"), msg("world")],
            pending: Vec::new(),
            usage: UsageTotals::default(),
            goal: None,
            team: None,
        };
        store.insert_full(s.clone()).await.expect("first insert ok");
        let back = store.load("s_import1").await.expect("reload");
        assert_eq!(back.messages.len(), 2, "messages must survive the import");
        assert_eq!(back.meta.title, "old chat");

        // 第二次同 ID：必须拒绝，不能把用户已有会话盖掉。
        let err = store.insert_full(s).await;
        assert!(err.is_err(), "same id must be refused");
        // 空 ID 也拒绝。
        let mut bad = Session {
            meta: SessionSummary::default(),
            messages: Vec::new(),
            pending: Vec::new(),
            usage: UsageTotals::default(),
            goal: None,
            team: None,
        };
        bad.meta.id = String::new();
        assert!(store.insert_full(bad).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 思考时长必须随消息落盘：重启/重放后界面靠它显示「思考了 N 秒」。
    /// 旧数据没有这个字段，反序列化必须当 0（界面据此隐藏时长），不能报错。
    #[test]
    fn thinking_duration_survives_a_store_roundtrip() {
        let parts = serde_json::json!({"Assistant": [
            {"Thinking": {"text": "hmm", "signature": "sig", "ms": 4200}},
            {"Text": "answer"}
        ]});
        let json = serde_json::to_string(&parts).unwrap();
        let back: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(back["Assistant"][0]["Thinking"]["ms"], 4200);

        // 旧格式（无 ms 字段）按 0 处理。
        let legacy: crate::provider::Msg =
            serde_json::from_str(r#"{"Assistant":[{"Thinking":{"text":"old","signature":""}}]}"#)
                .expect("legacy thinking must deserialize");
        match legacy {
            crate::provider::Msg::Assistant(ps) => match &ps[0] {
                crate::provider::AsstPart::Thinking { ms, .. } => assert_eq!(*ms, 0),
                _ => panic!("expected a thinking part"),
            },
            _ => panic!("expected an assistant message"),
        }
    }

    #[test]
    fn keep_non_text_preserves_tool_parts() {
        let arr = vec![
            serde_json::json!({"Thinking": {"text": "hmm", "signature": "s"}}),
            serde_json::json!({"Text": "let me check"}),
            serde_json::json!({"ToolUse": {"id": "c1", "name": "bash", "args": {}}}),
            serde_json::json!({"Text": "done"}),
            serde_json::json!("bare string part"),
        ];
        let kept = keep_non_text(arr);
        assert_eq!(kept.len(), 2);
        assert!(kept[0].get("Thinking").is_some());
        assert!(kept[1].get("ToolUse").is_some());
    }

    #[test]
    fn message_text_flattens_parts() {
        let v = serde_json::json!({"User": [{"Text": "你好"}, {"Text": "世界"}]});
        assert_eq!(message_text(&v).as_deref(), Some("你好\n世界"));
        let empty = serde_json::json!({"User": [{"Text": "   "}]});
        assert!(message_text(&empty).is_none());
        assert!(message_text(&serde_json::json!({})).is_none());
    }

    #[test]
    fn excerpt_marks_the_match() {
        let text = "这是第一句话，这是第二句话里面有 target 这个词，还有更多内容在这里继续延伸下去";
        let out = excerpt(text, "target").expect("must find the query");
        assert!(out.contains('\u{1}'), "match must be delimited");
        let parts: Vec<&str> = out.split('\u{1}').collect();
        assert_eq!(parts[1], "target");
    }

    #[test]
    fn excerpt_handles_cjk_boundaries() {
        // Matching on characters must not cut a CJK char in half.
        let text = "搜索会话内的消息内容";
        let out = excerpt(text, "会话").expect("must find the query");
        let parts: Vec<&str> = out.split('\u{1}').collect();
        assert_eq!(parts[1], "会话");
    }

    #[test]
    fn excerpt_misses_when_absent() {
        assert!(excerpt("完全无关的文本", "zzz").is_none());
        assert!(excerpt("短", "太长了").is_none());
    }

    #[test]
    fn text_cache_evicts_least_recently_used() {
        let mut idx: HashMap<String, TextEntry> = HashMap::new();
        let mk = |name: &str, bytes: usize, touched: std::time::Instant| {
            (
                name.to_string(),
                TextEntry {
                    stamp: ((0, 0), 0),
                    bytes,
                    touched,
                    items: Vec::new(),
                },
            )
        };
        let now = std::time::Instant::now();
        // old(100) is the least recently used, mid(100), new(100).
        let (k, v) = mk("old", 100, now);
        idx.insert(k, v);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let (k, v) = mk("mid", 100, std::time::Instant::now());
        idx.insert(k, v);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let (k, v) = mk("new", 100, std::time::Instant::now());
        idx.insert(k, v);

        // Budget of 250 must drop exactly one entry — the LRU one.
        evict_text_cache(&mut idx, 250);
        assert!(!idx.contains_key("old"), "least recently used must go first");
        assert!(idx.contains_key("mid"));
        assert!(idx.contains_key("new"));

        // A budget smaller than a single entry keeps at least one entry.
        evict_text_cache(&mut idx, 1);
        assert_eq!(idx.len(), 1, "the newest entry must survive");
    }

    #[test]
    fn summary_head_parses_without_messages() {        // The metadata fields come before `messages` in the file; the head
        // parser must read them without touching the message array.
        let full = r#"{
  "id": "s_abc",
  "title": "标题",
  "workspace": "/tmp",
  "created_at": 1,
  "updated_at": 2,
  "pinned": true,
  "archived": false,
  "group": "",
  "model": "",
  "permission_mode": "ask",
  "thinking_level": 2,
  "messages": [ { "role": "user", "parts": { "User": [ { "Text": "hi" } ] } } ],
  "usage": { "input_tokens": 1 }
}"#;
        let s = parse_summary_head(full).expect("head must parse");
        assert_eq!(s.id, "s_abc");
        assert_eq!(s.title, "标题");
        assert!(s.pinned);
        assert_eq!(s.thinking_level, 2);
        // No `messages` key in the head -> nothing to parse.
        assert!(parse_summary_head("{\"id\":\"x\"}").is_none());
    }

    #[test]
    fn a_session_file_without_a_team_still_loads() {
        // Older session files have no `team` key: the field must default
        // to None rather than failing the whole session load.
        let old = serde_json::json!({
            "id": "s_old",
            "title": "老会话",
            "workspace": "w",
            "created_at": 1,
            "updated_at": 2,
            "messages": [],
        });
        let s: Session = serde_json::from_value(old).expect("old files must load");
        assert!(s.team.is_none());
    }

    #[test]
    fn team_state_round_trips_through_the_session() {
        let mut team = crate::team::TeamState::new();
        team.next_task_number = 2;
        team.tasks.push(crate::team::TeamTask {
            id: "task-1".into(),
            revision: 3,
            subject: "写文档".into(),
            description: "写 README".into(),
            status: crate::team::TaskStatus::InProgress,
            owner_id: Some("s_child".into()),
            owner_name: Some("agent-1".into()),
            blocked_by: vec!["task-0".into()],
            write_scopes: vec!["docs".into()],
            created_by: "agent-1".into(),
        });
        let s = Session {
            meta: SessionSummary {
                id: "s_x".into(),
                title: "t".into(),
                workspace: "w".into(),
                created_at: 0,
                updated_at: 0,
                pinned: false,
                archived: false,
                group: String::new(),
                model: String::new(),
                permission_mode: "ask".into(),
                thinking_level: 2,
                outcome: String::new(),
            dot: false,
            },
            messages: Vec::new(),
            pending: Vec::new(),
            usage: UsageTotals::default(),
            goal: None,
            team: Some(team),
        };
        let json = serde_json::to_string(&s).expect("serialize");
        let back: Session = serde_json::from_str(&json).expect("deserialize");
        let t = back.team.expect("team survives the round trip");
        assert!(t.enabled);
        assert_eq!(t.tasks.len(), 1);
        assert_eq!(t.tasks[0].revision, 3);
        assert_eq!(t.tasks[0].owner_name.as_deref(), Some("agent-1"));
        assert_eq!(t.tasks[0].blocked_by, vec!["task-0".to_string()]);
        assert_eq!(t.tasks[0].write_scopes, vec!["docs".to_string()]);
        assert_eq!(t.tasks[0].status, crate::team::TaskStatus::InProgress);
    }

    #[tokio::test]
    async fn pending_inputs_are_stored_in_fifo_order_and_can_be_edited() {
        let root = std::env::temp_dir().join(format!(
            "parable-pending-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let store = SessionStore::with_root(root.clone());
        let s = store.create("w", "t").await;
        let item = |id: &str, kind: &str, text: &str| PendingInput {
            id: id.into(),
            kind: kind.into(),
            text: text.into(),
            attachments: Vec::new(),
            opts: crate::agent::TurnOptions::default(),
            created_at: 1,
        };
        store.add_pending(&s.meta.id, item("p1", "queue", "first")).await.unwrap();
        store.add_pending(&s.meta.id, item("p2", "queue", "second")).await.unwrap();
        store.add_pending(&s.meta.id, item("p3", "steer", "guide")).await.unwrap();
        assert_eq!(store.pending(&s.meta.id).await.len(), 3);

        let taken = store.take_pending(&s.meta.id, "queue", 1).await;
        assert_eq!(taken[0].id, "p1");
        assert_eq!(store.pending(&s.meta.id).await.len(), 2);
        assert!(store.update_pending_text(&s.meta.id, "p2", "edited").await.is_some());
        let promoted = store.promote_steer_to_queue(&s.meta.id).await;
        assert_eq!(promoted[0].id, "p3");
        assert_eq!(store.pending(&s.meta.id).await[1].kind, "queue");
        assert!(store.remove_pending(&s.meta.id, "p2").await.is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---------- id 路径校验 ----------

    #[test]
    fn valid_session_ids_follow_the_pattern() {
        // 合法：字母/数字/下划线/连字符，1..=80 位。
        for ok in ["s_abc", "A-b_9", "0"] {
            assert!(is_valid_session_id(ok), "{ok} must be valid");
        }
        // 非法：空串、路径分隔符、点、URL 编码、多字节字符、超长。
        let too_long = "x".repeat(81);
        for bad in [
            "",
            "a.b",
            "../../evil",
            "..%2F..%2Fevil",
            "a/b",
            "a\\b",
            "..",
            "会话",
            too_long.as_str(),
        ] {
            assert!(!is_valid_session_id(bad), "{bad} must be rejected");
        }
    }

    /// 穿越防护：load/exists/delete 拿到恶意 id 时不得碰 store 根之外的
    /// 文件——`../evil` 恰好指到根旁的 victim 文件，修复前 delete 会把它删掉。
    #[tokio::test]
    async fn traversal_ids_never_touch_files_outside_the_store() {
        let base = std::env::temp_dir().join(format!(
            "parable-pathcheck-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let store_root = base.join("store");
        let _ = std::fs::create_dir_all(&store_root);
        let victim = base.join("evil.json");
        std::fs::write(&victim, b"{}").expect("plant victim file");

        let store = SessionStore::with_root(store_root);
        for bad in ["../evil", "../../evil", "..%2F..%2Fevil", "a.b", ""] {
            assert!(store.load(bad).await.is_none(), "load({bad}) must be None");
            assert!(!store.exists(bad).await, "exists({bad}) must be false");
            store
                .delete(bad)
                .await
                .unwrap_or_else(|e| panic!("delete({bad}) must stay a no-op: {e}"));
        }
        assert!(victim.exists(), "store 根旁的文件不得被穿越 id 触碰");

        // 合法 id 的完整生命周期不受影响。
        let s = store.create("w", "t").await;
        assert!(store.load(&s.meta.id).await.is_some());
        assert!(store.exists(&s.meta.id).await);
        store.delete(&s.meta.id).await.unwrap();
        assert!(!store.exists(&s.meta.id).await);

        // 导入入口同样拒绝非法 id。
        let mut evil = Session {
            meta: SessionSummary { id: "../evil".into(), ..Default::default() },
            messages: Vec::new(),
            pending: Vec::new(),
            usage: UsageTotals::default(),
            goal: None,
            team: None,
        };
        assert!(store.insert_full(evil.clone()).await.is_err());
        evil.meta.id = "s_import_ok".into();
        assert!(store.insert_full(evil).await.is_ok());
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---------- 裁剪的组对齐 ----------

    /// 测试消息工厂：user 纯文本 / assistant 带 ToolUse / user 带 ToolResult。
    fn user_text(t: &str) -> StoredMessage {
        StoredMessage {
            role: "user".into(),
            parts: serde_json::json!({"User": [{"Text": t}]}),
            displays: Vec::new(),
            turn: String::new(),
            hidden: false,
            created_at: 1,
        }
    }

    fn assistant_tool_use(id: &str) -> StoredMessage {
        StoredMessage {
            role: "assistant".into(),
            parts: serde_json::json!({"Assistant": [
                {"ToolUse": {"id": id, "name": "bash", "args": {}}}
            ]}),
            displays: Vec::new(),
            turn: String::new(),
            hidden: false,
            created_at: 1,
        }
    }

    fn user_tool_result(id: &str) -> StoredMessage {
        StoredMessage {
            role: "user".into(),
            parts: serde_json::json!({"User": [
                {"ToolResult": {"id": id, "content": "ok", "is_error": false}}
            ]}),
            displays: Vec::new(),
            turn: String::new(),
            hidden: false,
            created_at: 1,
        }
    }

    /// 断言历史里没有 dangling tool_use / orphan tool_result：
    /// 含 ToolUse 的 assistant 之后必须紧跟配对的 tool_result 组，
    /// 含 ToolResult 的 user 之前必须有含 ToolUse 的 assistant。
    fn assert_no_dangling(msgs: &[StoredMessage]) {
        for (i, m) in msgs.iter().enumerate() {
            if has_tool_use(m) {
                assert!(
                    i + 1 < msgs.len() && has_tool_result(&msgs[i + 1]),
                    "message {i}: tool_use without its tool_result right after"
                );
            }
            if has_tool_result(m) {
                assert!(
                    i > 0 && has_tool_use(&msgs[i - 1]),
                    "message {i}: orphan tool_result without its tool_use before"
                );
            }
        }
    }

    /// 造一个 store，写入 [user, assistant(tool_use), user(tool_result), user]。
    async fn store_with_tool_group() -> (SessionStore, String, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "parable-align-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let store = SessionStore::with_root(root.clone());
        let id = "s_align_grp".to_string();
        store
            .insert_full(Session {
                meta: SessionSummary { id: id.clone(), ..Default::default() },
                messages: vec![
                    user_text("hi"),
                    assistant_tool_use("c1"),
                    user_tool_result("c1"),
                    user_text("next"),
                ],
                pending: Vec::new(),
                usage: UsageTotals::default(),
                goal: None,
                team: None,
            })
            .await
            .expect("seed messages");
        (store, id, root)
    }

    #[test]
    fn align_split_pulls_boundaries_to_group_heads() {
        let msgs = vec![
            user_text("hi"),
            assistant_tool_use("c1"),
            user_tool_result("c1"),
            user_text("next"),
        ];
        // 落在 tool_result 上（组头在切分点之前）→ 回退到组头 assistant。
        assert_eq!(align_split(&msgs, 2), 1);
        // 组尾之后的干净边界（整组都在切分点之前）→ 不动。
        assert_eq!(align_split(&msgs, 3), 3);
        // 组头本身 → 不动（组整体落在保留侧）。
        assert_eq!(align_split(&msgs, 1), 1);
        // 干净边界（首条 user）→ 不动。
        assert_eq!(align_split(&msgs, 0), 0);
        // 末尾边界（= len）→ 不动。
        assert_eq!(align_split(&msgs, 4), 4);
    }

    #[test]
    fn align_split_end_extends_to_group_tails() {
        let msgs = vec![
            user_text("hi"),
            assistant_tool_use("c1"),
            user_tool_result("c1"),
            user_text("next"),
        ];
        // 终点在含 ToolUse 的 assistant 之后 → 扩到把 tool_result 一并纳入。
        assert_eq!(align_split_end(&msgs, 2), 3);
        // 终点在 tool_result 上（组头在删除区间内）→ 扩到组尾。
        assert_eq!(align_split_end(&msgs, 3), 3);
        // 干净边界 → 不动。
        assert_eq!(align_split_end(&msgs, 1), 1);
        // 末尾边界 → 不动。
        assert_eq!(align_split_end(&msgs, 4), 4);
    }

    /// from=2（tool_result 处）删除：必须回退到组头，整组一起删，
    /// 留下的历史没有孤儿 tool_result。
    #[tokio::test]
    async fn delete_range_aligns_from_out_of_a_tool_group() {
        let (store, id, root) = store_with_tool_group().await;
        store.delete_range(&id, 2, None).await.unwrap();
        let msgs = store.messages(&id).await;
        assert_eq!(msgs.len(), 1, "整组回退后只剩开头的 user 消息");
        assert_eq!(msgs[0].role, "user");
        assert_no_dangling(&msgs);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// end=2（含 ToolUse 的 assistant 之后）删除：必须扩到把 tool_result
    /// 一并删掉，留下的历史没有 dangling tool_use。
    #[tokio::test]
    async fn delete_range_aligns_end_over_a_tool_group() {
        let (store, id, root) = store_with_tool_group().await;
        store.delete_range(&id, 0, Some(2)).await.unwrap();
        let msgs = store.messages(&id).await;
        assert_eq!(msgs.len(), 1, "end 对齐后只剩最后的 user 消息");
        assert_eq!(msgs[0].role, "user");
        assert_no_dangling(&msgs);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 只删 tool_result 那一条（from=2, end=3）：from 回退到组头后
    /// 变成删整组，干净边界上的 user 消息原样保留。
    #[tokio::test]
    async fn delete_range_single_tool_result_still_removes_the_group() {
        let (store, id, root) = store_with_tool_group().await;
        store.delete_range(&id, 2, Some(3)).await.unwrap();
        let msgs = store.messages(&id).await;
        assert_eq!(msgs.len(), 2, "剩下的两条都是干净的 user 消息");
        assert!(msgs.iter().all(|m| !has_tool_use(m) && !has_tool_result(m)));
        assert_no_dangling(&msgs);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 删首条 user（from=0, end=1）是干净边界：tool 组完整保留，不动。
    #[tokio::test]
    async fn delete_range_on_a_clean_boundary_keeps_the_group() {
        let (store, id, root) = store_with_tool_group().await;
        store.delete_range(&id, 0, Some(1)).await.unwrap();
        let msgs = store.messages(&id).await;
        assert_eq!(msgs.len(), 3);
        assert!(has_tool_use(&msgs[0]) && has_tool_result(&msgs[1]));
        assert_no_dangling(&msgs);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// compact_store 的 keep_from 落在组中间时回退到组头：摘要消息在最前，
    /// 其后第一组 tool_use/tool_result 完整保留。
    #[tokio::test]
    async fn compact_store_aligns_keep_from_to_group_head() {
        let (store, id, root) = store_with_tool_group().await;
        let folded = store.compact_store(&id, 2, "摘要").await.unwrap();
        assert_eq!(folded, 1, "对齐后真正折进摘要的是 1 条（组头前的那条 user）");
        let msgs = store.messages(&id).await;
        assert_eq!(msgs.len(), 4, "摘要 + tool 组 + 末尾 user");
        assert_eq!(message_text(&msgs[0].parts).as_deref(), Some("摘要"));
        assert!(has_tool_use(&msgs[1]));
        assert!(has_tool_result(&msgs[2]));
        assert_no_dangling(&msgs);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// keep_from 落在干净边界（组尾之后）时不回退：整组折进摘要。
    #[tokio::test]
    async fn compact_store_keeps_clean_boundaries() {
        let (store, id, root) = store_with_tool_group().await;
        let folded = store.compact_store(&id, 3, "摘要").await.unwrap();
        assert_eq!(folded, 3);
        let msgs = store.messages(&id).await;
        assert_eq!(msgs.len(), 2, "摘要 + 末尾 user");
        assert_no_dangling(&msgs);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---------- parse_summary_head ----------

    /// 元数据字符串值里出现 "messages" 字面量（值恰好等于它、或以它结尾），
    /// 头部解析必须跳过这些假键位置，切在真正的对象键上。
    #[test]
    fn summary_head_ignores_messages_literals_inside_strings() {
        let tricky = r#"{
  "id": "s_tricky",
  "title": "以 \"messages\" 结尾的标题",
  "workspace": "messages",
  "created_at": 1,
  "updated_at": 2,
  "messages": [{"role": "user", "parts": {"User": [{"Text": "hi"}]}}],
  "usage": {"input_tokens": 1}
}"#;
        let s = parse_summary_head(tricky).expect("must cut at the real key");
        assert_eq!(s.id, "s_tricky");
        assert_eq!(s.created_at, 1);
        assert_eq!(s.updated_at, 2);
    }
}
