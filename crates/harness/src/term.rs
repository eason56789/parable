//! Background terminal sessions.
//!
//! The side-pane terminal runs one command synchronously; the top-right dock
//! needs *background* jobs that keep running while the user chats and can be
//! inspected, cancelled and re-listed after a reconnect. This module owns that
//! registry:
//!
//! - `POST /api/term/start` spawns the command and returns immediately.
//! - live stdout rides the existing `tool.progress` SSE events (keyed by
//!   `callId`), so a client that was connected the whole time needs no polling.
//! - `/api/term/list` restores the list after a refresh or a new tab.
//! - `/api/term/kill` aborts the task; the child is killed on drop.
//!
//! Jobs are user-typed commands, not model proposals, so they do not go
//! through the permission gateway — same rule as the side-pane terminal.

use crate::event::{now_ms, EventHub};
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Keep at most this many finished jobs around; running ones are never pruned.
const MAX_JOBS: usize = 40;
/// Only the tail of the output is kept in the registry (SSE already streamed
/// the whole thing live).
const MAX_TAIL: usize = 24_000;
/// Hard ceiling for one command. The shell tool enforces it and kills the
/// whole process tree on expiry.
const TERM_TIMEOUT_MS: u64 = 600_000;

#[derive(Clone, Debug, Serialize)]
pub struct TermJob {
    pub id: String,
    pub call_id: String,
    pub command: String,
    pub cwd: String,
    /// `running` | `exited` | `killed`
    pub status: String,
    pub exit: Option<i32>,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    /// Tail of stdout+stderr, for clients that attach after the fact.
    pub output: String,
}

#[derive(Default)]
struct Inner {
    jobs: HashMap<String, TermJob>,
    handles: HashMap<String, tokio::task::JoinHandle<()>>,
    order: Vec<String>,
}

#[derive(Clone, Default)]
pub struct TerminalRegistry {
    inner: Arc<Mutex<Inner>>,
}

fn short_id() -> String {
    let raw = uuid::Uuid::new_v4().simple().to_string();
    format!("term_{}", &raw[..10])
}

/// B-37：剥离子进程输出里的 ANSI 转义序列与 C0/C1 控制字符（仅保留 `\n`、`\t`），
/// 避免终端模拟器被注入光标移动/清屏/OSC 链接等序列。
fn sanitize_controls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut skip_seq = false;
    for c in s.chars() {
        if skip_seq {
            let code = c as u32;
            // CSI/转义序列以 0x40–0x7E 的终结字节结束。
            if (0x40..=0x7E).contains(&code) {
                skip_seq = false;
            }
            continue;
        }
        let code = c as u32;
        match c {
            '\u{1B}' => skip_seq = true,
            '\n' | '\t' => out.push(c),
            c if code < 0x20 || c == '\u{7F}' || (0x80..=0x9F).contains(&code) => {}
            _ => out.push(c),
        }
    }
    out
}

fn tail(s: &str) -> String {
    let s = sanitize_controls(s);
    if s.chars().count() <= MAX_TAIL {
        return s.to_string();
    }
    let skip = s.chars().count() - MAX_TAIL;
    s.chars().skip(skip).collect()
}

impl TerminalRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn a command and return the job id immediately.
    pub async fn start(&self, hub: EventHub, cwd: PathBuf, command: String) -> TermJob {
        let id = short_id();
        let call_id = id.clone();
        let job = TermJob {
            id: id.clone(),
            call_id: call_id.clone(),
            command: command.clone(),
            cwd: cwd.display().to_string(),
            status: "running".into(),
            exit: None,
            started_at: now_ms(),
            ended_at: None,
            output: String::new(),
        };
        hub.publish(
            "",
            "term.start",
            json!({
                "id": id, "callId": call_id, "command": command,
                "cwd": cwd.display().to_string(), "at": job.started_at,
            }),
        );

        let reg = self.clone();
        let hub2 = hub.clone();
        let jid = id.clone();
        let cid = call_id.clone();
        let cmd = command.clone();
        let handle = tokio::spawn(async move {
            let res = crate::tools::shell::bash(&cmd, &cwd, TERM_TIMEOUT_MS, &hub2, "", &cid, "").await;
            let (exit, stdout, ok) = match res {
                Ok(out) => match &out.display {
                    crate::tools::DisplayKind::Shell { stdout, exit, .. } => {
                        (*exit, stdout.clone(), *exit == 0)
                    }
                    _ => (0, out.for_model.clone(), true),
                },
                Err(e) => (-1, format!("error: {e}"), false),
            };
            reg.finish(&jid, exit, stdout, ok).await;
            hub2.publish(
                "",
                "term.end",
                json!({"id": jid, "callId": cid, "exit": exit, "ok": ok}),
            );
        });

        let mut inner = self.inner.lock().await;
        inner.jobs.insert(id.clone(), job.clone());
        inner.handles.insert(id.clone(), handle);
        inner.order.push(id);
        Self::prune(&mut inner);
        inner.jobs.get(&job.id).cloned().unwrap_or(job)
    }

    async fn finish(&self, id: &str, exit: i32, output: String, ok: bool) {
        let mut inner = self.inner.lock().await;
        inner.handles.remove(id);
        let Some(job) = inner.jobs.get_mut(id) else { return };
        // B-38：已被 kill 的任务保留 killed 状态与 kill 事件，不被事后退出覆盖。
        if job.status == "killed" {
            return;
        }
        job.status = "exited".into();
        job.exit = Some(exit);
        job.ended_at = Some(now_ms());
        job.output = tail(&output);
        let _ = ok;
    }

    /// Abort a running job. The shell tool's child is `kill_on_drop`, so
    /// aborting the task tears the process down as well.
    pub async fn kill(&self, hub: &EventHub, id: &str) -> bool {
        let mut inner = self.inner.lock().await;
        let Some(handle) = inner.handles.remove(id) else {
            return false;
        };
        handle.abort();
        let call_id = inner
            .jobs
            .get(id)
            .map(|j| j.call_id.clone())
            .unwrap_or_else(|| id.to_string());
        if let Some(job) = inner.jobs.get_mut(id) {
            job.status = "killed".into();
            job.ended_at = Some(now_ms());
        }
        drop(inner);
        hub.publish(
            "",
            "term.end",
            json!({"id": id, "callId": call_id, "exit": null, "ok": false, "killed": true}),
        );
        true
    }

    pub async fn list(&self) -> Vec<TermJob> {
        let inner = self.inner.lock().await;
        let mut out: Vec<TermJob> = inner
            .order
            .iter()
            .filter_map(|id| inner.jobs.get(id).cloned())
            .collect();
        out.sort_by_key(|j| j.started_at);
        out
    }

    /// Drop finished jobs, keep running ones.
    pub async fn clear(&self) {
        let mut inner = self.inner.lock().await;
        let running: Vec<String> = inner
            .jobs
            .values()
            .filter(|j| j.status == "running")
            .map(|j| j.id.clone())
            .collect();
        inner.jobs.retain(|id, _| running.contains(id));
        inner.order.retain(|id| running.iter().any(|r| r == id));
    }

    fn prune(inner: &mut Inner) {
        while inner.jobs.len() > MAX_JOBS {
            let victim = inner
                .order
                .iter()
                .find(|id| inner.jobs.get(id.as_str()).map(|j| j.status != "running").unwrap_or(false))
                .cloned();
            match victim {
                Some(id) => {
                    inner.jobs.remove(&id);
                    inner.handles.remove(&id);
                    inner.order.retain(|x| *x != id);
                }
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_end_on_char_boundaries() {
        let long: String = std::iter::repeat('答').take(MAX_TAIL + 10).collect();
        let t = tail(&long);
        assert_eq!(t.chars().count(), MAX_TAIL);
        assert!(t.ends_with('答'));
    }

    #[test]
    fn tail_strips_ansi_and_control_chars() {
        let raw = "正常输出\u{1B}[31m红色\u{1B}[0m\n下一行\r\0\u{07}";
        let t = tail(raw);
        assert!(!t.contains('\u{1B}'));
        assert!(!t.contains('\r'));
        assert!(!t.contains('\0'));
        assert!(!t.contains('\u{07}'));
        assert!(t.contains("正常输出"));
        assert!(t.contains("红色"));
        assert!(t.contains('\n'));
    }

    #[tokio::test]
    async fn list_is_empty_before_any_job() {
        let reg = TerminalRegistry::new();
        assert!(reg.list().await.is_empty());
        assert!(!reg.kill(&EventHub::new(), "term_nope").await);
    }
}
