use serde::Serialize;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::broadcast;

/// 环形回放缓冲的容量。`text.delta` 每秒可达数百条，4k 条事件大约覆盖
/// 十几秒的流；单条 payload 平均几百字节，常驻内存不到 10 MB。
const RING_CAP: usize = 4096;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A single server -> frontend event. `payload` is flattened into the JSON object
/// so the frontend receives `{type, sessionId, seq, ts, ...}`.
#[derive(Clone, Debug, Serialize)]
pub struct Event {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub seq: u64,
    pub ts: u64,
    #[serde(flatten)]
    pub payload: Value,
}

/// Fan-out of state changes to every connected SSE client.
///
/// Besides the live `broadcast` channel the hub keeps a small ring of the most
/// recent events. A client that reconnects can ask for everything after the
/// last `seq` it saw instead of losing the gap: without it a dropped
/// `turn.end` (or the tail of a reasoning stream) stayed lost forever and the
/// UI sat on "思考中" until the user opened a brand-new tab.
#[derive(Clone)]
pub struct EventHub {
    tx: broadcast::Sender<Event>,
    seq: Arc<AtomicU64>,
    ring: Arc<StdMutex<VecDeque<Event>>>,
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

impl EventHub {
    pub fn new() -> Self {
        // `text.delta` fires once per token, so a single long answer can be
        // thousands of events. A small buffer made slow clients lag and lose
        // events — including `turn.end`, which left the UI stuck on "thinking"
        // forever. 64k events is a few MB at most and absorbs bursts.
        let (tx, _) = broadcast::channel(64 * 1024);
        Self {
            tx,
            seq: Arc::new(AtomicU64::new(1)),
            ring: Arc::new(StdMutex::new(VecDeque::with_capacity(64))),
        }
    }

    pub fn publish(&self, session_id: &str, kind: &str, payload: Value) -> Event {
        let ev = Event {
            kind: kind.to_string(),
            session_id: session_id.to_string(),
            seq: self.seq.fetch_add(1, Ordering::Relaxed),
            ts: now_ms(),
            payload,
        };
        // If nobody is listening the send fails; that is fine and expected.
        let _ = self.tx.send(ev.clone());
        if let Ok(mut ring) = self.ring.lock() {
            if ring.len() == RING_CAP {
                ring.pop_front();
            }
            ring.push_back(ev.clone());
        }
        ev
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// Subscribe and, in the same call, take a snapshot of every retained event
    /// with `seq > since`. Ordering matters: subscribing first guarantees that
    /// anything published between the snapshot and the stream is still
    /// delivered (duplicates are dropped by the client's `seq` watermark).
    pub fn subscribe_since(&self, since: u64) -> (Vec<Event>, broadcast::Receiver<Event>) {
        let rx = self.tx.subscribe();
        let backlog = match self.ring.lock() {
            Ok(ring) => ring
                .iter()
                .filter(|e| e.seq > since)
                .cloned()
                .collect::<Vec<_>>(),
            Err(poisoned) => poisoned
                .into_inner()
                .iter()
                .filter(|e| e.seq > since)
                .cloned()
                .collect(),
        };
        (backlog, rx)
    }

    /// Last sequence number handed out (0 when nothing was published yet).
    pub fn latest_seq(&self) -> u64 {
        self.seq.load(Ordering::Relaxed).saturating_sub(1)
    }

    /// How many events the replay buffer currently holds (test/diagnostic).
    pub fn buffered(&self) -> usize {
        self.ring.lock().map(|r| r.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_numbers_are_monotonic() {
        let hub = EventHub::new();
        let a = hub.publish("s", "turn.start", serde_json::json!({}));
        let b = hub.publish("s", "turn.end", serde_json::json!({}));
        assert!(b.seq > a.seq, "seq must increase");
    }

    #[tokio::test]
    async fn slow_subscriber_is_told_it_lagged() {
        // A subscriber that never reads must eventually get `Lagged` rather
        // than silently missing events — that silence is what left the UI
        // stuck on "thinking" (the missing turn.end never arrived).
        let hub = EventHub::new();
        let mut rx = hub.subscribe();
        // Fill well past the buffer capacity.
        for i in 0..70_000u32 {
            hub.publish("s", "text.delta", serde_json::json!({ "i": i }));
        }
        // Drain until the receiver reports the gap. Events still in the
        // buffer come first; the gap surfaces once those are consumed.
        let mut lagged = false;
        for _ in 0..200_000 {
            match rx.recv().await {
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    assert!(n > 0);
                    lagged = true;
                    break;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Ok(_) => {}
            }
        }
        assert!(lagged, "a stalled subscriber must be told it lagged");
    }

    #[tokio::test]
    async fn events_reach_a_prompt_subscriber() {
        let hub = EventHub::new();
        let mut rx = hub.subscribe();
        hub.publish("s1", "turn.end", serde_json::json!({ "ms": 12 }));
        let ev = rx.recv().await.expect("event must arrive");
        assert_eq!(ev.kind, "turn.end");
        assert_eq!(ev.session_id, "s1");
    }

    #[test]
    fn replay_covers_the_gap_after_a_reconnect() {
        let hub = EventHub::new();
        hub.publish("s", "turn.start", serde_json::json!({}));
        let seen = hub.latest_seq();
        // Everything below was published while the client was disconnected.
        hub.publish("s", "reasoning.delta", serde_json::json!({"text":"思"}));
        hub.publish("s", "text.delta", serde_json::json!({"text":"答"}));
        hub.publish("s", "turn.end", serde_json::json!({}));

        let (backlog, _rx) = hub.subscribe_since(seen);
        assert_eq!(backlog.len(), 3, "every missed event must be replayed");
        assert_eq!(backlog[0].kind, "reasoning.delta");
        assert_eq!(backlog[2].kind, "turn.end", "turn.end must survive a gap");
        assert!(backlog.iter().all(|e| e.seq > seen));
    }

    #[test]
    fn replay_buffer_is_bounded() {
        let hub = EventHub::new();
        for i in 0..(RING_CAP + 32) {
            hub.publish("s", "text.delta", serde_json::json!({ "i": i }));
        }
        assert_eq!(hub.buffered(), RING_CAP, "ring must not grow without a cap");
        let (backlog, _rx) = hub.subscribe_since(0);
        assert_eq!(backlog.len(), RING_CAP);
        // The oldest events were evicted, the newest are retained.
        assert_eq!(backlog.last().unwrap().kind, "text.delta");
        assert!(backlog.last().unwrap().seq > backlog[0].seq);
    }
}
