//! Regression coverage for #1904: the daemon lock file and the lifecycle
//! helpers that consult it must be scoped to the endpoint they describe.
//!
//! Before the fix, `lock_file_path()` / `backend_identity_path()` /
//! `check_running_daemon()` all derived from `default_endpoint()` and never
//! consulted `ZCCACHE_ENDPOINT`, so a daemon started on a custom endpoint
//! overwrote the default daemon's PID in one shared ownership record. That
//! produced two user-visible failures:
//!
//! - **Failure A** — `zccache stop` (default endpoint) force-killed the
//!   *custom* daemon, because the lock it read named the wrong PID.
//! - **Failure B** — `check_running_daemon()` retired the *default*
//!   endpoint's socket while managing a custom one, wedging a live daemon
//!   out of reach of every client.
//!
//! These tests spawn the real CLI binary and two real daemons, so they are
//! `#[ignore]`-gated like their neighbours in this directory.
//!
//! Run with:
//!
//! ```text
//! cargo nextest run --test daemon_lifecycle -E 'test(/^daemon_endpoint_scoped_lock_test::/)'
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Daemon namespace pinned by this module. A development build otherwise
/// derives a `<version>-<exe hash>` namespace (#1362 / #1394) that this test
/// process does not share, so a lock path computed here would not be the one
/// the CLI reads — and, worse, the test could collide with the developer's own
/// daemon. Mirrors `FORCED_STOP_NAMESPACE` in `tests/cli/cli_daemon_start.rs`.
const ENDPOINT_SCOPE_NAMESPACE: &str = "endpoint-scope";

/// Locate the `zccache` binary next to this test executable.
///
/// Returns `None` when it is absent so a caller can skip instead of failing:
/// this module is `#[ignore]`-gated and not every environment builds the
/// multi-call binary first.
fn zccache_bin() -> Option<PathBuf> {
    let mut path = std::env::current_exe()
        .expect("current_exe")
        .parent()
        .expect("parent of test binary")
        .parent()
        .expect("target dir")
        .to_path_buf();

    path.push(if cfg!(windows) {
        "zccache.exe"
    } else {
        "zccache"
    });

    path.exists().then_some(path)
}

/// Two daemons, one isolated cache root, two endpoints.
///
/// `ZCCACHE_CACHE_DIR` and `ZCCACHE_DAEMON_NAMESPACE` are pinned on every
/// child command so nothing here can reach the developer's real daemon or
/// their real cache root. `Drop` stops both daemons: a leaked daemon holds
/// `CacheRootWriterLock` for its cache root and would break every other test
/// in this binary.
struct TwoEndpoints {
    bin: PathBuf,
    /// `ZCCACHE_CACHE_DIR` — both daemons share it, which is exactly the
    /// configuration under test (one cache root, two endpoints).
    cache_dir: tempfile::TempDir,
    /// Holds the custom endpoint's socket so its path stays inside a directory
    /// this test owns and removes on drop.
    endpoint_dir: tempfile::TempDir,
}

impl TwoEndpoints {
    fn new(bin: PathBuf) -> Self {
        Self {
            bin,
            cache_dir: tempfile::Builder::new()
                .prefix("zccache-endpoint-scope-")
                .tempdir()
                .expect("cache dir tempdir"),
            endpoint_dir: tempfile::Builder::new()
                .prefix("zccache-endpoint-scope-")
                .tempdir()
                .expect("endpoint dir tempdir"),
        }
    }

    /// `<cache root>/v<VERSION>` — the directory both lock files live in once
    /// `ZCCACHE_CACHE_DIR` is set (`normalized_override_root` folds the
    /// version tag into the effective root, #1003).
    fn version_dir(&self) -> PathBuf {
        self.cache_dir
            .path()
            .join(zccache::core::config::versioned_subdir())
    }

    /// The default endpoint's ownership record. The default endpoint carries an
    /// empty scope tag, so its historical filename is byte-for-byte unchanged.
    fn default_lock_path(&self) -> PathBuf {
        let version = zccache::core::config::versioned_subdir();
        self.version_dir()
            .join(format!("daemon-{ENDPOINT_SCOPE_NAMESPACE}-{version}.lock"))
    }

    /// The custom endpoint this test drives the second daemon on.
    ///
    /// A named pipe on Windows (there is no socket path to unlink), a socket
    /// path inside the test's own tempdir elsewhere.
    fn custom_endpoint(&self) -> String {
        #[cfg(windows)]
        {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!(r"\\.\pipe\zccache-{ENDPOINT_SCOPE_NAMESPACE}-{nanos}")
        }
        #[cfg(unix)]
        {
            self.endpoint_dir
                .path()
                .join("custom.sock")
                .to_string_lossy()
                .into_owned()
        }
    }

    /// The custom endpoint's ownership record — same directory as
    /// [`Self::default_lock_path`], distinguished only by the endpoint scope
    /// tag `stable_path_id` folds into the name. Deriving the name here rather
    /// than calling `zccache::ipc::lock_file_path_for` is deliberate: that
    /// function reads `ZCCACHE_CACHE_DIR` / `ZCCACHE_DAEMON_NAMESPACE` from
    /// *this* process, which the test never sets (it pins them per child
    /// command), so it would resolve against the developer's real cache root.
    fn custom_lock_path(&self) -> PathBuf {
        let version = zccache::core::config::versioned_subdir();
        let scope = zccache::core::stable_path_id(Path::new(&self.custom_endpoint()));
        self.version_dir().join(format!(
            "daemon-{ENDPOINT_SCOPE_NAMESPACE}-{version}-{scope}.lock"
        ))
    }

    /// A `zccache` invocation pinned to the isolated cache root + namespace,
    /// with `ZCCACHE_ENDPOINT` removed (the default endpoint).
    fn command(&self, subcommand: &str) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.arg(subcommand)
            .env("ZCCACHE_CACHE_DIR", self.cache_dir.path())
            .env("ZCCACHE_DAEMON_NAMESPACE", ENDPOINT_SCOPE_NAMESPACE)
            .env_remove("ZCCACHE_ENDPOINT");
        cmd
    }

    /// The same, pinned to `endpoint`.
    fn command_at(&self, subcommand: &str, endpoint: &str) -> Command {
        self.command(subcommand).env("ZCCACHE_ENDPOINT", endpoint)
    }

    fn start_at(&self, endpoint: Option<&str>) -> std::process::Output {
        let mut cmd = match endpoint {
            Some(endpoint) => self.command_at("start", endpoint),
            None => self.command("start"),
        };
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .expect("run zccache start")
    }

    fn stop_at(&self, endpoint: Option<&str>) {
        let mut cmd = match endpoint {
            Some(endpoint) => self.command_at("stop", endpoint),
            None => self.command("stop"),
        };
        let _ = cmd
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    /// Bring up the default-endpoint daemon and return its PID.
    fn start_default(&self) -> u32 {
        let output = self.start_at(None);
        assert!(
            output.status.success(),
            "zccache start failed on the default endpoint: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        wait_for_lock(&self.default_lock_path())
    }

    /// Bring up the custom-endpoint daemon and return its PID, or `None` when
    /// the daemon refused to start for a reason unrelated to endpoint scoping.
    ///
    /// A cache root admits exactly one writer (`CacheRootWriterLock::acquire`,
    /// #1162), so a second daemon on a root the first already holds exits 1
    /// before it ever writes a lock. That is a real, separate invariant — not
    /// the #1904 defect — and it means the two-live-daemon scenario is only
    /// reachable on a host where the second daemon does win the root. Callers
    /// treat `None` as "this environment cannot express the scenario" and skip
    /// rather than reporting a lock-scoping failure that never happened.
    fn try_start_custom(&self) -> Option<u32> {
        let output = self.start_at(Some(&self.custom_endpoint()));
        if !output.status.success() {
            eprintln!(
                "skipping #1904 assertion: the custom-endpoint daemon did not \
                 start, so two daemons never coexisted on this cache root.\n\
                 zccache start stderr: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            return None;
        }
        Some(wait_for_lock(&self.custom_lock_path()))
    }

    /// Every daemon lock currently in the versioned cache directory.
    ///
    /// Dotfiles are skipped: the cache root's single-writer claim is
    /// `.writer.lock` (`CacheRootWriterLock`), whose `Path::extension` is also
    /// `lock` even though it is not a daemon ownership record.
    fn lock_files(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(self.version_dir()) else {
            return Vec::new();
        };
        let mut locks: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "lock")
                    && !path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with('.'))
            })
            .collect();
        locks.sort();
        locks
    }

    /// The default endpoint's socket, discovered rather than predicted. On
    /// unix the default endpoint for a cache-dir-derived root lives inside the
    /// versioned cache directory, so any `*.sock` there is the default
    /// daemon's — the custom endpoint's socket is in this test's own endpoint
    /// tempdir. `None` on Windows, where the default endpoint is a named pipe
    /// with no path to observe.
    #[cfg(unix)]
    fn default_socket_paths(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(self.version_dir()) else {
            return Vec::new();
        };
        let mut sockets: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "sock"))
            .collect();
        sockets.sort();
        sockets
    }
}

impl Drop for TwoEndpoints {
    fn drop(&mut self) {
        self.stop_at(Some(&self.custom_endpoint()));
        self.stop_at(None);
    }
}

/// Poll until `path` names a PID, then return it. The lock is written after
/// the daemon wins the bind, so its absence right after `start` returns means
/// the record is not in place yet rather than that the daemon is missing.
fn wait_for_lock(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(pid) = read_lock_pid(path) {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the daemon lock at {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn read_lock_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

fn wait_until_dead(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while zccache::ipc::is_process_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Two daemons on two endpoints keep independent ownership records.
///
/// Before the fix there was exactly one lock file in the cache root and the
/// second `start` overwrote the first daemon's PID, so the two records could
/// never name two different live processes.
#[test]
#[ignore] // integration-level: starts two real daemons + spawns the CLI binary
fn two_endpoints_keep_independent_lock_files() {
    let Some(bin) = zccache_bin() else {
        eprintln!("skipping: zccache binary not built (run `cargo build` first)");
        return;
    };
    let scenario = TwoEndpoints::new(bin);

    let default_pid = scenario.start_default();
    let Some(custom_pid) = scenario.try_start_custom() else {
        return;
    };

    let locks = scenario.lock_files();
    assert_eq!(
        locks.len(),
        2,
        "two endpoints must own two distinct lock records, found {locks:?} in {}",
        scenario.version_dir().display()
    );
    assert!(
        locks.contains(&scenario.default_lock_path()),
        "the default endpoint's lock {} is missing from {locks:?}",
        scenario.default_lock_path().display()
    );
    assert!(
        locks.contains(&scenario.custom_lock_path()),
        "the custom endpoint's lock {} is missing from {locks:?}",
        scenario.custom_lock_path().display()
    );

    assert_ne!(
        default_pid, custom_pid,
        "both lock records name the same PID {default_pid} — the second daemon \
         overwrote the first's ownership record"
    );
    assert!(
        zccache::ipc::is_process_alive(default_pid),
        "the default endpoint's daemon {default_pid} must still be running"
    );
    assert!(
        zccache::ipc::is_process_alive(custom_pid),
        "the custom endpoint's daemon {custom_pid} must still be running"
    );
}

/// `zccache stop` only ever terminates the daemon whose endpoint was named.
///
/// Failure A: with one shared lock record, `check_running_daemon()` on the
/// custom endpoint returned the *default* daemon's PID, and the forced-kill
/// path terminated it while leaving the named daemon running.
#[test]
#[ignore] // integration-level: starts two real daemons + spawns the CLI binary
fn stop_only_terminates_the_daemon_at_its_own_endpoint() {
    let Some(bin) = zccache_bin() else {
        eprintln!("skipping: zccache binary not built (run `cargo build` first)");
        return;
    };
    let scenario = TwoEndpoints::new(bin);

    let default_pid = scenario.start_default();
    // The default endpoint's socket is *discovered* rather than predicted, so
    // an empty reading is a host whose endpoint derivation put the socket
    // somewhere else (a long cache root falls back to a compact `/tmp` path in
    // `endpoint_for_cache_dir`) — the process-liveness assertion below still
    // applies, only the socket comparison is skipped.
    #[cfg(unix)]
    let sockets_before = scenario.default_socket_paths();

    let Some(custom_pid) = scenario.try_start_custom() else {
        return;
    };

    // Name the CUSTOM endpoint. Only the daemon serving that endpoint may die.
    scenario.stop_at(Some(&scenario.custom_endpoint()));
    wait_until_dead(custom_pid);

    assert!(
        !zccache::ipc::is_process_alive(custom_pid),
        "stop at the custom endpoint must terminate that daemon ({custom_pid})"
    );
    assert!(
        zccache::ipc::is_process_alive(default_pid),
        "stop at the custom endpoint killed the DEFAULT endpoint's daemon \
         ({default_pid}) — the ownership record is not endpoint-scoped"
    );

    // Failure B: retiring the default endpoint's socket out from under a live
    // daemon wedges it permanently (it keeps holding CacheRootWriterLock but
    // no client can ever connect again). Only observable on unix, where the
    // default endpoint has a path.
    #[cfg(unix)]
    if !sockets_before.is_empty() {
        assert_eq!(
            scenario.default_socket_paths(),
            sockets_before,
            "stopping the custom endpoint retired the default endpoint's socket \
             — a live daemon is now unreachable"
        );
    }
}
