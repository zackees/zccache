//! #1904 — the daemon ownership record is scoped to the endpoint.
//!
//! A second daemon started with `ZCCACHE_ENDPOINT=/tmp/custom.sock` used to
//! share one global lock file and identity sidecar with the default-endpoint
//! daemon, so `zccache stop` on the default endpoint could force-kill the
//! custom-endpoint daemon, and `check_running_daemon()` on a custom endpoint
//! unlinked the DEFAULT socket — wedging a live daemon out of reach of every
//! client.

use super::EnvGuard;
use crate::{
    backend_identity_path, backend_identity_path_for, check_running_daemon_for,
    daemon_identity_matches_for, default_endpoint, lock_file_path, lock_file_path_for,
    read_backend_identity, read_backend_identity_for, verify_daemon_pid_for,
    write_backend_identity, write_backend_identity_for, write_lock_file_for,
};

/// A non-default endpoint rooted inside the per-test cache dir, so even the
/// retire branch in `check_running_daemon_for` touches nothing shared.
fn custom_endpoint(cache: &std::path::Path, name: &str) -> String {
    cache.join(name).to_string_lossy().into_owned()
}

#[test]
fn two_endpoints_get_distinct_lock_files() {
    let cache = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set_cache_dir(cache.path());
    assert_ne!(
        lock_file_path_for(&custom_endpoint(cache.path(), "a.sock")),
        lock_file_path_for(&custom_endpoint(cache.path(), "b.sock"))
    );
}

#[test]
fn two_endpoints_get_distinct_backend_identity_paths() {
    let cache = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set_cache_dir(cache.path());
    assert_ne!(
        backend_identity_path_for(&custom_endpoint(cache.path(), "a.sock")),
        backend_identity_path_for(&custom_endpoint(cache.path(), "b.sock"))
    );
}

#[test]
fn the_default_endpoint_keeps_its_historical_lock_name() {
    // Compatibility guard: existing tests (and every already-installed build)
    // hardcode `daemon-{v}.lock` / `daemon-{ns}.running-process.json`. The
    // scope segment is appended ONLY for a non-default endpoint.
    let cache = tempfile::tempdir().unwrap();
    let cache_dir = cache.path().join("zc");
    let _env = EnvGuard::set_cache_dir(&cache_dir);

    let v = zccache_core::config::versioned_subdir();
    let eff = zccache_core::config::effective_cache_root_from_top_level(
        &zccache_core::NormalizedPath::from(cache_dir.clone()),
    );

    assert_eq!(lock_file_path_for(&default_endpoint()), lock_file_path());
    assert_eq!(
        lock_file_path_for(&default_endpoint()),
        eff.join(format!("daemon-{v}.lock"))
    );
    assert_eq!(
        backend_identity_path_for(&default_endpoint()),
        backend_identity_path()
    );
}

// Returns a PID that is confirmed dead, so `check_running_daemon_for` takes the
// retire branch rather than the "leave a live daemon alone" branch. A PID that
// merely *cannot* be verified (e.g. 1) would take the wrong branch.
#[cfg(unix)]
fn dead_pid() -> u32 {
    spawn_and_reap(std::process::Command::new("true"))
}

#[cfg(windows)]
fn dead_pid() -> u32 {
    // `Command::args` chains off `&mut Command` and returns the reference;
    // build the owned `Command` first so `spawn_and_reap` receives it (the
    // same `&mut Command` vs `Command` shape this module's sibling harness
    // hit in daemon_lifecycle).
    let mut command = std::process::Command::new("cmd");
    command.args(["/c", "exit", "0"]);
    spawn_and_reap(command)
}

fn spawn_and_reap(mut command: std::process::Command) -> u32 {
    let child = command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn short-lived helper process");
    let pid = child.id();
    child.wait_with_output().expect("reap helper process");
    assert!(
        !crate::is_process_alive(pid),
        "helper process {pid} must be reaped dead for this test to mean anything"
    );
    pid
}

/// Failure B: managing a custom endpoint must not retire the DEFAULT
/// endpoint's socket, or a live default daemon is wedged out of reach of
/// every client.
///
/// The fixture has to be a real unix socket, not a plain file at the same
/// path: `retire_socket_endpoint` refuses to unlink anything that is not a
/// socket (`InvalidInput`), so a regular file would make the pre-fix retire a
/// silent no-op and this test would pass against the bug it exists to catch.
#[cfg(unix)]
#[test]
fn check_running_daemon_does_not_retire_the_default_endpoint() {
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::net::UnixListener;

    let cache = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set_cache_dir(cache.path());
    let default = default_endpoint();
    let custom = custom_endpoint(cache.path(), "custom.sock");
    write_lock_file_for(&custom, dead_pid()).expect("seed custom-endpoint lock");

    // Bound and named — the listener stays alive for the whole test, so if the
    // path is unlinked the daemon behind it is unreachable even though the
    // process still holds the descriptor.
    let _listener = UnixListener::bind(&default).expect("bind the default endpoint socket");
    assert!(
        std::fs::symlink_metadata(&default)
            .expect("stat default endpoint")
            .file_type()
            .is_socket(),
        "the fixture must be a socket, or the retire under test is a no-op"
    );

    assert_eq!(check_running_daemon_for(&custom), None);
    assert!(
        std::path::Path::new(&default).exists(),
        "check_running_daemon_for({custom}) must not retire the default endpoint {default}"
    );
}

/// The control for the test above: the fixture really does disappear when the
/// endpoint it names is retired. Without it, the survival assertion above could
/// pass for the wrong reason (a path that was never a socket in the first
/// place).
#[cfg(unix)]
#[test]
fn check_running_daemon_does_retire_the_endpoint_it_was_given() {
    use std::os::unix::net::UnixListener;

    let cache = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set_cache_dir(cache.path());
    let custom = custom_endpoint(cache.path(), "custom.sock");
    write_lock_file_for(&custom, dead_pid()).expect("seed custom-endpoint lock");
    let _listener = UnixListener::bind(&custom).expect("bind the custom endpoint socket");

    assert_eq!(check_running_daemon_for(&custom), None);
    assert!(
        !std::path::Path::new(&custom).exists(),
        "a confirmed-dead custom-endpoint lock must retire {custom}, or the \
         survival test above proves nothing"
    );
}

#[test]
fn check_running_daemon_for_removes_only_its_own_lock() {
    let cache = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set_cache_dir(cache.path());
    let custom = custom_endpoint(cache.path(), "custom.sock");
    write_lock_file_for(&custom, dead_pid()).expect("seed custom-endpoint lock");
    write_lock_file_for(&default_endpoint(), std::process::id())
        .expect("seed default-endpoint lock with a live PID");

    assert_eq!(check_running_daemon_for(&custom), None);
    assert!(
        !lock_file_path_for(&custom).as_path().exists(),
        "the confirmed-dead custom-endpoint lock must be cleaned up"
    );
    assert!(
        lock_file_path_for(&default_endpoint()).as_path().exists(),
        "the live default-endpoint lock must survive"
    );
}

/// The other half of #1904's Failure A: the identity sidecar is an ownership
/// record too, and it was written and read through `default_cache_dir()` with
/// no endpoint argument.
///
/// A daemon started with `ZCCACHE_ENDPOINT=/tmp/custom.sock` therefore
/// overwrote the default daemon's identity. `force_kill_verified_daemon` then
/// validated the default endpoint's PID against the *custom* daemon's identity
/// and terminated it — "daemon process <B> terminated after IPC connection
/// failed" for a daemon the caller never reached.
///
/// Before the fix, `write_backend_identity` wrote to the default sidecar and
/// `read_backend_identity` read it back, so both the write and the read
/// assertions below failed.
#[test]
fn a_custom_endpoint_identity_never_lands_on_the_default_endpoint() {
    let cache = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set_cache_dir(cache.path());
    let custom = custom_endpoint(cache.path(), "custom.sock");

    let identity = super::fake_identity(4321, 1_700_000_000_000, "boot-a");
    write_backend_identity_for(&custom, &identity).expect("write custom-endpoint identity");

    assert_eq!(
        read_backend_identity_for(&custom).map(|d| d.pid()),
        Some(4321),
        "the daemon must be able to read back its own endpoint's identity"
    );
    assert!(
        read_backend_identity().is_none(),
        "#1904: a daemon on {custom} must not publish its identity as the DEFAULT \
         endpoint's — that record is what `zccache stop` authorises a kill against"
    );
}

/// The kill gate itself: a PID taken from one endpoint's lock must be
/// authorised only against that endpoint's identity. With the endpoint-less
/// read, a PID recorded for the default endpoint verified for *every*
/// endpoint, so `zccache stop <custom>` could terminate the default daemon.
#[test]
fn a_pid_is_never_authorised_against_another_endpoints_identity() {
    let cache = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set_cache_dir(cache.path());
    let custom = custom_endpoint(cache.path(), "custom.sock");

    // The default endpoint records the only identity on disk.
    let default_identity = super::fake_identity(1111, 1_700_000_000_000, "boot-a");
    write_backend_identity(&default_identity).expect("write default identity");

    assert!(
        !verify_daemon_pid_for(&custom, 1111),
        "#1904: the default endpoint's PID must not verify for {custom}"
    );
    assert!(
        !daemon_identity_matches_for(&custom, &default_identity),
        "#1904: the default endpoint's identity must not match anything recorded for {custom}"
    );
}
