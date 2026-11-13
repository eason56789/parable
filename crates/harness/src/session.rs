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
    #[serde(default)]
    pub created_at: u64,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Session {
    #[serde(flatten)]
    pub meta: SessionSummary,
    #[serde(default)]
    pub messages: Vec<StoredMessage>,
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
        let root = Config::home_dir().join("sessions");
        let _ = std::fs::create_dir_all(&root);
        Self {
            root,
            cache: Mutex::new(HashMap::new()),
            meta_index: Mutex::new(HashMap::new()),
            text_index: Mutex::new(HashMap::new()),
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
        }
    }

    fn path_for(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
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

    /// 原子落盘：临时文件 + rename（见 `crate::atomic_write`），失败返回错误由
    /// 调用方记录——静默丢写会让重载后的会话回退到旧状态。
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
        let p = self.path_for(id);
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
        self.path_for(id).exists()
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
        let snapshot = s.clone();
        drop(cache);
        if let Err(e) = self.persist(&snapshot).await {
            tracing::error!("session {id}: persist failed: {e}");
        }
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
        let snapshot = s.clone();
        drop(cache);
        if let Err(e) = self.persist(&snapshot).await {
            tracing::error!("session {id}: persist failed: {e}");
        }
        Ok(())
    }

    pub async fn add_usage(&self, id: &str, u: &crate::token::Usage, turns: u64, steps: u64, wall_ms: u64) {
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
            let snapshot = s.clone();
            drop(cache);
            if let Err(e) = self.persist(&snapshot).await {
                tracing::error!("session {id}: persist failed: {e}");
            }
        }
    }

    pub async fn delete(&self, id: &str) -> Result<(), HarnessError> {
        self.cache.lock().await.remove(id);
        let p = self.path_for(id);
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
            m.parts = serde_json::json!({ key: kept });
            if m.role == "assistant" {
                target = i;
            }
        }
        let m = &mut s.messages[target];
        let key = if m.role == "user" { "User" } else { "Assistant" };
        if let Some(arr) = m.parts.get_mut(key).and_then(|v| v.as_array_mut()) {
            arr.push(serde_json::json!({ "Text": text }));
        } else {
            m.parts = serde_json::json!({ key: [{ "Text": text }] });
        }
        // `displays` stays: the tool cards keep their rendered content.
        s.meta.updated_at = crate::event::now_ms();
        let snapshot = s.clone();
        drop(cache);
        if let Err(e) = self.persist(&snapshot).await {
            tracing::error!("session {id}: persist failed: {e}");
        }
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
            s.messages.drain(from..end);
            s.meta.updated_at = crate::event::now_ms();
            let snapshot = s.clone();
            drop(cache);
            if let Err(e) = self.persist(&snapshot).await {
                tracing::error!("session {id}: persist failed: {e}");
            }
        }
        Ok(())
    }

    /// Drop every message from `from` onward — used when re-running from a point.
    pub async fn truncate_from(&self, id: &str, from: usize) -> Result<(), HarnessError> {
        self.delete_range(id, from, None).await
    }

    /// Replace the first `keep_from` messages with one summary message — the
    /// persistence half of context compaction. Returns how many were folded.
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
                created_at: crate::event::now_ms(),
            },
        );
        s.meta.updated_at = crate::event::now_ms();
        let snapshot = s.clone();
        drop(cache);
        if let Err(e) = self.persist(&snapshot).await {
            tracing::error!("session {id}: persist failed: {e}");
        }
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
        let snapshot = s.clone();
        drop(cache);
        if let Err(e) = self.persist(&snapshot).await {
            tracing::error!("session {id}: persist failed: {e}");
        }
        Ok(())
    }

    /// Read the Agent Teams board of the tree rooted at this session.
    pub async fn get_team(&self, id: &str) -> Option<crate::team::TeamState> {
        self.load(id).await.and_then(|s| s.team)
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
        let snapshot = s.clone();
        drop(cache);
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
        let stamp = self
            .path_for(id)
            .metadata()
            .ok()
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
fn parse_summary_head(text: &str) -> Option<SessionSummary> {
    let cut = text.find("\"messages\"")?;
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
fn message_text(parts: &Value) -> Option<String> {
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

pub type SharedStore = Arc<SessionStore>;

#[cfg(test)]
mod tests {
    use super::*;

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
            },
            messages: Vec::new(),
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
}
