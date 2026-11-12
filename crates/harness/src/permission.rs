//! Permission gateway: risk classification, allow/deny rule table, and the broker
//! that suspends a tool call until the user decides.

use crate::error::HarnessError;
use crate::event::EventHub;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    /// Read-only, safe in any mode.
    ReadOnly,
    /// Modifies files inside the workspace.
    Write,
    /// Deletes or overwrites in a way that is hard to undo.
    Destructive,
    /// Talks to the network.
    Network,
    /// Executes a shell command.
    Shell,
}

impl Risk {
    pub fn as_str(&self) -> &'static str {
        match self {
            Risk::ReadOnly => "readonly",
            Risk::Write => "write",
            Risk::Destructive => "destructive",
            Risk::Network => "network",
            Risk::Shell => "shell",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Once,
    Always,
    Deny,
}

impl Decision {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "once" | "ok" | "allow" => Some(Decision::Once),
            "always" => Some(Decision::Always),
            "no" | "deny" => Some(Decision::Deny),
            _ => None,
        }
    }
    pub fn allowed(&self) -> bool {
        matches!(self, Decision::Once | Decision::Always)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    /// Tool name, or `*` for any tool.
    #[serde(default = "star")]
    pub tool: String,
    /// Glob-ish path pattern; `*` matches anything.
    #[serde(default = "star")]
    pub path: String,
    /// `allow` | `deny`
    pub action: String,
}

fn star() -> String {
    "*".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuleSet {
    #[serde(default)]
    pub allow: Vec<Rule>,
    #[serde(default)]
    pub deny: Vec<Rule>,
}

impl RuleSet {
    pub fn load() -> Self {
        std::fs::read_to_string(crate::config::Config::permissions_path())
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> crate::Result<()> {
        let p = crate::config::Config::permissions_path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let s = toml::to_string_pretty(self).map_err(|e| HarnessError::Config(e.to_string()))?;
        std::fs::write(p, s)?;
        Ok(())
    }

    fn matches(rule: &Rule, tool: &str, path: &str) -> bool {
        let tool_ok = rule.tool == "*" || rule.tool == tool;
        let path_ok = rule.path == "*" || glob_match(&rule.path, path);
        tool_ok && path_ok
    }

    pub fn lookup(&self, tool: &str, path: &str) -> Option<bool> {
        if self
            .deny
            .iter()
            .any(|r| Self::matches(r, tool, path))
        {
            return Some(false);
        }
        if self
            .allow
            .iter()
            .any(|r| Self::matches(r, tool, path))
        {
            return Some(true);
        }
        None
    }
}

/// Glob matching via `globset` (`*`, `?`, `**`, `[...]`), matched against the
/// full path and, for convenience, against the basename too.
fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    use globset::{Glob, GlobMatcher};
    let glob = match Glob::new(pattern) {
        Ok(g) => g,
        Err(_) => return pattern == text,
    };
    let m: GlobMatcher = glob.compile_matcher();
    if m.is_match(text) {
        return true;
    }
    // Also match when the pattern has no separator: treat it as "any dir".
    if !pattern.contains('/') && !pattern.contains('\\') {
        if let Some(name) = Path::new(text).file_name().and_then(|n| n.to_str()) {
            return m.is_match(name);
        }
    }
    false
}

/// Is `path` inside `workspace`?
pub fn inside_workspace(workspace: &Path, path: &Path) -> bool {
    let Ok(w) = workspace.canonicalize() else {
        return false;
    };
    let Ok(p) = path.canonicalize() else {
        // Non-existent path: compare the logical prefix.
        let wl = w.to_string_lossy().to_lowercase();
        let pl = path.to_string_lossy().to_lowercase();
        return pl.starts_with(&wl);
    };
    p.starts_with(&w)
}

pub struct PendingRequest {
    pub tx: oneshot::Sender<Decision>,
    pub tool: String,
    pub path: String,
}

/// Owns the rule table and the set of in-flight permission requests.
pub struct PermissionBroker {
    rules: RwLock<RuleSet>,
    pending: Mutex<HashMap<String, PendingRequest>>,
    hub: EventHub,
    session_id: Mutex<String>,
}

impl PermissionBroker {
    pub fn new(hub: EventHub) -> Self {
        Self {
            rules: RwLock::new(RuleSet::load()),
            pending: Mutex::new(HashMap::new()),
            hub,
            session_id: Mutex::new(String::new()),
        }
    }

    pub async fn set_session(&self, id: &str) {
        *self.session_id.lock().await = id.to_string();
    }

    /// Decide whether `tool` acting on `path` may run without asking.
    pub async fn auto_allow(&self, mode: &str, tool: &str, path: &str, workspace: &Path) -> bool {
        let rules = self.rules.read().await;
        match rules.lookup(tool, path) {
            Some(true) => true,
            Some(false) => false,
            None => match mode {
                "full" => true,
                "ws" => {
                    if matches!(tool, "read_file" | "glob" | "grep" | "list_dir") {
                        return true;
                    }
                    inside_workspace(workspace, Path::new(path))
                }
                _ => matches!(tool, "read_file" | "glob" | "grep" | "list_dir"),
            },
        }
    }

    /// Suspend until the user answers. Emits `permission.request` and waits.
    pub async fn request(
        &self,
        call_id: &str,
        tool: &str,
        path: &str,
        risk: Risk,
        reason: &str,
        diff_preview: Option<String>,
    ) -> Result<Decision, HarnessError> {
        let (tx, rx) = oneshot::channel();
        {
            let mut p = self.pending.lock().await;
            p.insert(
                call_id.to_string(),
                PendingRequest {
                    tx,
                    tool: tool.to_string(),
                    path: path.to_string(),
                },
            );
        }
        let sid = self.session_id.lock().await.clone();
        self.hub.publish(
            &sid,
            "permission.request",
            json!({
                "callId": call_id,
                "tool": tool,
                "path": path,
                "risk": risk.as_str(),
                "reason": reason,
                "diffPreview": diff_preview,
            }),
        );

        // Timeout: never hang the agent forever.
        let decision = tokio::select! {
            res = rx => res.unwrap_or(Decision::Deny),
            _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => Decision::Deny,
        };

        self.pending.lock().await.remove(call_id);

        if decision == Decision::Always {
            let mut rules = self.rules.write().await;
            rules.allow.push(Rule {
                tool: tool.to_string(),
                path: path.to_string(),
                action: "allow".into(),
            });
            let _ = rules.save();
        }
        self.hub.publish(
            &sid,
            "permission.resolved",
            json!({"callId": call_id, "decision": match decision {
                Decision::Once => "once",
                Decision::Always => "always",
                Decision::Deny => "deny",
            }}),
        );
        self.audit(call_id, tool, path, risk, decision).await;
        Ok(decision)
    }

    /// Answer a pending request from the HTTP layer.
    pub async fn resolve(&self, call_id: &str, decision: Decision) -> bool {
        let tx = self.pending.lock().await.remove(call_id).map(|p| p.tx);
        match tx {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    pub async fn rules(&self) -> RuleSet {
        self.rules.read().await.clone()
    }

    async fn audit(&self, call_id: &str, tool: &str, path: &str, risk: Risk, d: Decision) {
        let dir = crate::config::Config::logs_dir();
        let _ = std::fs::create_dir_all(&dir);
        let day = chrono::Local::now().format("%Y%m%d").to_string();
        let file = dir.join(format!("audit-{day}.jsonl"));
        let line = json!({
            "ts": crate::event::now_ms(),
            "callId": call_id,
            "tool": tool,
            "path": path,
            "risk": risk.as_str(),
            "decision": match d { Decision::Once => "once", Decision::Always => "always", Decision::Deny => "deny" },
        });
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(file) {
            let _ = writeln!(f, "{line}");
        }
    }
}

pub type SharedPermissions = Arc<PermissionBroker>;

#[allow(dead_code)]
fn _assert_send_sync() {
    fn assert<T: Send + Sync>() {}
    assert::<PermissionBroker>();
    let _: PathBuf = crate::config::Config::home_dir();
}
