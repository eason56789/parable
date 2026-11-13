//! Host-wide scheduled tasks.
//!
//! Design follows DeepSeek Harness's `schedule` subsystem (MIT, deepseek-ai/deepseek-harness)
//! as a behavioural reference: seven timing kinds, RFC 3339 UTC instants with millisecond
//! precision, *latest-only* catch-up, and delivery as an ordinary follow-up message into the
//! target session. See `docs/subsystems/schedule.md` upstream for the original contract.
//!
//! Deliberate simplifications in this port, stated plainly:
//! - `rrule` is not implemented; use `cron` instead. `weekly` is expressed internally
//!   as a cron expression but kept as its own kind for API fidelity.
//! - Persistence is a JSON file, not SQLite (same reasoning as `session.rs`).
//! - Delivery targets are `existing` / `reuse` / `new`; `new` creates a session per fire.

use crate::error::HarnessError;
use crate::event::EventHub;
use chrono::{DateTime, Datelike, Duration, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;

// ---------------------------------------------------------------- timing

/// The seven authoritative timing kinds. `Rrule` exists in the enum so stored
/// data round-trips, but `Timing::next_after` rejects it until it is implemented.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Timing {
    /// One-shot, N seconds from admission.
    After { after_seconds: u64 },
    /// One-shot at a committed UTC instant.
    At { at: String },
    /// Fixed interval, at least 60s, anchored to the first committed occurrence.
    Every { every_seconds: u64 },
    /// Daily at a local wall time in an explicit zone.
    Daily { time: String, #[serde(rename = "timeZone")] time_zone: String },
    /// `daily` plus a set of ISO weekdays (1 = Monday .. 7 = Sunday).
    Weekly {
        time: String,
        #[serde(rename = "timeZone")]
        time_zone: String,
        weekdays: Vec<u8>,
    },
    /// Canonical five-field cron in an explicit zone.
    Cron {
        expression: String,
        #[serde(rename = "timeZone")]
        time_zone: String,
    },
    /// RFC 5545 RRULE. Accepted for storage round-trip; not yet evaluated.
    Rrule {
        rrule: String,
        #[serde(rename = "timeZone")]
        time_zone: String,
    },
}

impl Timing {
    pub fn label(&self) -> String {
        match self {
            Timing::After { after_seconds } => format!("in {after_seconds}s"),
            Timing::At { at } => format!("at {at}"),
            Timing::Every { every_seconds } => format!("every {every_seconds}s"),
            Timing::Daily { time, time_zone } => format!("daily {time} {time_zone}"),
            Timing::Weekly { time, time_zone, .. } => format!("weekly {time} {time_zone}"),
            Timing::Cron { expression, time_zone } => format!("cron `{expression}` {time_zone}"),
            Timing::Rrule { rrule, .. } => format!("rrule `{rrule}`"),
        }
    }

    /// Next occurrence strictly after `after`, in UTC.
    ///
    /// `anchor` is the previously committed occurrence for interval rules, so
    /// `every` stays aligned to its original phase rather than drifting with
    /// catch-up.
    pub fn next_after(&self, after: DateTime<Utc>, anchor: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
        match self {
            Timing::After { .. } | Timing::At { .. } => None, // one-shot: no repeat

            Timing::Every { every_seconds } => {
                let step = Duration::seconds(*every_seconds as i64);
                if step.num_seconds() <= 0 {
                    return None;
                }
                let base = anchor.unwrap_or(after);
                let mut t = base;
                while t <= after {
                    t += step;
                }
                Some(t)
            }

            Timing::Daily { time, time_zone } => daily_next(after, time, time_zone, None),
            Timing::Weekly { time, time_zone, weekdays } => {
                daily_next(after, time, time_zone, Some(weekdays))
            }

            Timing::Cron { expression, time_zone } => {
                cron_next(after, expression, time_zone)
            }

            Timing::Rrule { .. } => {
                // Not implemented. Returning None marks the task as ended rather
                // than silently mis-firing.
                None
            }
        }
    }

    /// Parse and validate a rule at admission time.
    pub fn validate(&self) -> Result<(), HarnessError> {
        match self {
            Timing::After { after_seconds } => {
                if *after_seconds == 0 {
                    return Err(HarnessError::Other("after_seconds must be > 0".into()));
                }
            }
            Timing::At { at } => {
                parse_instant(at)?;
            }
            Timing::Every { every_seconds } => {
                if *every_seconds < 60 {
                    return Err(HarnessError::Other("every_seconds must be >= 60".into()));
                }
            }
            Timing::Daily { time, time_zone } | Timing::Weekly { time, time_zone, .. } => {
                parse_local_time(time)?;
                parse_zone(time_zone)?;
            }
            Timing::Cron { expression, time_zone } => {
                parse_zone(time_zone)?;
                let expr = normalize_cron(expression)?;
                cron::Schedule::from_str(&expr).map_err(|e| {
                    HarnessError::Other(format!("invalid cron expression `{expression}`: {e}"))
                })?;
            }
            Timing::Rrule { .. } => {
                return Err(HarnessError::Other(
                    "rrule is not supported yet; use cron".into(),
                ));
            }
        }
        Ok(())
    }
}

fn parse_zone(name: &str) -> Result<Tz, HarnessError> {
    Tz::from_str(name).map_err(|_| HarnessError::Other(format!("unknown time zone `{name}`")))
}

fn parse_local_time(s: &str) -> Result<NaiveTime, HarnessError> {
    NaiveTime::parse_from_str(s, "%H:%M:%S%.f")
        .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M"))
        .map_err(|_| HarnessError::Other(format!("invalid time `{s}` (use HH:MM or HH:MM:SS)")))
}

fn parse_instant(s: &str) -> Result<DateTime<Utc>, HarnessError> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|_| HarnessError::Other(format!("invalid RFC 3339 instant `{s}`")))
}

/// Next daily/weekly occurrence after `after`, honouring the DST rules DSH
/// documents: missing wall times are skipped, and an ambiguous overlap picks
/// the earlier instant.
fn daily_next(
    after: DateTime<Utc>,
    time: &str,
    zone_name: &str,
    weekdays: Option<&Vec<u8>>,
) -> Option<DateTime<Utc>> {
    let tz = parse_zone(zone_name).ok()?;
    let tod = parse_local_time(time).ok()?;
    let local_after = after.with_timezone(&tz);

    // Look a couple of weeks ahead; `weekly` never needs more than that.
    for day_offset in 0..=14i64 {
        let date = local_after.date_naive() + Duration::days(day_offset);
        if let Some(days) = weekdays {
            let iso = date.weekday().number_from_monday() as u8;
            if !days.contains(&iso) {
                continue;
            }
        }
        let naive = date.and_time(tod);
        let candidates: Vec<DateTime<Utc>> = match tz.from_local_datetime(&naive) {
            chrono::LocalResult::Single(t) => vec![t.with_timezone(&Utc)],
            chrono::LocalResult::Ambiguous(a, b) => {
                // Earlier instant wins on an overlap.
                let (x, y) = (a.with_timezone(&Utc), b.with_timezone(&Utc));
                if x <= y {
                    vec![x, y]
                } else {
                    vec![y, x]
                }
            }
            chrono::LocalResult::None => continue, // skipped: nonexistent wall time
        };
        for c in candidates {
            if c > after {
                return Some(c);
            }
        }
    }
    None
}

/// The `cron` crate parses seven fields (sec min hour dom month dow year), but
/// the schedule contract — and every user coming from cron(5) — speaks five.
/// Normalise so `*/15 * * * *` works as expected.
fn normalize_cron(expression: &str) -> Result<String, HarnessError> {
    let fields: Vec<&str> = expression.split_whitespace().collect();
    match fields.len() {
        5 => Ok(format!("0 {} *", expression.trim())),
        6 => Ok(format!("{} *", expression.trim())),
        7 => Ok(expression.trim().to_string()),
        n => Err(HarnessError::Other(format!(
            "cron expression must have 5, 6 or 7 fields, got {n}: `{expression}`"
        ))),
    }
}

fn cron_next(after: DateTime<Utc>, expression: &str, zone_name: &str) -> Option<DateTime<Utc>> {
    let tz = parse_zone(zone_name).ok()?;
    let expr = normalize_cron(expression).ok()?;
    let schedule = cron::Schedule::from_str(&expr).ok()?;
    // The `cron` crate iterates in the given zone; ask it for the next instant
    // after the UTC `after` expressed in that zone.
    let local_after = after.with_timezone(&tz);
    schedule
        .after(&local_after)
        .next()
        .map(|t| t.with_timezone(&Utc))
}

// ---------------------------------------------------------------- model

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Active,
    Paused,
    Ended,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    /// Deliver into this existing session.
    Existing { session_id: String },
    /// Create one session at admission and reuse it for every fire.
    Reuse,
    /// Create a fresh session for each fire.
    NewEachTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub name: String,
    /// The prompt delivered when the task fires.
    pub prompt: String,
    pub timing: Timing,
    pub target: Target,
    pub status: TaskStatus,
    /// Committed next occurrence, RFC 3339 UTC with millisecond precision.
    /// `None` means the rule has been admitted but not yet committed, or it is exhausted.
    #[serde(rename = "scheduledAt")]
    pub scheduled_at: Option<String>,
    /// Anchor for `every` interval phase. Equal to the first committed occurrence.
    #[serde(default, rename = "anchorAt")]
    pub anchor_at: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "lastRunAt", default)]
    pub last_run_at: Option<String>,
    #[serde(default, rename = "runCount")]
    pub run_count: u64,
    /// Session actually used by `reuse` / `new_each_time` after the first fire.
    #[serde(default, rename = "boundSessionId")]
    pub bound_session_id: Option<String>,
}

impl Task {
    /// Advance past `now` with latest-only catch-up: regardless of how many
    /// occurrences were missed, deliver at most one and commit the first future
    /// one. Exhaustion marks the task `ended`.
    pub fn catch_up(&mut self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        if self.status != TaskStatus::Active {
            return None;
        }
        let mut committed = self.scheduled_at.as_deref().and_then(|s| parse_instant(s).ok());
        let anchor = self.anchor_at.as_deref().and_then(|s| parse_instant(s).ok());

        // Nothing committed yet: compute the first occurrence from now.
        let mut due = committed.take();
        if due.is_none() {
            due = match &self.timing {
                // One-shots commit their target at admission; fall back to now-based.
                Timing::After { after_seconds } => {
                    Some(now + Duration::seconds(*after_seconds as i64))
                }
                Timing::At { at } => parse_instant(at).ok(),
                other => other.next_after(now, anchor),
            };
        }

        let mut fired = None;
        if let Some(d) = due {
            if d <= now {
                fired = Some(d);
                // Skip every missed occurrence and commit the first future one.
                let mut next = self.timing.next_after(now, anchor.or(Some(d)));
                while let Some(n) = next {
                    if n > now {
                        break;
                    }
                    next = self.timing.next_after(n, anchor.or(Some(d)));
                }
                self.scheduled_at = next.map(|n| n.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
                if self.scheduled_at.is_none() {
                    self.status = TaskStatus::Ended;
                }
            } else {
                self.scheduled_at = Some(d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
            }
        } else {
            self.status = TaskStatus::Ended;
        }
        fired
    }
}

// ---------------------------------------------------------------- store

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    tasks: Vec<Task>,
}

pub struct ScheduleStore {
    path: PathBuf,
    inner: RwLock<HashMap<String, Task>>,
}

impl ScheduleStore {
    pub fn new() -> Self {
        let path = crate::config::Config::home_dir().join("schedule.json");
        let inner = match std::fs::read_to_string(&path) {
            Ok(s) => match serde_json::from_str::<Persisted>(&s) {
                Ok(p) => p.tasks.into_iter().map(|t| (t.id.clone(), t)).collect(),
                Err(e) => {
                    // 损坏文件：先备份原始数据再回退空表，绝不静默清空。
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let mut bak = path.clone().into_os_string();
                    bak.push(format!(".corrupt-{ts}"));
                    let backup = PathBuf::from(bak);
                    match std::fs::rename(&path, &backup) {
                        Ok(()) => tracing::error!(
                            "schedule.json parse failed ({e}); original backed up to {}",
                            backup.display()
                        ),
                        Err(be) => tracing::error!(
                            "schedule.json parse failed ({e}); backup rename failed: {be}, file left in place"
                        ),
                    }
                    HashMap::new()
                }
            },
            Err(_) => HashMap::new(),
        };
        Self {
            path,
            inner: RwLock::new(inner),
        }
    }

    /// 原子落盘（lib.rs 的 atomic_write：临时文件 + rename），失败记 error。
    async fn persist(&self, map: &HashMap<String, Task>) {
        let mut tasks: Vec<Task> = map.values().cloned().collect();
        tasks.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        match serde_json::to_string_pretty(&Persisted { tasks }) {
            Ok(json) => {
                if let Err(e) = crate::atomic_write(&self.path, json.as_bytes()) {
                    tracing::error!("schedule.json persist failed: {e}");
                }
            }
            Err(e) => tracing::error!("schedule.json serialize failed: {e}"),
        }
    }

    pub async fn insert(&self, t: Task) -> Result<(), HarnessError> {
        {
            let mut m = self.inner.write().await;
            m.insert(t.id.clone(), t.clone());
            self.persist(&m).await;
        }
        Ok(())
    }

    pub async fn get(&self, id: &str) -> Option<Task> {
        self.inner.read().await.get(id).cloned()
    }

    pub async fn list(&self) -> Vec<Task> {
        let mut v: Vec<Task> = self.inner.read().await.values().cloned().collect();
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        v
    }

    pub async fn update<F: FnOnce(&mut Task)>(&self, id: &str, f: F) -> Result<Task, HarnessError> {
        let mut m = self.inner.write().await;
        let t = m
            .get_mut(id)
            .ok_or_else(|| HarnessError::NotFound(format!("task {id}")))?;
        f(t);
        let out = t.clone();
        self.persist(&m).await;
        Ok(out)
    }

    pub async fn delete(&self, id: &str) -> Result<(), HarnessError> {
        let mut m = self.inner.write().await;
        m.remove(id);
        self.persist(&m).await;
        Ok(())
    }
}

// ---------------------------------------------------------------- service

pub struct ScheduleService {
    store: Arc<ScheduleStore>,
    hub: EventHub,
    /// 注入的共享 SessionStore（`with_sessions`）；未注入时退回临时 store。
    sessions: Option<crate::session::SharedStore>,
    /// 注入的 workspace 根（`with_workspace`）；Reuse/NewEachTime 建会话用。
    workspace: PathBuf,
}

impl ScheduleService {
    pub fn new(store: Arc<ScheduleStore>, hub: EventHub) -> Self {
        Self {
            store,
            hub,
            sessions: None,
            workspace: PathBuf::from("."),
        }
    }

    /// 注入共享 SessionStore（server.rs 装配时调用）。
    pub fn with_sessions(mut self, sessions: crate::session::SharedStore) -> Self {
        self.sessions = Some(sessions);
        self
    }

    /// 注入 workspace 根路径（server.rs 装配时调用）。
    pub fn with_workspace(mut self, workspace: PathBuf) -> Self {
        self.workspace = workspace;
        self
    }

    fn session_store(&self) -> crate::session::SharedStore {
        self.sessions
            .clone()
            .unwrap_or_else(|| Arc::new(crate::session::SessionStore::new()))
    }

    pub fn store(&self) -> Arc<ScheduleStore> {
        self.store.clone()
    }

    /// Create and commit a task. `now` fixes the anchor so `every` keeps phase.
    pub async fn create(
        &self,
        name: String,
        prompt: String,
        timing: Timing,
        target: Target,
    ) -> Result<Task, HarnessError> {
        timing.validate()?;
        if name.trim().is_empty() {
            return Err(HarnessError::Other("name must not be empty".into()));
        }
        if prompt.trim().is_empty() {
            return Err(HarnessError::Other("prompt must not be empty".into()));
        }
        let now = Utc::now();
        let mut t = Task {
            id: format!("sc_{}", &uuid::Uuid::new_v4().simple().to_string()[..10]),
            name,
            prompt,
            timing: timing.clone(),
            target,
            status: TaskStatus::Active,
            scheduled_at: None,
            anchor_at: None,
            created_at: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            last_run_at: None,
            run_count: 0,
            bound_session_id: None,
        };
        // Commit the first occurrence now so `every` is anchored at admission.
        let first = match &t.timing {
            Timing::After { after_seconds } => Some(now + Duration::seconds(*after_seconds as i64)),
            Timing::At { at } => parse_instant(at).ok(),
            other => other.next_after(now, None),
        };
        if let Some(f) = first {
            t.scheduled_at = Some(f.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
            if matches!(t.timing, Timing::Every { .. }) {
                t.anchor_at = Some(f.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
            }
        } else {
            t.status = TaskStatus::Ended;
        }
        let out = t.clone();
        self.store.insert(t).await?;
        self.publish(&out);
        Ok(out)
    }

    fn publish(&self, t: &Task) {
        self.hub.publish("", "schedule.updated", serde_json::to_value(t).unwrap_or_default());
    }

    /// Tasks that are due at `now`, with catch-up applied.
    /// Returns `(task, fired_at)` pairs; the caller delivers and then records the run.
    ///
    /// 竞态说明：catch_up 的"读-改-写"放进同一把写锁里完成，消除了原先
    /// `get()` 克隆副本与 `update()` 回写之间的窗口。last_run_at / run_count
    /// 的推进语义：投递在本函数之外由调用方 fire-and-forget 执行（拿不到
    /// 投递结果），因此这里在**任何投递 spawn 之前**就推进 scheduled_at
    /// （catch_up 本身）并把 last_run_at 记为本次应触发的 fired 时刻——
    /// 采用 latest-only 语义：即使投递失败也不会重复触发同一次 occurrence，
    /// 代价是投递丢失只留下调用方日志。
    pub async fn due(&self, now: DateTime<Utc>) -> Vec<(Task, DateTime<Utc>)> {
        let mut out = Vec::new();
        let ids: Vec<String> = {
            let m = self.store.inner.read().await;
            m.values()
                .filter(|t| t.status == TaskStatus::Active)
                .map(|t| t.id.clone())
                .collect()
        };
        for id in ids {
            let (changed, fired) = {
                let mut m = self.store.inner.write().await;
                let Some(t) = m.get_mut(&id) else { continue };
                let before = (
                    t.scheduled_at.clone(),
                    t.status,
                    t.last_run_at.clone(),
                    t.run_count,
                );
                let f = t.catch_up(now);
                if let Some(fired_at) = f {
                    // last_run_at 记为“应触发”的 fired 时刻（而非投递时刻）。
                    t.last_run_at =
                        Some(fired_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
                    t.run_count += 1;
                }
                let changed = before
                    != (
                        t.scheduled_at.clone(),
                        t.status,
                        t.last_run_at.clone(),
                        t.run_count,
                    );
                (changed, f.map(|fired_at| (t.clone(), fired_at)))
            };
            if changed {
                let m = self.store.inner.read().await;
                self.store.persist(&m).await;
            }
            if let Some(pair) = fired {
                out.push(pair);
            }
        }
        out
    }

    /// Resolve the session a task should deliver into, creating one if needed.
    ///
    /// 会话创建/校验走注入的共享 `SessionStore`（`with_sessions`）；新建会话
    /// 归属注入的 workspace 根（`with_workspace`），不再退回 current_dir()。
    pub async fn resolve_session(&self, t: &mut Task) -> Result<String, HarnessError> {
        let store = self.session_store();
        match &t.target {
            Target::Existing { session_id } => {
                // 明确校验：会话必须存在且未归档，否则报错（调用方据此把本轮
                // 投递标失败并记录原因），绝不静默制造/投进幽灵会话。
                if !store.exists(session_id).await {
                    return Err(HarnessError::NotFound(format!(
                        "target session {session_id} does not exist (task `{}` delivery aborted)",
                        t.id
                    )));
                }
                if store.is_archived(session_id).await {
                    return Err(HarnessError::Other(format!(
                        "target session {session_id} is archived (task `{}` delivery aborted)",
                        t.id
                    )));
                }
                Ok(session_id.clone())
            }
            Target::Reuse => {
                if let Some(s) = &t.bound_session_id {
                    // 绑定会话仍在且未归档才复用；被删除/归档则重建一个，
                    // 避免此后每次投递都失败。
                    if store.exists(s).await && !store.is_archived(s).await {
                        return Ok(s.clone());
                    }
                }
                let created = store
                    .create(
                        &self.workspace.to_string_lossy(),
                        &format!("⏱ {}", t.name),
                    )
                    .await;
                let id = created.meta.id.clone();
                self.store
                    .update(&t.id, |x| x.bound_session_id = Some(id.clone()))
                    .await
                    .ok();
                t.bound_session_id = Some(id.clone());
                Ok(id)
            }
            Target::NewEachTime => {
                let created = store
                    .create(
                        &self.workspace.to_string_lossy(),
                        &format!("⏱ {}", t.name),
                    )
                    .await;
                Ok(created.meta.id)
            }
        }
    }
}

pub type SharedSchedule = Arc<ScheduleService>;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    fn at(s: &str) -> DateTime<Utc> {
        parse_instant(s).unwrap()
    }

    #[test]
    fn every_keeps_phase() {
        let t = Timing::Every { every_seconds: 600 };
        let anchor = at("2026-01-01T00:00:00.000Z");
        let after = at("2026-01-01T00:25:00.000Z");
        let n = t.next_after(after, Some(anchor)).unwrap();
        // 00:30 is the next 10-minute slot from the 00:00 anchor.
        assert_eq!(n.to_rfc3339(), "2026-01-01T00:30:00+00:00");
    }

    #[test]
    fn after_is_one_shot() {
        let t = Timing::After { after_seconds: 10 };
        assert!(t.next_after(Utc::now(), None).is_none());
    }

    #[test]
    fn daily_skips_missing_wall_time() {
        // 2026-03-08 02:30 does not exist in America/New_York (spring forward).
        let t = Timing::Daily {
            time: "02:30:00".into(),
            time_zone: "America/New_York".into(),
        };
        let after = at("2026-03-08T06:00:00.000Z"); // 01:00 EST
        let n = t.next_after(after, None).expect("should skip to the next valid day");
        let local = n.with_timezone(&chrono_tz::America::New_York);
        assert!(local.date_naive() > chrono::NaiveDate::from_ymd_opt(2026, 3, 8).unwrap());
    }

    #[test]
    fn weekly_only_fires_on_selected_days() {
        let t = Timing::Weekly {
            time: "09:00:00".into(),
            time_zone: "UTC".into(),
            weekdays: vec![1], // Monday only
        };
        // 2026-01-03 is a Saturday.
        let after = at("2026-01-03T00:00:00.000Z");
        let n = t.next_after(after, None).unwrap();
        assert_eq!(n.weekday().number_from_monday(), 1);
    }

    #[test]
    fn cron_expression_evaluates() {
        let t = Timing::Cron {
            expression: "*/15 * * * *".into(),
            time_zone: "UTC".into(),
        };
        let after = at("2026-01-01T00:07:00.000Z");
        let n = t.next_after(after, None).unwrap();
        assert_eq!(n.minute(), 15);
    }

    #[test]
    fn catch_up_is_latest_only() {
        let mut task = Task {
            id: "t1".into(),
            name: "n".into(),
            prompt: "p".into(),
            timing: Timing::Every { every_seconds: 60 },
            target: Target::Reuse,
            status: TaskStatus::Active,
            scheduled_at: Some("2026-01-01T00:00:00.000Z".into()),
            anchor_at: Some("2026-01-01T00:00:00.000Z".into()),
            created_at: "2026-01-01T00:00:00.000Z".into(),
            last_run_at: None,
            run_count: 0,
            bound_session_id: None,
        };
        // Now is 5 minutes past: 5 occurrences were missed.
        let now = at("2026-01-01T00:05:00.000Z");
        let fired = task.catch_up(now);
        assert!(fired.is_some(), "must fire exactly once");
        assert_eq!(task.run_count, 0, "catch_up itself does not count runs");
        // Next committed occurrence is in the future, not in the missed gap.
        let next = parse_instant(task.scheduled_at.as_deref().unwrap()).unwrap();
        assert!(next > now);
    }

    #[test]
    fn one_shot_ends_after_firing() {
        let mut task = Task {
            id: "t2".into(),
            name: "n".into(),
            prompt: "p".into(),
            timing: Timing::At { at: "2026-01-01T00:10:00.000Z".into() },
            target: Target::Reuse,
            status: TaskStatus::Active,
            scheduled_at: Some("2026-01-01T00:10:00.000Z".into()),
            anchor_at: None,
            created_at: "2026-01-01T00:00:00.000Z".into(),
            last_run_at: None,
            run_count: 0,
            bound_session_id: None,
        };
        let now = at("2026-01-01T00:15:00.000Z");
        assert!(task.catch_up(now).is_some());
        assert_eq!(task.status, TaskStatus::Ended);
        assert!(task.scheduled_at.is_none());
    }

    #[test]
    fn rejects_bad_rules() {
        assert!(Timing::Every { every_seconds: 5 }.validate().is_err());
        assert!(Timing::After { after_seconds: 0 }.validate().is_err());
        assert!(Timing::Cron {
            expression: "not a cron".into(),
            time_zone: "UTC".into(),
        }
        .validate()
        .is_err());
        assert!(Timing::Rrule {
            rrule: "FREQ=DAILY".into(),
            time_zone: "UTC".into(),
        }
        .validate()
        .is_err());
    }
}
