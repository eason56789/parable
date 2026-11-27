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
use tokio::sync::{Mutex, RwLock};

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
                    return Err(HarnessError::Other(
                        "after_seconds must be greater than 0".into(),
                    ));
                }
            }
            Timing::At { at } => {
                parse_instant(at)?;
            }
            Timing::Every { every_seconds } => {
                if *every_seconds < 60 {
                    return Err(HarnessError::Other(
                        "every_seconds must be at least 60".into(),
                    ));
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
                    HarnessError::Other(format!(
                        "invalid cron expression `{expression}`: {e} (expected 5 fields, e.g. `*/15 * * * *`)"
                    ))
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

/// 任务触发时到底做什么。
///
/// 旧数据只有「把一段提示词投给 agent」这一种动作，所以 `Agent` 是
/// `#[serde(other)]` 之外的默认值——老 JSON 反序列化后仍然是原行为。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskAction {
    /// 把 prompt 作为新一轮投给 agent（原有行为）。
    Agent,
    /// 直接执行一条 shell 命令（不经过模型，零 token）。
    Shell { command: String },
}

impl Default for TaskAction {
    fn default() -> Self {
        TaskAction::Agent
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub name: String,
    /// 触发时做什么。缺省 = 投递提示词（兼容旧任务文件）。
    #[serde(default)]
    pub action: TaskAction,
    /// The prompt delivered when the task fires.
    pub prompt: String,
    /// 触发时用哪个模型（`provider/model`）。空 = 跟随当前全局模型。
    #[serde(default, rename = "modelSel")]
    pub model_sel: String,
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
    /// 重算下一次触发时刻并重置 anchor。
    ///
    /// 改 timing 时必须重算：否则任务会卡在旧的时间点上——要么永远不再触发，
    /// 要么立刻误触发。一次性规则（`after` / `at`）若算不出下一次，则任务结束。
    pub fn recompute_next(&mut self, now: DateTime<Utc>) {
        self.recompute_next_inner(now, false);
    }

    /// 从 paused 恢复成 active：重算下一次触发，但保留 anchor。
    ///
    /// 保留 anchor 是为了 `every` 的相位不漂移——暂停一小时再恢复，间隔任务
    /// 不该突然整体平移一小时。
    pub fn resume_next(&mut self, now: DateTime<Utc>) {
        self.recompute_next_inner(now, true);
    }

    fn recompute_next_inner(&mut self, now: DateTime<Utc>, keep_anchor: bool) {
        let anchor = if keep_anchor { self.anchor_at.take() } else { None };
        self.scheduled_at = None;
        let first = match &self.timing {
            Timing::After { after_seconds } => Some(now + Duration::seconds(*after_seconds as i64)),
            Timing::At { at } => parse_instant(at).ok(),
            other => other.next_after(now, anchor.as_ref().and_then(|a| parse_instant(a).ok())),
        };
        if let Some(f) = first {
            let iso = f.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            self.scheduled_at = Some(iso.clone());
            // `every` 以首次提交时刻为相位锚点，之后不漂移。
            match &anchor {
                Some(a) if matches!(self.timing, Timing::Every { .. }) => {
                    self.anchor_at = Some(a.clone())
                }
                Some(_) => self.anchor_at = None,
                None => {
                    if matches!(self.timing, Timing::Every { .. }) {
                        self.anchor_at = Some(iso);
                    }
                }
            }
        } else {
            self.anchor_at = None;
            self.status = TaskStatus::Ended;
        }
    }

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

// ---------------------------------------------------------------- run history

/// 运行历史的一条记录：任务触发一次 = 一条。
///
/// 字段命名沿用项目既有的 camelCase 风格（`startedAt` / `finishedAt` /
/// `exitCode` / `taskId` / `taskName` / `sessionId`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    /// 本条记录的唯一 id（`run_` 前缀 + 短随机串）。
    pub id: String,
    #[serde(rename = "taskId")]
    pub task_id: String,
    #[serde(rename = "taskName")]
    pub task_name: String,
    /// 开始时刻（RFC 3339 UTC，毫秒精度）。
    #[serde(rename = "startedAt")]
    pub started_at: String,
    /// 结束时刻；尚未结束（投递成功但模型还在跑）为 null。
    #[serde(rename = "finishedAt")]
    pub finished_at: Option<String>,
    /// 本次运行是否成功。
    pub ok: bool,
    /// shell 动作的退出码；agent 动作为 null。
    #[serde(rename = "exitCode")]
    pub exit_code: Option<i32>,
    /// 命令输出（只保留尾部若干 KB）。
    pub output: String,
    /// 失败原因。
    pub error: Option<String>,
    /// agent 动作投递到的会话。
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
}

impl RunRecord {
    /// 新建一条记录：id 自动生成，开始时刻取当前 UTC。
    pub fn new(task_id: &str, task_name: &str) -> Self {
        Self {
            id: format!("run_{}", &uuid::Uuid::new_v4().simple().to_string()[..10]),
            task_id: task_id.to_string(),
            task_name: task_name.to_string(),
            started_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            finished_at: None,
            ok: false,
            exit_code: None,
            output: String::new(),
            error: None,
            session_id: None,
        }
    }
}

/// 运行历史保留条数上限：超出后丢弃最旧的记录。
pub const MAX_RUN_RECORDS: usize = 200;

/// 命令输出只留尾部 4KB：关键信息（错误栈、退出行）通常在末尾。
pub fn tail_output(s: &str) -> String {
    const MAX: usize = 4096;
    if s.len() <= MAX {
        return s.to_string();
    }
    // 按字符边界回退，避免切在多字节字符中间。
    let mut start = s.len() - MAX;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("… [truncated {} bytes] …\n{}", s.len() - MAX, &s[start..])
}

// ---------------------------------------------------------------- store

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    tasks: Vec<Task>,
}

/// 运行历史的落盘格式（与任务表分开一个文件，互不牵连）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedRuns {
    #[serde(default)]
    runs: Vec<RunRecord>,
}

pub struct ScheduleStore {
    path: PathBuf,
    /// 运行历史文件：与任务表同目录，命名 `runs.json`。
    runs_path: PathBuf,
    inner: RwLock<HashMap<String, Task>>,
    /// 运行历史（超出上限丢最旧）。
    runs: Mutex<Vec<RunRecord>>,
}

impl ScheduleStore {
    pub fn new() -> Self {
        Self::with_path(crate::config::Config::schedule_path())
    }

    /// 指定任务表路径构造（`runs.json` 落在同一目录）。
    ///
    /// 服务器在显式指定数据根（`serve_in` 的 `home`）时用这个入口，
    /// 让任务表与运行历史一起落在测试/沙箱目录里，不碰用户真实 home。
    pub fn with_path(path: PathBuf) -> Self {
        let runs_path = path
            .parent()
            .map(|p| p.join("runs.json"))
            .unwrap_or_else(|| PathBuf::from("runs.json"));
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
        // 运行历史：坏文件直接忽略（历史丢了不影响任务本身）。
        let mut runs: Vec<RunRecord> = match std::fs::read_to_string(&runs_path) {
            Ok(s) => serde_json::from_str::<PersistedRuns>(&s)
                .map(|p| p.runs)
                .unwrap_or_else(|e| {
                    tracing::warn!("runs.json parse failed, ignoring history: {e}");
                    Vec::new()
                }),
            Err(_) => Vec::new(),
        };
        if runs.len() > MAX_RUN_RECORDS {
            let excess = runs.len() - MAX_RUN_RECORDS;
            runs.drain(0..excess);
        }
        Self {
            path,
            runs_path,
            inner: RwLock::new(inner),
            runs: Mutex::new(runs),
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

    /// 运行历史原子落盘。
    async fn persist_runs(&self, runs: &[RunRecord]) {
        match serde_json::to_string_pretty(&PersistedRuns { runs: runs.to_vec() }) {
            Ok(json) => {
                if let Err(e) = crate::atomic_write(&self.runs_path, json.as_bytes()) {
                    tracing::error!("runs.json persist failed: {e}");
                }
            }
            Err(e) => tracing::error!("runs.json serialize failed: {e}"),
        }
    }

    /// 追加一条运行记录；超过上限时丢弃最旧的。
    pub async fn record_run(&self, r: RunRecord) {
        let mut g = self.runs.lock().await;
        g.push(r);
        if g.len() > MAX_RUN_RECORDS {
            let excess = g.len() - MAX_RUN_RECORDS;
            g.drain(0..excess);
        }
        self.persist_runs(&g).await;
    }

    /// 读取运行历史：`task_id` 为 `Some` 时只返回该任务的记录。
    /// 按 `started_at` 倒序（最新在前）。
    pub async fn runs_for(&self, task_id: Option<&str>) -> Vec<RunRecord> {
        let g = self.runs.lock().await;
        // 先反序收集：同毫秒的多条记录靠稳定排序保持"后写的在前"。
        let mut out: Vec<RunRecord> = g
            .iter()
            .rev()
            .filter(|r| match task_id {
                Some(t) => r.task_id == t,
                None => true,
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        out
    }

    /// 就地更新一条运行记录（如后台终端跑完后回填退出码与输出）。
    /// 找不到记录时返回 false。
    pub async fn update_run<F: FnOnce(&mut RunRecord)>(&self, id: &str, f: F) -> bool {
        let mut g = self.runs.lock().await;
        let mut found = false;
        if let Some(rec) = g.iter_mut().find(|r| r.id == id) {
            f(rec);
            found = true;
        }
        if found {
            self.persist_runs(&g).await;
        }
        found
    }

    /// 每个任务最近一次运行记录（用于列表接口的 `lastRunOk`）。
    pub async fn last_runs(&self) -> HashMap<String, RunRecord> {
        let g = self.runs.lock().await;
        let mut map: HashMap<String, RunRecord> = HashMap::new();
        // 倒序遍历：同一毫秒内的多条记录，取写入最晚的那条。
        for r in g.iter().rev() {
            let entry = map.entry(r.task_id.clone()).or_insert_with(|| r.clone());
            if r.started_at > entry.started_at {
                *entry = r.clone();
            }
        }
        map
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
        action: TaskAction,
        model_sel: String,
        timing: Timing,
        target: Target,
    ) -> Result<Task, HarnessError> {
        timing.validate()?;
        if name.trim().is_empty() {
            return Err(HarnessError::Other("name must not be empty".into()));
        }
        // 只有 agent 动作才需要提示词；shell 动作只要命令行非空。
        let needs_prompt = matches!(action, TaskAction::Agent) && prompt.trim().is_empty();
        if needs_prompt {
            return Err(HarnessError::Other("prompt must not be empty".into()));
        }
        if let TaskAction::Shell { command } = &action {
            if command.trim().is_empty() {
                return Err(HarnessError::Other("shell command must not be empty".into()));
            }
        }
        let now = Utc::now();
        let mut t = Task {
            id: format!("sc_{}", &uuid::Uuid::new_v4().simple().to_string()[..10]),
            name,
            action,
            prompt,
            model_sel,
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
        t.recompute_next(now);
        let out = t.clone();
        self.store.insert(t).await?;
        self.publish(&out);
        Ok(out)
    }

    fn publish(&self, t: &Task) {
        self.hub.publish("", "schedule.updated", serde_json::to_value(t).unwrap_or_default());
    }

    /// 任务被编辑后广播一次（server.rs 的 PATCH 用）。
    pub fn publish_task(&self, t: &Task) {
        self.publish(t);
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
            action: TaskAction::Agent,
            prompt: "p".into(),
            model_sel: String::new(),
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
            action: TaskAction::Agent,
            prompt: "p".into(),
            model_sel: String::new(),
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

    /// 三类常见非法 timing 的报错文案要能一眼看懂。
    #[test]
    fn bad_timing_messages_are_readable() {
        let every = Timing::Every { every_seconds: 5 }
            .validate()
            .unwrap_err()
            .to_string();
        assert!(every.contains("every_seconds"), "got: {every}");
        assert!(every.contains("60"), "got: {every}");

        let after = Timing::After { after_seconds: 0 }
            .validate()
            .unwrap_err()
            .to_string();
        assert!(after.contains("after_seconds"), "got: {after}");

        let cron = Timing::Cron {
            expression: "not a cron".into(),
            time_zone: "UTC".into(),
        }
        .validate()
        .unwrap_err()
        .to_string();
        assert!(cron.contains("cron"), "got: {cron}");
    }

    /// `after` 一次性任务：触发一次后 status 必须变 ended。
    #[test]
    fn after_one_shot_ends_after_firing() {
        let mut task = Task {
            id: "t3".into(),
            name: "n".into(),
            action: TaskAction::Agent,
            prompt: "p".into(),
            model_sel: String::new(),
            timing: Timing::After { after_seconds: 60 },
            target: Target::Reuse,
            status: TaskStatus::Active,
            scheduled_at: Some("2026-01-01T00:01:00.000Z".into()),
            anchor_at: None,
            created_at: "2026-01-01T00:00:00.000Z".into(),
            last_run_at: None,
            run_count: 0,
            bound_session_id: None,
        };
        let now = at("2026-01-01T00:02:00.000Z");
        assert!(task.catch_up(now).is_some(), "must fire once");
        assert_eq!(task.status, TaskStatus::Ended);
        assert!(task.scheduled_at.is_none());
    }

    /// 重算下一次触发：改 timing 后 scheduled_at 必须跟着变。
    #[test]
    fn recompute_next_resets_schedule() {
        let mut task = Task {
            id: "t4".into(),
            name: "n".into(),
            action: TaskAction::Agent,
            prompt: "p".into(),
            model_sel: String::new(),
            timing: Timing::Every { every_seconds: 3600 },
            target: Target::Reuse,
            status: TaskStatus::Active,
            scheduled_at: Some("2026-01-01T00:00:00.000Z".into()),
            anchor_at: Some("2026-01-01T00:00:00.000Z".into()),
            created_at: "2026-01-01T00:00:00.000Z".into(),
            last_run_at: None,
            run_count: 0,
            bound_session_id: None,
        };
        let now = at("2026-01-01T00:10:00.000Z");
        task.timing = Timing::Every { every_seconds: 600 };
        task.recompute_next(now);
        let next = parse_instant(task.scheduled_at.as_deref().unwrap()).unwrap();
        assert_eq!(next.to_rfc3339(), "2026-01-01T00:20:00+00:00");
        // `every` 的相位锚点重置到新的首次提交时刻。
        assert_eq!(task.anchor_at.as_deref(), Some("2026-01-01T00:20:00.000Z"));
    }

    /// 恢复任务（pause → resume）：重算下一次但保留 anchor（相位不漂移）。
    #[test]
    fn resume_keeps_every_anchor() {
        let mut task = Task {
            id: "t5".into(),
            name: "n".into(),
            action: TaskAction::Agent,
            prompt: "p".into(),
            model_sel: String::new(),
            timing: Timing::Every { every_seconds: 600 },
            target: Target::Reuse,
            status: TaskStatus::Paused,
            scheduled_at: Some("2026-01-01T00:00:00.000Z".into()),
            anchor_at: Some("2026-01-01T00:00:00.000Z".into()),
            created_at: "2026-01-01T00:00:00.000Z".into(),
            last_run_at: None,
            run_count: 0,
            bound_session_id: None,
        };
        let now = at("2026-01-01T01:00:00.000Z");
        task.status = TaskStatus::Active;
        task.resume_next(now);
        // 锚点保持 00:00，下一次仍是整点后的第一个 10 分钟槽。
        assert_eq!(task.anchor_at.as_deref(), Some("2026-01-01T00:00:00.000Z"));
        let next = parse_instant(task.scheduled_at.as_deref().unwrap()).unwrap();
        assert_eq!(next.to_rfc3339(), "2026-01-01T01:10:00+00:00");
    }

    /// 运行历史：写入、读取、按 task 过滤、倒序。
    #[tokio::test]
    async fn runs_are_recorded_and_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::with_path(dir.path().join("schedule.json"));

        let mut r1 = RunRecord::new("t1", "first");
        r1.started_at = "2026-01-01T00:00:00.000Z".into();
        r1.ok = true;
        let mut r2 = RunRecord::new("t1", "first");
        r2.started_at = "2026-01-01T00:01:00.000Z".into();
        r2.ok = false;
        let mut r3 = RunRecord::new("t2", "second");
        r3.started_at = "2026-01-01T00:02:00.000Z".into();
        r3.ok = true;

        store.record_run(r1.clone()).await;
        store.record_run(r2.clone()).await;
        store.record_run(r3.clone()).await;

        // 全部：倒序（最新在前）。
        let all = store.runs_for(None).await;
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].task_id, "t2");
        assert_eq!(all[1].task_id, "t1");
        assert_eq!(all[2].started_at, "2026-01-01T00:00:00.000Z");

        // 按 task 过滤。
        let only = store.runs_for(Some("t1")).await;
        assert_eq!(only.len(), 2);
        assert!(only.iter().all(|r| r.task_id == "t1"));
        assert_eq!(only[0].started_at, "2026-01-01T00:01:00.000Z");
        assert!(!only[0].ok, "the failed run must stay flagged as failed");

        // 最近一次运行：用于列表接口的 lastRunOk。
        let last = store.last_runs().await;
        assert!(last.contains_key("t1"));
        assert!(!last["t1"].ok);
        assert!(last.contains_key("t2"));
        assert!(last["t2"].ok);
    }

    /// 运行历史上限 200 条：超限丢最旧。
    #[tokio::test]
    async fn run_history_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::with_path(dir.path().join("schedule.json"));
        for i in 0..(MAX_RUN_RECORDS + 25) {
            let mut r = RunRecord::new("t", "n");
            r.started_at = format!("2026-01-01T00:{:02}:{:02}.000Z", i / 60, i % 60);
            store.record_run(r).await;
        }
        let all = store.runs_for(None).await;
        assert_eq!(all.len(), MAX_RUN_RECORDS);
        // 最旧的 25 条被丢掉，最新的那条还在（倒序返回，最新在前）。
        assert_eq!(all[0].started_at, "2026-01-01T00:03:44.000Z");
        assert!(
            all.iter()
                .all(|r| r.started_at.as_str() >= "2026-01-01T00:00:25.000Z")
        );    }

    /// 运行历史要能跨进程重启读回来（runs.json 落盘）。
    #[tokio::test]
    async fn run_history_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("schedule.json");
        {
            let store = ScheduleStore::with_path(path.clone());
            let mut r = RunRecord::new("t9", "persisted");
            r.ok = true;
            r.exit_code = Some(0);
            r.output = "hello".into();
            store.record_run(r).await;
        }
        let store = ScheduleStore::with_path(path);
        let all = store.runs_for(None).await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].task_id, "t9");
        assert_eq!(all[0].exit_code, Some(0));
        assert_eq!(all[0].output, "hello");
    }

    /// 坏掉的 runs.json 只能丢历史，不能影响任务表。
    #[tokio::test]
    async fn corrupt_runs_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("schedule.json");
        std::fs::write(dir.path().join("runs.json"), "{ not json").unwrap();
        let store = ScheduleStore::with_path(path);
        assert!(store.runs_for(None).await.is_empty());
    }

    /// 输出截断只保留尾部 4KB，且不切坏多字节字符。
    #[test]
    fn tail_output_keeps_the_end() {
        let long = "答".repeat(10_000);
        let out = tail_output(&long);
        assert!(out.len() <= 4096 + 64, "too long: {}", out.len());
        assert!(out.ends_with(&"答".repeat(10)));
        // 短输出原样返回。
        assert_eq!(tail_output("short"), "short");
    }
}
