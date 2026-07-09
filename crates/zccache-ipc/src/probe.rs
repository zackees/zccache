//! Raw local-socket reachability probe.
//!
//! A bare `Stream::connect` liveness check with a helper-thread deadline,
//! used by the `RUNNING_PROCESS_FAKE_BACKEND` test seam in [`super::broker`].
//! Extracted from the (now-removed) `broker_v2` module in issue #1001: the v2
//! broker surface was dead code, but this probe is a live, broker-agnostic
//! primitive, so it moved here rather than being deleted with the rest.

use std::time::Duration;

use interprocess::local_socket::traits::Stream as _;
use interprocess::local_socket::Stream;

/// Default deadline for the [`probe_local_socket`] liveness check.
///
/// This is a probe, not a usage connect — 250 ms is generous for any
/// local-socket dial that is actually going to succeed. If it doesn't answer
/// within this window the seam is assumed unreachable and the caller falls
/// back to the direct-connect path (see [`super::broker`]).
const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// Probe an arbitrary local-socket endpoint for reachability without going
/// through any broker negotiation.
///
/// Returns `Ok(())` when the endpoint is reachable, `Err(io::Error)`
/// otherwise. The opened stream is closed immediately — this is a liveness
/// probe, not a connection acquisition.
pub fn probe_local_socket(endpoint: &str) -> std::io::Result<()> {
    if endpoint.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "probe_local_socket: empty endpoint",
        ));
    }
    probe_local_socket_with_deadline(endpoint, DEFAULT_PROBE_TIMEOUT)
}

/// Same as [`probe_local_socket`] but with a caller-supplied deadline.
///
/// MUST be called from inside `tokio::task::spawn_blocking` if invoked from an
/// async context — the helper-thread bound prevents an infinite hang but the
/// *outer* call still occupies the calling thread until the deadline elapses.
pub fn probe_local_socket_with_deadline(endpoint: &str, deadline: Duration) -> std::io::Result<()> {
    let endpoint = endpoint.to_owned();
    call_with_io_deadline("probe_local_socket", deadline, move || {
        // Pass the endpoint string verbatim on both platforms, aligned with
        // upstream `running_process::broker::server::connection::
        // local_socket_name`.
        #[cfg(windows)]
        let name = {
            use interprocess::local_socket::{GenericNamespaced, ToNsName};
            ToNsName::to_ns_name::<GenericNamespaced>(endpoint.as_str())?
        };

        #[cfg(unix)]
        let name = {
            use interprocess::local_socket::{GenericFilePath, ToFsName};
            ToFsName::to_fs_name::<GenericFilePath>(endpoint.as_str())?
        };

        let stream = Stream::connect(name)?;
        drop(stream);
        Ok(())
    })
}

/// Run a blocking `io::Result<T>`-returning closure on a helper thread,
/// bounded by `deadline`. On deadline the helper thread is leaked (there is no
/// portable way to cancel a `Stream::connect` mid-call) but the calling thread
/// returns promptly with an `ErrorKind::TimedOut`.
fn call_with_io_deadline<T, F>(label: &'static str, deadline: Duration, f: F) -> std::io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(deadline) {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("{label}: exceeded deadline of {deadline:?}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_local_socket_no_listener_returns_err() {
        // A syntactically valid but unbound endpoint must return Err, never
        // hang, and never panic.
        #[cfg(windows)]
        let endpoint = r"\\.\pipe\zccache-probe-no-listener-1001";
        #[cfg(unix)]
        let endpoint = "/tmp/zccache-probe-no-listener-1001.sock";
        let err = probe_local_socket(endpoint).expect_err("no listener => Err");
        assert!(matches!(
            err.kind(),
            std::io::ErrorKind::NotFound
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::Other
        ));
    }

    #[test]
    fn probe_local_socket_empty_endpoint_is_invalid_input() {
        let err = probe_local_socket("").expect_err("empty => Err");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
