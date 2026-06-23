//! Unit tests for the `ipc` module. Split out of `mod.rs` to keep that
//! file under the repo per-file LOC ceiling; included via `#[path]` from
//! `mod.rs` so `super` still resolves to the `ipc` module.

use super::test_env::ENV_LOCK;
use super::*;
use std::ffi::OsString;
use std::sync::MutexGuard;

struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    previous_cache_dir: Option<OsString>,
    previous_namespace: Option<OsString>,
    previous_running_process_disable: Option<OsString>,
}

impl EnvGuard {
    fn set_cache_dir(value: &std::path::Path) -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous_cache_dir = std::env::var_os(crate::core::config::CACHE_DIR_ENV);
        let previous_namespace = std::env::var_os(crate::core::config::DAEMON_NAMESPACE_ENV);
        let previous_running_process_disable = std::env::var_os(RUNNING_PROCESS_DISABLE_ENV);
        std::env::set_var(crate::core::config::CACHE_DIR_ENV, value);
        std::env::remove_var(crate::core::config::DAEMON_NAMESPACE_ENV);
        Self {
            _lock: lock,
            previous_cache_dir,
            previous_namespace,
            previous_running_process_disable,
        }
    }

    fn set_cache_dir_and_namespace(value: &std::path::Path, namespace: &str) -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous_cache_dir = std::env::var_os(crate::core::config::CACHE_DIR_ENV);
        let previous_namespace = std::env::var_os(crate::core::config::DAEMON_NAMESPACE_ENV);
        let previous_running_process_disable = std::env::var_os(RUNNING_PROCESS_DISABLE_ENV);
        std::env::set_var(crate::core::config::CACHE_DIR_ENV, value);
        std::env::set_var(crate::core::config::DAEMON_NAMESPACE_ENV, namespace);
        Self {
            _lock: lock,
            previous_cache_dir,
            previous_namespace,
            previous_running_process_disable,
        }
    }

    fn isolate_running_process_disable() -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous_cache_dir = std::env::var_os(crate::core::config::CACHE_DIR_ENV);
        let previous_namespace = std::env::var_os(crate::core::config::DAEMON_NAMESPACE_ENV);
        let previous_running_process_disable = std::env::var_os(RUNNING_PROCESS_DISABLE_ENV);
        std::env::remove_var(RUNNING_PROCESS_DISABLE_ENV);
        Self {
            _lock: lock,
            previous_cache_dir,
            previous_namespace,
            previous_running_process_disable,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous_cache_dir {
            Some(value) => std::env::set_var(crate::core::config::CACHE_DIR_ENV, value),
            None => std::env::remove_var(crate::core::config::CACHE_DIR_ENV),
        }
        match &self.previous_namespace {
            Some(value) => std::env::set_var(crate::core::config::DAEMON_NAMESPACE_ENV, value),
            None => std::env::remove_var(crate::core::config::DAEMON_NAMESPACE_ENV),
        }
        match &self.previous_running_process_disable {
            Some(value) => std::env::set_var(RUNNING_PROCESS_DISABLE_ENV, value),
            None => std::env::remove_var(RUNNING_PROCESS_DISABLE_ENV),
        }
    }
}

fn test_daemon_status(endpoint: &str) -> crate::protocol::DaemonStatus {
    crate::protocol::DaemonStatus {
        version: crate::core::VERSION.to_string(),
        daemon_namespace: "test".to_string(),
        endpoint: endpoint.to_string(),
        private_daemon: crate::protocol::PrivateDaemonStatus::shared(),
        artifact_count: 0,
        cache_size_bytes: 0,
        metadata_entries: 0,
        uptime_secs: 1,
        cache_hits: 0,
        cache_misses: 0,
        total_compilations: 0,
        non_cacheable: 0,
        compile_errors: 0,
        compile_errors_cached: 0,
        time_saved_ms: 0,
        total_links: 0,
        link_hits: 0,
        link_misses: 0,
        link_non_cacheable: 0,
        dep_graph_contexts: 0,
        dep_graph_files: 0,
        sessions_total: 0,
        sessions_active: 0,
        cache_dir: std::env::temp_dir().into(),
        dep_graph_version: crate::depgraph::DEPGRAPH_VERSION,
        dep_graph_disk_size: 0,
        dep_graph_persisted: false,
    }
}

#[tokio::test]
async fn daemon_control_roundtrip_auto_prefers_prost_for_status() {
    let endpoint = unique_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).unwrap();
    let expected_endpoint = endpoint.clone();

    let server = tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap();
        let msg: Option<
            crate::protocol::DecodedWireMessage<
                crate::protocol::Request,
                crate::protocol::wire_prost::zccache_v1::Request,
            >,
        > = conn.recv_wire().await.unwrap();
        match msg {
            Some(crate::protocol::DecodedWireMessage::ProstV16(request)) => {
                assert_eq!(request.request_id, "control-status");
                assert!(matches!(
                    request.body,
                    Some(crate::protocol::wire_prost::zccache_v1::request::Body::Status(_))
                ));
                let response = Response::Status(test_daemon_status(&expected_endpoint));
                let response =
                    wire_prost::supported_control_response_to_prost(&response, &request.request_id)
                        .unwrap();
                conn.send_prost(&response).await.unwrap();
            }
            other => panic!("expected prost status request, got {other:?}"),
        }
    });

    let response = daemon_control_roundtrip_with_selection(
        &endpoint,
        DaemonControlRequest::Status,
        None,
        wire_prost::ClientWireSelection::Auto,
    )
    .await
    .unwrap();

    match response {
        Some(Response::Status(status)) => assert_eq!(status.endpoint, endpoint),
        other => panic!("expected Status response, got {other:?}"),
    }

    server.await.unwrap();
}

#[tokio::test]
async fn daemon_control_roundtrip_auto_prefers_prost_for_clear() {
    let endpoint = unique_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).unwrap();

    let server = tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap();
        let msg: Option<
            crate::protocol::DecodedWireMessage<
                crate::protocol::Request,
                crate::protocol::wire_prost::zccache_v1::Request,
            >,
        > = conn.recv_wire().await.unwrap();
        match msg {
            Some(crate::protocol::DecodedWireMessage::ProstV16(request)) => {
                assert_eq!(request.request_id, "control-clear");
                assert!(matches!(
                    request.body,
                    Some(crate::protocol::wire_prost::zccache_v1::request::Body::Clear(_))
                ));
                let response = Response::Cleared {
                    artifacts_removed: 1,
                    metadata_cleared: 2,
                    dep_graph_contexts_cleared: 3,
                    on_disk_bytes_freed: 4,
                };
                let response =
                    wire_prost::supported_control_response_to_prost(&response, &request.request_id)
                        .unwrap();
                conn.send_prost(&response).await.unwrap();
            }
            other => panic!("expected prost clear request, got {other:?}"),
        }
    });

    let response = daemon_control_roundtrip_with_selection(
        &endpoint,
        DaemonControlRequest::Clear,
        None,
        wire_prost::ClientWireSelection::Auto,
    )
    .await
    .unwrap();

    match response {
        Some(Response::Cleared {
            artifacts_removed,
            metadata_cleared,
            dep_graph_contexts_cleared,
            on_disk_bytes_freed,
        }) => {
            assert_eq!(artifacts_removed, 1);
            assert_eq!(metadata_cleared, 2);
            assert_eq!(dep_graph_contexts_cleared, 3);
            assert_eq!(on_disk_bytes_freed, 4);
        }
        other => panic!("expected Cleared response, got {other:?}"),
    }

    server.await.unwrap();
}

#[tokio::test]
async fn daemon_control_roundtrip_bincode_selection_stays_v15_for_status() {
    let endpoint = unique_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).unwrap();
    let expected_endpoint = endpoint.clone();

    let server = tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap();
        let request: Option<crate::protocol::Request> = conn.recv().await.unwrap();
        assert_eq!(request, Some(crate::protocol::Request::Status));
        conn.send(&Response::Status(test_daemon_status(&expected_endpoint)))
            .await
            .unwrap();
    });

    let response = daemon_control_roundtrip_with_selection(
        &endpoint,
        DaemonControlRequest::Status,
        None,
        wire_prost::ClientWireSelection::BincodeV15,
    )
    .await
    .unwrap();

    match response {
        Some(Response::Status(status)) => assert_eq!(status.endpoint, endpoint),
        other => panic!("expected bincode Status response, got {other:?}"),
    }

    server.await.unwrap();
}

#[tokio::test]
async fn daemon_control_roundtrip_auto_falls_back_to_bincode_for_old_daemon() {
    use super::broker::RUNNING_PROCESS_FAKE_BACKEND_ENV;
    use super::test_env::EnvVarGuard;

    let _env = EnvVarGuard::set_all(&[
        (RUNNING_PROCESS_DISABLE_ENV, Some("1".to_string())),
        (RUNNING_PROCESS_FAKE_BACKEND_ENV, None),
        (ZCCACHE_BROKER_CONNECT_ENV, None),
    ]);
    let endpoint = unique_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).unwrap();
    let expected_endpoint = endpoint.clone();

    let server = tokio::spawn(async move {
        let mut first = listener.accept().await.unwrap();
        match first.recv::<crate::protocol::Request>().await {
            Err(IpcError::Protocol(crate::protocol::ProtocolError::VersionMismatch {
                expected: crate::protocol::BINCODE_PROTOCOL_VERSION,
                received: crate::protocol::PROST_PROTOCOL_VERSION,
            })) => {
                first
                    .send(&Response::Error {
                        message: "protocol version mismatch: expected v15, received v16"
                            .to_string(),
                    })
                    .await
                    .unwrap();
            }
            Ok(None) => {}
            other => panic!("v16 prost request must not decode as v15 bincode: {other:?}"),
        }

        let mut second = listener.accept().await.unwrap();
        let request: Option<crate::protocol::Request> = second.recv().await.unwrap();
        assert_eq!(request, Some(crate::protocol::Request::Status));
        second
            .send(&Response::Status(test_daemon_status(&expected_endpoint)))
            .await
            .unwrap();
    });

    let response = daemon_control_roundtrip_with_selection(
        &endpoint,
        DaemonControlRequest::Status,
        None,
        wire_prost::ClientWireSelection::Auto,
    )
    .await
    .unwrap();

    match response {
        Some(Response::Status(status)) => assert_eq!(status.endpoint, endpoint),
        other => panic!("expected fallback Status response, got {other:?}"),
    }

    server.await.unwrap();
}

/// Issue #720 Phase 1: with the broker lane active (here via the
/// fake-backend seam, which yields a `Broker` route), the control
/// roundtrip must carry real traffic over the version-checked 0x7A63
/// FrameV1 envelope rather than resolve-and-drop to a bincode re-dial.
/// The server asserts it decodes a `FrameV1` request and echoes a
/// FrameV1 response on the frame correlation id.
#[tokio::test]
async fn broker_lane_control_roundtrip_uses_frame_v1() {
    use super::broker::RUNNING_PROCESS_FAKE_BACKEND_ENV;
    use super::test_env::EnvVarGuard;

    let endpoint = unique_test_endpoint();
    let _env = EnvVarGuard::set_all(&[
        (RUNNING_PROCESS_DISABLE_ENV, None),
        (
            RUNNING_PROCESS_FAKE_BACKEND_ENV,
            Some(to_running_process_endpoint(&endpoint)),
        ),
        (ZCCACHE_BROKER_CONNECT_ENV, Some("1".to_string())),
    ]);

    let mut listener = IpcListener::bind(&endpoint).unwrap();
    let expected_endpoint = endpoint.clone();

    // The broker resolution dial connects and is dropped before the data
    // connection arrives, so keep accepting until the FrameV1 request lands.
    let server = tokio::spawn(async move {
        loop {
            let mut conn = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => return false,
            };
            let msg: Option<
                crate::protocol::DecodedWireMessage<
                    crate::protocol::Request,
                    wire_prost::zccache_v1::Request,
                >,
            > = match conn.recv_wire().await {
                Ok(msg) => msg,
                // Resolution dial closed without a payload; keep waiting.
                Err(_) => continue,
            };
            match msg {
                Some(crate::protocol::DecodedWireMessage::FrameV1 {
                    message,
                    request_id,
                }) => {
                    assert_eq!(message.request_id, "control-status");
                    assert!(matches!(
                        message.body,
                        Some(wire_prost::zccache_v1::request::Body::Status(_))
                    ));
                    let response = Response::Status(test_daemon_status(&expected_endpoint));
                    let response = wire_prost::supported_control_response_to_prost(
                        &response,
                        &message.request_id,
                    )
                    .unwrap();
                    conn.send_frame_v1_response(&response, request_id)
                        .await
                        .unwrap();
                    return true;
                }
                None => continue,
                Some(other) => panic!("expected FrameV1 status request, got {other:?}"),
            }
        }
    });

    let response = daemon_control_roundtrip_with_selection(
        &endpoint,
        DaemonControlRequest::Status,
        None,
        wire_prost::ClientWireSelection::Auto,
    )
    .await
    .unwrap();

    match response {
        Some(Response::Status(status)) => assert_eq!(status.endpoint, endpoint),
        other => panic!("expected Status response, got {other:?}"),
    }
    assert!(
        server.await.unwrap(),
        "server must have decoded a FrameV1 control request"
    );
}

#[test]
fn cache_dir_override_moves_endpoint_and_lock_file() {
    let root = tempfile::tempdir().unwrap();
    let cache_dir = root.path().join("zc");
    let _env = EnvGuard::set_cache_dir(&cache_dir);

    let endpoint = default_endpoint();
    #[cfg(unix)]
    assert_eq!(
        endpoint,
        cache_dir.join("daemon.sock").to_string_lossy().into_owned()
    );
    #[cfg(windows)]
    {
        assert!(endpoint.starts_with(r"\\.\pipe\zccache-"));
        assert!(endpoint.ends_with(&crate::core::stable_path_id(&cache_dir)));
    }

    assert_eq!(lock_file_path(), cache_dir.join("daemon.lock"));
}

#[test]
fn different_cache_roots_get_different_endpoints() {
    let a = NormalizedPath::from("/tmp/zccache-a");
    let b = NormalizedPath::from("/tmp/zccache-b");
    assert_ne!(
        endpoint_for_cache_dir(&a, None),
        endpoint_for_cache_dir(&b, None)
    );
}

#[test]
fn daemon_namespace_moves_endpoint_and_lock_file() {
    let root = tempfile::tempdir().unwrap();
    let cache_dir = root.path().join("zc");
    let _env = EnvGuard::set_cache_dir_and_namespace(&cache_dir, "soldr-dev");

    let endpoint = default_endpoint();
    #[cfg(unix)]
    assert_eq!(
        endpoint,
        cache_dir
            .join("daemon-soldr-dev.sock")
            .to_string_lossy()
            .into_owned()
    );
    #[cfg(windows)]
    {
        assert!(endpoint.starts_with(r"\\.\pipe\zccache-"));
        assert!(endpoint.ends_with("-soldr-dev"));
        assert!(endpoint.contains(&crate::core::stable_path_id(&cache_dir)));
    }

    assert_eq!(lock_file_path(), cache_dir.join("daemon-soldr-dev.lock"));
}

#[test]
fn same_cache_root_different_daemon_namespaces_do_not_share_identity() {
    let root = tempfile::tempdir().unwrap();
    let cache_dir = root.path().join("zc");

    let (endpoint_a, lock_a) = {
        let _env = EnvGuard::set_cache_dir_and_namespace(&cache_dir, "soldr-dev-a");
        (default_endpoint(), lock_file_path())
    };
    let (endpoint_b, lock_b) = {
        let _env = EnvGuard::set_cache_dir_and_namespace(&cache_dir, "soldr-dev-b");
        (default_endpoint(), lock_file_path())
    };

    assert_ne!(endpoint_a, endpoint_b);
    assert_ne!(lock_a, lock_b);
}

#[test]
fn running_process_disable_requires_exact_one() {
    let _env = EnvGuard::isolate_running_process_disable();

    assert!(!running_process_disabled());

    std::env::set_var(RUNNING_PROCESS_DISABLE_ENV, "true");
    assert!(!running_process_disabled());

    std::env::set_var(RUNNING_PROCESS_DISABLE_ENV, "1");
    assert!(running_process_disabled());
}

#[test]
fn private_daemon_name_derives_endpoint_from_cache_root() {
    let root = tempfile::tempdir().unwrap();
    let cache_dir = root.path().join("zc");
    let endpoint = endpoint_for_private_daemon_name(Some(&cache_dir), "soldr dev");

    #[cfg(unix)]
    assert_eq!(
        endpoint,
        cache_dir
            .join("daemon-soldr_dev.sock")
            .to_string_lossy()
            .into_owned()
    );
    #[cfg(windows)]
    {
        assert!(endpoint.starts_with(r"\\.\pipe\zccache-"));
        assert!(endpoint.ends_with("-soldr_dev"));
        assert!(endpoint.contains(&crate::core::stable_path_id(&cache_dir)));
    }
}

#[cfg(windows)]
#[test]
fn pipe_name_keeps_safe_username_endpoint_unchanged() {
    assert_eq!(pipe_name("zackees", None), r"\\.\pipe\zccache-zackees");
}

#[cfg(windows)]
#[test]
fn pipe_name_sanitizes_username_spaces() {
    let endpoint = pipe_name("Zach Vorhies", None);
    assert!(endpoint.starts_with(r"\\.\pipe\zccache-Zach_Vorhies-"));
    assert!(!endpoint.contains(' '));
}

#[cfg(unix)]
#[test]
fn cache_dir_endpoint_falls_back_to_short_unix_socket_path() {
    let root = tempfile::tempdir().unwrap();
    let cache_dir = root
        .path()
        .join("this")
        .join("is")
        .join("a")
        .join("deep")
        .join("private")
        .join("zccache")
        .join("cache")
        .join("directory")
        .join("that")
        .join("would")
        .join("exceed")
        .join("sockaddr_un")
        .join("path")
        .join("limits");

    let endpoint = endpoint_for_cache_dir(&cache_dir, Some("soldr-dev"));

    assert!(
        endpoint.len() <= MAX_PORTABLE_UNIX_SOCKET_PATH_BYTES,
        "endpoint too long: {endpoint}"
    );
    assert!(endpoint.starts_with("/tmp/zccache-"));
    assert!(endpoint.contains(&crate::core::stable_path_id(&cache_dir)));
    assert!(endpoint.ends_with("-daemon-soldr-dev.sock"));
}

/// On macOS, `daemon_exe_for_pid` must reject a PID whose
/// executable is something other than `zccache-daemon`. Until
/// `proc_pidpath` was wired up, this returned `None` and
/// `verify_pid_exe_stem` fell back to alive-only — which meant a
/// recycled PID in `daemon.lock` could keep the CLI talking to a
/// random process on a shared CI runner. This test would have
/// failed before that fix.
#[cfg(target_os = "macos")]
#[test]
fn recycled_pid_is_rejected_on_macos() {
    use std::process::Stdio;

    // `/bin/sleep 60` — guaranteed-alive, not zccache-daemon.
    let mut sleeper = std::process::Command::new("sleep")
        .arg("60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn /bin/sleep");
    let pid = sleeper.id();

    let exe = daemon_exe_for_pid(pid);
    let verified = verify_pid_exe_stem(pid, "zccache-daemon");

    // Clean up before assertions so a panic doesn't orphan the child.
    let _ = sleeper.kill();
    let _ = sleeper.wait();

    let exe = exe.expect("proc_pidpath must succeed for an alive child");
    let basename = exe
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_owned();
    assert_eq!(
        basename, "sleep",
        "proc_pidpath should report `sleep` as the executable"
    );
    assert!(
        !verified,
        "verify_pid_exe_stem must reject a /bin/sleep PID even though it is alive"
    );
}

#[test]
fn exe_stem_matches_strips_exe_suffix_and_compares_basename() {
    use std::path::Path;
    assert!(exe_stem_matches(
        Path::new("/usr/bin/zccache-daemon"),
        "zccache-daemon"
    ));
    // A different binary at the same PID must not be accepted.
    assert!(!exe_stem_matches(
        Path::new("/usr/bin/bash"),
        "zccache-daemon"
    ));
    assert!(!exe_stem_matches(
        Path::new("/usr/bin/zccache-daemon-x"),
        "zccache-daemon"
    ));
}

/// Windows-only: backslash-separated paths require the OS-native
/// `Path::file_name` semantics. On Unix `\` is a regular filename
/// character, so the same assertion would fail there (issue #143).
#[cfg(windows)]
#[test]
fn exe_stem_matches_strips_exe_suffix_on_windows() {
    use std::path::Path;
    assert!(exe_stem_matches(
        Path::new(r"C:\bin\zccache-daemon.exe"),
        "zccache-daemon"
    ));
}

/// Regression test for issue #132: a stale `daemon.lock` restored from a
/// CI cache can carry a PID that's been recycled by an unrelated process
/// on a fresh runner. `check_running_daemon` must NOT report that process
/// as our daemon — otherwise `zccache stop` would `force_kill_process`
/// the unrelated process.
///
/// We use the test's own PID, which is guaranteed alive but is clearly
/// not zccache-daemon, then assert the lock file is treated as stale.
#[test]
fn stale_lock_with_recycled_pid_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let cache_dir = root.path().join("zc");
    let _env = EnvGuard::set_cache_dir(&cache_dir);

    let lock = lock_file_path();
    write_lock_file(std::process::id()).unwrap();
    assert!(lock.exists());

    // The test process is alive but is not zccache-daemon — must be rejected.
    // (On macOS we can't read the exe path, so this test relaxes there: see
    // `daemon_exe_for_pid` for the platform fallback.)
    #[cfg(any(target_os = "linux", windows))]
    {
        assert!(check_running_daemon().is_none());
        assert!(!lock.exists(), "stale lock file should have been removed");
    }
}

// ─── #640 probe_existing_daemon ───────────────────────────────────────

#[tokio::test]
async fn probe_returns_false_when_no_lock_file() {
    let cache = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set_cache_dir(cache.path());
    // No lock file written.
    assert!(!probe_existing_daemon("anything", std::time::Duration::from_millis(50)).await);
}

#[tokio::test]
async fn probe_returns_false_when_lock_file_records_self_pid() {
    let cache = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set_cache_dir(cache.path());
    // Self-PID early-out: we must NEVER probe our own process —
    // otherwise a sibling racing-init thread that wrote our PID
    // into the lock file would cause us to deadlock waiting for
    // ourselves to accept.
    write_lock_file(std::process::id()).unwrap();
    assert!(!probe_existing_daemon("anything", std::time::Duration::from_millis(50)).await);
}

#[tokio::test]
async fn probe_returns_false_when_lock_file_pid_is_not_a_daemon() {
    let cache = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set_cache_dir(cache.path());
    // PID 1 exists everywhere (init / System) but is definitely not
    // zccache-daemon, so verify_daemon_pid rejects it. The probe
    // must short-circuit at the PID-verification step BEFORE
    // attempting any IPC connect — otherwise we'd waste the
    // timeout budget on a doomed handshake against init.
    write_lock_file(1).unwrap();
    let start = std::time::Instant::now();
    let result = probe_existing_daemon(
        "garbage-endpoint-that-could-never-exist",
        std::time::Duration::from_millis(500),
    )
    .await;
    let elapsed = start.elapsed();
    assert!(!result);
    // Short-circuit means we returned faster than the connect
    // timeout — proves we never attempted the connect.
    assert!(
        elapsed < std::time::Duration::from_millis(250),
        "probe should have short-circuited via verify_daemon_pid, \
             not waited for the connect timeout — elapsed {elapsed:?}"
    );
}

/// Issue #774 regression. On Windows the kernel keeps a process object
/// alive as long as **any** handle references it — Task Manager, Process
/// Explorer, a sibling tool monitoring the daemon, the running-process
/// broker, etc. Plain `OpenProcess` on the dead PID still returns a
/// valid handle in that state, so the previous `is_process_alive`
/// implementation reported the dead daemon as alive and the CLI looped
/// against an orphaned pipe until the user rebooted.
///
/// This test spawns a short-lived process, waits for it to exit, then
/// pins the kernel process object open via `OpenProcess` and asserts
/// that `is_process_alive` correctly returns `false` — the
/// `WaitForSingleObject(handle, 0) == WAIT_TIMEOUT` check added in the
/// fix is what makes that work.
#[cfg(windows)]
#[test]
fn is_process_alive_returns_false_for_terminated_referenced_process() {
    use std::process::{Command, Stdio};

    let mut child = Command::new("cmd")
        .args(["/c", "exit", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cmd /c exit 0");
    let pid = child.id();
    let status = child.wait().expect("wait child");
    assert!(status.success(), "child should have exited cleanly");

    // Pin the kernel process object alive by holding our own OpenProcess
    // handle — this is exactly what an external monitor (Task Manager,
    // a parent shell's job table, fbuild's process tracker) does.
    #[allow(clashing_extern_declarations)]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn CloseHandle(handle: isize) -> i32;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    let pin = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    assert_ne!(
        pin, 0,
        "expected to pin the kernel process object open — without this the OS \
         may have already reaped the PID and the test would be trivially passing"
    );

    assert!(
        !is_process_alive(pid),
        "PID {pid} terminated cleanly but is_process_alive returned true \
         (Windows process-object zombie — issue #774)"
    );

    unsafe { CloseHandle(pin) };
}
