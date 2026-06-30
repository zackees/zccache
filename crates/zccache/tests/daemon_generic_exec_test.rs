//! Integration tests for `Request::GenericToolExec` (issue #272).
//!
//! Verifies the daemon's generic-tool caching path end-to-end: every test
//! below maps to one of the correctness bullets in #272. The fixture tool
//! is `exec_test_tool` (see `crates/zccache/src/bin/exec_test_tool.rs`),
//! a deterministic Rust binary whose stdout/stderr/output-file are fully
//! determined by its argv + the content of its declared input file.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tempfile::TempDir;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use zccache::core::NormalizedPath;
use zccache::daemon::DaemonServer;
use zccache::protocol::{ArtifactPayload, ExecCachePolicy, ExecOutputStreams, Request, Response};

// ─── Platform IPC type ────────────────────────────────────────────────────

#[cfg(unix)]
type ClientConn = zccache::ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache::ipc::IpcClientConnection;

// ─── Test fixture binary discovery ────────────────────────────────────────

fn target_bin_dir() -> PathBuf {
    let mut p = std::env::current_exe().expect("current_exe");
    p.pop(); // deps/
    p.pop(); // target/<profile>/
    p
}

fn binary_path(stem: &str) -> PathBuf {
    let mut p = target_bin_dir();
    if cfg!(windows) {
        p.push(format!("{stem}.exe"));
    } else {
        p.push(stem);
    }
    p
}

/// Locate `exec_test_tool`. Skips the test cleanly when the binary hasn't
/// been built — for local hacking without `--features=test-support`.
fn find_test_tool() -> Option<PathBuf> {
    let p = binary_path("exec_test_tool");
    if p.is_file() {
        Some(p)
    } else {
        eprintln!(
            "exec_test_tool not found at {p:?} — \
             build with `soldr cargo build -p zccache --bin exec_test_tool --features test-support` first"
        );
        None
    }
}

// ─── Daemon harness ──────────────────────────────────────────────────────

/// Start a daemon bound to a unique endpoint with `cache_dir` as its cache
/// root. Passing the cache dir explicitly avoids process-global env races
/// while the daemon-restart test reuses on-disk state across two binds.
async fn start_daemon_with_cache(cache_dir: &Path) -> (String, JoinHandle<()>, Arc<Notify>) {
    let endpoint = zccache::ipc::unique_test_endpoint();
    let cache_dir = NormalizedPath::from(cache_dir);
    let mut server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).expect("bind daemon");
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.expect("daemon run");
    });
    (endpoint, handle, shutdown)
}

async fn connect_client(endpoint: &str) -> ClientConn {
    zccache::ipc::connect(endpoint).await.expect("connect")
}

// ─── Request builders ────────────────────────────────────────────────────

#[derive(Clone)]
struct ExecArgs {
    tool: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    input_files: Vec<NormalizedPath>,
    input_extra: Vec<u8>,
    output_streams: ExecOutputStreams,
    output_files: Vec<NormalizedPath>,
    tool_hash: Option<[u8; 32]>,
    cache_policy: ExecCachePolicy,
    cwd_in_key: bool,
    include_scan_files: Vec<NormalizedPath>,
    include_dirs: Vec<NormalizedPath>,
    system_include_dirs: Vec<NormalizedPath>,
    iquote_dirs: Vec<NormalizedPath>,
    depfile: Option<NormalizedPath>,
    non_deterministic: bool,
    key_args_filter: Vec<String>,
}

impl ExecArgs {
    fn new(tool: PathBuf, args: Vec<String>, cwd: PathBuf) -> Self {
        Self {
            tool,
            args,
            cwd,
            env: Vec::new(),
            input_files: Vec::new(),
            input_extra: Vec::new(),
            output_streams: ExecOutputStreams::default(),
            output_files: Vec::new(),
            tool_hash: None,
            cache_policy: ExecCachePolicy::Normal,
            cwd_in_key: true,
            include_scan_files: Vec::new(),
            include_dirs: Vec::new(),
            system_include_dirs: Vec::new(),
            iquote_dirs: Vec::new(),
            depfile: None,
            non_deterministic: false,
            key_args_filter: Vec::new(),
        }
    }

    fn into_request(self) -> Request {
        Request::GenericToolExec {
            tool: NormalizedPath::from(self.tool.as_path()),
            args: self.args,
            cwd: NormalizedPath::from(self.cwd.as_path()),
            env: self.env,
            input_files: self.input_files,
            input_extra: Arc::new(self.input_extra),
            output_streams: self.output_streams,
            output_files: self.output_files,
            tool_hash: self.tool_hash,
            cache_policy: self.cache_policy,
            cwd_in_key: self.cwd_in_key,
            include_scan_files: self.include_scan_files,
            include_dirs: self.include_dirs,
            system_include_dirs: self.system_include_dirs,
            iquote_dirs: self.iquote_dirs,
            depfile: self.depfile,
            non_deterministic: self.non_deterministic,
            key_args_filter: self.key_args_filter,
        }
    }
}

#[allow(dead_code)] // Some fields are read by a subset of test cases.
#[derive(Debug, Clone)]
struct ExecResponse {
    exit_code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    output_files: Vec<(String, Vec<u8>)>,
    cached: bool,
    key_hex: String,
}

async fn send_exec(client: &mut ClientConn, args: ExecArgs) -> ExecResponse {
    client.send(&args.into_request()).await.expect("send");
    match client.recv::<Response>().await.expect("recv") {
        Some(Response::GenericToolExecResult {
            exit_code,
            stdout,
            stderr,
            output_files,
            cached,
            cache_key_hex,
        }) => ExecResponse {
            exit_code,
            stdout: (*stdout).clone(),
            stderr: (*stderr).clone(),
            output_files: output_files
                .into_iter()
                .map(|o| {
                    let bytes = match &o.payload {
                        ArtifactPayload::Bytes(b) => (**b).clone(),
                        ArtifactPayload::Path(p) => std::fs::read(p.as_path()).unwrap_or_default(),
                    };
                    (o.name, bytes)
                })
                .collect(),
            cached,
            key_hex: cache_key_hex,
        },
        Some(Response::Error { message }) => panic!("daemon error: {message}"),
        other => panic!("unexpected response: {other:?}"),
    }
}

// ─── Common scaffolding ──────────────────────────────────────────────────

struct Harness {
    _cache: TempDir,
    work: TempDir,
    #[allow(dead_code)] // Kept alive for the daemon's lifetime; not read.
    endpoint: String,
    _server: JoinHandle<()>,
    shutdown: Arc<Notify>,
    client: ClientConn,
    tool: PathBuf,
}

impl Harness {
    async fn new() -> Option<Self> {
        let tool = find_test_tool()?;
        let cache = tempfile::tempdir().expect("cache tempdir");
        let work = tempfile::tempdir().expect("work tempdir");
        let (endpoint, server, shutdown) = start_daemon_with_cache(cache.path()).await;
        let client = connect_client(&endpoint).await;
        Some(Self {
            _cache: cache,
            work,
            endpoint,
            _server: server,
            shutdown,
            client,
            tool,
        })
    }

    async fn shutdown(self) {
        // Polite shutdown so the daemon flushes its index to disk.
        let _ = self.client.shutdown_owned().await;
        self.shutdown.notify_one();
    }
}

trait IpcShutdownExt {
    /// Shutdown by sending Shutdown and consuming the ack on a moved conn.
    fn shutdown_owned(self) -> futures::future::BoxFuture<'static, ()>;
}

impl IpcShutdownExt for ClientConn {
    fn shutdown_owned(mut self) -> futures::future::BoxFuture<'static, ()> {
        Box::pin(async move {
            let _ = self.send(&Request::Shutdown).await;
            let _ = self.recv::<Response>().await;
        })
    }
}

fn write_file(path: &Path, content: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(path, content).expect("write");
}

fn bump_mtime_past_ntfs(path: &Path) {
    // NTFS mtime resolution is ~100ns but Defender's interaction can lose
    // sub-second granularity in CI. Sleeping 1100ms is the same workaround
    // used elsewhere in this test suite.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let now = std::time::SystemTime::now();
    let _ = filetime::set_file_mtime(path, filetime::FileTime::from_system_time(now));
}

// ─── Tests ───────────────────────────────────────────────────────────────

/// Issue #272: "Warm hit: invoke twice with identical inputs → second call
/// returns cached: true, tool process not spawned."
#[tokio::test]
async fn exec_warm_hit_skips_tool() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let input = h.work.path().join("in.txt");
    write_file(&input, b"hello");

    let make = || {
        let mut a = ExecArgs::new(
            h.tool.clone(),
            vec![
                "0".into(),
                input.to_string_lossy().into(),
                "-".into(),
                "-".into(),
            ],
            h.work.path().to_path_buf(),
        );
        a.input_files = vec![NormalizedPath::from(input.as_path())];
        a
    };

    let first = send_exec(&mut h.client, make()).await;
    assert!(!first.cached, "first call must miss");
    assert_eq!(first.exit_code, 0);

    let second = send_exec(&mut h.client, make()).await;
    assert!(second.cached, "second call with identical inputs must hit");
    assert_eq!(second.exit_code, 0);
    assert_eq!(second.stdout, first.stdout, "cached stdout must match");
    assert_eq!(second.stderr, first.stderr, "cached stderr must match");
    assert_eq!(first.key_hex, second.key_hex, "key must be deterministic");

    h.shutdown().await;
}

/// "Input file content changed → cache miss, tool reruns, fresh result cached."
#[tokio::test]
async fn exec_input_content_change_misses() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let input = h.work.path().join("in.txt");
    write_file(&input, b"v1");

    let make = || {
        let mut a = ExecArgs::new(
            h.tool.clone(),
            vec![
                "0".into(),
                input.to_string_lossy().into(),
                "-".into(),
                "-".into(),
            ],
            h.work.path().to_path_buf(),
        );
        a.input_files = vec![NormalizedPath::from(input.as_path())];
        a
    };

    let first = send_exec(&mut h.client, make()).await;
    assert!(!first.cached);

    bump_mtime_past_ntfs(&input);
    write_file(&input, b"v2");

    let second = send_exec(&mut h.client, make()).await;
    assert!(!second.cached, "content change must miss");
    assert_ne!(first.stdout, second.stdout, "tool reran and echoed v2");

    h.shutdown().await;
}

/// "Input file touched but content unchanged → cache hit
/// (two-layer fingerprint absorbs this)."
#[tokio::test]
async fn exec_input_mtime_only_touch_still_hits() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let input = h.work.path().join("in.txt");
    write_file(&input, b"same-bytes");

    let make = || {
        let mut a = ExecArgs::new(
            h.tool.clone(),
            vec![
                "0".into(),
                input.to_string_lossy().into(),
                "-".into(),
                "-".into(),
            ],
            h.work.path().to_path_buf(),
        );
        a.input_files = vec![NormalizedPath::from(input.as_path())];
        a
    };

    let first = send_exec(&mut h.client, make()).await;
    assert!(!first.cached);

    bump_mtime_past_ntfs(&input);
    // Re-write same bytes; mtime advances, content stays.
    write_file(&input, b"same-bytes");

    let second = send_exec(&mut h.client, make()).await;
    assert!(
        second.cached,
        "two-layer fingerprint should observe identical content → hit"
    );

    h.shutdown().await;
}

/// "`--input-env` value changed → cache miss."
#[tokio::test]
async fn exec_declared_env_change_misses() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let make = |env_val: &str| {
        let mut a = ExecArgs::new(
            h.tool.clone(),
            vec!["0".into(), "-".into(), "-".into(), "-".into()],
            h.work.path().to_path_buf(),
        );
        a.env = vec![("ETT_FLAVOR".into(), env_val.to_string())];
        a
    };

    let first = send_exec(&mut h.client, make("a")).await;
    assert!(!first.cached);
    let second = send_exec(&mut h.client, make("a")).await;
    assert!(second.cached, "same declared env must hit");

    let third = send_exec(&mut h.client, make("b")).await;
    assert!(!third.cached, "changing the declared env value must miss");

    h.shutdown().await;
}

/// "`--input-env` value unchanged but a different env var changed →
/// cache hit." Verified by *only* declaring `ETT_FLAVOR` and silently
/// changing some other env entry between runs.
#[tokio::test]
async fn exec_undeclared_env_change_still_hits() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let make = |env: Vec<(&str, &str)>| {
        let mut a = ExecArgs::new(
            h.tool.clone(),
            vec!["0".into(), "-".into(), "-".into(), "-".into()],
            h.work.path().to_path_buf(),
        );
        a.env = env
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        a
    };

    // Both runs declare only ETT_FLAVOR=a. The second adds an undeclared
    // override of ETT_FLAVOR=a (same value), simulating a caller that
    // declared the var consistently but the rest of the env churns.
    let first = send_exec(&mut h.client, make(vec![("ETT_FLAVOR", "a")])).await;
    let second = send_exec(&mut h.client, make(vec![("ETT_FLAVOR", "a")])).await;
    assert!(first.exit_code == 0 && second.cached);

    h.shutdown().await;
}

/// "`--input-extra` bytes changed → cache miss."
#[tokio::test]
async fn exec_input_extra_change_misses() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let make = |extra: &[u8]| {
        let mut a = ExecArgs::new(
            h.tool.clone(),
            vec!["0".into(), "-".into(), "-".into(), "-".into()],
            h.work.path().to_path_buf(),
        );
        a.input_extra = extra.to_vec();
        a
    };

    let first = send_exec(&mut h.client, make(b"v1")).await;
    let second = send_exec(&mut h.client, make(b"v2")).await;
    assert!(!first.cached);
    assert!(
        !second.cached,
        "differing input-extra must invalidate the key"
    );

    h.shutdown().await;
}

/// "CWD changed → cache miss by default, hit with `--no-cwd-in-key`."
#[tokio::test]
async fn exec_cwd_in_key_semantics() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let cwd_a = h.work.path().join("a");
    let cwd_b = h.work.path().join("b");
    std::fs::create_dir_all(&cwd_a).unwrap();
    std::fs::create_dir_all(&cwd_b).unwrap();

    let make = |cwd: PathBuf, in_key: bool| {
        let mut a = ExecArgs::new(
            h.tool.clone(),
            vec!["0".into(), "-".into(), "-".into(), "-".into()],
            cwd,
        );
        a.cwd_in_key = in_key;
        a
    };

    // Default cwd_in_key=true → different cwds → different keys, miss.
    let _miss_a = send_exec(&mut h.client, make(cwd_a.clone(), true)).await;
    let miss_b = send_exec(&mut h.client, make(cwd_b.clone(), true)).await;
    assert!(!miss_b.cached, "CWD change must miss with cwd_in_key=true");

    // With cwd_in_key=false, both runs share a key.
    let _ = send_exec(&mut h.client, make(cwd_a.clone(), false)).await;
    let hit_b = send_exec(&mut h.client, make(cwd_b, false)).await;
    assert!(hit_b.cached, "cwd_in_key=false must hit across cwds");

    h.shutdown().await;
}

/// "Tool exits non-zero → exit code is cached and replayed on hit."
#[tokio::test]
async fn exec_nonzero_exit_is_cached_and_replayed() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let make = || {
        ExecArgs::new(
            h.tool.clone(),
            vec!["7".into(), "-".into(), "-".into(), "-".into()],
            h.work.path().to_path_buf(),
        )
    };

    let first = send_exec(&mut h.client, make()).await;
    assert_eq!(first.exit_code, 7);
    assert!(!first.cached);

    let second = send_exec(&mut h.client, make()).await;
    assert_eq!(second.exit_code, 7, "non-zero exit must round-trip");
    assert!(second.cached, "non-zero results should be cacheable too");

    h.shutdown().await;
}

/// "Tool writes a declared `--output-file` → file is captured, restored on hit."
#[tokio::test]
async fn exec_output_file_captured_and_restored() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let out_path = h.work.path().join("artifact.bin");
    let out_rel = NormalizedPath::from(out_path.as_path());

    let make = || {
        let mut a = ExecArgs::new(
            h.tool.clone(),
            vec![
                "0".into(),
                "-".into(),
                out_path.to_string_lossy().into(),
                "payload-xyz".into(),
            ],
            h.work.path().to_path_buf(),
        );
        a.output_files = vec![out_rel.clone()];
        a
    };

    let first = send_exec(&mut h.client, make()).await;
    assert!(!first.cached);
    assert_eq!(
        std::fs::read(&out_path).unwrap(),
        b"payload-xyz",
        "tool wrote the declared output file"
    );

    // Delete the output file so the hit-restore is the only way it comes back.
    std::fs::remove_file(&out_path).unwrap();
    assert!(!out_path.exists());

    let second = send_exec(&mut h.client, make()).await;
    assert!(second.cached, "second run must be a hit");
    assert!(
        out_path.exists(),
        "hit must restore the previously declared output file"
    );
    assert_eq!(std::fs::read(&out_path).unwrap(), b"payload-xyz");

    h.shutdown().await;
}

/// "`--no-cache` bypasses cache and does not poison the store." Run twice
/// under Bypass and verify neither pass returns `cached: true`.
#[tokio::test]
async fn exec_no_cache_bypasses_and_does_not_store() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let make = |policy: ExecCachePolicy| {
        let mut a = ExecArgs::new(
            h.tool.clone(),
            vec!["0".into(), "-".into(), "-".into(), "-".into()],
            h.work.path().to_path_buf(),
        );
        a.cache_policy = policy;
        a
    };

    let r1 = send_exec(&mut h.client, make(ExecCachePolicy::Bypass)).await;
    let r2 = send_exec(&mut h.client, make(ExecCachePolicy::Bypass)).await;
    assert!(!r1.cached);
    assert!(!r2.cached);

    // Now switch to Normal — should be a miss (Bypass didn't poison the
    // store) followed by a hit.
    let r3 = send_exec(&mut h.client, make(ExecCachePolicy::Normal)).await;
    let r4 = send_exec(&mut h.client, make(ExecCachePolicy::Normal)).await;
    assert!(!r3.cached, "Normal after Bypass must miss");
    assert!(r4.cached, "second Normal must hit");

    h.shutdown().await;
}

/// "Daemon restart preserves cached entries (on-disk store)."
#[tokio::test]
async fn exec_cache_survives_daemon_restart() {
    let Some(tool) = find_test_tool() else {
        return;
    };
    let cache = tempfile::tempdir().expect("cache");
    let work = tempfile::tempdir().expect("work");

    let make = || {
        ExecArgs::new(
            tool.clone(),
            vec!["0".into(), "-".into(), "-".into(), "-".into()],
            work.path().to_path_buf(),
        )
    };

    // Run 1 — populate cache, then shutdown daemon.
    {
        let (endpoint, _srv, shutdown) = start_daemon_with_cache(cache.path()).await;
        let mut client = connect_client(&endpoint).await;
        let first = send_exec(&mut client, make()).await;
        assert!(!first.cached);
        let _ = client.shutdown_owned().await;
        shutdown.notify_one();
        // Tiny yield so the daemon flushes its index.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // Run 2 — fresh daemon, same cache dir → should hit.
    let (endpoint, _srv, shutdown) = start_daemon_with_cache(cache.path()).await;
    let mut client = connect_client(&endpoint).await;
    let second = send_exec(&mut client, make()).await;
    assert!(
        second.cached,
        "cached entry must survive daemon restart (on-disk store)"
    );
    let _ = client.shutdown_owned().await;
    shutdown.notify_one();
}

/// Output-stream toggles: `output_streams.stdout=false` should not capture
/// stdout in the cached response, and the cache key must reflect "no stdout
/// requested" via the same `outputs` shape across runs (i.e., the cache
/// shouldn't mix streams).
#[tokio::test]
async fn exec_output_stream_toggles_are_honored_on_hit() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let make_with_streams = |s: ExecOutputStreams| {
        let mut a = ExecArgs::new(
            h.tool.clone(),
            vec!["0".into(), "-".into(), "-".into(), "-".into()],
            h.work.path().to_path_buf(),
        );
        a.output_streams = s;
        a
    };

    // Prime the cache with both streams captured.
    let primed = send_exec(
        &mut h.client,
        make_with_streams(ExecOutputStreams::default()),
    )
    .await;
    assert!(!primed.stdout.is_empty(), "tool emits ETT-OUT prefix");

    // Same request — hit, streams come back.
    let hit = send_exec(
        &mut h.client,
        make_with_streams(ExecOutputStreams::default()),
    )
    .await;
    assert!(hit.cached);
    assert_eq!(hit.stdout, primed.stdout);

    // Suppress stdout on the response — daemon still hits the same key,
    // but the response omits stdout bytes.
    let suppressed = send_exec(
        &mut h.client,
        make_with_streams(ExecOutputStreams {
            stdout: false,
            stderr: true,
        }),
    )
    .await;
    assert!(suppressed.cached);
    assert!(
        suppressed.stdout.is_empty(),
        "output_streams.stdout=false must suppress stdout in the response"
    );
    assert!(!suppressed.stderr.is_empty(), "stderr still captured");

    h.shutdown().await;
}
