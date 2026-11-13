//! Persistent usage journal and aggregation.
//!
//! The session store only keeps *per-session cumulative* totals
//! (`UsageTotals` in `session.rs`), which is not enough for a usage chart:
//! there is no day granularity and no model attribution. This module keeps a
//! separate append-only journal — one line per turn — under
//! `~/.parable/usage/journal.jsonl`, and aggregates it into the snapshot the
//! settings page renders.
//!
//! Design notes:
//!
//! * The journal is **append-only**. One small `write + flush` per turn; no
//!   rewriting of the whole file, so it stays cheap no matter how long the
//!   history gets.
//! * A truncated last line (power loss mid-append) is skipped on load rather
//!   than failing the whole journal.
//! * Legacy sessions that predate the journal are imported **once**, on the
//!   first snapshot, guarded by the existence of the journal file. The import
//!   attributes a session's cumulative tokens to the session's last-updated
//!   day under a `__legacy__` bucket: the historical per-model split was
//!   never recorded, so it is not invented.
//!
//! All aggregation here is pure and synchronous over an in-memory `Vec`, so
//! the snapshot is cheap to compute and trivial to unit test.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use chrono::{Datelike, Timelike};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;

/// Model bucket used for sessions imported from before the journal existed.
pub const LEGACY_MODEL: &str = "__legacy__";

/// One turn's worth of usage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    /// Epoch milliseconds when the turn finished.
    pub ts: u64,
    /// Local calendar day, `YYYY-MM-DD`.
    pub day: String,
    pub session_id: String,
    /// Bare model id actually sent to the provider (e.g. `mimo-v2.6-pro`).
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    /// Wall-clock duration of the turn.
    #[serde(default)]
    pub wall_ms: u64,
    /// Number of model turns this record accounts for. A journal record is
    /// always 1; a legacy session record carries the session's turn count.
    #[serde(default = "one")]
    pub turns: u64,
}

fn one() -> u64 {
    1
}

impl UsageRecord {
    /// Total tokens attributed to this record.
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read + self.cache_write
    }}

/// Number of weeks shown in the heatmap (a GitHub-style year view).
const HEATMAP_WEEKS: usize = 53;
/// Days per week column.
const HEATMAP_DAYS: usize = 7;

/// The append-only journal plus its in-memory mirror.
pub struct UsageJournal {
    dir: PathBuf,
    file: PathBuf,
    /// Sentinel written once the legacy session import has run. Kept separate
    /// from the journal file: a journal that already holds real turns must
    /// not make the legacy import look "already done".
    marker: PathBuf,
    records: Mutex<Vec<UsageRecord>>,
    /// Set once the legacy import has run, so it never runs twice.
    backfilled: Mutex<bool>,
}

impl UsageJournal {
    /// Create a journal rooted at `dir` (typically `~/.parable/usage`).
    ///
    /// The directory is created eagerly and the journal file is read back
    /// right away, so the usage history survives a restart.
    pub fn new(dir: PathBuf) -> Self {
        let file = dir.join("journal.jsonl");
        let marker = dir.join("legacy-imported");
        let _ = std::fs::create_dir_all(&dir);
        let records = load_journal_file(&file);
        Self {
            dir,
            file,
            marker,
            records: Mutex::new(records),
            backfilled: Mutex::new(false),
        }
    }

    /// Directory holding the journal.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Record one turn. Appends a single JSON line to the journal file.
    ///
    /// A write failure is logged, never propagated: losing a usage line must
    /// not fail the turn that produced it.
    pub async fn record(&self, rec: UsageRecord) {
        let mut line = match serde_json::to_string(&rec) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("usage journal: serialize failed: {e}");
                return;
            }
        };
        line.push('\n');
        {
            let mut recs = self.records.lock().await;
            recs.push(rec);
        }
        if let Err(e) = append_line(&self.file, line.as_bytes()) {
            tracing::error!("usage journal: append failed: {e}");
        }
    }

    /// All records, oldest first.
    pub async fn records(&self) -> Vec<UsageRecord> {
        self.records.lock().await.clone()
    }

    /// Read the journal file into memory. Corrupt lines are skipped.
    #[cfg(test)]
    fn load_from_disk(&self) -> Vec<UsageRecord> {
        load_journal_file(&self.file)
    }

    /// Import usage from session files written before the journal existed.
    ///
    /// Idempotent: the sentinel file records that the import already ran, so
    /// the second call is a no-op. Each session contributes one record
    /// carrying its cumulative totals, attributed to the day the session was
    /// last updated.
    pub async fn backfill_legacy(&self, sessions_root: &Path) {
        {
            let done = self.backfilled.lock().await;
            if *done {
                return;
            }
        }
        // The sentinel — not the journal file — marks the import as done.
        // A journal that already holds real turns must still get the legacy
        // sessions imported, otherwise their tokens are lost from the total.
        if self.marker.exists() {
            let mut done = self.backfilled.lock().await;
            *done = true;
            return;
        }
        let imported = import_sessions(sessions_root);
        if !imported.is_empty() {
            let mut lines = String::new();
            for r in &imported {
                if let Ok(line) = serde_json::to_string(r) {
                    lines.push_str(&line);
                    lines.push('\n');
                }
            }
            if let Err(e) = append_line(&self.file, lines.as_bytes()) {
                tracing::error!("usage journal: legacy append failed: {e}");
            }
            let mut recs = self.records.lock().await;
            recs.extend(imported);
        }
        // Write the sentinel last: if the import crashes midway the next run
        // retries instead of silently losing the legacy sessions.
        if let Err(e) = std::fs::write(&self.marker, b"1") {
            tracing::warn!("usage journal: could not write legacy-import marker: {e}");
        }
        let mut done = self.backfilled.lock().await;
        *done = true;
    }

    /// Aggregate the journal into the snapshot the settings page renders.
    ///
    /// `summary` and `heatmap` are always all-time; `daily` and `models` are
    /// scoped to `range` (`all`, `7d` or `30d`).
    pub async fn snapshot(&self, range: &str, sessions_root: &Path) -> Value {
        self.backfill_legacy(sessions_root).await;
        let recs = self.records.lock().await.clone();
        build_snapshot(&recs, range, crate::event::now_ms())
    }
}

/// Read a journal file into records. Corrupt lines are skipped.
fn load_journal_file(path: &Path) -> Vec<UsageRecord> {
    let mut out = Vec::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<UsageRecord>(line) {
            Ok(r) => out.push(r),
            // A partially written last line (power loss) is expected;
            // anything else is worth a warning but must not be fatal.
            Err(e) => tracing::warn!("usage journal: skipping unparseable line: {e}"),
        }
    }
    out
}

/// Append bytes to a file, creating it if needed.
fn append_line(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(bytes)?;
    f.flush()
}

/// Import cumulative usage from legacy session files.
fn import_sessions(root: &Path) -> Vec<UsageRecord> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(u) = v.get("usage") else { continue };
        let ts = u.get("wall_ms").and_then(|x| x.as_u64()).unwrap_or(0);
        let input = u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
        let output = u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
        let cache_read = u.get("cache_read").and_then(|x| x.as_u64()).unwrap_or(0);
        let cache_write = u.get("cache_write").and_then(|x| x.as_u64()).unwrap_or(0);
        let turns = u.get("turns").and_then(|x| x.as_u64()).unwrap_or(0);
        if input + output + cache_read + cache_write == 0 {
            continue;
        }
        let id = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        // The session's last-updated day is the best available anchor for
        // when those tokens were spent.
        let updated = v.get("updated_at").and_then(|x| x.as_u64()).unwrap_or(0);
        let ts = if updated > 0 { updated } else { ts };
        out.push(UsageRecord {
            ts,
            day: day_key(ts),
            session_id: id,
            model: LEGACY_MODEL.to_string(),
            input_tokens: input,
            output_tokens: output,
            cache_read,
            cache_write,
            // The session's cumulative turns, so the imported record keeps
            // the true turn count instead of collapsing to one.
            turns,
            // wall_ms is the session's total across all turns; keep it so
            // `longestSessionMs` reflects the real chat length.
            wall_ms: u.get("wall_ms").and_then(|x| x.as_u64()).unwrap_or(0),
        });
    }
    out
}

/// Local calendar day key (`YYYY-MM-DD`) for an epoch-millisecond timestamp.
pub fn day_key(ts: u64) -> String {
    let dt = chrono::DateTime::from_timestamp_millis(ts as i64)
        .unwrap_or_else(|| chrono::DateTime::from_timestamp_millis(0).unwrap());
    dt.with_timezone(&chrono::Local)
        .format("%Y-%m-%d")
        .to_string()
}

/// The day key for "now" (local time).
pub fn today_key() -> String {
    day_key(crate::event::now_ms())
}

/// Days between two `YYYY-MM-DD` keys (b - a).
fn days_between(a: &str, b: &str) -> i64 {
    match (
        chrono::NaiveDate::parse_from_str(a, "%Y-%m-%d"),
        chrono::NaiveDate::parse_from_str(b, "%Y-%m-%d"),
    ) {
        (Ok(x), Ok(y)) => (y - x).num_days(),
        _ => 0,
    }
}

/// Per-day totals.
#[derive(Default, Clone)]
struct DayTotals {
    total: u64,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    turns: u64,
    by_model: HashMap<String, u64>,
}

/// Per-session totals, used for the "top sessions" list.
#[derive(Default, Clone)]
struct SessionTotals {
    total: u64,
    input: u64,
    output: u64,
    turns: u64,
    wall_ms: u64,
    first_ts: u64,
    last_ts: u64,
    by_model: HashMap<String, u64>,
}

/// Accumulates tokens by hour of day and by weekday, for the activity charts.
///
/// Both are indexed 0..24 (hours) and 0..7 (weekday, Monday = 0).
#[derive(Default)]
struct Activity {
    hours: [u64; 24],
    hour_turns: [u64; 24],
    weekdays: [u64; 7],
    weekday_turns: [u64; 7],
}

impl Activity {
    fn add(&mut self, rec: &UsageRecord) {
        // The timestamp is the source of truth for "when did this happen";
        // the `day` key is only the calendar bucket.
        let dt = chrono::DateTime::from_timestamp_millis(rec.ts as i64)
            .map(|d| d.with_timezone(&chrono::Local));
        let Some(dt) = dt else { return };
        let h = dt.hour() as usize;
        let w = dt.weekday().num_days_from_monday() as usize;
        let t = rec.total();
        self.hours[h] += t;
        self.hour_turns[h] += rec.turns.max(1);
        self.weekdays[w] += t;
        self.weekday_turns[w] += rec.turns.max(1);
    }
}

/// Build the full snapshot from the records. Pure function — unit tested.
fn build_snapshot(recs: &[UsageRecord], range: &str, now: u64) -> Value {
    let today = day_key(now);
    let today_date = chrono::NaiveDate::parse_from_str(&today, "%Y-%m-%d").ok();

    // ---- per-day and per-session aggregation over the whole history
    let mut days: BTreeMap<String, DayTotals> = BTreeMap::new();
    let mut session_ms: HashMap<&str, u64> = HashMap::new();
    let mut session_tokens: HashMap<&str, u64> = HashMap::new();
    let mut sessions: HashSet<&str> = HashSet::new();
    let mut total_tokens: u64 = 0;
    let mut total_input: u64 = 0;
    let mut total_output: u64 = 0;
    let mut total_cache_read: u64 = 0;
    let mut total_cache_write: u64 = 0;
    let mut total_turns: u64 = 0;
    let mut peak_day: u64 = 0;
    let mut activity = Activity::default();
    let mut session_detail: HashMap<String, SessionTotals> = HashMap::new();

    for r in recs {
        let t = r.total();
        total_tokens += t;
        total_input += r.input_tokens;
        total_output += r.output_tokens;
        total_cache_read += r.cache_read;
        total_cache_write += r.cache_write;
        total_turns += r.turns.max(1);
        sessions.insert(r.session_id.as_str());
        *session_ms.entry(r.session_id.as_str()).or_insert(0) += r.wall_ms;
        *session_tokens.entry(r.session_id.as_str()).or_insert(0) += t;
        activity.add(r);

        let sd = session_detail.entry(r.session_id.clone()).or_default();
        sd.total += t;
        sd.input += r.input_tokens;
        sd.output += r.output_tokens;
        sd.turns += r.turns.max(1);
        sd.wall_ms += r.wall_ms;
        if sd.first_ts == 0 || r.ts < sd.first_ts {
            sd.first_ts = r.ts;
        }
        if r.ts > sd.last_ts {
            sd.last_ts = r.ts;
        }
        *sd.by_model.entry(r.model.clone()).or_insert(0) += t;

        let d = days.entry(r.day.clone()).or_default();
        d.total += t;
        d.input += r.input_tokens;
        d.output += r.output_tokens;
        d.cache_read += r.cache_read;
        d.cache_write += r.cache_write;
        d.turns += r.turns.max(1);
        *d.by_model.entry(r.model.clone()).or_insert(0) += t;
        if d.total > peak_day {
            peak_day = d.total;
        }
    }

    // ---- streaks
    let active_days: Vec<&String> = days.keys().collect();
    let (current_streak, longest_streak) = streaks(&active_days, &today);
    let longest_session_ms = session_ms.values().copied().max().unwrap_or(0);

    let cache_denom = total_input + total_cache_read + total_cache_write;
    let cache_hit_rate = if cache_denom == 0 {
        0.0
    } else {
        total_cache_read as f64 / cache_denom as f64
    };

    // ---- heatmap: last 53 weeks ending on the current week
    let heatmap = build_heatmap(&days, today_date, &today);

    // ---- range scoping for the charts
    let range_days: Option<i64> = match range {
        "7d" => Some(7),
        "30d" => Some(30),
        _ => None,
    };
    let cutoff = range_days.and_then(|n| {
        today_date
            .map(|d| d - chrono::Duration::days(n - 1))
            .map(|d| d.format("%Y-%m-%d").to_string())
    });

    let mut daily: Vec<Value> = Vec::new();
    for (day, d) in days.iter() {
        if let Some(c) = &cutoff {
            if day.as_str() < c.as_str() {
                continue;
            }
        }
        let models: Vec<Value> = {
            let mut v: Vec<(&String, &u64)> = d.by_model.iter().collect();
            v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            v.into_iter()
                .map(|(m, t)| serde_json::json!({"modelId": m, "totalTokens": t}))
                .collect()
        };
        daily.push(serde_json::json!({
            "date": day,
            "totalTokens": d.total,
            "inputTokens": d.input,
            "outputTokens": d.output,
            "cacheRead": d.cache_read,
            "cacheWrite": d.cache_write,
            "turns": d.turns,
            "models": models,
        }));
    }
    // Per-model totals need the true per-model split, not the day's
    // aggregate — recompute from the records for the selected range.
    let mut model_totals: HashMap<String, (u64, u64, u64)> = HashMap::new();
    let mut model_requests: HashMap<String, u64> = HashMap::new();
    for r in recs {
        if let Some(c) = &cutoff {
            if r.day.as_str() < c.as_str() {
                continue;
            }
        }
        let e = model_totals.entry(r.model.clone()).or_insert((0, 0, 0));
        e.0 += r.total();
        e.1 += r.input_tokens;
        e.2 += r.output_tokens;
        *model_requests.entry(r.model.clone()).or_insert(0) += 1;
    }
    let range_total: u64 = model_totals.values().map(|v| v.0).sum();
    let mut models: Vec<Value> = model_totals
        .into_iter()
        .map(|(m, v)| {
            let share = if range_total == 0 {
                0.0
            } else {
                v.0 as f64 / range_total as f64
            };
            serde_json::json!({
                "modelId": m,
                "totalTokens": v.0,
                "inputTokens": v.1,
                "outputTokens": v.2,
                "requestCount": model_requests.get(&m).copied().unwrap_or(0),
                "share": share,
            })
        })
        .collect();
    models.sort_by(|a, b| {
        b.get("totalTokens")
            .and_then(|v| v.as_u64())
            .cmp(&a.get("totalTokens").and_then(|v| v.as_u64()))
            .then(
                a.get("modelId")
                    .and_then(|v| v.as_str())
                    .cmp(&b.get("modelId").and_then(|v| v.as_str())),
            )
    });

    // Top sessions by token spend, scoped to the same range as the charts.
    // A session's cost is attributed to the session as a whole — a chat that
    // started before the cutoff still shows up if it was active inside it.
    let in_range: HashSet<&str> = match &cutoff {
        Some(c) => recs
            .iter()
            .filter(|r| r.day.as_str() >= c.as_str())
            .map(|r| r.session_id.as_str())
            .collect(),
        None => recs.iter().map(|r| r.session_id.as_str()).collect(),
    };
    let mut top_sessions: Vec<(String, SessionTotals)> = session_detail
        .into_iter()
        .filter(|(id, _)| in_range.contains(id.as_str()))
        .collect();
    top_sessions.sort_by(|a, b| b.1.total.cmp(&a.1.total).then(a.0.cmp(&b.0)));
    top_sessions.truncate(10);
    let top_sessions: Vec<Value> = top_sessions
        .into_iter()
        .map(|(id, s)| {
            let model = s
                .by_model
                .iter()
                .max_by_key(|(_, v)| **v)
                .map(|(m, _)| m.clone())
                .unwrap_or_default();
            serde_json::json!({
                "sessionId": id,
                "totalTokens": s.total,
                "inputTokens": s.input,
                "outputTokens": s.output,
                "turns": s.turns,
                "wallMs": s.wall_ms,
                "firstTs": s.first_ts,
                "lastTs": s.last_ts,
                "model": model,
            })
        })
        .collect();

    let hours: Vec<Value> = activity
        .hours
        .iter()
        .enumerate()
        .map(|(i, v)| serde_json::json!({"tokens": v, "turns": activity.hour_turns[i]}))
        .collect();
    let weekdays: Vec<Value> = activity
        .weekdays
        .iter()
        .enumerate()
        .map(|(i, v)| serde_json::json!({"tokens": v, "turns": activity.weekday_turns[i]}))
        .collect();

    serde_json::json!({
        "range": range,
        "generatedAt": now,
        "summary": {
            "totalTokens": total_tokens,
            "inputTokens": total_input,
            "outputTokens": total_output,
            "cacheReadTokens": total_cache_read,
            "cacheCreationTokens": total_cache_write,
            "cacheHitRate": (cache_hit_rate * 10000.0).round() / 10000.0,
            "totalSessions": sessions.len(),
            "totalTurns": total_turns,
            "activeDays": days.len(),
            "currentStreakDays": current_streak,
            "longestStreakDays": longest_streak,
            "longestSessionMs": longest_session_ms,
            "peakDayTokens": peak_day,
        },
        "heatmap": heatmap,
        "daily": daily,
        "models": models,
        "activity": {
            "hours": hours,
            "weekdays": weekdays,
        },
        "topSessions": top_sessions,
    })
}

/// Current and longest run of consecutive active days.
///
/// The current streak is anchored on today; if today has no activity yet it
/// still counts a streak that ended yesterday (the usual "you haven't used it
/// *yet* today" leniency).
fn streaks(active_days: &[&String], today: &str) -> (u64, u64) {
    if active_days.is_empty() {
        return (0, 0);
    }
    // Longest run anywhere.
    let mut longest = 0u64;
    let mut run = 0u64;
    let mut prev: Option<String> = None;
    for d in active_days {
        if let Some(p) = &prev {
            if days_between(p, d) == 1 {
                run += 1;
            } else {
                run = 1;
            }
        } else {
            run = 1;
        }
        if run > longest {
            longest = run;
        }
        prev = Some((*d).clone());
    }

    // Current streak: walk backwards from today (or yesterday).
    let set: std::collections::HashSet<&str> = active_days.iter().map(|d| d.as_str()).collect();
    let today_date = chrono::NaiveDate::parse_from_str(today, "%Y-%m-%d").ok();
    let mut anchor = today_date;
    if let Some(d) = today_date {
        if !set.contains(today) {
            // Try yesterday.
            let y = (d - chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
            if set.contains(y.as_str()) {
                anchor = Some(d - chrono::Duration::days(1));
            } else {
                return (0, longest);
            }
        }
    }
    let Some(mut cur) = anchor else {
        return (0, longest);
    };
    let mut current = 0u64;
    loop {
        let key = cur.format("%Y-%m-%d").to_string();
        if set.contains(key.as_str()) {
            current += 1;
            cur -= chrono::Duration::days(1);
        } else {
            break;
        }
    }
    (current, longest)
}

/// GitHub-style heatmap for the last 53 weeks.
fn build_heatmap(
    days: &BTreeMap<String, DayTotals>,
    today: Option<chrono::NaiveDate>,
    today_key: &str,
) -> Value {
    if today.is_none() {
        return serde_json::json!({
            "startDate": Value::Null, "endDate": Value::Null,
            "maxTokens": 0, "weeks": []
        });
    }
    let today = today.unwrap();
    // The heatmap ends on the Saturday of the current week (GitHub style),
    // but days after today are rendered as null.
    let end = today + chrono::Duration::days(6 - today.weekday().num_days_from_monday() as i64);
    let start = end - chrono::Duration::days(HEATMAP_WEEKS as i64 * HEATMAP_DAYS as i64 - 1);

    // Level ramp: quartiles over the non-zero days.
    let mut totals: Vec<u64> = days
        .iter()
        .filter(|(_, d)| d.total > 0)
        .map(|(_, d)| d.total)
        .collect();
    totals.sort_unstable();
    let max_tokens = totals.last().copied().unwrap_or(0);
    let q = |p: f64| -> u64 {
        if totals.is_empty() {
            return 0;
        }
        let idx = ((totals.len() as f64 - 1.0) * p).round() as usize;
        totals[idx.min(totals.len() - 1)]
    };
    let (q1, q2, q3) = (q(0.25), q(0.5), q(0.75));

    let mut weeks: Vec<Value> = Vec::new();
    for w in 0..HEATMAP_WEEKS {
        let col_start = start + chrono::Duration::days((w * HEATMAP_DAYS) as i64);
        let mut cells: Vec<Value> = Vec::new();
        for d in 0..HEATMAP_DAYS {
            let day = col_start + chrono::Duration::days(d as i64);
            let key = day.format("%Y-%m-%d").to_string();
            if day > today {
                // Beyond today — no cell.
                cells.push(Value::Null);
                continue;
            }
            let t = days.get(&key);
            let total = t.map(|x| x.total).unwrap_or(0);
            let turns = t.map(|x| x.turns).unwrap_or(0);
            let level = if total == 0 {
                0
            } else if total <= q1 {
                1
            } else if total <= q2 {
                2
            } else if total <= q3 {
                3
            } else {
                4
            };
            cells.push(serde_json::json!({
                "date": key,
                "level": level,
                "totalTokens": total,
                "turnCount": turns,
            }));
        }
        weeks.push(serde_json::json!({"weekIndex": w, "days": cells}));
    }
    let _ = today_key;
    serde_json::json!({
        "startDate": start.format("%Y-%m-%d").to_string(),
        "endDate": end.format("%Y-%m-%d").to_string(),
        "maxTokens": max_tokens,
        "weeks": weeks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn rec(day: &str, session: &str, model: &str, total: u64, wall_ms: u64) -> UsageRecord {
        UsageRecord {
            ts: 0,
            day: day.to_string(),
            session_id: session.to_string(),
            model: model.to_string(),
            input_tokens: total,
            output_tokens: 0,
            cache_read: 0,
            cache_write: 0,
            wall_ms,
            turns: 1,
        }
    }

    fn summary_of(v: &Value) -> &Value {
        &v["summary"]
    }

    #[test]
    fn streaks_count_consecutive_days() {
        let owned = ["2026-11-01", "2026-11-02", "2026-11-03"]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<String>>();
        let refs: Vec<&String> = owned.iter().collect();
        let (cur, longest) = streaks(&refs, "2026-11-03");
        assert_eq!(longest, 3);
        assert_eq!(cur, 3);
    }

    #[test]
    fn streaks_break_on_gap() {
        let d1 = "2026-11-01".to_string();
        let d3 = "2026-11-03".to_string();
        let refs = vec![&d1, &d3];
        let (cur, longest) = streaks(&refs, "2026-11-03");
        assert_eq!(longest, 1);
        assert_eq!(cur, 1);
    }

    #[test]
    fn current_streak_allows_today_to_be_quiet() {
        // Active yesterday and the day before; today has no activity yet.
        let d1 = "2026-11-01".to_string();
        let d2 = "2026-11-02".to_string();
        let refs = vec![&d1, &d2];
        let (cur, longest) = streaks(&refs, "2026-11-03");
        assert_eq!(longest, 2);
        // The streak ending yesterday still counts as the current streak.
        assert_eq!(cur, 2);
    }

    #[test]
    fn snapshot_buckets_by_day_and_finds_peak() {
        let recs = vec![
            rec("2026-11-01", "s1", "m1", 100, 1_000),
            rec("2026-11-01", "s1", "m1", 50, 500),
            rec("2026-11-02", "s2", "m2", 300, 2_000),
        ];
        let v = build_snapshot(&recs, "all", 0);
        let s = summary_of(&v);
        assert_eq!(s["totalTokens"], 450u64);
        // The busiest day is 2026-11-02 with 300 tokens.
        assert_eq!(s["peakDayTokens"], 300u64);
        assert_eq!(s["totalSessions"], 2u64);
        assert_eq!(s["totalTurns"], 3u64);
        assert_eq!(s["activeDays"], 2u64);
        // The longest chat is the session with the most wall time.
        assert_eq!(s["longestSessionMs"], 2_000u64);
        let daily = v["daily"].as_array().unwrap();
        assert_eq!(daily.len(), 2);
        assert_eq!(daily[0]["date"], "2026-11-01");
        assert_eq!(daily[0]["totalTokens"], 150u64);
        assert_eq!(daily[1]["date"], "2026-11-02");
        assert_eq!(daily[1]["totalTokens"], 300u64);
    }

    #[test]
    fn model_shares_sum_to_one() {
        let recs = vec![
            rec("2026-11-01", "s1", "m1", 100, 10),
            rec("2026-11-02", "s1", "m2", 300, 10),
        ];
        let v = build_snapshot(&recs, "all", 0);
        let models = v["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        let sum: f64 = models.iter().map(|m| m["share"].as_f64().unwrap()).sum();
        assert!((sum - 1.0).abs() < 1e-9, "shares must sum to 1, got {sum}");
        // Sorted by tokens descending.
        assert_eq!(models[0]["modelId"], "m2");
        assert_eq!(models[0]["totalTokens"], 300u64);
        assert_eq!(models[1]["modelId"], "m1");
    }

    #[test]
    fn range_scopes_the_charts() {
        // `now` must be a real timestamp: the range cutoff is computed
        // relative to "today", so a zero timestamp would scope against 1970.
        let now = crate::event::now_ms();
        let today = today_key();
        // One record today, one far in the past.
        let old = chrono::Local::now() - chrono::Duration::days(90);
        let recs = vec![
            rec(&today, "s1", "m1", 100, 10),
            rec(&old.format("%Y-%m-%d").to_string(), "s2", "m2", 900, 10),
        ];
        let all = build_snapshot(&recs, "all", now);
        assert_eq!(all["daily"].as_array().unwrap().len(), 2);
        let d7 = build_snapshot(&recs, "7d", now);
        assert_eq!(d7["daily"].as_array().unwrap().len(), 1);
        assert_eq!(d7["models"].as_array().unwrap().len(), 1);
        assert_eq!(d7["models"][0]["modelId"], "m1");
        // The summary is always all-time.
        assert_eq!(summary_of(&d7)["totalTokens"], 1000u64);
    }

    #[test]
    fn heatmap_has_53_weeks_of_7_days() {
        let recs = vec![rec(&today_key(), "s1", "m1", 100, 10)];
        let v = build_snapshot(&recs, "all", 0);
        let hm = &v["heatmap"];
        let weeks = hm["weeks"].as_array().unwrap();
        assert_eq!(weeks.len(), HEATMAP_WEEKS);
        assert_eq!(weeks[0]["days"].as_array().unwrap().len(), HEATMAP_DAYS);
        // Cells after today are null.
        let last = weeks[HEATMAP_WEEKS - 1]["days"].as_array().unwrap();
        assert!(last.iter().any(|c| c.is_null()));
    }

    #[test]
    fn journal_appends_and_reloads() {
        let dir = std::env::temp_dir().join(format!("parable-usage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let j = UsageJournal::new(dir.clone());
        let r = rec("2026-11-01", "s1", "m1", 42, 7);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                j.record(r.clone()).await;
            });
        let loaded = j.load_from_disk();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].total(), 42);
        // A truncated last line must not break loading.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("journal.jsonl"))
            .unwrap();
        f.write_all(b"{\"ts\":1,\"day\":\"2026-").unwrap();
        drop(f);
        let loaded = j.load_from_disk();
        assert_eq!(loaded.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn journal_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("parable-usage2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        {
            let j = UsageJournal::new(dir.clone());
            rt.block_on(async {
                j.record(rec("2026-11-01", "s1", "m1", 42, 7)).await;
            });
        }
        // A fresh journal over the same directory must see the old record.
        let j2 = UsageJournal::new(dir.clone());
        rt.block_on(async {
            let recs = j2.records().await;
            assert_eq!(recs.len(), 1);
            assert_eq!(recs[0].total(), 42);
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_import_runs_even_when_the_journal_already_has_turns() {
        // Regression: the import used to be skipped whenever journal.jsonl
        // existed, so a journal that already held real turns silently lost
        // every legacy session's tokens.
        let base = std::env::temp_dir().join(format!("parable-usagelg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let sessions = base.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("s_abc.json"),
            r#"{"id":"s_abc","updated_at":1767225600000,"messages":[],
                "usage":{"input_tokens":100,"output_tokens":20,"cache_read":0,
                "cache_write":0,"turns":2,"steps":3,"wall_ms":4000}}"#,
        )
        .unwrap();
        let j = UsageJournal::new(base.join("usage"));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // A real turn arrives first — the journal file now exists.
            j.record(rec("2026-11-13", "s_new", "m1", 7, 5)).await;
            j.backfill_legacy(&sessions).await;
            // Both the new turn and the legacy session must be counted.
            let snap = j.snapshot("all", &sessions).await;
            assert_eq!(snap["summary"]["totalTokens"], 127u64);
            assert_eq!(snap["summary"]["totalTurns"], 3u64);
            assert_eq!(snap["models"].as_array().unwrap().len(), 2);
        });
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn legacy_import_runs_once_and_is_not_double_counted() {
        let base = std::env::temp_dir().join(format!("parable-usagelt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let sessions = base.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("s_abc.json"),
            r#"{"id":"s_abc","title":"t","updated_at":1767225600000,
                "messages":[],"usage":{"input_tokens":100,"output_tokens":20,
                "cache_read":0,"cache_write":0,"turns":3,"steps":5,"wall_ms":4000}}"#,
        )
        .unwrap();
        let j = UsageJournal::new(base.join("usage"));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            j.backfill_legacy(&sessions).await;
            let first = j.records().await.len();
            assert_eq!(first, 1);
            // A second call must not import anything again.
            j.backfill_legacy(&sessions).await;
            assert_eq!(j.records().await.len(), 1);
            let snap = j.snapshot("all", &sessions).await;
            assert_eq!(snap["summary"]["totalTokens"], 120u64);
            assert_eq!(snap["models"][0]["modelId"], LEGACY_MODEL);
            assert_eq!(snap["summary"]["longestSessionMs"], 4000u64);
            // The imported record keeps the session's real turn count.
            assert_eq!(snap["summary"]["totalTurns"], 3u64);
        });
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A record with an explicit timestamp, for the activity charts.
    fn rec_at(ts: u64, session: &str, model: &str, total: u64) -> UsageRecord {
        let mut r = rec(&day_key(ts), session, model, total, 100);
        r.ts = ts;
        r
    }

    #[test]
    fn activity_buckets_by_hour_and_weekday() {
        // 2026-11-09 is a Monday; 10:00 local.
        let monday_10 = chrono::Local
            .with_ymd_and_hms(2026, 11, 9, 10, 0, 0)
            .unwrap()
            .timestamp_millis() as u64;
        // The next day, Tuesday, 14:00.
        let tuesday_14 = chrono::Local
            .with_ymd_and_hms(2026, 11, 10, 14, 0, 0)
            .unwrap()
            .timestamp_millis() as u64;

        let recs = vec![
            rec_at(monday_10, "s1", "m1", 100),
            rec_at(tuesday_14, "s1", "m1", 250),
        ];
        let v = build_snapshot(&recs, "all", tuesday_14);
        let act = &v["activity"];
        let hours = act["hours"].as_array().unwrap();
        assert_eq!(hours.len(), 24);
        // Every hour is present, so the chart never has holes.
        assert!(hours.iter().all(|h| h["tokens"].is_u64()));
        let sum: u64 = hours.iter().map(|h| h["tokens"].as_u64().unwrap()).sum();
        assert_eq!(sum, 350u64);

        let weekdays = act["weekdays"].as_array().unwrap();
        assert_eq!(weekdays.len(), 7);
        // Monday = index 0, Tuesday = index 1.
        assert_eq!(weekdays[0]["tokens"], 100u64);
        assert_eq!(weekdays[1]["tokens"], 250u64);
        assert_eq!(weekdays[0]["turns"], 1u64);
    }

    #[test]
    fn top_sessions_are_ranked_and_capped() {
        let now = crate::event::now_ms();
        let today = today_key();
        let mut recs = Vec::new();
        // s1 is the heaviest, s2 next, s3 the lightest.
        recs.push(rec(&today, "s1", "m1", 900, 100));
        recs.push(rec(&today, "s2", "m2", 500, 100));
        recs.push(rec(&today, "s3", "m1", 100, 100));
        // Twelve more sessions, all cheap: only the top ten survive.
        for i in 0..12 {
            recs.push(rec(&today, &format!("s_extra{i}"), "m1", 50, 10));
        }
        let v = build_snapshot(&recs, "all", now);
        let top = v["topSessions"].as_array().unwrap();
        assert_eq!(top.len(), 10);
        // Sorted by tokens descending.
        assert_eq!(top[0]["sessionId"], "s1");
        assert_eq!(top[0]["totalTokens"], 900u64);
        assert_eq!(top[1]["sessionId"], "s2");
        assert_eq!(top[2]["sessionId"], "s3");
        // The dominant model is reported for the session.
        assert_eq!(top[0]["model"], "m1");
        assert_eq!(top[1]["model"], "m2");
    }

    #[test]
    fn top_sessions_respect_the_range() {
        let now = crate::event::now_ms();
        let today = today_key();
        let old = (chrono::Local::now() - chrono::Duration::days(90))
            .format("%Y-%m-%d")
            .to_string();
        let recs = vec![
            rec(&today, "s_new", "m1", 100, 10),
            rec(&old, "s_old", "m2", 900, 10),
        ];
        // "all" shows both, "7d" only the recent one.
        let all = build_snapshot(&recs, "all", now);
        assert_eq!(all["topSessions"].as_array().unwrap().len(), 2);
        let d7 = build_snapshot(&recs, "7d", now);
        let top = d7["topSessions"].as_array().unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0]["sessionId"], "s_new");
    }
}
