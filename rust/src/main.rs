//! mousehole (Rust backend) — drop-in replacement for the Bun backend.
//!
//! Module map mirrors the spec documents in docs/rust-rewrite/:
//! - config:    env parsing incl. _FILE variants        (config-auth-boundary.md §1)
//! - logger:    [LEVEL] stdout logging contract         (config-auth-boundary.md §5)
//! - boundary:  host/auth/origin/content-type checks    (config-auth-boundary.md §4)
//! - session:   password login sessions + bearer auth   (config-auth-boundary.md §2)
//! - api:       route handlers + error contract         (api-contract.md)
//! - sse:       server-sent events stream               (api-contract.md §7)
//! - mam:       MAM dynamic-seedbox contact + host info (mam-behavior.md)
//! - scheduler: contact cycle + manual trigger          (mam-behavior.md §4-6)
//! - state:     persisted JSON state + migrations       (state-and-build.md §1)
//! - assets:    embedded Vite frontend bundle           (state-and-build.md §2.3)
//!
//! Clippy: `result_large_err` is allowed crate-wide — `AppError` carries a
//! ready-to-send response body/headers by design, and boxing it would clutter
//! every handler for zero practical gain at this request rate.
//!
//! Startup order is contract (state-and-build.md §4): config (fail fast) →
//! log threshold → context → bind → banner → security validation (may abort)
//! → scheduler start. Shutdown: stop scheduler (cancel timer, drain the
//! in-flight contact) → stop server → exit 0.

#![allow(clippy::result_large_err)]

mod api;
mod assets;
mod boundary;
mod config;
mod error;
mod logger;
mod mam;
mod scheduler;
mod session;
mod sse;
mod state;
mod timefmt;

use std::collections::HashMap;
use std::sync::Arc;

const VERSION: &str = "0.5.0";

fn git_hash() -> String {
    option_env!("PUBLIC_GIT_HASH")
        .map(str::to_string)
        .or_else(|| std::env::var("PUBLIC_GIT_HASH").ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

// `mousehole healthcheck` — the Docker HEALTHCHECK entry. A bare-hands HTTP
// GET so the probe doesn't drag in (or pay for) the async stack. Unlike the
// Bun image's healthcheck this honors MOUSEHOLE_PORT instead of hardcoding
// 5010 — strictly less surprising.
fn healthcheck() -> ! {
    use std::io::{Read, Write};
    let port = std::env::var("MOUSEHOLE_PORT")
        .ok()
        .and_then(|p| p.trim().parse::<u16>().ok())
        .unwrap_or(5010);
    let ok = (|| -> Option<bool> {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
        s.set_read_timeout(Some(std::time::Duration::from_secs(8))).ok()?;
        s.write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .ok()?;
        let mut buf = [0u8; 64];
        let n = s.read(&mut buf).ok()?;
        Some(String::from_utf8_lossy(&buf[..n]).contains(" 200 "))
    })()
    .unwrap_or(false);
    std::process::exit(if ok { 0 } else { 1 });
}

// Single-threaded runtime: every workload here is IO-bound and low-rate, and
// each additional worker thread costs stack + allocator arenas against the
// <5 MB resident-memory target.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        healthcheck();
    }
    let env: HashMap<String, String> = std::env::vars().collect();
    let cfg = match config::Config::from_env(&env, &|p| std::fs::read_to_string(p)) {
        Ok(c) => c,
        Err(e) => {
            logger::error(&e.0);
            std::process::exit(1);
        }
    };
    logger::set_level(cfg.log_level);
    let cfg = Arc::new(cfg);

    // Context wiring: sessions close their SSE streams on deletion; the
    // scheduler's persisted-contact notifications pump into the SSE registry.
    let sse = sse::SseRegistry::new();
    let sse_for_sessions = sse.clone();
    let sessions = session::SessionStore::new(
        cfg.session_duration_seconds,
        Arc::new(move |id| sse_for_sessions.close_session_streams(id)),
    );
    let (notify_tx, mut notify_rx) = tokio::sync::broadcast::channel::<()>(8);
    let store = state::Store::new(&cfg.state_dir_path);
    let mam = mam::MamClient::new(cfg.mam_request_timeout_seconds, None);
    let sched = scheduler::Scheduler::new(store, mam, cfg.update_interval_seconds, notify_tx);
    let scheduler_handle = sched.handle();

    let sse_pump = sse.clone();
    tokio::spawn(async move {
        while notify_rx.recv().await.is_ok() {
            sse_pump.notify();
        }
    });

    let ctx = api::AppContext {
        config: cfg.clone(),
        sessions,
        scheduler: scheduler_handle.clone(),
        sse,
    };
    let app = api::router(ctx);

    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", cfg.port)).await {
        Ok(l) => l,
        Err(e) => {
            logger::error(&format!("failed to bind port {}: {e}", cfg.port));
            std::process::exit(1);
        }
    };
    let addr = listener.local_addr().expect("listener has a local addr");
    logger::info(&format!(
        "Mousehole v{VERSION} ({}) running at http://{addr}/",
        git_hash()
    ));

    if let Err(e) = config::validate_runtime_security_config(&cfg) {
        logger::error(&e.0);
        std::process::exit(1);
    }

    scheduler_handle.start();

    let shutdown_handle = scheduler_handle.clone();
    let shutdown = async move {
        let ctrl_c = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("install SIGTERM handler");
            tokio::select! {
                _ = ctrl_c => {},
                _ = term.recv() => {},
            }
        }
        #[cfg(not(unix))]
        ctrl_c.await;
        logger::info("Shutting down...");
        // Drain: cancel the timer and wait out any in-flight contact before
        // the server stops accepting.
        shutdown_handle.stop().await;
    };

    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
    {
        logger::error(&format!("server error: {e}"));
        std::process::exit(1);
    }
}
