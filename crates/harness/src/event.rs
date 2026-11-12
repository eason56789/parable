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
        let (tx, _) = broadcast::channel(4096);
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
