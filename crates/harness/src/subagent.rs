//! Subagents: delegate a task to an isolated child agent, keep the child visible
//! to its parent in a catalog, and expose control tools to the model.
//!
//! Design follows DeepSeek Harness's `subagent` subsystem (MIT, deepseek-ai/deepseek-harness)
//! as a behavioural reference — specifically the delegation seam, the child catalog that
//! keeps every created child discoverable, and the `delivery: parent | caller` split.
//!
//! Deliberate simplifications in this port, stated plainly:
//! - One backend only: an in-process child with its own message history. The
//!   out-of-process backends upstream (ACP / Codex / Claude Code / SDK) are not ported.
//! - Children are one-shot; `prepareContinuable` follow-up turns are not ported.
//! - `outputSchema` / `agentPreset` capability flags are not ported.

use crate::error::HarnessError;
use crate::event::EventHub;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

/// Where the child's result is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Push the result into the parent's conversation as a message.
    Parent,
    /// Return the result to the caller only (used by nested delegation).
    Caller,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnRequest {
    /// Short task label kept in the catalog.
    pub label: String,
    /// Model-facing name: 1–32 lower-case letters, digits or hyphens, starting
    /// with a letter. Omission selects the first free `agent-N`.
    #[serde(default)]
    pub name: Option<String>,
    /// The task the child should carry out.
    pub task: String,
    /// Initial working directory; relative paths resolve against the parent's.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Reuse the parent's conversation as context instead of starting cold.
    #[serde(default)]
    pub seed_history: bool,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub delivery: Option<Delivery>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildStatus {
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Child {
    pub id: String,
    /// Model-facing name.
    pub name: String,
    pub label: String,
    pub task: String,
    pub status: ChildStatus,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "startedAt")]
    pub started_at: String,
    #[serde(rename = "endedAt", default)]
    pub ended_at: Option<String>,
    /// Final answer the child produced, once it settles.
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default, rename = "toolCalls")]
    pub tool_calls: u64,
}

// ---------------------------------------------------------------- catalog

/// Every child the parent created, discoverable whether running or settled.
pub struct ChildCatalog {
    children: RwLock<HashMap<String, Child>>,
    order: Mutex<Vec<String>>,
}

impl ChildCatalog {
    pub fn new() -> Self {
        Self {
            children: RwLock::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
        }
    }

    pub async fn add(&self, c: Child) {
        self.order.lock().await.push(c.id.clone());
        self.children.write().await.insert(c.id.clone(), c);
    }

    pub async fn get(&self, id: &str) -> Option<Child> {
        self.children.read().await.get(id).cloned()
    }

    pub async fn list(&self) -> Vec<Child> {
        let order = self.order.lock().await.clone();
        let map = self.children.read().await;
        order.iter().filter_map(|id| map.get(id).cloned()).collect()
    }

    pub async fn update<F: FnOnce(&mut Child)>(&self, id: &str, f: F) {
        if let Some(c) = self.children.write().await.get_mut(id) {
            f(c);
        }
    }

    /// Allocate the first free `agent-N` name.
    pub async fn next_free_name(&self) -> String {
        let taken: Vec<String> = self
            .children
            .read()
            .await
            .values()
            .map(|c| c.name.clone())
            .collect();
        for n in 1..1000 {
            let cand = format!("agent-{n}");
            if !taken.contains(&cand) {
                return cand;
            }
        }
        format!("agent-{}", uuid::Uuid::new_v4().simple())
    }
}

/// DSH's naming rule: 1–32 lower-case letters, digits or hyphens, starting with
/// a letter, and never `lead` (reserved for the root agent).
pub fn valid_name(s: &str) -> bool {
    if s.is_empty() || s.len() > 32 || s == "lead" {
        return false;
    }
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

// ---------------------------------------------------------------- service

pub struct SubagentService {
    pub catalog: Arc<ChildCatalog>,
    hub: EventHub,
    running: Arc<Mutex<HashMap<String, tokio_util::sync::CancellationToken>>>,
    /// 注入的共享 AgentRuntime（`with_runtime`，server.rs 装配时调用）：
    /// 用于子 turn 完成判定（`is_running`）、超时/取消时终止子 turn
    /// （`interrupt`），以及继承父 turn 的 `TurnOptions`（权限模式等）。
    runtime: Option<Arc<crate::agent::AgentRuntime>>,
}

impl SubagentService {
    pub fn new(hub: EventHub) -> Self {
        Self {
            catalog: Arc::new(ChildCatalog::new()),
            hub,
            running: Arc::new(Mutex::new(HashMap::new())),
            runtime: None,
        }
    }

    /// 注入共享 AgentRuntime（builder；`new()` 签名保持不变）。
    pub fn with_runtime(mut self, rt: Arc<crate::agent::AgentRuntime>) -> Self {
        self.runtime = Some(rt);
        self
    }

    /// Start a child and return its catalog id immediately.
    pub async fn spawn(
        &self,
        starter: Arc<dyn crate::tools::TurnStarter>,
        parent_session: &str,
        parent_workspace: &str,
        req: SpawnRequest,
    ) -> Result<String, HarnessError> {
        if req.task.trim().is_empty() {
            return Err(HarnessError::Other("task must not be empty".into()));
        }
        if req.label.trim().is_empty() {
            return Err(HarnessError::Other("label must not be empty".into()));
        }
        // Delivery::Caller 目前结果无处投递——在校验阶段直接拒绝，绝不静默丢失。
        if req.delivery == Some(Delivery::Caller) {
            return Err(HarnessError::Other(
                "Caller 结果回传未支持（结果无处投递），请改用 Delivery::Parent".into(),
            ));
        }
        let name = match &req.name {
            Some(n) => {
                if !valid_name(n) {
                    return Err(HarnessError::Other(format!(
                        "`{n}` is not a valid agent name: use 1-32 lower-case letters, digits or hyphens, starting with a letter (and not `lead`)"
                    )));
                }
                n.clone()
            }
            None => self.catalog.next_free_name().await,
        };

        // The child gets its own session so its history is isolated from the parent.
        let store = crate::session::SessionStore::new();
        let child_session = store
            .create(parent_workspace, &format!("⚙ {label}", label = req.label))
            .await
            .meta
            .id;

        let child_id = format!("sa_{}", &uuid::Uuid::new_v4().simple().to_string()[..10]);
        let child = Child {
            id: child_id.clone(),
            name: name.clone(),
            label: req.label.clone(),
            task: req.task.clone(),
            status: ChildStatus::Running,
            session_id: child_session.clone(),
            started_at: crate::event::now_ms().to_string(),
            ended_at: None,
            result: None,
            error: None,
            tool_calls: 0,
        };
        self.catalog.add(child.clone()).await;
        self.hub.publish(
            parent_session,
            "subagent.start",
            json!({"agentId": child_id, "name": name, "label": req.label}),
        );

        // Seeding: hand the child the parent's history so prior context matters.
        if req.seed_history {
            let msgs = store.neutral_messages(parent_session).await;
            for m in msgs.iter() {
                // role 跟消息类型对齐（此前全部塞 "user"，assistant 上下文失真）：
                // assistant → "assistant"，user/tool_result → "user"。parts 内的
                // User/Assistant 标签由 Msg 序列化自带，保持正确。
                let role = match m {
                    crate::provider::Msg::Assistant(_) => "assistant",
                    crate::provider::Msg::User(_) => "user",
                };
                store
                    .append_message(
                        &child_session,
                        crate::session::StoredMessage {
                            role: role.into(),
                            parts: serde_json::to_value(m).unwrap_or(Value::Null),
                            displays: Vec::new(),
                            created_at: crate::event::now_ms(),
                        },
                    )
                    .await
                    .ok();
            }
        }

        // 子 turn 选项：优先继承父 turn 的 TurnOptions（permission_mode、
        // thinking_level、plan_mode 一并继承，不再硬编码 "full"）；模型选择
        // 用 req.model（为空则继承父的）。未注入 runtime 时退回 starter 的默认。
        let child_opts: Option<crate::agent::TurnOptions> = match self.runtime.as_ref() {
            Some(rt) => {
                let mut o = rt
                    .current_turn_options(parent_session)
                    .await
                    .unwrap_or_else(crate::agent::TurnOptions::default);
                if !req.model.is_empty() {
                    o.model_sel = req.model.clone();
                }
                Some(o)
            }
            None => None,
        };

        let cancel = tokio_util::sync::CancellationToken::new();
        self.running.lock().await.insert(child_id.clone(), cancel.clone());

        let cat = self.catalog.clone();
        let hub = self.hub.clone();
        let cid = child_id.clone();
        let cname = name.clone();
        let label = req.label.clone();
        let task = req.task.clone();
        let model = req.model.clone();
        let parent_sid = parent_session.to_string();
        let child_sid = child_session.clone();
        let delivery = req.delivery.unwrap_or(Delivery::Parent);
        let running = self.running.clone();
        let runtime = self.runtime.clone();

        tokio::spawn(async move {
            // A child turn is a normal turn on its own session.
            // 一律经由 trait 对象（dyn TurnStarter）启动，保持 future 类型
            // 擦除：直接 await AgentRuntime::start_turn 会让它的 opaque
            // future 经 tokio::spawn 的 Send 检查递归套回自身（E0391）。
            let res = if let Some(opts) = child_opts.clone() {
                starter.start_child_opts(&child_sid, &task, opts).await
            } else {
                starter.start_child(&child_sid, &task, &model).await
            };

            // 等子 turn 收尾。注入 runtime 后以 `is_running` 为准——流式输出
            // 中途的 assistant 消息也可能带非空 Text，不能只看最后一条消息。
            const POLL_MS: u64 = 500;
            const MAX_POLLS: u64 = 600; // 600 × 500ms = 300s 超时
            let mut final_text = String::new();
            let mut tool_calls_seen: u64 = 0;
            let mut timed_out = false;
            let mut polls = 0u64;
            loop {
                if cancel.is_cancelled() {
                    // 父侧取消：同时终止子 turn，避免留下孤儿轮继续跑。
                    if let Some(rt) = runtime.as_ref() {
                        rt.interrupt(&child_sid).await;
                    }
                    break;
                }
                if polls >= MAX_POLLS {
                    timed_out = true;
                    break;
                }
                polls += 1;
                let msgs = store_neutral(&child_sid).await;
                // 统计子会话消息里的 ToolUse parts 数，更新目录计数。
                let tc = count_tool_uses(&msgs);
                if tc != tool_calls_seen {
                    tool_calls_seen = tc;
                    cat.update(&cid, |c| c.tool_calls = tc).await;
                }
                match runtime.as_ref() {
                    Some(rt) => {
                        if !rt.is_running(&child_sid).await {
                            // 子 turn 已结束：取最后一条 assistant 消息中的非空 Text。
                            final_text = last_assistant_text(&msgs).unwrap_or_default();
                            break;
                        }
                    }
                    None => {
                        // 未注入 runtime 时退回旧行为：最后一条消息出现非空文本即收尾。
                        if let Some(last) = msgs.last() {
                            if let Some(text) = last_text(last) {
                                if !text.trim().is_empty() {
                                    final_text = text;
                                    break;
                                }
                            }
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(POLL_MS)).await;
            }

            let (status, err) = if timed_out {
                // 超时：先 interrupt 子 turn 并等它清槽，再标 Failed——不留孤儿运行槽。
                if let Some(rt) = runtime.as_ref() {
                    rt.interrupt(&child_sid).await;
                    for _ in 0..20 {
                        if !rt.is_running(&child_sid).await {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
                (ChildStatus::Failed, Some("child agent timed out after 300s".to_string()))
            } else if cancel.is_cancelled() {
                (ChildStatus::Cancelled, Some("cancelled by parent".to_string()))
            } else if let Err(e) = res {
                (ChildStatus::Failed, Some(e.to_string()))
            } else if final_text.trim().is_empty() {
                (ChildStatus::Failed, Some("child produced no output".to_string()))
            } else {
                (ChildStatus::Done, None)
            };

            let summary = if matches!(status, ChildStatus::Done) {
                final_text.trim().to_string()
            } else {
                String::new()
            };

            cat.update(&cid, |c| {
                c.status = status;
                c.ended_at = Some(crate::event::now_ms().to_string());
                if !summary.is_empty() {
                    c.result = Some(summary.clone());
                }
                c.error = err.clone();
            })
            .await;

            running.lock().await.remove(&cid);

            let payload = json!({
                "agentId": cid, "name": cname, "label": label,
                "status": match status {
                    ChildStatus::Done => "done",
                    ChildStatus::Failed => "failed",
                    ChildStatus::Cancelled => "cancelled",
                    ChildStatus::Running => "running",
                },
                "result": summary,
                "error": err,
            });
            hub.publish(&parent_sid, "subagent.end", payload.clone());
            if delivery == Delivery::Parent {
                hub.publish(
                    &parent_sid,
                    "subagent.result",
                    json!({
                        "agentId": cid, "name": cname, "label": label,
                        "result": summary, "error": err,
                    }),
                );
            }
        });

        Ok(child_id)
    }

    pub async fn interrupt(&self, child_id: &str) -> bool {
        let c = self.running.lock().await.get(child_id).cloned();
        if let Some(c) = c {
            c.cancel();
            // 同步取消子会话的 turn：只停轮询不停子 turn 会留下孤儿轮继续跑。
            if let Some(ch) = self.catalog.get(child_id).await {
                if let Some(rt) = &self.runtime {
                    rt.interrupt(&ch.session_id).await;
                }
            }
            self.catalog
                .update(child_id, |ch| {
                    if ch.status == ChildStatus::Running {
                        ch.status = ChildStatus::Cancelled;
                        ch.ended_at = Some(crate::event::now_ms().to_string());
                    }
                })
                .await;
            true
        } else {
            false
        }
    }

    pub async fn list(&self) -> Vec<Child> {
        self.catalog.list().await
    }
}

async fn store_neutral(session_id: &str) -> Vec<crate::provider::Msg> {
    let s = crate::session::SessionStore::new();
    s.neutral_messages(session_id).await
}

fn last_text(m: &crate::provider::Msg) -> Option<String> {
    use crate::provider::AsstPart;
    match m {
        crate::provider::Msg::Assistant(parts) => {
            let mut out = String::new();
            for p in parts {
                if let AsstPart::Text(t) = p {
                    if !t.trim().is_empty() {
                        out.push_str(t);
                    }
                }
            }
            if out.trim().is_empty() {
                None
            } else {
                Some(out)
            }
        }
        _ => None,
    }
}

/// 统计消息序列里 ToolUse parts 的总数（子代理的 tool_calls 计数口径）。
fn count_tool_uses(msgs: &[crate::provider::Msg]) -> u64 {
    use crate::provider::AsstPart;
    msgs.iter()
        .filter_map(|m| match m {
            crate::provider::Msg::Assistant(parts) => Some(parts),
            _ => None,
        })
        .flat_map(|parts| parts.iter())
        .filter(|p| matches!(p, AsstPart::ToolUse { .. }))
        .count() as u64
}

/// 取最后一条含非空 Text 的 assistant 消息——不是"最后一条消息"：
/// 子 turn 结束后尾随的 tool_result 用户消息不该挡住结果提取。
fn last_assistant_text(msgs: &[crate::provider::Msg]) -> Option<String> {
    msgs.iter().rev().find_map(last_text)
}

pub type SharedSubagents = Arc<SubagentService>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_rules() {
        assert!(valid_name("agent-1"));
        assert!(valid_name("researcher"));
        assert!(valid_name("a"));
        assert!(!valid_name("lead"), "reserved for the root agent");
        assert!(!valid_name(""), "empty");
        assert!(!valid_name("Agent"), "upper-case");
        assert!(!valid_name("1abc"), "must start with a letter");
        assert!(!valid_name("a b"), "no spaces");
        assert!(!valid_name(&"a".repeat(33)), "max 32");
    }

    #[tokio::test]
    async fn catalog_allocates_free_names() {
        let cat = ChildCatalog::new();
        assert_eq!(cat.next_free_name().await, "agent-1");
        cat.add(Child {
            id: "x".into(),
            name: "agent-1".into(),
            label: "l".into(),
            task: "t".into(),
            status: ChildStatus::Running,
            session_id: "s".into(),
            started_at: "0".into(),
            ended_at: None,
            result: None,
            error: None,
            tool_calls: 0,
        })
        .await;
        assert_eq!(cat.next_free_name().await, "agent-2");
        assert_eq!(cat.list().await.len(), 1);
    }

    #[test]
    fn delivery_round_trips() {
        let d: Delivery = serde_json::from_str("\"parent\"").unwrap();
        assert_eq!(d, Delivery::Parent);
        let d: Delivery = serde_json::from_str("\"caller\"").unwrap();
        assert_eq!(d, Delivery::Caller);
    }
}
