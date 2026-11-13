//! Agent Teams: a shared task board for one root Session and its subagents.
//!
//! Design follows DeepSeek Harness's `team` subsystem (MIT, deepseek-ai/deepseek-harness)
//! as a behavioural reference — specifically the shared task DAG, the compare-and-set
//! `revision` on every mutation, the Lead-or-owner authorisation rule, and the
//! advisory (never locking) `writeScopes` overlap warning.
//! See `docs/subsystems/agent-team.md` upstream for the original contract.
//!
//! Deliberate simplifications in this port, stated plainly:
//! - The board lives inside the root Session's own JSON file, not in a separate
//!   event log with a replay projection. Parable's `SessionStore` persists whole
//!   files, so a snapshot per mutation is the honest fit.
//! - One board per Session, and a Session's board is the board of the tree rooted
//!   at it. Children reach the board through their root, never a second copy.
//! - `team/member` roster records and the historical `team/message/*` mailbox are
//!   not ported: teammates *are* subagents, so the subagent catalogue is the roster.

use crate::error::HarnessError;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Maximum non-deleted tasks retained by one board.
pub const MAX_TASKS: usize = 256;

/// Maximum length of a task subject, in characters.
const MAX_SUBJECT: usize = 200;

/// Maximum length of a task description, in characters.
const MAX_DESCRIPTION: usize = 16_384;

/// Durable task lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Unstarted, or released back to the pool.
    Pending,
    /// Claimed by a member and being worked on.
    InProgress,
    /// Finished; satisfies the tasks it blocks.
    Completed,
    /// A retained tombstone. Blocks nothing, and is never listed.
    Deleted,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Completed => "completed",
            TaskStatus::Deleted => "deleted",
        }
    }
}

/// Supported task mutation actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskAction {
    Claim,
    Release,
    Edit,
    SetDependencies,
    Complete,
    Reopen,
    Reassign,
    Delete,
}

impl TaskAction {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "claim" => TaskAction::Claim,
            "release" => TaskAction::Release,
            "edit" => TaskAction::Edit,
            "set_dependencies" => TaskAction::SetDependencies,
            "complete" => TaskAction::Complete,
            "reopen" => TaskAction::Reopen,
            "reassign" => TaskAction::Reassign,
            "delete" => TaskAction::Delete,
            _ => return None,
        })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            TaskAction::Claim => "claim",
            TaskAction::Release => "release",
            TaskAction::Edit => "edit",
            TaskAction::SetDependencies => "set_dependencies",
            TaskAction::Complete => "complete",
            TaskAction::Reopen => "reopen",
            TaskAction::Reassign => "reassign",
            TaskAction::Delete => "delete",
        }
    }
}

/// One whole task snapshot. Every mutation increments `revision`, which is the
/// compare-and-set precondition a caller must present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamTask {
    pub id: String,
    pub revision: u64,
    pub subject: String,
    pub description: String,
    pub status: TaskStatus,
    /// Session id of the member that owns the task.
    #[serde(default)]
    pub owner_id: Option<String>,
    /// Agent name captured at assignment time.
    #[serde(default)]
    pub owner_name: Option<String>,
    /// Task ids that must complete before this one may be claimed.
    #[serde(default)]
    pub blocked_by: Vec<String>,
    /// Advisory workspace-relative path prefixes this task expects to modify.
    #[serde(default)]
    pub write_scopes: Vec<String>,
}

/// The whole board for one tree, persisted with the root Session.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TeamState {
    /// Whether team mode is on for this tree.
    #[serde(default)]
    pub enabled: bool,
    /// Next task number to hand out. Ids are `task-<n>`.
    #[serde(default)]
    pub next_task_number: u64,
    /// Every task ever created, including deleted tombstones.
    #[serde(default)]
    pub tasks: Vec<TeamTask>,
}

impl TeamState {
    pub fn new() -> Self {
        Self {
            enabled: true,
            next_task_number: 1,
            tasks: Vec::new(),
        }
    }

    /// The live (non-deleted) tasks, in creation order.
    pub fn live_tasks(&self) -> Vec<&TeamTask> {
        self.tasks
            .iter()
            .filter(|t| t.status != TaskStatus::Deleted)
            .collect()
    }

    pub fn find(&self, id: &str) -> Option<&TeamTask> {
        self.tasks.iter().find(|t| t.id == id)
    }

    fn find_mut(&mut self, id: &str) -> Option<&mut TeamTask> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }
}

/// A task plus the derived fields the UI and the model need.
///
/// `ready` is true when the task is pending and every blocker completed.
/// `write_scope_warnings` names the other in-progress tasks whose advisory write
/// scopes overlap this one's — a hint, never a lock.
#[derive(Debug, Clone, Serialize)]
pub struct TaskView {
    pub id: String,
    pub revision: u64,
    pub subject: String,
    pub description: String,
    pub status: TaskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_name: Option<String>,
    pub blocked_by: Vec<String>,
    pub write_scopes: Vec<String>,
    pub ready: bool,
    pub write_scope_warnings: Vec<String>,
}

// ---------------------------------------------------------------- validation

/// Normalise one required, human-authored string.
fn required_text(value: &str, field: &str, max_len: usize) -> Result<String, HarnessError> {
    let text = value.trim();
    if text.is_empty() {
        return Err(HarnessError::Tool(format!("{field} must be non-empty")));
    }
    if text.chars().count() > max_len {
        return Err(HarnessError::Tool(format!(
            "{field} exceeds {max_len} characters"
        )));
    }
    Ok(text.to_string())
}

/// Normalise one workspace-relative write-scope prefix.
///
/// Advisory only: this is a hint to other members about which files a task
/// expects to touch, not an access control decision.
pub fn normalize_scope(value: &str) -> Result<String, HarnessError> {
    let normalized = value
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_string();
    let segments: Vec<&str> = normalized.split('/').collect();
    if normalized.is_empty()
        || normalized.starts_with('/')
        || has_drive_prefix(&normalized)
        || segments.iter().any(|s| s.is_empty() || *s == "." || *s == "..")
    {
        return Err(HarnessError::Tool(format!(
            "invalid workspace-relative write scope `{value}`"
        )));
    }
    Ok(normalized)
}

/// Whether a path prefix carries a Windows drive letter (`c:/…`).
fn has_drive_prefix(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Whether two normalised path prefixes overlap on path components.
fn scopes_overlap(left: &str, right: &str) -> bool {
    left == right || left.starts_with(&format!("{right}/")) || right.starts_with(&format!("{left}/"))
}

/// The reason a task graph is invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphViolation {
    /// A blocker names a task that does not exist, or was deleted.
    Missing,
    /// A task lists the same blocker twice.
    Duplicate,
    /// The dependency edges form a cycle.
    Cycle,
}

/// Validate the whole active task graph after replacing one candidate task.
///
/// Ported from DSH's `assertTaskGraphCandidate`: a blocker must name a live
/// task, must not repeat, must not be the task itself, and the edges must stay
/// acyclic.
pub fn assert_task_graph(tasks: &[TeamTask], candidate: &TeamTask) -> Result<(), GraphViolation> {
    let mut map: std::collections::HashMap<&str, &TeamTask> = tasks
        .iter()
        .map(|t| (t.id.as_str(), t))
        .collect();
    map.insert(candidate.id.as_str(), candidate);

    for task in map.values() {
        if task.status == TaskStatus::Deleted {
            continue;
        }
        let mut seen: HashSet<&str> = HashSet::new();
        for blocker in &task.blocked_by {
            if blocker == &task.id {
                return Err(GraphViolation::Cycle);
            }
            if !seen.insert(blocker.as_str()) {
                return Err(GraphViolation::Duplicate);
            }
            match map.get(blocker.as_str()) {
                None => return Err(GraphViolation::Missing),
                Some(b) if b.status == TaskStatus::Deleted => return Err(GraphViolation::Missing),
                Some(_) => {}
            }
        }
    }

    // Cycle detection: colour the graph with a visiting/visited pair.
    let mut visiting: HashSet<String> = HashSet::new();
    let mut visited: HashSet<String> = HashSet::new();
    for id in map.keys() {
        if visit(id, &map, &mut visiting, &mut visited) {
            return Err(GraphViolation::Cycle);
        }
    }
    Ok(())
}

/// Depth-first cycle check over the blocker edges.
fn visit(
    id: &str,
    map: &std::collections::HashMap<&str, &TeamTask>,
    visiting: &mut HashSet<String>,
    visited: &mut HashSet<String>,
) -> bool {
    if visiting.contains(id) {
        return true;
    }
    if visited.contains(id) {
        return false;
    }
    let Some(task) = map.get(id) else { return false };
    if task.status == TaskStatus::Deleted {
        return false;
    }
    visiting.insert(id.to_string());
    for blocker in &task.blocked_by {
        if visit(blocker, map, visiting, visited) {
            return true;
        }
    }
    visiting.remove(id);
    visited.insert(id.to_string());
    false
}

/// Whether every blocker of `task` has completed.
pub fn task_ready(state: &TeamState, task: &TeamTask) -> bool {
    task.blocked_by.iter().all(|id| {
        state
            .find(id)
            .map(|t| t.status == TaskStatus::Completed)
            .unwrap_or(false)
    })
}

/// Derive the view of one task: readiness plus advisory write-scope overlaps.
pub fn project_task_view(state: &TeamState, task: &TeamTask) -> TaskView {
    let mut warnings = Vec::new();
    for other in &state.tasks {
        if other.id == task.id || other.status != TaskStatus::InProgress {
            continue;
        }
        if task
            .write_scopes
            .iter()
            .any(|l| other.write_scopes.iter().any(|r| scopes_overlap(l, r)))
        {
            warnings.push(format!("write scopes overlap with {}", other.id));
        }
    }
    TaskView {
        id: task.id.clone(),
        revision: task.revision,
        subject: task.subject.clone(),
        description: task.description.clone(),
        status: task.status,
        owner_id: task.owner_id.clone(),
        owner_name: task.owner_name.clone(),
        blocked_by: task.blocked_by.clone(),
        write_scopes: task.write_scopes.clone(),
        ready: task.status == TaskStatus::Pending && task_ready(state, task),
        write_scope_warnings: warnings,
    }
}

// ---------------------------------------------------------------- requests

/// Input for creating one shared task.
#[derive(Debug, Clone, Default)]
pub struct CreateTask {
    pub subject: String,
    pub description: String,
    pub blocked_by: Vec<String>,
    pub write_scopes: Vec<String>,
}

/// A compare-and-set mutation of one shared task.
#[derive(Debug, Clone)]
pub struct UpdateTask {
    pub task_id: String,
    /// The revision the caller believes is current. A mismatch is rejected.
    pub expected_revision: u64,
    pub action: TaskAction,
    pub subject: Option<String>,
    pub description: Option<String>,
    pub blocked_by: Option<Vec<String>>,
    pub write_scopes: Option<Vec<String>>,
    /// Session id of the new owner, for `reassign`.
    pub owner: Option<String>,
    /// Agent name to record alongside the new owner.
    pub owner_name: Option<String>,
}

/// Who is asking, and in what capacity.
#[derive(Debug, Clone)]
pub struct Caller {
    /// The caller's session id.
    pub session_id: String,
    /// The caller's agent name, recorded on tasks it claims.
    pub name: String,
    /// The root session of the tree the caller belongs to.
    pub root_id: String,
    /// Whether the caller *is* the root (the Lead).
    pub is_lead: bool,
}

/// The outcome of a board mutation.
#[derive(Debug, Clone)]
pub enum Mutation {
    /// The task was created.
    Created(TaskView),
    /// The task was updated.
    Updated(TaskView),
}

/// Apply one create request to a board, returning the new task.
///
/// The caller owns the board lock; this function is pure with respect to the
/// board's history and only appends.
pub fn create_task(
    state: &mut TeamState,
    caller: &Caller,
    req: &CreateTask,
) -> Result<TaskView, HarnessError> {
    if !state.enabled {
        return Err(HarnessError::Other(
            "agent teams are not enabled for this session".into(),
        ));
    }
    let live = state.live_tasks().len();
    if live >= MAX_TASKS {
        return Err(HarnessError::Other(format!(
            "team task limit {MAX_TASKS} reached"
        )));
    }
    let id = format!("task-{}", state.next_task_number);
    if state.find(&id).is_some() {
        return Err(HarnessError::Other("team task id space exhausted".into()));
    }
    let subject = required_text(&req.subject, "subject", MAX_SUBJECT)?;
    let description = required_text(&req.description, "description", MAX_DESCRIPTION)?;
    let blocked_by = normalize_blockers(&req.blocked_by, state, None)?;
    let write_scopes = normalize_scopes(&req.write_scopes)?;

    let task = TeamTask {
        id,
        revision: 1,
        subject,
        description,
        status: TaskStatus::Pending,
        owner_id: None,
        owner_name: None,
        blocked_by,
        write_scopes,
    };
    assert_task_graph(&state.tasks, &task).map_err(graph_error)?;
    state.next_task_number += 1;
    let view = project_task_view(state, &task);
    state.tasks.push(task);
    let _ = caller;
    Ok(view)
}

/// Apply one compare-and-set mutation to a board.
pub fn update_task(
    state: &mut TeamState,
    caller: &Caller,
    req: &UpdateTask,
) -> Result<TaskView, HarnessError> {
    if !state.enabled {
        return Err(HarnessError::Other(
            "agent teams are not enabled for this session".into(),
        ));
    }
    let current = state
        .find(&req.task_id)
        .cloned()
        .ok_or_else(|| HarnessError::NotFound(format!("team task `{}`", req.task_id)))?;
    if current.revision != req.expected_revision {
        return Err(HarnessError::Other(format!(
            "stale team task `{}` revision {}; current revision is {}",
            current.id, req.expected_revision, current.revision
        )));
    }
    if current.status == TaskStatus::Deleted {
        return Err(HarnessError::Other(format!(
            "team task `{}` is deleted",
            current.id
        )));
    }

    let lead = caller.is_lead;
    let owner = current.owner_id.as_deref() == Some(caller.session_id.as_str());
    let authorize_owner = || -> Result<(), HarnessError> {
        if !lead && !owner {
            return Err(HarnessError::PermissionDenied(
                "task mutation requires its owner or the team lead".into(),
            ));
        }
        Ok(())
    };

    let mut next = current.clone();
    match req.action {
        TaskAction::Claim => {
            if current
                .owner_id
                .as_ref()
                .map(|o| o != &caller.session_id)
                .unwrap_or(false)
            {
                return Err(HarnessError::Other(format!(
                    "team task `{}` is owned by another member",
                    current.id
                )));
            }
            if current.status != TaskStatus::Pending || !task_ready(state, &current) {
                return Err(HarnessError::Other(format!(
                    "team task `{}` is not ready to claim",
                    current.id
                )));
            }
            next.status = TaskStatus::InProgress;
            next.owner_id = Some(caller.session_id.clone());
            next.owner_name = Some(caller.name.clone());
        }
        TaskAction::Release => {
            authorize_owner()?;
            if current.status != TaskStatus::InProgress {
                return Err(HarnessError::Other(
                    "only an in-progress task can be released".into(),
                ));
            }
            next.status = TaskStatus::Pending;
            next.owner_id = None;
            next.owner_name = None;
        }
        TaskAction::Edit => {
            authorize_owner()?;
            if req.subject.is_none() && req.description.is_none() && req.write_scopes.is_none() {
                return Err(HarnessError::Tool(
                    "task edit requires subject, description, or write_scopes".into(),
                ));
            }
            if let Some(s) = &req.subject {
                next.subject = required_text(s, "subject", MAX_SUBJECT)?;
            }
            if let Some(d) = &req.description {
                next.description = required_text(d, "description", MAX_DESCRIPTION)?;
            }
            if let Some(w) = &req.write_scopes {
                next.write_scopes = normalize_scopes(w)?;
            }
        }
        TaskAction::SetDependencies => {
            authorize_owner()?;
            let blockers = req.blocked_by.as_ref().ok_or_else(|| {
                HarnessError::Tool("set_dependencies requires blocked_by".into())
            })?;
            next.blocked_by = normalize_blockers(blockers, state, Some(&current.id))?;
        }
        TaskAction::Complete => {
            authorize_owner()?;
            if current.status != TaskStatus::InProgress {
                return Err(HarnessError::Other(
                    "only an in-progress task can complete".into(),
                ));
            }
            next.status = TaskStatus::Completed;
        }
        TaskAction::Reopen => {
            authorize_owner()?;
            if current.status != TaskStatus::Completed {
                return Err(HarnessError::Other(
                    "only a completed task can reopen".into(),
                ));
            }
            next.status = TaskStatus::Pending;
            next.owner_id = None;
            next.owner_name = None;
        }
        TaskAction::Reassign => {
            if !lead {
                return Err(HarnessError::PermissionDenied(
                    "only the team lead can reassign tasks".into(),
                ));
            }
            if current.status != TaskStatus::Pending && current.status != TaskStatus::InProgress {
                return Err(HarnessError::Other(
                    "only a pending or in-progress task can be reassigned".into(),
                ));
            }
            match &req.owner {
                None => {
                    next.status = TaskStatus::Pending;
                    next.owner_id = None;
                    next.owner_name = None;
                }
                Some(owner) => {
                    if !task_ready(state, &current) {
                        return Err(HarnessError::Other(format!(
                            "team task `{}` is blocked",
                            current.id
                        )));
                    }
                    next.status = TaskStatus::InProgress;
                    next.owner_id = Some(owner.clone());
                    next.owner_name = req.owner_name.clone();
                }
            }
        }
        TaskAction::Delete => {
            authorize_owner()?;
            let dependent = state.tasks.iter().find(|t| {
                t.status != TaskStatus::Deleted
                    && t.id != current.id
                    && t.blocked_by.contains(&current.id)
            });
            if let Some(d) = dependent {
                return Err(HarnessError::Other(format!(
                    "team task `{}` still blocks `{}`",
                    current.id, d.id
                )));
            }
            next.status = TaskStatus::Deleted;
        }
    }
    next.revision = current.revision + 1;
    assert_task_graph(&state.tasks, &next).map_err(graph_error)?;
    let view = project_task_view(state, &next);
    if let Some(slot) = state.find_mut(&current.id) {
        *slot = next;
    }
    Ok(view)
}

/// Normalise and de-duplicate blocker ids against the current graph.
fn normalize_blockers(
    values: &[String],
    state: &TeamState,
    self_id: Option<&str>,
) -> Result<Vec<String>, HarnessError> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for id in values {
        let id = id.trim();
        if id.is_empty() {
            return Err(HarnessError::Tool("blocker id must be non-empty".into()));
        }
        if Some(id) == self_id {
            return Err(HarnessError::Other(
                "a team task cannot block itself".into(),
            ));
        }
        if !seen.insert(id.to_string()) {
            return Err(HarnessError::Tool(format!("duplicate blocker `{id}`")));
        }
        match state.find(id) {
            Some(t) if t.status != TaskStatus::Deleted => {}
            _ => {
                return Err(HarnessError::NotFound(format!("blocker task `{id}`")));
            }
        }
        out.push(id.to_string());
    }
    Ok(out)
}

/// Normalise and de-duplicate write scopes.
fn normalize_scopes(values: &[String]) -> Result<Vec<String>, HarnessError> {
    let mut out: Vec<String> = Vec::new();
    for v in values {
        let s = normalize_scope(v)?;
        if !out.contains(&s) {
            out.push(s);
        }
    }
    Ok(out)
}

/// Map a graph violation onto a user-facing error.
fn graph_error(v: GraphViolation) -> HarnessError {
    match v {
        GraphViolation::Missing => HarnessError::Other(
            "a blocker names a task that does not exist or was deleted".into(),
        ),
        GraphViolation::Duplicate => HarnessError::Tool("a task lists the same blocker twice".into()),
        GraphViolation::Cycle => HarnessError::Other("the task dependency graph would have a cycle".into()),
    }
}

/// The whole board, rendered for the model and the UI.
pub fn board_view(state: &TeamState) -> Vec<TaskView> {
    state.live_tasks().iter().map(|t| project_task_view(state, t)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(id: &str, lead: bool) -> Caller {
        Caller {
            session_id: id.to_string(),
            name: id.to_string(),
            root_id: "root".into(),
            is_lead: lead,
        }
    }

    fn board() -> TeamState {
        TeamState::new()
    }

    #[test]
    fn create_appends_and_numbers_tasks() {
        let mut s = board();
        let v = create_task(
            &mut s,
            &caller("root", true),
            &CreateTask {
                subject: "写文档".into(),
                description: "写 README".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(v.id, "task-1");
        assert_eq!(v.revision, 1);
        assert_eq!(v.status, TaskStatus::Pending);
        assert!(v.ready);
        assert_eq!(s.tasks.len(), 1);
        assert_eq!(s.next_task_number, 2);
    }

    #[test]
    fn create_rejects_empty_subject() {
        let mut s = board();
        let e = create_task(
            &mut s,
            &caller("root", true),
            &CreateTask {
                subject: "  ".into(),
                description: "d".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(e.to_string().contains("subject"));
    }

    #[test]
    fn create_rejects_disabled_board() {
        let mut s = board();
        s.enabled = false;
        assert!(create_task(
            &mut s,
            &caller("root", true),
            &CreateTask {
                subject: "a".into(),
                description: "b".into(),
                ..Default::default()
            }
        )
        .is_err());
    }

    #[test]
    fn create_rejects_missing_blocker() {
        let mut s = board();
        let e = create_task(
            &mut s,
            &caller("root", true),
            &CreateTask {
                subject: "a".into(),
                description: "b".into(),
                blocked_by: vec!["task-99".into()],
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(e, HarnessError::NotFound(_)));
    }

    #[test]
    fn claim_requires_ready_task() {
        let mut s = board();
        create_task(
            &mut s,
            &caller("root", true),
            &CreateTask {
                subject: "a".into(),
                description: "b".into(),
                ..Default::default()
            },
        )
        .unwrap();
        create_task(
            &mut s,
            &caller("root", true),
            &CreateTask {
                subject: "c".into(),
                description: "d".into(),
                blocked_by: vec!["task-1".into()],
                ..Default::default()
            },
        )
        .unwrap();
        // task-2 is blocked by task-1, which is still pending.
        let e = update_task(
            &mut s,
            &caller("agent-1", false),
            &UpdateTask {
                task_id: "task-2".into(),
                expected_revision: 1,
                action: TaskAction::Claim,
                subject: None,
                description: None,
                blocked_by: None,
                write_scopes: None,
                owner: None,
                owner_name: None,
            },
        )
        .unwrap_err();
        assert!(e.to_string().contains("not ready"));
    }

    #[test]
    fn claim_records_owner_and_bumps_revision() {
        let mut s = board();
        create_task(
            &mut s,
            &caller("root", true),
            &CreateTask {
                subject: "a".into(),
                description: "b".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let v = update_task(
            &mut s,
            &caller("agent-1", false),
            &UpdateTask {
                task_id: "task-1".into(),
                expected_revision: 1,
                action: TaskAction::Claim,
                subject: None,
                description: None,
                blocked_by: None,
                write_scopes: None,
                owner: None,
                owner_name: None,
            },
        )
        .unwrap();
        assert_eq!(v.status, TaskStatus::InProgress);
        assert_eq!(v.revision, 2);
        assert_eq!(v.owner_id.as_deref(), Some("agent-1"));
        assert_eq!(v.owner_name.as_deref(), Some("agent-1"));
    }

    #[test]
    fn second_agent_cannot_claim_owned_task() {
        let mut s = board();
        create_task(
            &mut s,
            &caller("root", true),
            &CreateTask {
                subject: "a".into(),
                description: "b".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let claim = |rev: u64| UpdateTask {
            task_id: "task-1".into(),
            expected_revision: rev,
            action: TaskAction::Claim,
            subject: None,
            description: None,
            blocked_by: None,
            write_scopes: None,
            owner: None,
            owner_name: None,
        };
        update_task(&mut s, &caller("agent-1", false), &claim(1)).unwrap();
        // The task is now revision 2 and owned by agent-1.
        let e = update_task(&mut s, &caller("agent-2", false), &claim(2)).unwrap_err();
        assert!(e.to_string().contains("owned by another member"));
    }

    #[test]
    fn stale_revision_is_rejected() {
        let mut s = board();
        create_task(
            &mut s,
            &caller("root", true),
            &CreateTask {
                subject: "a".into(),
                description: "b".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let e = update_task(
            &mut s,
            &caller("root", true),
            &UpdateTask {
                task_id: "task-1".into(),
                expected_revision: 7,
                action: TaskAction::Complete,
                subject: None,
                description: None,
                blocked_by: None,
                write_scopes: None,
                owner: None,
                owner_name: None,
            },
        )
        .unwrap_err();
        assert!(e.to_string().contains("stale"));
    }

    #[test]
    fn stranger_cannot_mutate_a_task() {
        let mut s = board();
        create_task(
            &mut s,
            &caller("root", true),
            &CreateTask {
                subject: "a".into(),
                description: "b".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let e = update_task(
            &mut s,
            &caller("stranger", false),
            &UpdateTask {
                task_id: "task-1".into(),
                expected_revision: 1,
                action: TaskAction::Complete,
                subject: None,
                description: None,
                blocked_by: None,
                write_scopes: None,
                owner: None,
                owner_name: None,
            },
        )
        .unwrap_err();
        assert!(matches!(e, HarnessError::PermissionDenied(_)));
    }

    #[test]
    fn only_lead_can_reassign() {
        let mut s = board();
        create_task(
            &mut s,
            &caller("root", true),
            &CreateTask {
                subject: "a".into(),
                description: "b".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let e = update_task(
            &mut s,
            &caller("agent-1", false),
            &UpdateTask {
                task_id: "task-1".into(),
                expected_revision: 1,
                action: TaskAction::Reassign,
                subject: None,
                description: None,
                blocked_by: None,
                write_scopes: None,
                owner: Some("agent-2".into()),
                owner_name: Some("agent-2".into()),
            },
        )
        .unwrap_err();
        assert!(matches!(e, HarnessError::PermissionDenied(_)));

        let v = update_task(
            &mut s,
            &caller("root", true),
            &UpdateTask {
                task_id: "task-1".into(),
                expected_revision: 1,
                action: TaskAction::Reassign,
                subject: None,
                description: None,
                blocked_by: None,
                write_scopes: None,
                owner: Some("agent-2".into()),
                owner_name: Some("agent-2".into()),
            },
        )
        .unwrap();
        assert_eq!(v.owner_id.as_deref(), Some("agent-2"));
        assert_eq!(v.status, TaskStatus::InProgress);
    }

    #[test]
    fn complete_requires_in_progress() {
        let mut s = board();
        create_task(
            &mut s,
            &caller("root", true),
            &CreateTask {
                subject: "a".into(),
                description: "b".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let e = update_task(
            &mut s,
            &caller("root", true),
            &UpdateTask {
                task_id: "task-1".into(),
                expected_revision: 1,
                action: TaskAction::Complete,
                subject: None,
                description: None,
                blocked_by: None,
                write_scopes: None,
                owner: None,
                owner_name: None,
            },
        )
        .unwrap_err();
        assert!(e.to_string().contains("in-progress"));
    }

    #[test]
    fn release_clears_the_owner() {
        let mut s = board();
        create_task(
            &mut s,
            &caller("root", true),
            &CreateTask {
                subject: "a".into(),
                description: "b".into(),
                ..Default::default()
            },
        )
        .unwrap();
        update_task(
            &mut s,
            &caller("agent-1", false),
            &UpdateTask {
                task_id: "task-1".into(),
                expected_revision: 1,
                action: TaskAction::Claim,
                subject: None,
                description: None,
                blocked_by: None,
                write_scopes: None,
                owner: None,
                owner_name: None,
            },
        )
        .unwrap();
        let v = update_task(
            &mut s,
            &caller("agent-1", false),
            &UpdateTask {
                task_id: "task-1".into(),
                expected_revision: 2,
                action: TaskAction::Release,
                subject: None,
                description: None,
                blocked_by: None,
                write_scopes: None,
                owner: None,
                owner_name: None,
            },
        )
        .unwrap();
        assert_eq!(v.status, TaskStatus::Pending);
        assert!(v.owner_id.is_none());
        assert!(v.ready);
    }

    #[test]
    fn deleting_a_blocked_task_is_rejected() {
        let mut s = board();
        create_task(
            &mut s,
            &caller("root", true),
            &CreateTask {
                subject: "a".into(),
                description: "b".into(),
                ..Default::default()
            },
        )
        .unwrap();
        create_task(
            &mut s,
            &caller("root", true),
            &CreateTask {
                subject: "c".into(),
                description: "d".into(),
                blocked_by: vec!["task-1".into()],
                ..Default::default()
            },
        )
        .unwrap();
        let e = update_task(
            &mut s,
            &caller("root", true),
            &UpdateTask {
                task_id: "task-1".into(),
                expected_revision: 1,
                action: TaskAction::Delete,
                subject: None,
                description: None,
                blocked_by: None,
                write_scopes: None,
                owner: None,
                owner_name: None,
            },
        )
        .unwrap_err();
        assert!(e.to_string().contains("still blocks"));
    }

    #[test]
    fn dependency_cycle_is_rejected() {
        let mut s = board();
        create_task(
            &mut s,
            &caller("root", true),
            &CreateTask {
                subject: "a".into(),
                description: "b".into(),
                ..Default::default()
            },
        )
        .unwrap();
        create_task(
            &mut s,
            &caller("root", true),
            &CreateTask {
                subject: "c".into(),
                description: "d".into(),
                blocked_by: vec!["task-1".into()],
                ..Default::default()
            },
        )
        .unwrap();
        // task-1 blocked by task-2 closes the loop.
        let e = update_task(
            &mut s,
            &caller("root", true),
            &UpdateTask {
                task_id: "task-1".into(),
                expected_revision: 1,
                action: TaskAction::SetDependencies,
                subject: None,
                description: None,
                blocked_by: Some(vec!["task-2".into()]),
                write_scopes: None,
                owner: None,
                owner_name: None,
            },
        )
        .unwrap_err();
        assert!(e.to_string().contains("cycle"));
    }

    #[test]
    fn a_task_cannot_block_itself() {
        let mut s = board();
        create_task(
            &mut s,
            &caller("root", true),
            &CreateTask {
                subject: "a".into(),
                description: "b".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let e = update_task(
            &mut s,
            &caller("root", true),
            &UpdateTask {
                task_id: "task-1".into(),
                expected_revision: 1,
                action: TaskAction::SetDependencies,
                subject: None,
                description: None,
                blocked_by: Some(vec!["task-1".into()]),
                write_scopes: None,
                owner: None,
                owner_name: None,
            },
        )
        .unwrap_err();
        assert!(e.to_string().contains("cannot block itself"));
    }

    #[test]
    fn write_scope_overlap_is_reported() {
        let mut s = board();
        for subject in ["a", "b"] {
            create_task(
                &mut s,
                &caller("root", true),
                &CreateTask {
                    subject: subject.into(),
                    description: "d".into(),
                    write_scopes: vec!["src/core".into()],
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let claim = |n: &str| UpdateTask {
            task_id: n.into(),
            expected_revision: 1,
            action: TaskAction::Claim,
            subject: None,
            description: None,
            blocked_by: None,
            write_scopes: None,
            owner: None,
            owner_name: None,
        };
        update_task(&mut s, &caller("agent-1", false), &claim("task-1")).unwrap();
        let v = update_task(&mut s, &caller("agent-2", false), &claim("task-2")).unwrap();
        assert_eq!(v.write_scope_warnings.len(), 1);
        assert!(v.write_scope_warnings[0].contains("task-1"));
    }

    #[test]
    fn write_scopes_normalise() {
        assert_eq!(normalize_scope("./src/").unwrap(), "src");
        assert_eq!(normalize_scope("src\\core").unwrap(), "src/core");
        assert!(normalize_scope("/abs").is_err());
        assert!(normalize_scope("../up").is_err());
        assert!(normalize_scope("C:/abs").is_err());
        assert!(normalize_scope("").is_err());
    }

    #[test]
    fn task_limit_is_enforced() {
        let mut s = board();
        for i in 0..MAX_TASKS {
            create_task(
                &mut s,
                &caller("root", true),
                &CreateTask {
                    subject: format!("t{i}"),
                    description: "d".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let e = create_task(
            &mut s,
            &caller("root", true),
            &CreateTask {
                subject: "overflow".into(),
                description: "d".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(e.to_string().contains("limit"));
    }

    #[test]
    fn deleted_tasks_stay_off_the_board() {
        let mut s = board();
        create_task(
            &mut s,
            &caller("root", true),
            &CreateTask {
                subject: "a".into(),
                description: "b".into(),
                ..Default::default()
            },
        )
        .unwrap();
        update_task(
            &mut s,
            &caller("root", true),
            &UpdateTask {
                task_id: "task-1".into(),
                expected_revision: 1,
                action: TaskAction::Delete,
                subject: None,
                description: None,
                blocked_by: None,
                write_scopes: None,
                owner: None,
                owner_name: None,
            },
        )
        .unwrap();
        assert_eq!(s.live_tasks().len(), 0);
        assert_eq!(s.tasks.len(), 1, "the tombstone is retained");
        assert!(board_view(&s).is_empty());
    }

    #[test]
    fn action_parses_from_text() {
        assert_eq!(TaskAction::parse("claim"), Some(TaskAction::Claim));
        assert_eq!(
            TaskAction::parse("set_dependencies"),
            Some(TaskAction::SetDependencies)
        );
        assert_eq!(TaskAction::parse("nope"), None);
    }
}
