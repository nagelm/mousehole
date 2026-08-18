//! Server-Sent Events: a contentless "something changed, re-pull GET /state"
//! signal to open dashboards. The payload is deliberately empty — GET /state
//! is the single source of truth. Nothing on connect, no keep-alives, no
//! retry hints. Streams register under their session id ("" for token/no-auth
//! clients, which are never force-closed); deleting a session closes its
//! streams cleanly so the browser reconnects, 401s, and lands on login.
//! (Wire details: docs/rust-rewrite/api-contract.md §7.)

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

const FRAME: &[u8] = b"data: changed\n\n";

#[derive(Clone)]
pub struct SseRegistry {
    inner: Arc<Inner>,
}

struct Inner {
    next_id: AtomicU64,
    clients: Mutex<HashMap<u64, Client>>,
}

struct Client {
    session_id: String,
    tx: mpsc::Sender<&'static [u8]>,
}

/// Unregisters the stream when the response body is dropped (client gone).
pub struct StreamGuard {
    registry: SseRegistry,
    id: u64,
    pub rx: mpsc::Receiver<&'static [u8]>,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.registry.inner.clients.lock().unwrap().remove(&self.id);
    }
}

impl SseRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                next_id: AtomicU64::new(1),
                clients: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn register(&self, session_id: &str) -> StreamGuard {
        // Capacity is per-client backlog; a slow client that misses a frame
        // simply refetches on the next one — frames are contentless.
        let (tx, rx) = mpsc::channel(8);
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner.clients.lock().unwrap().insert(
            id,
            Client {
                session_id: session_id.to_string(),
                tx,
            },
        );
        StreamGuard {
            registry: self.clone(),
            id,
            rx,
        }
    }

    /// Broadcast `data: changed` to every client; dead clients are dropped.
    pub fn notify(&self) {
        let mut clients = self.inner.clients.lock().unwrap();
        clients.retain(|_, c| c.tx.try_send(FRAME).is_ok() || !c.tx.is_closed());
    }

    /// §7.4: dropping the sender ends the stream cleanly on the client side.
    pub fn close_session_streams(&self, session_id: &str) {
        if session_id.is_empty() {
            return; // "" is never a real session
        }
        self.inner
            .clients
            .lock()
            .unwrap()
            .retain(|_, c| c.session_id != session_id);
    }

    #[cfg(test)]
    fn client_count(&self) -> usize {
        self.inner.clients.lock().unwrap().len()
    }
}

impl Default for SseRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn notify_reaches_all_and_close_ends_session_streams() {
        let reg = SseRegistry::new();
        let mut a = reg.register("sess-a");
        let mut b = reg.register(""); // token client
        reg.notify();
        assert_eq!(a.rx.recv().await, Some(FRAME));
        assert_eq!(b.rx.recv().await, Some(FRAME));

        reg.close_session_streams("sess-a");
        // a's sender is gone → clean end-of-stream
        assert_eq!(a.rx.recv().await, None);
        // token stream unaffected
        reg.notify();
        assert_eq!(b.rx.recv().await, Some(FRAME));
    }

    #[tokio::test]
    async fn dropping_the_guard_unregisters() {
        let reg = SseRegistry::new();
        {
            let _g = reg.register("s");
            assert_eq!(reg.client_count(), 1);
        }
        assert_eq!(reg.client_count(), 0);
    }
}
