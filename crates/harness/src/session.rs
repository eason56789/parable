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
use std::path::PathBuf;
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
}

impl SessionStore {
    pub fn new() -> Self {
        let root = Config::home_dir().join("sessions");
        let _ = std::fs::create_dir_all(&root);
        Self {
            root,
            cache: Mutex::new(HashMap::new()),
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

    pub async fn list(&self, query: &str) -> Vec<SessionSummary> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.root) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some("json") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&p) else {
                    continue;
                };
                let Ok(s) = serde_json::from_str::<Session>(&text) else {
                    continue;
                };
                if !query.is_empty()
                    && !s.meta.title.to_lowercase().contains(&query.to_lowercase())
                {
                    continue;
                }
                out.push(s.meta);
            }
        }
        out.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then(b.updated_at.cmp(&a.updated_at))
        });
        out
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
}
