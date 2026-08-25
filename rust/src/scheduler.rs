//! The contact loop. Every contact — startup, the interval timer,
//! POST /updates, PUT /cookie — funnels through `commit_contact` under one
//! FIFO mutex, so concurrent triggers can't interleave reads/writes or lose
//! a cookie. Rescheduling happens in the `finally` position (even a failed
//! write re-arms the timer) and BEFORE the caller builds its response, so
//! `nextContactAt` is always fresh — the countdown UI keys off it.
//! Deliberately no jitter, no backoff, no retry: a 429/403 is recorded,
//! displayed, and retried at the same fixed cadence. (Details:
//! docs/rust-rewrite/mam-behavior.md §4–§6.)

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use tokio::sync::broadcast;

use crate::error::AppError;
use crate::logger;
use crate::mam::MamClient;
use crate::state::{
    ContactError, IpUpdate, MamContact, SerializedMamContact, SerializedState, State, Store,
    STATE_VERSION,
};
use crate::timefmt;

pub struct Scheduler {
    inner: Arc<Inner>,
}

struct Inner {
    mutex: tokio::sync::Mutex<()>,
    store: Store,
    // Rebuildable: see MamClient::rebuilt. Guarded by `mutex` in practice —
    // every contact holds it — the std Mutex is only for the swap.
    mam: std::sync::Mutex<MamClient>,
    transport_failures: AtomicU32,
    interval_seconds: f64,
    timer: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    next_contact_at: std::sync::Mutex<Option<jiff::Zoned>>,
    stopped: AtomicBool,
    notify: broadcast::Sender<()>,
}

impl Scheduler {
    pub fn new(
        store: Store,
        mam: MamClient,
        interval_seconds: f64,
        notify: broadcast::Sender<()>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                mutex: tokio::sync::Mutex::new(()),
                store,
                mam: std::sync::Mutex::new(mam),
                transport_failures: AtomicU32::new(0),
                interval_seconds,
                timer: std::sync::Mutex::new(None),
                next_contact_at: std::sync::Mutex::new(None),
                stopped: AtomicBool::new(false),
                notify,
            }),
        }
    }

    pub fn handle(&self) -> SchedulerHandle {
        SchedulerHandle {
            inner: self.inner.clone(),
        }
    }
}

#[derive(Clone)]
pub struct SchedulerHandle {
    inner: Arc<Inner>,
}

impl SchedulerHandle {
    /// §5.1 — the single entry point for every contact.
    pub async fn commit_contact(
        &self,
        new_cookie: Option<String>,
    ) -> Result<SerializedState, AppError> {
        let inner = &self.inner;
        let _guard = inner.mutex.lock().await;
        let result = async {
            let disk = inner.store.read_if_exists()?;
            let base = apply_cookie(disk, new_cookie);
            let mam = inner.mam.lock().unwrap().clone();
            let serialized = contact_mam(base.as_ref(), &mam).await;
            inner.store.write(&serialized)?;
            let _ = inner.notify.send(());
            self.note_transport_outcome(&serialized);
            Ok(serialized)
        }
        .await;
        // `finally`: always reschedule, before the caller builds a response.
        self.schedule_next();
        result
    }

    /// Netns-bounce self-heal (2026-08-25): consecutive TRANSPORT failures
    /// (MAM unreached — cookie rejections are `Reached` and never count)
    /// first rebuild the HTTP client, then give up the process entirely so
    /// the supervisor's `unless-stopped` restarts it — the one recovery
    /// proven to work when the shared network namespace is bounced under
    /// us. 9 failures at the default 300 s interval bounds the wedge to
    /// ~45 min instead of forever-with-a-green-healthcheck.
    fn note_transport_outcome(&self, serialized: &SerializedState) {
        use crate::state::SerializedMamContact as C;
        let inner = &self.inner;
        let unreached = matches!(
            serialized.last_mam_contact,
            Some(C::Unreached { .. })
        );
        if !unreached {
            inner.transport_failures.store(0, Ordering::SeqCst);
            return;
        }
        // Orphaned-namespace detector: sharing another container's netns
        // means that container's RESTART creates a fresh namespace and
        // leaves this still-running process trapped in the old one — dead
        // wg0, no eth0, no way back from inside the process (proven live
        // 2026-08-25: client rebuilds cannot cure it; only a container
        // restart re-attaches). eth0 vanishing from the namespace is the
        // crisp signature: exit immediately so the supervisor re-attaches
        // us to the current namespace.
        if !std::path::Path::new("/sys/class/net/eth0").exists() {
            logger::error(
                "eth0 is gone from this network namespace — the parent                  container restarted and left us orphaned; exiting so the                  supervisor re-attaches us to the live namespace",
            );
            std::process::exit(2);
        }
        let n = inner.transport_failures.fetch_add(1, Ordering::SeqCst) + 1;
        if n >= 9 {
            logger::error(
                "9 consecutive transport failures — client rebuilds did not                  recover; exiting so the supervisor restarts the process",
            );
            std::process::exit(2);
        }
        if n % 3 == 0 {
            logger::error(&format!(
                "{n} consecutive transport failures — rebuilding the HTTP                  client (stale connector state after a namespace bounce?)"
            ));
            let fresh = inner.mam.lock().unwrap().rebuilt();
            *inner.mam.lock().unwrap() = fresh;
        }
    }

    /// §5.2 — cancel-and-rearm one-shot timer; fixed interval, no jitter,
    /// no backoff.
    pub fn schedule_next(&self) {
        let inner = &self.inner;
        if let Some(h) = inner.timer.lock().unwrap().take() {
            h.abort();
        }
        if inner.stopped.load(Ordering::SeqCst) {
            *inner.next_contact_at.lock().unwrap() = None;
            return;
        }
        let interval = inner.interval_seconds;
        let handle_for_task = SchedulerHandle {
            inner: inner.clone(),
        };
        let task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs_f64(interval)).await;
            if let Err(e) = handle_for_task.commit_contact(None).await {
                logger::error(&e.body.message);
            }
        });
        *inner.timer.lock().unwrap() = Some(task);
        let next = timefmt::plus_seconds(&timefmt::now_zoned(), interval);
        logger::info(&format!(
            "Next automatic update scheduled for {}",
            timefmt::to_wire(&next)
        ));
        *inner.next_contact_at.lock().unwrap() = Some(next);
    }

    /// §5.3 — fire-and-forget first contact; the listener serves before it
    /// resolves.
    pub fn start(&self) {
        let handle = self.clone();
        tokio::spawn(async move {
            if let Err(e) = handle.commit_contact(None).await {
                logger::error(&e.body.message);
            }
        });
        logger::info(&format!(
            "Background update task started, running on {} second interval",
            self.inner.interval_seconds
        ));
    }

    /// §5.4 — cancel timer, then drain any in-flight contact by acquiring
    /// (and dropping only after acquisition) the transaction mutex.
    pub async fn stop(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        if let Some(h) = self.inner.timer.lock().unwrap().take() {
            h.abort();
        }
        *self.inner.next_contact_at.lock().unwrap() = None;
        let _drain = self.inner.mutex.lock().await;
    }

    pub fn next_contact_at(&self) -> Option<jiff::Zoned> {
        self.inner.next_contact_at.lock().unwrap().clone()
    }

    pub fn store(&self) -> &Store {
        &self.inner.store
    }
}

/// PUT /cookie splices the new cookie in before contacting MAM (§5.1).
fn apply_cookie(disk: Option<State>, new_cookie: Option<String>) -> Option<State> {
    match new_cookie {
        None => disk,
        Some(c) => {
            let mut s = disk.unwrap_or_else(State::fresh);
            s.cookie = Some(c);
            Some(s)
        }
    }
}

/// §4 — contacts MAM; NEVER fails. Transport/parse errors are recorded in
/// the returned state.
pub async fn contact_mam(prev: Option<&State>, mam: &MamClient) -> SerializedState {
    let at = timefmt::to_wire(&timefmt::now_zoned());
    let effective = prev.and_then(|s| s.effective_cookie()).map(str::to_string);
    // On failure the previous cookie is carried forward VERBATIM (empty
    // string included); on a successful cookie-less contact it is dropped.
    let carried = prev.and_then(|s| s.cookie.clone());

    match effective {
        None => match mam.get_host_info().await {
            Ok(info) => {
                logger::info("No cookie set yet. Visit the web UI to configure.");
                SerializedState {
                    version: STATE_VERSION,
                    cookie: None,
                    last_mam_contact: Some(SerializedMamContact::Reached {
                        at,
                        reached: true,
                        ip: info.ip,
                        asn: info.asn,
                        as_name: info.as_name,
                        ip_update: None,
                    }),
                }
            }
            Err(e) => unreached(at, carried, e),
        },
        Some(cookie) => match mam.update_mam_ip(&cookie).await {
            Ok(result) => {
                log_network_change(prev, &result.ip, result.asn);
                if result.success {
                    logger::info(&format!("MAM update: {}", result.msg));
                } else {
                    logger::error(&format!(
                        "MAM update not applied ({}): {}",
                        result.http_status, result.msg
                    ));
                }
                SerializedState {
                    version: STATE_VERSION,
                    cookie: Some(result.rotated_cookie.unwrap_or(cookie)),
                    last_mam_contact: Some(SerializedMamContact::Reached {
                        at,
                        reached: true,
                        ip: result.ip,
                        asn: result.asn,
                        as_name: result.as_name,
                        ip_update: Some(IpUpdate {
                            success: result.success,
                            msg: result.msg,
                            http_status: result.http_status,
                        }),
                    }),
                }
            }
            Err(e) => unreached(at, carried, e),
        },
    }
}

fn unreached(at: String, cookie: Option<String>, e: AppError) -> SerializedState {
    logger::error(&format!("Could not reach MAM: {}", e.body.message));
    SerializedState {
        version: STATE_VERSION,
        cookie,
        last_mam_contact: Some(SerializedMamContact::Unreached {
            at,
            reached: false,
            error: ContactError {
                error_type: e.body.error_type,
                message: e.body.message,
            },
        }),
    }
}

/// §9 — `Network change: IP <old> -> <new>, ASN <old> -> <new>`, only the
/// changed parts, only when the previous contact was reached.
fn log_network_change(prev: Option<&State>, new_ip: &str, new_asn: i64) {
    let Some(MamContact {
        body: SerializedMamContact::Reached { ip, asn, .. },
        ..
    }) = prev.and_then(|s| s.last_mam_contact.as_ref())
    else {
        return;
    };
    let mut parts = Vec::new();
    if ip != new_ip {
        parts.push(format!("IP {ip} -> {new_ip}"));
    }
    if *asn != new_asn {
        parts.push(format!("ASN {asn} -> {new_asn}"));
    }
    if !parts.is_empty() {
        logger::info(&format!("Network change: {}", parts.join(", ")));
    }
}
