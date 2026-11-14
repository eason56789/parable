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
//!
//! Tree shape: every child records the session that spawned it, its own depth,
//! and the root of its tree. The root session identifies the Agent Teams board
//! the tree shares (`crate::team`). Nesting is allowed up to [`MAX_DEPTH`].

use crate::error::HarnessError;
use crate::event::EventHub;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{watch, Mutex, RwLock};

/// How deep a delegation chain may go. The root session is depth 0, so a
/// great-grandchild is depth 3. Beyond this, `spawn_agent` is refused.
pub const MAX_DEPTH: u32 = 3;

/// Where the child's result is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Push the result into the parent's conversation as a message.
    Parent,
    /// Return the result to the caller only (used by nested delegation).
    Caller,
}

/// How a child finished.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildOutcome {
    pub status: ChildStatus,
    pub result: String,
    pub error: Option<String>,
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
    /// The session that spawned this child.
    #[serde(default, rename = "parentSession")]
    pub parent_session: String,
    /// The root session of this child's tree. Also the id of the shared team board.
    #[serde(default, rename = "rootSession")]
    pub root_session: String,
    /// 0 for the root session, 1 for its direct children, and so on.
    #[serde(default)]
    pub depth: u32,
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

/// Every child the process created, discoverable whether running or settled.
///
/// The map is flat, but each record carries its parent, so a tree can be
/// reconstructed by walking `parent_session` links.
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

    /// The child whose own session is `session_id`, if any.
    pub async fn find_by_session(&self, session_id: &str) -> Option<Child> {
        self.children
            .read()
            .await
            .values()
            .find(|c| c.session_id == session_id)
            .cloned()
    }

    pub async fn list(&self) -> Vec<Child> {
        let order = self.order.lock().await.clone();
        let map = self.children.read().await;
        order.iter().filter_map(|id| map.get(id).cloned()).collect()
    }

    /// The direct children of one session.
    pub async fn children_of(&self, parent: &str) -> Vec<Child> {
        self.list()
            .await
            .into_iter()
            .filter(|c| c.parent_session == parent)
            .collect()
    }

    /// Every descendant of one session, at any depth.
    ///
    /// A child records the root of its own tree, so the whole subtree is the
    /// set of records whose root matches. Direct children included.
    pub async fn descendants_of(&self, root: &str) -> Vec<Child> {
        self.list()
            .await
            .into_iter()
            .filter(|c| c.root_session == root)
            .collect()
    }

    pub async fn update<F: FnOnce(&mut Child)>(&self, id: &str, f: F) {
        if let Some(c) = self.children.write().await.get_mut(id) {
            f(c);
        }
    }

    /// Allocate the first free `agent-N` **within one tree**, so two
    /// independent trees can each have an `agent-1` without colliding.
    ///
    /// Scoped by the tree root, not the immediate parent: a grandchild and
    /// its uncle share a tree and must not both be offered `agent-1`.
    pub async fn next_free_name(&self, root: &str) -> String {
        let taken: Vec<String> = self
            .descendants_of(root)
            .await
            .into_iter()
            .map(|c| c.name)
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

/// 命名规则：1–32 个字符（按字符计），允许中文、字母、数字与连字符，
/// 首字符须是中文或字母；`lead` 保留给根代理。子代理名称由模型起，
/// 期望是贴切的简短中文名。
pub fn valid_name(s: &str) -> bool {
    if s.is_empty() || s.chars().count() > 32 || s == "lead" {
        return false;
    }
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || c == '-')
}

/// 从任务标签提取一个合法名称（模型漏填 name 时的兜底）：去掉空白与
/// 常见标点，只留中文/字母/数字/连字符；提取不出就返回 None。
pub fn name_from_label(label: &str) -> Option<String> {
    let cleaned: String = label
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-')
        .collect();
    let name: String = cleaned.chars().take(32).collect();
    if valid_name(&name) {
        Some(name)
    } else {
        None
    }
}

// ---------------------------------------------------------------- service

pub struct SubagentService {
    pub catalog: Arc<ChildCatalog>,
    hub: EventHub,
    running: Arc<Mutex<HashMap<String, tokio_util::sync::CancellationToken>>>,
    /// 注入的共享 AgentRuntime（`with_runtime`，server.rs 装配时调用）：
    /// 用于子 turn 完成判定（`is_running`）、超时/取消时终止子 turn
    /// （`interrupt`），以及继承父 turn 的 `TurnOptions`（权限模式等）。
    ///
    /// 后绑定（`RwLock`）而非构造期字段：runtime 里挂着 subagents，而
    /// subagents 又需要 runtime——两者互相持有会形成构造期死循环。用后绑定
    /// 打破它，一套 subagents 服务即可服务整棵树（子代理也能再 spawn）。
    runtime: Arc<RwLock<Option<Arc<crate::agent::AgentRuntime>>>>,
    /// 每个子代理的完成通道：`agent_wait` 在这里挂起，直到子代理收尾。
    outcomes: Arc<RwLock<HashMap<String, watch::Sender<Option<ChildOutcome>>>>>,
    /// 共享的会话存储。
    ///
    /// 轮询子代理的输出必须走**同一个** store 实例：另起一个
    /// `SessionStore::new()` 会得到自己的缓存，而缓存不会随磁盘变化失效，
    /// 于是轮询永远看到子会话的第一份（空）快照——子代理明明答了，父代理
    /// 却读到"没有输出"。
    store: Option<crate::session::SharedStore>,
}

impl SubagentService {
    pub fn new(hub: EventHub) -> Self {
        Self {
            catalog: Arc::new(ChildCatalog::new()),
            hub,
            running: Arc::new(Mutex::new(HashMap::new())),
            runtime: Arc::new(RwLock::new(None)),
            outcomes: Arc::new(RwLock::new(HashMap::new())),
            store: None,
        }
    }

    /// 注入共享 AgentRuntime（builder；`new()` 签名保持不变）。
    ///
    /// The runtime slot is late-bound, so this cannot await: use
    /// [`SubagentService::set_runtime`] after construction instead. This
    /// builder exists for callers that already hold an `Arc<AgentRuntime>`
    /// and want the service ready in one expression.
    pub fn with_runtime(self, rt: Arc<crate::agent::AgentRuntime>) -> Self {
        let runtime = self.runtime.clone();
        // The lock is uncontended at construction time, so blocking here is
        // safe and keeps the builder synchronous.
        let mut guard = runtime.blocking_write();
        *guard = Some(rt);
        drop(guard);
        self
    }

    /// 后绑定 AgentRuntime：装配顺序是 runtime → subagents → runtime，
    /// 中间那一步用这个方法完成，不形成构造期环。
    pub async fn set_runtime(&self, rt: Arc<crate::agent::AgentRuntime>) {
        *self.runtime.write().await = Some(rt);
    }

    /// 绑定共享的会话存储（轮询子代理输出用）。
    pub fn with_store(mut self, store: crate::session::SharedStore) -> Self {
        self.store = Some(store);
        self
    }

    /// The root session and depth of `parent_session`'s tree.
    ///
    /// A session that the catalog knows as a child continues its own tree;
    /// any other session is the root of a new tree.
    async fn lineage(&self, parent_session: &str) -> (String, u32) {
        match self.catalog.find_by_session(parent_session).await {
            Some(parent) => (parent.root_session, parent.depth + 1),
            None => (parent_session.to_string(), 1),
        }
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
        let (root_session, depth) = self.lineage(parent_session).await;
        if depth > MAX_DEPTH {
            return Err(HarnessError::Other(format!(
                "delegation depth limit reached: this session is already at depth {depth} (max {MAX_DEPTH})"
            )));
        }
        let name = match &req.name {
            Some(n) => {
                if !valid_name(n) {
                    return Err(HarnessError::Other(format!(
                        "`{n}` is not a valid agent name: use 1-32 Chinese characters, letters, digits or hyphens, starting with a Chinese character or letter (and not `lead`)"
                    )));
                }
                n.clone()
            }
            // 模型漏填 name 时：先从中文标签提取，提取不出再退 agent-N。
            None => match name_from_label(&req.label) {
                Some(n) => {
                    let free = self.catalog.next_free_name(&root_session).await;
                    // 提取名与 agent-N 同名时归一到空闲名，避免撞车
                    if n == free {
                        free
                    } else if self.catalog.get(&n).await.is_none() {
                        n
                    } else {
                        format!("{n}-{}", &free[free.rfind('-').map(|i| i + 1).unwrap_or(0)..])
                    }
                }
                None => self.catalog.next_free_name(&root_session).await,
            },
        };

        // The child gets its own session so its history is isolated from the parent.
        // Use the shared store when we have one: a private instance has its own
        // cache, which would not see the child's later writes.
        let shared_store = self.store.clone();
        let store = shared_store.clone().unwrap_or_else(|| {
            Arc::new(crate::session::SessionStore::new())
        });
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
            parent_session: parent_session.to_string(),
            root_session: root_session.clone(),
            depth,
            started_at: crate::event::now_ms().to_string(),
            ended_at: None,
            result: None,
            error: None,
            tool_calls: 0,
        };
        self.catalog.add(child.clone()).await;
        // 完成通道：先建好再放子代理，避免"子代理已完成但等待者还没注册"。
        let (tx, _rx) = watch::channel(None);
        self.outcomes.write().await.insert(child_id.clone(), tx);
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
                            turn: String::new(),
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
        let child_opts: Option<crate::agent::TurnOptions> = match self.runtime.read().await.as_ref() {
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

        // 子 turn 启动：await 的只是 start_turn 的「派发」本身（实际轮在它
        // 内部的 spawned task 里跑），这里不持有 opaque future，无 E0391。
        let started = if let Some(opts) = child_opts.clone() {
            starter.start_child_opts(&child_session, &req.task, opts).await
        } else {
            starter.start_child(&child_session, &req.task, &req.model).await
        };
        let shared_store = self.store.clone().unwrap_or_else(|| {
            Arc::new(crate::session::SessionStore::new())
        });
        self.spawn_watcher(
            child_id.clone(),
            name.clone(),
            req.label.clone(),
            parent_session.to_string(),
            child_session.clone(),
            cancel,
            req.delivery.unwrap_or(Delivery::Parent),
            started,
            self.store.clone(),
        );

        Ok(child_id)
    }

    /// 子代理收尾监视器：轮询子 turn 状态，收尾后更新目录、唤醒等待者
    /// （agent_wait）、向父会话投递结果。spawn 与 say（续聊）共用。
    ///
    /// `started` 携带启动阶段的 `Result`：start_turn 失败时子代理也要落到
    /// Failed 终态，而不是永远 Running。
    fn spawn_watcher(
        &self,
        cid: String,
        cname: String,
        label: String,
        parent_sid: String,
        child_sid: String,
        cancel: tokio_util::sync::CancellationToken,
        delivery: Delivery,
        started: Result<String, HarnessError>,
        shared_store: Option<crate::session::SharedStore>,
    ) {
        let cat = self.catalog.clone();
        let hub = self.hub.clone();
        let running = self.running.clone();
        let runtime = self.runtime.clone();
        let outcomes = self.outcomes.clone();
        tokio::spawn(async move {
            let res = started;
            // A child turn is a normal turn on its own session.
            // 一律经由 trait 对象（dyn TurnStarter）启动，保持 future 类型
            // 擦除：直接 await AgentRuntime::start_turn 会让它的 opaque
            // future 经 tokio::spawn 的 Send 检查递归套回自身（E0391）。
            // 子 turn 已由调用方（spawn / say）启动；这里只负责监视收尾。

            // 等子 turn 收尾。注入 runtime 后以 `is_running` 为准——流式输出
            // 中途的 assistant 消息也可能带非空 Text，不能只看最后一条消息。
            const POLL_MS: u64 = 500;
            // 1800s 上限：子代理常被派去干长活（大型重构/多文件检索），
            // 300s 会把正当运行中的任务误杀成 Failed。仍保留上限防孤儿。
            const MAX_POLLS: u64 = 3600;
            let mut final_text = String::new();
            let mut tool_calls_seen: u64 = 0;
            let mut timed_out = false;
            let mut polls = 0u64;
            // 轮询用**共享** store：另起一个实例会带上自己的缓存，而缓存不会
            // 随磁盘变化失效，于是永远读到子会话创建时的空快照。
            let poll_store = shared_store.clone().unwrap_or_else(|| {
                Arc::new(crate::session::SessionStore::new())
            });
            loop {
                if cancel.is_cancelled() {
                    // 父侧取消：同时终止子 turn，避免留下孤儿轮继续跑。
                    if let Some(rt) = runtime.read().await.as_ref() {
                        rt.interrupt(&child_sid).await;
                    }
                    break;
                }
                if polls >= MAX_POLLS {
                    timed_out = true;
                    break;
                }
                polls += 1;
                let msgs = poll_store.neutral_messages(&child_sid).await;
                // 统计子会话消息里的 ToolUse parts 数，更新目录计数。
                let tc = count_tool_uses(&msgs);
                if tc != tool_calls_seen {
                    tool_calls_seen = tc;
                    cat.update(&cid, |c| c.tool_calls = tc).await;
                }
                match runtime.read().await.as_ref() {
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
                if let Some(rt) = runtime.read().await.as_ref() {
                    rt.interrupt(&child_sid).await;
                    for _ in 0..20 {
                        if !rt.is_running(&child_sid).await {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
                (ChildStatus::Failed, Some(format!("child agent timed out after {}s", MAX_POLLS * POLL_MS / 1000)))
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

            // 唤醒所有等待者（agent_wait）：终态经通道广播，等待者立刻返回。
            let outcome = ChildOutcome {
                status,
                result: summary.clone(),
                error: err.clone(),
            };
            if let Some(tx) = outcomes.read().await.get(&cid) {
                let _ = tx.send(Some(outcome));
            }

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
                // 把结果写进父会话的历史，父代理下一轮就能看到；本轮内要立刻
                // 看到则用 agent_wait（它把结果作为 tool_result 回填给模型）。
                let text = format!(
                    "[subagent `{}` (task: {}) finished]\n\n{}",
                    cname,
                    label,
                    payload["result"].as_str().unwrap_or_default()
                );
                let deliver_store = shared_store.clone().unwrap_or_else(|| {
                    Arc::new(crate::session::SessionStore::new())
                });
                let _ = deliver_store
                    .append_message(
                        &parent_sid,
                        crate::session::StoredMessage {
                            role: "user".into(),
                            parts: json!({"User": [{"Text": text}]}),
                            displays: Vec::new(),
                            turn: String::new(),
                            created_at: crate::event::now_ms(),
                        },
                    )
                    .await;
                hub.publish(
                    &parent_sid,
                    "subagent.result",
                    json!({
                        "agentId": cid, "name": cname, "label": label,
                        "result": payload["result"], "error": err,
                    }),
                );
            }
        });
    }

    /// 向子代理说话。运行中 → 注入 steering（下一个模型步边界生效）；
    /// 已收尾 → 在其子会话上续开新轮（历史完整保留），目录状态复活并
    /// 重挂收尾监视器。父会话收到 `subagent.say` 事件。
    ///
    /// 带附件时 steer 自动退化为 queue（本轮结束后作为下一轮送达）——
    /// enqueue_input 内部处理该退化与立即 drain，附件与 opts 全程透传。
    ///
    /// `agent` 接受子代理 id 或其子会话 id（前端手头只有会话 id）。
    pub async fn say(
        &self,
        agent: &str,
        text: &str,
        attachments: Vec<crate::agent::Attachment>,
        opts: crate::agent::TurnOptions,
    ) -> Result<(String, String, String), HarnessError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(HarnessError::Other("text must not be empty".into()));
        }
        let Some(child) = self.resolve_any(agent).await else {
            return Err(HarnessError::Other(format!("no such agent: {agent}")));
        };
        let Some(rt) = self.runtime.read().await.clone() else {
            return Err(HarnessError::Other("agent runtime not ready".into()));
        };
        let sid = child.session_id.clone();
        let was_running = matches!(child.status, ChildStatus::Running) && rt.is_running(&sid).await;
        // steer 失效（子 turn 恰好刚结束）会自动升级为 queue 并立刻 drain 成
        // 新轮 —— enqueue_input 内部已处理该竞态。
        let item = rt.enqueue_input(&sid, text, attachments, opts, "steer").await?;
        if !was_running {
            self.revive(&child).await;
        }
        self.hub.publish(
            &child.parent_session,
            "subagent.say",
            json!({"agentId": child.id, "name": child.name, "label": child.label, "text": text}),
        );
        Ok((child.id, sid, item.kind))
    }

    /// 按 id 或子会话 id 解析子代理。
    async fn resolve_any(&self, agent: &str) -> Option<Child> {
        if let Some(c) = self.catalog.get(agent).await {
            return Some(c);
        }
        self.catalog.find_by_session(agent).await
    }

    /// 复活已收尾的子代理：清终态、重挂完成通道与取消令牌、重启监视器。
    async fn revive(&self, child: &Child) {
        let cancel = tokio_util::sync::CancellationToken::new();
        self.running
            .lock()
            .await
            .insert(child.id.clone(), cancel.clone());
        let (tx, _rx) = watch::channel(None);
        self.outcomes.write().await.insert(child.id.clone(), tx);
        self.catalog.update(&child.id, |c| {
            c.status = ChildStatus::Running;
            c.ended_at = None;
            c.error = None;
        })
        .await;
        self.hub.publish(
            &child.parent_session,
            "subagent.start",
            json!({"agentId": child.id, "name": child.name, "label": child.label}),
        );
        let shared_store = self.store.clone().unwrap_or_else(|| {
            Arc::new(crate::session::SessionStore::new())
        });
        self.spawn_watcher(
            child.id.clone(),
            child.name.clone(),
            child.label.clone(),
            child.parent_session.clone(),
            child.session_id.clone(),
            cancel,
            Delivery::Parent,
            Ok(String::new()),
            self.store.clone(),
        );
    }

    /// Wait for one child of `parent_session`'s tree to finish.
    ///
    /// Returns the child's outcome, or `None` if the timeout elapsed first.
    pub async fn wait_for(
        &self,
        id: &str,
        timeout: std::time::Duration,
    ) -> Option<ChildOutcome> {
        let mut rx = {
            let map = self.outcomes.read().await;
            map.get(id)?.subscribe()
        };
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // 先看当前值再看是否变化：watch 保留最新值，不会漏掉"已完成"。
            if let Some(outcome) = rx.borrow().clone() {
                return Some(outcome);
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            if tokio::time::timeout(remaining, rx.changed()).await.is_err() {
                return None;
            }
        }
    }

    /// The child id addressed by `ref` — a catalog id, an agent name, or a
    /// child session id, restricted to the tree rooted at `root_session`.
    pub async fn resolve_child(&self, root_session: &str, r: &str) -> Option<Child> {
        let tree = self.catalog.descendants_of(root_session).await;
        if let Some(c) = tree.iter().find(|c| c.id == r) {
            return Some(c.clone());
        }
        if let Some(c) = tree.iter().find(|c| c.name == r) {
            return Some(c.clone());
        }
        tree.iter().find(|c| c.session_id == r).cloned()
    }

    pub async fn interrupt(&self, child_id: &str) -> bool {
        let c = self.running.lock().await.get(child_id).cloned();
        if let Some(c) = c {
            c.cancel();
            // 同步取消子会话的 turn：只停轮询不停子 turn 会留下孤儿轮继续跑。
            if let Some(ch) = self.catalog.get(child_id).await {
                if let Some(rt) = self.runtime.read().await.as_ref() {
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
            // 唤醒等待者：被中断的子代理不该让 agent_wait 一直挂到超时。
            let outcome = ChildOutcome {
                status: ChildStatus::Cancelled,
                result: String::new(),
                error: Some("cancelled by parent".to_string()),
            };
            if let Some(tx) = self.outcomes.read().await.get(child_id) {
                let _ = tx.send(Some(outcome));
            }
            true
        } else {
            false
        }
    }

    pub async fn list(&self) -> Vec<Child> {
        self.catalog.list().await
    }

    /// Every descendant of one tree root.
    pub async fn tree(&self, root_session: &str) -> Vec<Child> {
        self.catalog.descendants_of(root_session).await
    }
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
        assert!(valid_name("文档整理师"), "Chinese names are the expected form");
        assert!(valid_name("统计员2"));
        assert!(valid_name("Agent"), "letters of any case are fine now");
        assert!(!valid_name("lead"), "reserved for the root agent");
        assert!(!valid_name(""), "empty");
        assert!(!valid_name("统计 员"), "spaces rejected");
        assert!(!valid_name("-统计"), "must start with a letter/CJK char");
        assert!(!valid_name(&"名".repeat(33)), "over 32 chars");
        assert!(!valid_name("1abc"), "must start with a letter");
        assert!(!valid_name("a b"), "no spaces");
        assert!(!valid_name(&"a".repeat(33)), "max 32");
    }

    #[tokio::test]
    async fn catalog_allocates_free_names() {
        let cat = ChildCatalog::new();
        assert_eq!(cat.next_free_name("s1").await, "agent-1");
        cat.add(Child {
            id: "x".into(),
            name: "agent-1".into(),
            label: "l".into(),
            task: "t".into(),
            status: ChildStatus::Running,
            session_id: "s".into(),
            parent_session: "s1".into(),
            root_session: "s1".into(),
            depth: 1,
            started_at: "0".into(),
            ended_at: None,
            result: None,
            error: None,
            tool_calls: 0,
        })
        .await;
        assert_eq!(cat.next_free_name("s1").await, "agent-2");
        assert_eq!(cat.list().await.len(), 1);
    }

    #[tokio::test]
    async fn names_are_scoped_per_tree() {
        let cat = ChildCatalog::new();
        // Two independent trees may each have an `agent-1`.
        assert_eq!(cat.next_free_name("root-a").await, "agent-1");
        assert_eq!(cat.next_free_name("root-b").await, "agent-1");
    }
    #[tokio::test]
    async fn descendants_follow_the_root() {
        let cat = ChildCatalog::new();
        let mk = |id: &str, parent: &str, root: &str, depth: u32| Child {
            id: id.into(),
            name: id.into(),
            label: "l".into(),
            task: "t".into(),
            status: ChildStatus::Running,
            session_id: format!("{id}-sess"),
            parent_session: parent.into(),
            root_session: root.into(),
            depth,
            started_at: "0".into(),
            ended_at: None,
            result: None,
            error: None,
            tool_calls: 0,
        };
        cat.add(mk("c1", "root", "root", 1)).await;
        cat.add(mk("c2", "c1-sess", "root", 2)).await;
        cat.add(mk("other", "root2", "root2", 1)).await;

        let tree = cat.descendants_of("root").await;
        assert_eq!(tree.len(), 2, "the grandchild and the child, not the other tree");
        assert!(tree.iter().all(|c| c.root_session == "root"));

        assert_eq!(cat.children_of("root").await.len(), 1);
        assert_eq!(cat.children_of("c1-sess").await.len(), 1);
        assert_eq!(cat.find_by_session("c2-sess").await.unwrap().id, "c2");
    }

    #[tokio::test]
    async fn lineage_tracks_nesting() {
        let svc = SubagentService::new(EventHub::new());
        // An unknown session is a tree root.
        assert_eq!(svc.lineage("s_root").await, ("s_root".to_string(), 1));
        svc.catalog
            .add(Child {
                id: "c1".into(),
                name: "agent-1".into(),
                label: "l".into(),
                task: "t".into(),
                status: ChildStatus::Running,
                session_id: "child_sess".into(),
                parent_session: "s_root".into(),
                root_session: "s_root".into(),
                depth: 1,
                started_at: "0".into(),
                ended_at: None,
                result: None,
                error: None,
                tool_calls: 0,
            })
            .await;
        // A child of that child is depth 2, same root.
        assert_eq!(
            svc.lineage("child_sess").await,
            ("s_root".to_string(), 2)
        );
    }

    #[tokio::test]
    async fn resolve_child_accepts_id_name_and_session() {
        let svc = SubagentService::new(EventHub::new());
        svc.catalog
            .add(Child {
                id: "sa_1".into(),
                name: "researcher".into(),
                label: "l".into(),
                task: "t".into(),
                status: ChildStatus::Running,
                session_id: "sess-1".into(),
                parent_session: "root".into(),
                root_session: "root".into(),
                depth: 1,
                started_at: "0".into(),
                ended_at: None,
                result: None,
                error: None,
                tool_calls: 0,
            })
            .await;
        assert_eq!(
            svc.resolve_child("root", "sa_1").await.unwrap().name,
            "researcher"
        );
        assert_eq!(
            svc.resolve_child("root", "researcher").await.unwrap().id,
            "sa_1"
        );
        assert_eq!(
            svc.resolve_child("root", "sess-1").await.unwrap().id,
            "sa_1"
        );
        // A different tree cannot address it.
        assert!(svc.resolve_child("other-root", "researcher").await.is_none());
    }

    #[tokio::test]
    async fn wait_returns_the_settled_outcome() {
        let svc = Arc::new(SubagentService::new(EventHub::new()));
        let (tx, _rx) = watch::channel(None);
        svc.outcomes.write().await.insert("c1".into(), tx.clone());
        let svc2 = svc.clone();
        let waiter = tokio::spawn(async move {
            svc2.wait_for("c1", std::time::Duration::from_secs(5)).await
        });
        // Let the waiter subscribe before the outcome lands.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tx.send(Some(ChildOutcome {
            status: ChildStatus::Done,
            result: "the answer".into(),
            error: None,
        }))
        .unwrap();
        let outcome = waiter.await.unwrap().expect("outcome");
        assert_eq!(outcome.result, "the answer");
        assert_eq!(outcome.status, ChildStatus::Done);
    }

    #[tokio::test]
    async fn wait_times_out_cleanly() {
        let svc = SubagentService::new(EventHub::new());
        let (tx, _rx) = watch::channel(None);
        svc.outcomes.write().await.insert("c1".into(), tx);
        let out = svc
            .wait_for("c1", std::time::Duration::from_millis(100))
            .await;
        assert!(out.is_none(), "timeout returns None, never hangs");
    }

    #[tokio::test]
    async fn wait_on_an_unknown_child_returns_none() {
        let svc = SubagentService::new(EventHub::new());
        assert!(svc
            .wait_for("nope", std::time::Duration::from_millis(50))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn spawn_beyond_the_depth_cap_is_refused() {
        let svc = SubagentService::new(EventHub::new());
        // A starter that never runs: the spawn call itself is what we test.
        struct NoopStarter;
        #[async_trait::async_trait]
        impl crate::tools::TurnStarter for NoopStarter {
            async fn start_child(
                &self,
                _session_id: &str,
                _prompt: &str,
                _model: &str,
            ) -> Result<String, HarnessError> {
                Ok("t".into())
            }
        }
        let starter: Arc<dyn crate::tools::TurnStarter> = Arc::new(NoopStarter);

        // Walk the tree down to the cap.
        let mut parent = "root".to_string();
        for depth in 1..=crate::subagent::MAX_DEPTH {
            let id = svc
                .spawn(
                    starter.clone(),
                    &parent,
                    ".",
                    SpawnRequest {
                        label: format!("l{depth}"),
                        name: None,
                        task: "t".into(),
                        cwd: None,
                        seed_history: false,
                        model: String::new(),
                        delivery: Some(Delivery::Parent),
                    },
                )
                .await
                .expect("spawn inside the cap must work");
            let child = svc.catalog.get(&id).await.expect("child exists");
            assert_eq!(child.depth, depth);
            assert_eq!(child.root_session, "root");
            parent = child.session_id;
        }
        // One more level is refused with a clear message.
        let err = svc
            .spawn(
                starter,
                &parent,
                ".",
                SpawnRequest {
                    label: "too deep".into(),
                    name: None,
                    task: "t".into(),
                    cwd: None,
                    seed_history: false,
                    model: String::new(),
                    delivery: Some(Delivery::Parent),
                },
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("depth limit"), "got: {err}");
    }

    #[test]
    fn delivery_round_trips() {
        let d: Delivery = serde_json::from_str("\"parent\"").unwrap();
        assert_eq!(d, Delivery::Parent);
        let d: Delivery = serde_json::from_str("\"caller\"").unwrap();
        assert_eq!(d, Delivery::Caller);
    }
}
