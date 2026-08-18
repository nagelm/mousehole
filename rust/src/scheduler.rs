//! The contact loop. Every contact — startup, the interval timer,
//! POST /updates, PUT /cookie — funnels through `commit_contact` under one
//! FIFO mutex, so concurrent triggers can't interleave reads/writes or lose
//! a cookie. Rescheduling happens in the `finally` position (even a failed
//! write re-arms the timer) and BEFORE the caller builds its response, so
//! `nextContactAt` is always fresh — the countdown UI keys off it.
//! Deliberately no jitter, no backoff, no retry: a 429/403 is recorded,
//! displayed, and retried at the same fixed cadence. (Details:
//! docs/rust-rewrite/mam-behavior.md §4–§6.)

use std::sync::atomic::{AtomicBool, Ordering};
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
    mam: MamClient,
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
                mam,
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
            let serialized = contact_mam(base.as_ref(), &inner.mam).await;
            inner.store.write(&serialized)?;
            let _ = inner.notify.send(());
            Ok(serialized)
        }
        .await;
        // `finally`: always reschedule, before the caller builds a response.
        self.schedule_next();
        result
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
