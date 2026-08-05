//! In-process peer messaging bus for a root parent session and its subagents.
//!
//! One [`AgentBus`] is shared (`Arc`) by Main and all depth-1 children. It is
//! **not** the workspace remote hub (`xai-grok-workspace::hub`).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::Notify;

/// Soft cap on message body size (UTF-8 bytes).
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024;
/// Per-peer mailbox depth; oldest messages are dropped when full.
pub const MAX_MAILBOX_DEPTH: usize = 32;
/// Soft rate limit: max sends per sender per window.
const RATE_LIMIT_COUNT: u32 = 20;
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);

/// Stable roster id for the root parent agent.
pub const MAIN_PEER_ID: &str = "Main";

/// Optional wake hook: deliver a human-readable interjection body to a peer's
/// live session (typically via `SessionCommand::Interject`).
pub type PeerWakeFn = Arc<dyn Fn(String) + Send + Sync>;

/// Status of a roster entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerStatus {
    Running,
    Gone,
}

/// One peer on the bus roster.
#[derive(Clone)]
pub struct PeerInfo {
    pub id: String,
    pub label: String,
    pub status: PeerStatus,
    /// When the peer registered (or was last refreshed).
    pub registered_at: Instant,
    wake: Option<PeerWakeFn>,
}

impl std::fmt::Debug for PeerInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerInfo")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("status", &self.status)
            .field("has_wake", &self.wake.is_some())
            .finish()
    }
}

/// A single hub message.
#[derive(Debug, Clone)]
pub struct AgentHubMessage {
    pub id: String,
    pub from: String,
    pub to: String,
    pub text: String,
    pub reply_to: Option<String>,
    pub ts_unix_ms: u64,
}

/// Outcome of [`AgentBus::send`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    Delivered { message_id: String },
    Failed { reason: String },
}

struct RateWindow {
    window_start: Instant,
    count: u32,
}

struct BusInner {
    peers: HashMap<String, PeerInfo>,
    mailboxes: HashMap<String, VecDeque<AgentHubMessage>>,
    rates: HashMap<String, RateWindow>,
    next_msg: AtomicU64,
}

/// Session-scoped peer bus. Clone is cheap (`Arc`).
#[derive(Clone)]
pub struct AgentBus {
    inner: Arc<Mutex<BusInner>>,
    /// Wakes waiters when any mailbox receives a message.
    notify: Arc<Notify>,
}

impl Default for AgentBus {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentBus {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(BusInner {
                peers: HashMap::new(),
                mailboxes: HashMap::new(),
                rates: HashMap::new(),
                next_msg: AtomicU64::new(1),
            })),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Register or refresh a peer. `wake` is invoked (best-effort) when a
    /// message is delivered while the peer is `Running`.
    pub fn register(&self, id: impl Into<String>, label: impl Into<String>, wake: Option<PeerWakeFn>) {
        let id = id.into();
        let label = label.into();
        let mut g = self.inner.lock().expect("agent bus lock");
        g.peers.insert(
            id.clone(),
            PeerInfo {
                id: id.clone(),
                label,
                status: PeerStatus::Running,
                registered_at: Instant::now(),
                wake,
            },
        );
        g.mailboxes.entry(id).or_default();
    }

    /// Mark peer gone; further sends fail with `peer_gone`. Mailbox is retained
    /// so the peer can still drain via `inbox` until GC (not implemented in v1).
    pub fn mark_gone(&self, id: &str) {
        let mut g = self.inner.lock().expect("agent bus lock");
        if let Some(p) = g.peers.get_mut(id) {
            p.status = PeerStatus::Gone;
            p.wake = None;
        }
    }

    /// Snapshot of roster (including gone peers still known).
    pub fn list(&self) -> Vec<PeerInfo> {
        let g = self.inner.lock().expect("agent bus lock");
        let mut peers: Vec<_> = g.peers.values().cloned().collect();
        peers.sort_by(|a, b| a.id.cmp(&b.id));
        // Strip wake from debug clones is already done; for API we return
        // peers without exposing wake to tools — clone keeps wake for bus only.
        peers
            .into_iter()
            .map(|mut p| {
                p.wake = None;
                p
            })
            .collect()
    }

    /// Fire-and-forget send. Enqueues on target mailbox and optionally wakes.
    pub fn send(
        &self,
        from: &str,
        to: &str,
        text: &str,
        reply_to: Option<&str>,
    ) -> SendOutcome {
        let text = text.trim();
        if text.is_empty() {
            return SendOutcome::Failed {
                reason: "empty message".into(),
            };
        }
        if text.len() > MAX_MESSAGE_BYTES {
            return SendOutcome::Failed {
                reason: format!(
                    "message exceeds {MAX_MESSAGE_BYTES} bytes (got {})",
                    text.len()
                ),
            };
        }
        if from == to {
            return SendOutcome::Failed {
                reason: "cannot send to self".into(),
            };
        }

        let wake_body;
        let message_id;
        {
            let mut g = self.inner.lock().expect("agent bus lock");

            // Rate limit (lookup then update — avoid holding entry borrow across peers).
            let now = Instant::now();
            {
                let rate = g.rates.entry(from.to_string()).or_insert(RateWindow {
                    window_start: now,
                    count: 0,
                });
                if now.duration_since(rate.window_start) > RATE_LIMIT_WINDOW {
                    rate.window_start = now;
                    rate.count = 0;
                }
                if rate.count >= RATE_LIMIT_COUNT {
                    return SendOutcome::Failed {
                        reason: format!(
                            "rate limit: max {RATE_LIMIT_COUNT} sends per {}s",
                            RATE_LIMIT_WINDOW.as_secs()
                        ),
                    };
                }
            }

            let target_status = match g.peers.get(to) {
                Some(target) => target.status,
                None => {
                    return SendOutcome::Failed {
                        reason: format!("unknown peer {to:?}; call agent_hub list"),
                    };
                }
            };
            if target_status == PeerStatus::Gone {
                return SendOutcome::Failed {
                    reason: format!("peer {to:?} is gone"),
                };
            }
            if !g.peers.contains_key(from) {
                // Auto-register sender as Running with empty label if missing
                // (tests / late wiring). Production always registers first.
                g.peers.insert(
                    from.to_string(),
                    PeerInfo {
                        id: from.to_string(),
                        label: from.to_string(),
                        status: PeerStatus::Running,
                        registered_at: Instant::now(),
                        wake: None,
                    },
                );
            }

            let n = g.next_msg.fetch_add(1, Ordering::Relaxed);
            message_id = format!("m{n}");
            let msg = AgentHubMessage {
                id: message_id.clone(),
                from: from.to_string(),
                to: to.to_string(),
                text: text.to_string(),
                reply_to: reply_to.map(str::to_string),
                ts_unix_ms: system_unix_ms(),
            };

            let box_ = g.mailboxes.entry(to.to_string()).or_default();
            while box_.len() >= MAX_MAILBOX_DEPTH {
                box_.pop_front();
            }
            box_.push_back(msg);

            if let Some(rate) = g.rates.get_mut(from) {
                rate.count = rate.count.saturating_add(1);
            }

            wake_body = g.peers.get(to).and_then(|p| p.wake.clone()).map(|w| {
                let body = format!(
                    "[agent_hub message from {from} id={message_id}]\n{text}"
                );
                (w, body)
            });
        }

        self.notify.notify_waiters();

        if let Some((wake, body)) = wake_body {
            wake(body);
        }

        SendOutcome::Delivered { message_id }
    }

    /// Drain the caller's mailbox (non-blocking).
    pub fn inbox(&self, id: &str) -> Vec<AgentHubMessage> {
        let mut g = self.inner.lock().expect("agent bus lock");
        g.mailboxes
            .get_mut(id)
            .map(|q| q.drain(..).collect())
            .unwrap_or_default()
    }

    /// Pending count without drain.
    pub fn pending_count(&self, id: &str) -> usize {
        let g = self.inner.lock().expect("agent bus lock");
        g.mailboxes.get(id).map(|q| q.len()).unwrap_or(0)
    }

    /// Wait until `id` has mail or timeout. Returns drained messages (may be empty on timeout).
    pub async fn wait_inbox(&self, id: &str, timeout: Duration) -> Vec<AgentHubMessage> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let drained = self.inbox(id);
                if !drained.is_empty() {
                    return drained;
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Vec::new();
            }
            // Arm notified before re-check to avoid missing a wake.
            let notified = self.notify.notified();
            {
                let drained = self.inbox(id);
                if !drained.is_empty() {
                    return drained;
                }
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(remaining) => {
                    return self.inbox(id);
                }
            }
        }
    }
}

fn system_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn send_inbox_list_roundtrip() {
        let bus = AgentBus::new();
        bus.register(MAIN_PEER_ID, "main", None);
        bus.register("a", "scout", None);
        bus.register("b", "reviewer", None);

        let peers = bus.list();
        assert_eq!(peers.len(), 3);

        match bus.send("a", "b", "hello peer", None) {
            SendOutcome::Delivered { message_id } => {
                assert!(message_id.starts_with('m'));
            }
            other => panic!("expected delivered, got {other:?}"),
        }

        let inbox = bus.inbox("b");
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].from, "a");
        assert_eq!(inbox[0].text, "hello peer");
        assert!(bus.inbox("b").is_empty());
    }

    #[test]
    fn send_to_gone_fails() {
        let bus = AgentBus::new();
        bus.register("a", "a", None);
        bus.register("b", "b", None);
        bus.mark_gone("b");
        match bus.send("a", "b", "hi", None) {
            SendOutcome::Failed { reason } => assert!(reason.contains("gone")),
            other => panic!("expected failed, got {other:?}"),
        }
    }

    #[test]
    fn size_limit() {
        let bus = AgentBus::new();
        bus.register("a", "a", None);
        bus.register("b", "b", None);
        let big = "x".repeat(MAX_MESSAGE_BYTES + 1);
        match bus.send("a", "b", &big, None) {
            SendOutcome::Failed { reason } => assert!(reason.contains("exceeds")),
            other => panic!("expected failed, got {other:?}"),
        }
    }

    #[test]
    fn mailbox_drops_oldest() {
        let bus = AgentBus::new();
        bus.register("b", "b", None);
        // One message per distinct sender so the per-sender rate limit does not
        // hide the mailbox-depth drop policy.
        for i in 0..(MAX_MAILBOX_DEPTH + 5) {
            let from = format!("a{i}");
            bus.register(&from, &from, None);
            match bus.send(&from, "b", &format!("msg{i}"), None) {
                SendOutcome::Delivered { .. } => {}
                other => panic!("expected delivered for msg{i}, got {other:?}"),
            }
        }
        let inbox = bus.inbox("b");
        assert_eq!(inbox.len(), MAX_MAILBOX_DEPTH);
        assert_eq!(inbox[0].text, format!("msg{}", 5));
    }

    #[test]
    fn wake_is_called_on_send() {
        let bus = AgentBus::new();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        let wake: PeerWakeFn = Arc::new(move |_body| {
            hits2.fetch_add(1, Ordering::SeqCst);
        });
        bus.register("a", "a", None);
        bus.register("b", "b", Some(wake));
        let _ = bus.send("a", "b", "ping", None);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn wait_inbox_unblocks_on_send() {
        let bus = AgentBus::new();
        bus.register("a", "a", None);
        bus.register("b", "b", None);
        let bus2 = bus.clone();
        let h = tokio::spawn(async move {
            bus2.wait_inbox("b", Duration::from_secs(2)).await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = bus.send("a", "b", "async hi", None);
        let msgs = h.await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text, "async hi");
    }
}
