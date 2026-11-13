use serde::Serialize;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;

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
#[derive(Clone)]
pub struct EventHub {
    tx: broadcast::Sender<Event>,
    seq: Arc<AtomicU64>,
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
        ev
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
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
}
