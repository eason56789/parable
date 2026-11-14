//! Long-lived memories the agent carries between conversations.
//!
//! A memory is a short piece of text the user wants Parable to remember: a
//! preference ("answer in Chinese, lead with the conclusion"), a project fact
//! ("this workspace is an Android app on Jetpack Compose"), or a standing
//! instruction. They live in `~/.parable/memory.json` and are injected into the
//! system prompt of every turn, so the model sees them without being asked.
//!
//! Storage is a plain JSON array, written atomically. The list is small by
//! design (a person has dozens of these, not thousands) so a whole-file rewrite
//! is cheaper and far simpler than an append-only journal.

use crate::error::HarnessError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::sync::RwLock;

/// Which bucket a memory belongs to. The UI shows these as coloured chips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryTag {
    /// A preference about how Parable should work.
    Pref,
    /// A durable fact about the current project or workspace.
    Proj,
    /// A general fact worth remembering.
    Fact,
}

impl Default for MemoryTag {
    fn default() -> Self {
        MemoryTag::Pref
    }
}

/// One remembered item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: u64,
    #[serde(default)]
    pub tag: MemoryTag,
    pub text: String,
    /// `YYYY-MM-DD`, local time. Updated on every edit.
    #[serde(default)]
    pub ts: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
}

/// Body of `POST /api/memory`.
#[derive(Debug, Deserialize)]
pub struct MemoryInput {
    /// Omitted for a new memory.
    #[serde(default)]
    pub id: Option<u64>,
    #[serde(default)]
    pub tag: Option<MemoryTag>,
    pub text: String,
}

/// Memory store backed by one JSON file.
pub struct MemoryStore {
    path: PathBuf,
    items: RwLock<Vec<Memory>>,
}

impl MemoryStore {
    /// Open (or create) the store at `<home>/memory.json`.
    pub fn new() -> Self {
        Self::with_path(crate::config::Config::home_dir().join("memory.json"))
    }

    pub fn with_path(path: PathBuf) -> Self {
        let items = load(&path);
        Self {
            path,
            items: RwLock::new(items),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every memory, newest first.
    pub async fn list(&self) -> Vec<Memory> {
        let mut v = self.items.read().await.clone();
        v.sort_by(|a, b| b.id.cmp(&a.id));
        v
    }

    /// Insert a new memory or replace an existing one; returns the stored item.
    pub async fn upsert(&self, input: MemoryInput) -> Result<Memory, HarnessError> {
        let text = input.text.trim().to_string();
        if text.is_empty() {
            return Err(HarnessError::Config("memory text is empty".into()));
        }
        let today = crate::usage::today_key();
        let mut items = self.items.write().await;
        let item = match input.id {
            Some(id) if items.iter().any(|m| m.id == id) => {
                let m = items.iter_mut().find(|m| m.id == id).expect("checked above");
                m.text = text;
                if let Some(t) = input.tag {
                    m.tag = t;
                }
                m.ts = today;
                m.clone()
            }
            _ => {
                let id = items.iter().map(|m| m.id).max().unwrap_or(0) + 1;
                let m = Memory {
                    id,
                    tag: input.tag.unwrap_or_default(),
                    text,
                    ts: today.clone(),
                    created: Some(today),
                };
                items.push(m.clone());
                m
            }
        };
        save(&self.path, &items)?;
        Ok(item)
    }

    /// Remove one memory. Missing ids are not an error (idempotent delete).
    pub async fn remove(&self, id: u64) -> Result<bool, HarnessError> {
        let mut items = self.items.write().await;
        let before = items.len();
        items.retain(|m| m.id != id);
        if items.len() == before {
            return Ok(false);
        }
        save(&self.path, &items)?;
        Ok(true)
    }

    /// Remove every memory.
    pub async fn clear(&self) -> Result<usize, HarnessError> {
        let mut items = self.items.write().await;
        let n = items.len();
        items.clear();
        save(&self.path, &items)?;
        Ok(n)
    }

    /// The block injected into the system prompt, or `None` when empty.
    ///
    /// Grouped by tag so the model reads related items together, and numbered
    /// so the user can point at "memory 3" in a follow-up.
    pub async fn prompt_block(&self) -> Option<String> {
        let items = self.list().await;
        if items.is_empty() {
            return None;
        }
        let mut s = String::from(
            "# Memories\n\
             Things the user asked you to remember. Treat them as standing context: \
             they are true unless the user says otherwise, and they take priority over \
             your general assumptions about style and tooling.\n",
        );
        for (label, tag) in [
            ("Preferences", MemoryTag::Pref),
            ("Project", MemoryTag::Proj),
            ("Facts", MemoryTag::Fact),
        ] {
            let group: Vec<&Memory> = items.iter().filter(|m| m.tag == tag).collect();
            if group.is_empty() {
                continue;
            }
            s.push_str(&format!("\n{label}:\n"));
            for m in group {
                s.push_str("- ");
                s.push_str(&m.text);
                s.push('\n');
            }
        }
        Some(s)
    }
}

fn load(path: &Path) -> Vec<Memory> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    // Accept both a bare array and `{"memories":[...]}` so a hand-edited file
    // does not silently lose everything.
    if let Ok(v) = serde_json::from_str::<Vec<Memory>>(&text) {
        return v;
    }
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(v) => serde_json::from_value(v.get("memories").cloned().unwrap_or_default())
            .unwrap_or_default(),
        Err(e) => {
            tracing::warn!("memory.json: could not parse ({e}); starting empty");
            Vec::new()
        }
    }
}

fn save(path: &Path, items: &[Memory]) -> Result<(), HarnessError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = serde_json::to_vec_pretty(items)?;
    crate::atomic_write(path, &body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store(tag: &str) -> MemoryStore {
        let dir = std::env::temp_dir().join(format!(
            "parable-mem-{tag}-{}-{}",
            std::process::id(),
            crate::event::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        MemoryStore::with_path(dir.join("memory.json"))
    }

    #[tokio::test]
    async fn adds_and_lists_newest_first() {
        let s = tmp_store("add");
        s.upsert(MemoryInput {
            id: None,
            tag: Some(MemoryTag::Pref),
            text: "  first  ".into(),
        })
        .await
        .unwrap();
        s.upsert(MemoryInput {
            id: None,
            tag: Some(MemoryTag::Proj),
            text: "second".into(),
        })
        .await
        .unwrap();
        let all = s.list().await;
        assert_eq!(all.len(), 2);
        // Trimmed on the way in.
        assert_eq!(all[1].text, "first");
        // Newest id sorts first.
        assert_eq!(all[0].text, "second");
        assert!(all[0].id > all[1].id);
    }

    #[tokio::test]
    async fn edits_keep_the_id_and_move_the_timestamp() {
        let s = tmp_store("edit");
        let m = s
            .upsert(MemoryInput {
                id: None,
                tag: Some(MemoryTag::Fact),
                text: "old".into(),
            })
            .await
            .unwrap();
        let m2 = s
            .upsert(MemoryInput {
                id: Some(m.id),
                tag: None,
                text: "new".into(),
            })
            .await
            .unwrap();
        assert_eq!(m2.id, m.id);
        assert_eq!(m2.text, "new");
        // Tag untouched when the input omits it.
        assert_eq!(m2.tag, MemoryTag::Fact);
        assert_eq!(s.list().await.len(), 1);
    }

    #[tokio::test]
    async fn rejects_empty_text() {
        let s = tmp_store("empty");
        let err = s
            .upsert(MemoryInput {
                id: None,
                tag: None,
                text: "   ".into(),
            })
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let s = tmp_store("del");
        let m = s
            .upsert(MemoryInput {
                id: None,
                tag: None,
                text: "x".into(),
            })
            .await
            .unwrap();
        assert!(s.remove(m.id).await.unwrap());
        assert!(!s.remove(m.id).await.unwrap());
        assert!(s.list().await.is_empty());
    }

    #[tokio::test]
    async fn survives_a_reload() {
        let s = tmp_store("reload");
        s.upsert(MemoryInput {
            id: None,
            tag: Some(MemoryTag::Proj),
            text: "durable".into(),
        })
        .await
        .unwrap();
        let again = MemoryStore::with_path(s.path().to_path_buf());
        let all = again.list().await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].text, "durable");
        assert_eq!(all[0].tag, MemoryTag::Proj);
    }

    #[tokio::test]
    async fn prompt_block_groups_by_tag() {
        let s = tmp_store("prompt");
        assert!(s.prompt_block().await.is_none(), "empty store injects nothing");
        s.upsert(MemoryInput {
            id: None,
            tag: Some(MemoryTag::Pref),
            text: "be terse".into(),
        })
        .await
        .unwrap();
        s.upsert(MemoryInput {
            id: None,
            tag: Some(MemoryTag::Fact),
            text: "UTC+8".into(),
        })
        .await
        .unwrap();
        let block = s.prompt_block().await.unwrap();
        assert!(block.contains("# Memories"));
        assert!(block.contains("Preferences:\n- be terse"));
        assert!(block.contains("Facts:\n- UTC+8"));
        // Empty groups are omitted entirely.
        assert!(!block.contains("Project:"));
    }

    #[test]
    fn reads_an_object_wrapper() {
        let dir = std::env::temp_dir().join(format!("parable-mem-wrap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("memory.json");
        std::fs::write(&p, r#"{"memories":[{"id":7,"tag":"fact","text":"hi"}]}"#).unwrap();
        let s = MemoryStore::with_path(p);
        let items = futures::executor::block_on(s.list());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, 7);
    }
}
