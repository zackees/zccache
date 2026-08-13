//! Cross-process token bucket implementing the GNU make jobserver
//! protocol.
//!
//! [Spec — POSIX jobserver](https://www.gnu.org/software/make/manual/html_node/POSIX-Jobserver.html)
//!
//! ## Why this lives in the daemon
//!
//! Issue #813 / #816 — the zccache daemon owns a single global pool of
//! compile tokens. Every cargo invocation that the daemon spawns gets
//! `MAKEFLAGS=-j --jobserver-auth=<pool>` in its env; the cargo + rustc
//! ecosystem natively cooperates via the protocol. Cross-cargo
//! coordination falls out for free — no custom IPC, no client-side
//! agreement.
//!
//! ## What this module ships in sub-task #815
//!
//! Just the pipe primitive: create a pool of `N` tokens, hand out an
//! auth string, clean up on drop. **No daemon integration here.** That
//! belongs in sub-task #816 (env injection, override env, daemon-state
//! ownership). Keeping the primitive isolated lets the cross-platform
//! IPC glue land in one focused review.
//!
//! ## Platform support
//!
//! - **POSIX** (Linux, macOS): anonymous pipe via `pipe2(O_CLOEXEC)`.
//!   Tokens are single bytes on the pipe; auth string is
//!   `--jobserver-auth=R,W` where R and W are the read/write file
//!   descriptors. This is the protocol the daemon ships with in v1
//!   because the Docker validation harness for sub-task #817 runs on
//!   Linux, so unblocking that path is the priority.
//! - **Windows** (named pipe via `\\.\pipe\jobserver-...`): deferred to
//!   a follow-up sub-task. The Windows form of the protocol is
//!   `--jobserver-auth=fifo:<name>` per GNU make 4.4+; the
//!   implementation requires a server-side message loop that's its own
//!   focused review. Until that lands, `JobserverPool::create`
//!   returns an error on Windows; callers fall back to today's
//!   uncapped behavior.

/// One token bucket exposed through the GNU make jobserver protocol.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct JobserverPool {
    capacity: usize,
    inner: zccache_platform::process::jobserver::NativeJobserver,
}

#[allow(dead_code)]
impl JobserverPool {
    pub(crate) fn create(capacity: usize) -> std::io::Result<Self> {
        if capacity == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "JobserverPool capacity must be > 0; use no jobserver instead of capacity=0",
            ));
        }
        let inner = zccache_platform::process::jobserver::NativeJobserver::create(capacity)?;
        Ok(Self { capacity, inner })
    }

    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    pub(crate) fn auth_string(&self) -> String {
        self.inner.auth_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_with_zero_capacity_is_invalid() {
        let error = JobserverPool::create(0).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn create_matches_host_capability() {
        match JobserverPool::create(8) {
            Ok(pool) => {
                assert!(zccache_platform::process::jobserver::is_supported());
                assert_eq!(pool.capacity(), 8);
                let auth = pool.auth_string();
                let parts: Vec<&str> = auth.split(',').collect();
                assert_eq!(parts.len(), 2, "auth string should be R,W: {auth:?}");
                assert!(parts.iter().all(|part| part.parse::<i32>().is_ok()));
            }
            Err(error) => {
                assert!(!zccache_platform::process::jobserver::is_supported());
                assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
            }
        }
    }
}
