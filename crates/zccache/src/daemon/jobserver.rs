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
//!   focused review. Until that lands, [`JobserverPool::create`]
//!   returns an error on Windows; callers fall back to today's
//!   uncapped behavior.

#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

/// One token bucket exposed via the GNU make jobserver protocol.
///
/// Created with a fixed capacity at daemon start; lives for the
/// daemon's lifetime; tokens are returned to the bucket as compile
/// jobs finish.
///
/// Note on the `#[allow(dead_code)]`: this primitive ships ahead of
/// its consumer. Sub-task #816 of the #813 epic wires it into the
/// daemon-state init + cargo env injection paths. Removing the allow
/// when #816 lands is part of that PR's checklist.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct JobserverPool {
    capacity: usize,
    #[cfg(unix)]
    inner: PosixPipe,
    // Windows variant intentionally absent in this sub-task — see
    // module docs for the deferral rationale.
    #[cfg(not(unix))]
    _phantom: std::marker::PhantomData<()>,
}

#[allow(dead_code)]
impl JobserverPool {
    /// Allocate a pool with `capacity` tokens. Returns `Err` on
    /// unsupported platforms (Windows, until the follow-up sub-task
    /// adds the named-pipe form) and on POSIX `pipe2` / write failures.
    pub(crate) fn create(capacity: usize) -> std::io::Result<Self> {
        if capacity == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "JobserverPool capacity must be > 0; use no jobserver instead of capacity=0",
            ));
        }

        #[cfg(unix)]
        {
            let inner = PosixPipe::create(capacity)?;
            Ok(Self { capacity, inner })
        }

        #[cfg(not(unix))]
        {
            let _ = capacity;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "JobserverPool is POSIX-only in this build; Windows support \
                 (named pipe form, --jobserver-auth=fifo:NAME) is a separate \
                 sub-task of #813",
            ))
        }
    }

    /// Total tokens this pool was created with. Does not reflect
    /// current availability — by design, the pool is opaque about its
    /// in-flight state; the kernel pipe is the only source of truth.
    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    /// The `--jobserver-auth=<value>` payload to inject into spawned
    /// cargo/rustc env (typically via `MAKEFLAGS`).
    ///
    /// POSIX form: `R,W` where R and W are the raw FDs of the pipe's
    /// read and write ends. The spawned process inherits the FDs
    /// (we deliberately do NOT set `O_CLOEXEC` here — see
    /// [`PosixPipe::create`] for the platform notes).
    pub(crate) fn auth_string(&self) -> String {
        #[cfg(unix)]
        {
            format!("{},{}", self.inner.read_fd(), self.inner.write_fd())
        }

        #[cfg(not(unix))]
        {
            unreachable!("JobserverPool cannot be constructed on non-POSIX in this build")
        }
    }
}

#[cfg(unix)]
#[derive(Debug)]
struct PosixPipe {
    read: OwnedFd,
    write: OwnedFd,
}

#[cfg(unix)]
impl PosixPipe {
    fn create(capacity: usize) -> std::io::Result<Self> {
        // `pipe2(O_CLOEXEC)` keeps these FDs out of unrelated forks.
        // The daemon explicitly clears CLOEXEC at spawn time for the
        // children that should inherit (sub-task #816's responsibility);
        // by default, descendants are isolated.
        let mut fds = [0_i32; 2];
        let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };

        // Prime the pipe with one byte per token.
        let bytes = vec![b'+'; capacity];
        let written = unsafe {
            libc::write(
                write.as_raw_fd(),
                bytes.as_ptr() as *const libc::c_void,
                bytes.len(),
            )
        };
        if written < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if (written as usize) != bytes.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "jobserver pipe priming wrote {} of {} bytes",
                    written,
                    bytes.len()
                ),
            ));
        }

        Ok(Self { read, write })
    }

    fn read_fd(&self) -> RawFd {
        self.read.as_raw_fd()
    }

    fn write_fd(&self) -> RawFd {
        self.write.as_raw_fd()
    }
}

#[cfg(unix)]
use std::os::fd::FromRawFd;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn create_with_capacity_succeeds() {
        let pool = JobserverPool::create(8).unwrap();
        assert_eq!(pool.capacity(), 8);
        let auth = pool.auth_string();
        // Format: "R,W" where R and W are decimal file descriptors.
        let parts: Vec<&str> = auth.split(',').collect();
        assert_eq!(parts.len(), 2, "auth string should be R,W: got {auth:?}");
        let _r: i32 = parts[0].parse().expect("read fd parseable");
        let _w: i32 = parts[1].parse().expect("write fd parseable");
    }

    #[cfg(unix)]
    #[test]
    fn create_with_zero_capacity_is_invalid() {
        let err = JobserverPool::create(0).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[cfg(unix)]
    #[test]
    fn pipe_contains_capacity_tokens_after_create() {
        use std::os::fd::AsRawFd;
        let pool = JobserverPool::create(3).unwrap();
        // Drain the pipe directly; each byte is one token. The pool
        // must have exactly `capacity` bytes available.
        let mut buf = [0_u8; 32];
        let n = unsafe {
            libc::read(
                pool.inner.read.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        assert!(n >= 0, "read should succeed on a freshly-primed pipe");
        assert_eq!(n as usize, 3, "pipe should hold exactly 3 tokens");
        for &b in &buf[..3] {
            assert_eq!(b, b'+', "token byte should be '+': got {b:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn dropping_pool_closes_pipe() {
        use std::os::fd::AsRawFd;
        let read_fd;
        {
            let pool = JobserverPool::create(1).unwrap();
            read_fd = pool.inner.read.as_raw_fd();
            // pool drops at end of block.
        }
        // The dropped pool's OwnedFds close their FDs. A subsequent
        // read against the (now-closed) FD should fail with EBADF.
        let mut buf = [0_u8; 1];
        let n = unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        assert_eq!(n, -1, "read on closed fd should fail");
        let err = std::io::Error::last_os_error();
        assert_eq!(err.raw_os_error(), Some(libc::EBADF));
    }

    #[cfg(not(unix))]
    #[test]
    fn windows_create_returns_unsupported() {
        let err = JobserverPool::create(4).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
    }
}
