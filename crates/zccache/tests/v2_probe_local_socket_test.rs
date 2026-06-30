//! Integration tests for `zccache::ipc::broker_v2::probe_local_socket`.
//!
//! Round-2 audit (#842 ledger): the existing `probe_local_socket_no_listener_returns_err`
//! covered only the negative path. Production callers (the
//! `RUNNING_PROCESS_FAKE_BACKEND` seam in `broker.rs`) depend on:
//!
//! 1. Live listener → probe returns Ok(())
//! 2. Stale / missing listener → probe returns a typed Err quickly
//! 3. Malformed endpoint (empty, NUL-embedded) → probe Errs without
//!    panicking deep in `interprocess`
//!
//! Without (1) we have no proof the probe's name-encoding round-trips
//! against the OS. Without (3) a user-supplied env-var value could
//! crash the daemon's seam-resolution.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use interprocess::local_socket::traits::Listener as _;
use interprocess::local_socket::ListenerOptions;
use std::time::{SystemTime, UNIX_EPOCH};
use zccache::ipc::broker_v2::probe_local_socket;

/// Mint a unique endpoint per test to avoid cross-run collisions.
fn unique_endpoint(label: &str) -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    if cfg!(windows) {
        format!(r"\\.\pipe\zccache-probe-{label}-{nonce}")
    } else {
        std::env::temp_dir()
            .join(format!("zccache-probe-{label}-{nonce}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

fn wrap_name(endpoint: &str) -> interprocess::local_socket::Name<'_> {
    use interprocess::local_socket::prelude::*;
    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        endpoint
            .to_ns_name::<GenericNamespaced>()
            .expect("to_ns_name")
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        endpoint
            .to_fs_name::<GenericFilePath>()
            .expect("to_fs_name")
    }
}

/// Live listener round-trip: probe must succeed against an actual
/// listener bound at the endpoint. After the listener drops, the same
/// probe must Err. Pins the name-encoding contract against any future
/// `interprocess` bump.
#[test]
fn probe_local_socket_succeeds_against_live_listener() {
    let endpoint = unique_endpoint("live");
    // Pre-clean (Unix only) — Drop of a leaked test from a prior run
    // could leave a stale socket.
    #[cfg(unix)]
    let _ = std::fs::remove_file(&endpoint);
    let listener = ListenerOptions::new()
        .name(wrap_name(&endpoint))
        .create_sync()
        .expect("bind listener");

    probe_local_socket(&endpoint).expect("live listener must be reachable");

    drop(listener);
    #[cfg(unix)]
    let _ = std::fs::remove_file(&endpoint);

    let err = probe_local_socket(&endpoint).expect_err("after drop, probe must Err");
    assert!(
        !err.to_string().is_empty(),
        "probe error must carry a non-empty message"
    );
}

/// Malformed endpoint: empty string must Err, not panic deep in
/// `interprocess::local_socket::Name::to_ns_name` / `to_fs_name`.
#[test]
fn probe_local_socket_rejects_empty_endpoint() {
    let err = probe_local_socket("").expect_err("empty endpoint must Err, not panic");
    let _ = err;
}

/// Malformed endpoint: NUL byte embedded must Err, not panic. NULs are
/// rejected by both `to_ns_name` (Windows) and `to_fs_name` (Unix)
/// because they're invalid in OS pipe/socket names; the test pins this
/// rejection so a future `interprocess` change to accept-with-truncate
/// is a deliberate decision.
#[test]
fn probe_local_socket_rejects_nul_in_endpoint() {
    let bad = if cfg!(windows) {
        "\\\\.\\pipe\\zccache-probe\0evil"
    } else {
        "/tmp/zccache-probe\0evil.sock"
    };
    let err = probe_local_socket(bad).expect_err("NUL must be rejected, not panic");
    let _ = err;
}

/// P1-4 from #848 (Windows arm): the `\\.\pipe\` prefix on its own is
/// not a valid endpoint — there's no pipe NAME after the prefix. The
/// probe must Err (not panic, not block until the deadline, not return
/// Ok). Pins the contract from a downstream consumer so a future
/// interprocess relaxation surfaces here. No-op on Unix where the
/// prefix is meaningless.
#[cfg(windows)]
#[test]
fn probe_local_socket_rejects_pipe_prefix_only() {
    let err = probe_local_socket(r"\\.\pipe\")
        .expect_err(r"\\.\pipe\ on its own has no pipe name and must Err");
    let _ = err;
}

/// Bound listener + open connect from probe is *fast* — the probe must
/// not hold the connection past `drop`. Builds a listener, runs probe
/// in a tight loop 20×; total time must stay well below the
/// `DEFAULT_PROBE_TIMEOUT` × 20 budget.
#[test]
fn probe_local_socket_does_not_hold_connection_past_drop() {
    let endpoint = unique_endpoint("nohold");
    #[cfg(unix)]
    let _ = std::fs::remove_file(&endpoint);
    let listener = ListenerOptions::new()
        .name(wrap_name(&endpoint))
        .create_sync()
        .expect("bind listener");
    // Background-drain the listener so it can keep accepting new connects.
    std::thread::spawn(move || {
        while let Ok(stream) = listener.accept() {
            drop(stream);
        }
    });
    let start = std::time::Instant::now();
    for _ in 0..20 {
        probe_with_startup_retry(&endpoint).expect("probe ok");
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "20 probes took {elapsed:?}; expected well under 2s"
    );
    #[cfg(unix)]
    let _ = std::fs::remove_file(&endpoint);
}

fn probe_with_startup_retry(endpoint: &str) -> std::io::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        match probe_local_socket(endpoint) {
            Ok(()) => return Ok(()),
            Err(err)
                if cfg!(windows)
                    && err.kind() == std::io::ErrorKind::NotFound
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(err) => return Err(err),
        }
    }
}
