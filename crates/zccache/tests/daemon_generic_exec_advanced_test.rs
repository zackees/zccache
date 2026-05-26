//! Advanced integration tests for `Request::GenericToolExec` (issue #272).
//!
//! Covers the bullets the basic suite (`daemon_generic_exec_test.rs`)
//! defers: Path A (include scan), Path B (depfile), non-determinism,
//! key-args filter, hybrid Path A+B, concurrent-caller coalescing, the
//! tool-binary-change/touch checklist items, missing-input diagnostics,
//! and the cache-restore (normalized-mtime) scenario.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tempfile::TempDir;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use zccache::core::NormalizedPath;
use zccache::daemon::DaemonServer;
use zccache::protocol::{ArtifactPayload, ExecCachePolicy, ExecOutputStreams, Request, Response};

#[cfg(unix)]
type ClientConn = zccache::ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache::ipc::IpcClientConnection;

// ─── Fixture binary discovery (mirrors basic test file) ───────────────────

fn target_bin_dir() -> PathBuf {
    let mut p = std::env::current_exe().expect("current_exe");
    p.pop();
    p.pop();
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

fn find_test_tool() -> Option<PathBuf> {
    let p = binary_path("exec_test_tool");
    if p.is_file() {
        Some(p)
    } else {
        eprintln!("exec_test_tool not found at {p:?} — skipping");
        None
    }
}

// ─── Daemon harness ──────────────────────────────────────────────────────

async fn start_daemon(cache_dir: &Path) -> (String, JoinHandle<()>, Arc<Notify>) {
    let endpoint = zccache::ipc::unique_test_endpoint();
    let cache_dir = NormalizedPath::from(cache_dir);
    let mut server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).expect("bind");
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.expect("daemon run");
    });
    (endpoint, handle, shutdown)
}

async fn connect_client(endpoint: &str) -> ClientConn {
    zccache::ipc::connect(endpoint).await.expect("connect")
}

// ─── Request builder ─────────────────────────────────────────────────────

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

#[allow(dead_code)]
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

struct Harness {
    _cache: TempDir,
    work: TempDir,
    _server: JoinHandle<()>,
    shutdown: Arc<Notify>,
    #[allow(dead_code)] // Kept alive for the daemon's lifetime; not read directly.
    endpoint: String,
    client: ClientConn,
    tool: PathBuf,
}

impl Harness {
    async fn new() -> Option<Self> {
        let tool = find_test_tool()?;
        let cache = tempfile::tempdir().ok()?;
        let work = tempfile::tempdir().ok()?;
        let (endpoint, server, shutdown) = start_daemon(cache.path()).await;
        let client = connect_client(&endpoint).await;
        Some(Self {
            _cache: cache,
            work,
            _server: server,
            shutdown,
            endpoint,
            client,
            tool,
        })
    }

    async fn shutdown(self) {
        use futures::future::FutureExt;
        let mut conn = self.client;
        let _ = conn.send(&Request::Shutdown).await;
        let _ = conn.recv::<Response>().await;
        self.shutdown.notify_one();
        // Yield to let the daemon's exit path finalize.
        tokio::task::yield_now().now_or_never();
    }
}

fn write_file(path: &Path, content: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(path, content).expect("write");
}

fn bump_mtime(path: &Path) {
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let now = std::time::SystemTime::now();
    let _ = filetime::set_file_mtime(path, filetime::FileTime::from_system_time(now));
}

// ─── Path A: include scan ────────────────────────────────────────────────

/// Path A: editing a transitively-included header invalidates the key.
#[tokio::test]
async fn path_a_edit_transitive_header_misses() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let inc = h.work.path().join("include");
    std::fs::create_dir_all(&inc).unwrap();
    let header = inc.join("util.h");
    write_file(&header, b"// util\n#pragma once\nint util();\n");

    let src = h.work.path().join("foo.cpp");
    write_file(&src, b"#include \"util.h\"\nint main(){return util();}\n");

    let make = || {
        let mut a = ExecArgs::new(
            h.tool.clone(),
            vec!["0".into(), "-".into(), "-".into(), "-".into()],
            h.work.path().to_path_buf(),
        );
        a.include_scan_files = vec![NormalizedPath::from(src.as_path())];
        a.include_dirs = vec![NormalizedPath::from(inc.as_path())];
        a
    };

    let first = send_exec(&mut h.client, make()).await;
    assert!(!first.cached);
    let warm = send_exec(&mut h.client, make()).await;
    assert!(warm.cached, "warm hit before edit");

    bump_mtime(&header);
    write_file(&header, b"// util v2\n#pragma once\nlong util();\n");

    let miss = send_exec(&mut h.client, make()).await;
    assert!(
        !miss.cached,
        "editing a scanned header must invalidate the key"
    );

    h.shutdown().await;
}

/// Path A: editing a *non*-included header in the same `-I` directory must
/// remain a hit (the scanner only follows actual `#include` chains).
#[tokio::test]
async fn path_a_unrelated_header_in_same_dir_still_hits() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let inc = h.work.path().join("include");
    std::fs::create_dir_all(&inc).unwrap();
    write_file(&inc.join("used.h"), b"#pragma once\nint a();\n");
    write_file(&inc.join("sibling.h"), b"#pragma once\nint b();\n");

    let src = h.work.path().join("foo.cpp");
    write_file(&src, b"#include \"used.h\"\nint main(){return a();}\n");

    let make = || {
        let mut a = ExecArgs::new(
            h.tool.clone(),
            vec!["0".into(), "-".into(), "-".into(), "-".into()],
            h.work.path().to_path_buf(),
        );
        a.include_scan_files = vec![NormalizedPath::from(src.as_path())];
        a.include_dirs = vec![NormalizedPath::from(inc.as_path())];
        a
    };

    let _first = send_exec(&mut h.client, make()).await;
    let warm = send_exec(&mut h.client, make()).await;
    assert!(warm.cached, "warm hit before unrelated edit");

    bump_mtime(&inc.join("sibling.h"));
    write_file(&inc.join("sibling.h"), b"#pragma once\nlong b();\n");

    let hit = send_exec(&mut h.client, make()).await;
    assert!(
        hit.cached,
        "editing a header that's *not* in the include chain must not invalidate"
    );

    h.shutdown().await;
}

/// Path A: adding a new `#include` that pulls in a new header must miss
/// AND the new header must enter the dep set for next time.
#[tokio::test]
async fn path_a_added_include_pulls_in_new_header() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let inc = h.work.path().join("include");
    std::fs::create_dir_all(&inc).unwrap();
    write_file(&inc.join("a.h"), b"#pragma once\n");
    write_file(&inc.join("b.h"), b"#pragma once\n");

    let src = h.work.path().join("foo.cpp");
    write_file(&src, b"#include \"a.h\"\n");

    let make = || {
        let mut a = ExecArgs::new(
            h.tool.clone(),
            vec!["0".into(), "-".into(), "-".into(), "-".into()],
            h.work.path().to_path_buf(),
        );
        a.include_scan_files = vec![NormalizedPath::from(src.as_path())];
        a.include_dirs = vec![NormalizedPath::from(inc.as_path())];
        a
    };

    let _first = send_exec(&mut h.client, make()).await;

    // Now add `#include "b.h"` to the source — the scanner will resolve b.h
    // for the first time, so the key shifts.
    bump_mtime(&src);
    write_file(&src, b"#include \"a.h\"\n#include \"b.h\"\n");

    let miss = send_exec(&mut h.client, make()).await;
    assert!(!miss.cached, "adding an #include must shift the key");

    // Subsequent calls with the new source must hit (dep set now contains b.h).
    let hit = send_exec(&mut h.client, make()).await;
    assert!(hit.cached, "second call with updated dep set must hit");

    h.shutdown().await;
}

// ─── Path B: depfile ─────────────────────────────────────────────────────

/// Path B: first invocation primes the dep set; second invocation reads the
/// stored `.deps` sidecar, hashes deps, and hits.
#[tokio::test]
async fn path_b_first_invocation_then_warm_hit() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let input = h.work.path().join("in.txt");
    write_file(&input, b"hello");
    let out = h.work.path().join("out.txt");
    let depfile = h.work.path().join("out.d");

    let make = || {
        let mut a = ExecArgs::new(
            h.tool.clone(),
            vec![
                "0".into(),
                input.to_string_lossy().into(),
                out.to_string_lossy().into(),
                "OK".into(),
                depfile.to_string_lossy().into(),
            ],
            h.work.path().to_path_buf(),
        );
        a.output_files = vec![NormalizedPath::from(out.as_path())];
        a.depfile = Some(NormalizedPath::from(depfile.as_path()));
        a
    };

    let first = send_exec(&mut h.client, make()).await;
    assert!(!first.cached, "first invocation cold");
    assert!(depfile.exists(), "tool emitted the depfile");

    let warm = send_exec(&mut h.client, make()).await;
    assert!(
        warm.cached,
        "warm invocation must load the .deps sidecar and hit"
    );

    h.shutdown().await;
}

/// Path B: editing a file listed in the depfile invalidates the full key.
#[tokio::test]
async fn path_b_edit_listed_dep_misses() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let extra = h.work.path().join("extra.dep");
    write_file(&extra, b"extra-v1");
    let input = h.work.path().join("in.txt");
    write_file(&input, b"hello");
    let out = h.work.path().join("out.txt");
    let depfile = h.work.path().join("out.d");

    let make = || {
        let mut a = ExecArgs::new(
            h.tool.clone(),
            vec![
                "0".into(),
                input.to_string_lossy().into(),
                out.to_string_lossy().into(),
                "OK".into(),
                depfile.to_string_lossy().into(),
                "-".into(), // tick_file (unused)
                extra.to_string_lossy().into(),
            ],
            h.work.path().to_path_buf(),
        );
        a.output_files = vec![NormalizedPath::from(out.as_path())];
        a.depfile = Some(NormalizedPath::from(depfile.as_path()));
        a
    };

    let _first = send_exec(&mut h.client, make()).await;
    let warm = send_exec(&mut h.client, make()).await;
    assert!(warm.cached, "warm hit before extra-dep edit");

    bump_mtime(&extra);
    write_file(&extra, b"extra-v2");

    let miss = send_exec(&mut h.client, make()).await;
    assert!(
        !miss.cached,
        "editing a depfile-listed dep must invalidate the full key"
    );

    h.shutdown().await;
}

/// Path B: tool exits non-zero before emitting a complete depfile → entry
/// not cached; next invocation is a clean first-run.
#[tokio::test]
async fn path_b_failed_run_does_not_cache() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let input = h.work.path().join("in.txt");
    write_file(&input, b"hello");
    let out = h.work.path().join("out.txt");
    let depfile = h.work.path().join("out.d");

    // exec_test_tool skips writing the depfile on non-zero exit, exactly
    // like a real tool that aborted before flushing it.
    let make_fail = || {
        let mut a = ExecArgs::new(
            h.tool.clone(),
            vec![
                "3".into(),
                input.to_string_lossy().into(),
                out.to_string_lossy().into(),
                "OK".into(),
                depfile.to_string_lossy().into(),
            ],
            h.work.path().to_path_buf(),
        );
        a.output_files = vec![NormalizedPath::from(out.as_path())];
        a.depfile = Some(NormalizedPath::from(depfile.as_path()));
        a
    };

    let r1 = send_exec(&mut h.client, make_fail()).await;
    assert_eq!(r1.exit_code, 3);
    assert!(!depfile.exists(), "tool aborted before writing depfile");

    let r2 = send_exec(&mut h.client, make_fail()).await;
    assert!(
        !r2.cached,
        "second failed run must not be served from cache (no .deps sidecar)"
    );

    h.shutdown().await;
}

/// Path B: depfile references a file that no longer exists → miss + rerun.
#[tokio::test]
async fn path_b_missing_dep_forces_miss() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let extra = h.work.path().join("vanishing.dep");
    write_file(&extra, b"present");
    let input = h.work.path().join("in.txt");
    write_file(&input, b"hello");
    let out = h.work.path().join("out.txt");
    let depfile = h.work.path().join("out.d");

    let make = || {
        let mut a = ExecArgs::new(
            h.tool.clone(),
            vec![
                "0".into(),
                input.to_string_lossy().into(),
                out.to_string_lossy().into(),
                "OK".into(),
                depfile.to_string_lossy().into(),
                "-".into(),
                extra.to_string_lossy().into(),
            ],
            h.work.path().to_path_buf(),
        );
        a.output_files = vec![NormalizedPath::from(out.as_path())];
        a.depfile = Some(NormalizedPath::from(depfile.as_path()));
        a
    };

    let _first = send_exec(&mut h.client, make()).await;
    let warm = send_exec(&mut h.client, make()).await;
    assert!(warm.cached, "warm hit");

    // Vanish the extra dep — next lookup must treat the entry as stale.
    std::fs::remove_file(&extra).unwrap();

    let miss = send_exec(&mut h.client, make()).await;
    assert!(
        !miss.cached,
        "depfile referencing a vanished file must force a miss"
    );

    h.shutdown().await;
}

/// Hybrid Path A + Path B: changing either invalidates the key.
#[tokio::test]
async fn hybrid_path_a_b_both_contribute_to_key() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let inc = h.work.path().join("include");
    std::fs::create_dir_all(&inc).unwrap();
    let header = inc.join("hdr.h");
    write_file(&header, b"#pragma once\nint hdr();\n");

    let src = h.work.path().join("foo.cpp");
    write_file(&src, b"#include \"hdr.h\"\n");

    let extra = h.work.path().join("extra.gen");
    write_file(&extra, b"gen-v1");
    let out = h.work.path().join("out.txt");
    let depfile = h.work.path().join("out.d");

    let make = || {
        let mut a = ExecArgs::new(
            h.tool.clone(),
            vec![
                "0".into(),
                src.to_string_lossy().into(),
                out.to_string_lossy().into(),
                "OK".into(),
                depfile.to_string_lossy().into(),
                "-".into(),
                extra.to_string_lossy().into(),
            ],
            h.work.path().to_path_buf(),
        );
        a.include_scan_files = vec![NormalizedPath::from(src.as_path())];
        a.include_dirs = vec![NormalizedPath::from(inc.as_path())];
        a.output_files = vec![NormalizedPath::from(out.as_path())];
        a.depfile = Some(NormalizedPath::from(depfile.as_path()));
        a
    };

    let _first = send_exec(&mut h.client, make()).await;
    let warm = send_exec(&mut h.client, make()).await;
    assert!(warm.cached, "warm hit on the hybrid path");

    // Editing the Path A header invalidates.
    bump_mtime(&header);
    write_file(&header, b"#pragma once\nlong hdr();\n");
    let after_a = send_exec(&mut h.client, make()).await;
    assert!(!after_a.cached, "Path A header edit must miss");

    // Now editing the Path B-only dep must also miss.
    let _ = send_exec(&mut h.client, make()).await;
    bump_mtime(&extra);
    write_file(&extra, b"gen-v2");
    let after_b = send_exec(&mut h.client, make()).await;
    assert!(!after_b.cached, "Path B dep edit must miss");

    h.shutdown().await;
}

// ─── Non-determinism + key-args filter ──────────────────────────────────

/// `non_deterministic=true` must never return `cached=true`, even on
/// repeated calls. Subsequent Normal-policy calls must MISS (no poisoning).
#[tokio::test]
async fn non_deterministic_never_caches() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let make = |nd: bool, policy: ExecCachePolicy| {
        let mut a = ExecArgs::new(
            h.tool.clone(),
            vec!["0".into(), "-".into(), "-".into(), "-".into()],
            h.work.path().to_path_buf(),
        );
        a.non_deterministic = nd;
        a.cache_policy = policy;
        a
    };

    let r1 = send_exec(&mut h.client, make(true, ExecCachePolicy::Normal)).await;
    let r2 = send_exec(&mut h.client, make(true, ExecCachePolicy::Normal)).await;
    assert!(!r1.cached);
    assert!(!r2.cached, "non-deterministic must never report cached");

    // Switching off the flag must hit a clean cache: first MISS (store
    // happens now), second HIT.
    let r3 = send_exec(&mut h.client, make(false, ExecCachePolicy::Normal)).await;
    let r4 = send_exec(&mut h.client, make(false, ExecCachePolicy::Normal)).await;
    assert!(
        !r3.cached,
        "non_deterministic runs above must not have poisoned the cache"
    );
    assert!(r4.cached);

    h.shutdown().await;
}

/// `key_args_filter` drops matching args from the cache key (but the tool
/// still receives them).
#[tokio::test]
async fn key_args_filter_excludes_matching_args() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let make = |trailing: Vec<&str>| {
        let mut args: Vec<String> = vec!["0".into(), "-".into(), "-".into(), "-".into()];
        for t in trailing {
            args.push(t.to_string());
        }
        let mut a = ExecArgs::new(h.tool.clone(), args, h.work.path().to_path_buf());
        a.key_args_filter = vec!["^--noise=".into()];
        a
    };

    let first = send_exec(&mut h.client, make(vec!["--noise=a"])).await;
    let second = send_exec(&mut h.client, make(vec!["--noise=b"])).await;
    assert!(!first.cached);
    assert!(
        second.cached,
        "filtered args must not shift the cache key — `--noise=a` and `--noise=b` share a key"
    );
    assert_eq!(first.key_hex, second.key_hex);

    h.shutdown().await;
}

// ─── Concurrent coalescing ──────────────────────────────────────────────

/// Issue #272: "Concurrent callers with identical inputs → daemon
/// coalesces, tool runs once, both callers get the result." Verified via a
/// side-effect counter file the tool appends to on every actual spawn.
#[tokio::test]
async fn concurrent_callers_coalesce_to_single_tool_spawn() {
    let Some(_) = find_test_tool() else {
        return;
    };
    let tool = find_test_tool().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (endpoint, server, shutdown) = start_daemon(cache.path()).await;

    let tick_file = work.path().join("ticks.txt");
    let _ = std::fs::remove_file(&tick_file);

    let make_req = || Request::GenericToolExec {
        tool: NormalizedPath::from(tool.as_path()),
        args: vec![
            "0".into(),
            "-".into(),
            "-".into(),
            "-".into(),
            "-".into(),
            tick_file.to_string_lossy().into(),
        ],
        cwd: NormalizedPath::from(work.path()),
        env: vec![],
        input_files: vec![],
        input_extra: Arc::new(Vec::new()),
        output_streams: ExecOutputStreams::default(),
        output_files: vec![],
        tool_hash: None,
        cache_policy: ExecCachePolicy::Normal,
        cwd_in_key: true,
        include_scan_files: vec![],
        include_dirs: vec![],
        system_include_dirs: vec![],
        iquote_dirs: vec![],
        depfile: None,
        non_deterministic: false,
        key_args_filter: vec![],
    };

    // Two independent client connections.
    let endpoint_a = endpoint.clone();
    let endpoint_b = endpoint.clone();
    let req_a = make_req();
    let req_b = make_req();

    let task_a = tokio::spawn(async move {
        let mut c = connect_client(&endpoint_a).await;
        c.send(&req_a).await.unwrap();
        c.recv::<Response>().await.unwrap()
    });
    let task_b = tokio::spawn(async move {
        let mut c = connect_client(&endpoint_b).await;
        c.send(&req_b).await.unwrap();
        c.recv::<Response>().await.unwrap()
    });

    let (a, b) = tokio::join!(task_a, task_b);
    let (a, b) = (a.unwrap(), b.unwrap());

    for r in [&a, &b] {
        match r {
            Some(Response::GenericToolExecResult { exit_code, .. }) => {
                assert_eq!(*exit_code, 0)
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    // The tool ticks on every actual spawn. With coalescing exactly one of
    // the two callers spawns; the other waits on the in-flight Notify and
    // gets the freshly stored result.
    let ticks = std::fs::read_to_string(&tick_file).unwrap_or_default();
    let count = ticks.lines().filter(|l| *l == "tick").count();
    assert_eq!(
        count, 1,
        "exactly one tool spawn must service two concurrent identical requests \
         (saw {count}: {ticks:?})"
    );

    drop(server);
    shutdown.notify_one();
}

// ─── Tool binary change/touch ───────────────────────────────────────────

/// "Tool binary content changed → cache miss (binary hash differs)."
/// Implemented via `--tool-hash` override so the test does not need to
/// physically swap an executable on disk.
#[tokio::test]
async fn tool_hash_override_change_misses() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let make = |hash_byte: u8| {
        let mut a = ExecArgs::new(
            h.tool.clone(),
            vec!["0".into(), "-".into(), "-".into(), "-".into()],
            h.work.path().to_path_buf(),
        );
        a.tool_hash = Some([hash_byte; 32]);
        a
    };

    let first = send_exec(&mut h.client, make(1)).await;
    let same = send_exec(&mut h.client, make(1)).await;
    assert!(!first.cached);
    assert!(same.cached, "same tool_hash override hits");

    let other = send_exec(&mut h.client, make(2)).await;
    assert!(
        !other.cached,
        "different tool_hash override must miss (binary identity differs)"
    );

    h.shutdown().await;
}

/// "Tool binary touched but content unchanged → cache hit." The compiler
/// hash cache's `(mtime, size)` fast path triggers a re-hash, but content
/// stays identical → same cache key → hit.
#[tokio::test]
async fn tool_binary_touch_keeps_cache() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    // Use a copy of the real tool so we can bump its mtime without
    // disturbing anyone else.
    let tool_copy = h.work.path().join("tool-copy.exe");
    std::fs::copy(&h.tool, &tool_copy).unwrap();

    let make = || {
        ExecArgs::new(
            tool_copy.clone(),
            vec!["0".into(), "-".into(), "-".into(), "-".into()],
            h.work.path().to_path_buf(),
        )
    };

    let _first = send_exec(&mut h.client, make()).await;
    bump_mtime(&tool_copy);
    let hit = send_exec(&mut h.client, make()).await;
    assert!(
        hit.cached,
        "mtime-only touch on the tool binary must remain a hit (content unchanged)"
    );

    h.shutdown().await;
}

// ─── Cache-restore / normalized mtimes ──────────────────────────────────

/// "Cache-restore scenario: tar-restore the cache root with normalized
/// mtimes → invocations content-hash inputs, find matches, hit." We
/// approximate the tar restore by stat-rewriting every input file's mtime
/// to a fixed value between two daemon runs.
#[tokio::test]
async fn cache_restore_with_normalized_mtimes_still_hits() {
    let Some(tool) = find_test_tool() else {
        return;
    };
    let cache = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();

    let input = work.path().join("in.txt");
    write_file(&input, b"hello");

    let make_req = || Request::GenericToolExec {
        tool: NormalizedPath::from(tool.as_path()),
        args: vec![
            "0".into(),
            input.to_string_lossy().into(),
            "-".into(),
            "-".into(),
        ],
        cwd: NormalizedPath::from(work.path()),
        env: vec![],
        input_files: vec![NormalizedPath::from(input.as_path())],
        input_extra: Arc::new(Vec::new()),
        output_streams: ExecOutputStreams::default(),
        output_files: vec![],
        tool_hash: None,
        cache_policy: ExecCachePolicy::Normal,
        cwd_in_key: true,
        include_scan_files: vec![],
        include_dirs: vec![],
        system_include_dirs: vec![],
        iquote_dirs: vec![],
        depfile: None,
        non_deterministic: false,
        key_args_filter: vec![],
    };

    {
        let (endpoint, _srv, shutdown) = start_daemon(cache.path()).await;
        let mut client = connect_client(&endpoint).await;
        client.send(&make_req()).await.unwrap();
        let _ = client.recv::<Response>().await.unwrap();
        let _ = client.send(&Request::Shutdown).await;
        let _ = client.recv::<Response>().await;
        shutdown.notify_one();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // Normalize the input's mtime to a fixed past value, mirroring what
    // `tar --mtime` would do during restore.
    let normalized = filetime::FileTime::from_unix_time(1_600_000_000, 0);
    let _ = filetime::set_file_mtime(&input, normalized);

    // Bring a fresh daemon up on the same cache root and verify the warm
    // hit still works.
    let (endpoint, _srv, shutdown) = start_daemon(cache.path()).await;
    let mut client = connect_client(&endpoint).await;
    client.send(&make_req()).await.unwrap();
    let resp = client.recv::<Response>().await.unwrap();
    match resp {
        Some(Response::GenericToolExecResult { cached, .. }) => {
            assert!(
                cached,
                "tar-restore with normalized mtimes must still produce a content-hash hit"
            );
        }
        other => panic!("unexpected: {other:?}"),
    }

    let _ = client.send(&Request::Shutdown).await;
    let _ = client.recv::<Response>().await;
    shutdown.notify_one();
}

// ─── Caller missing required input → diagnostic ─────────────────────────

/// "Caller missing required `--input-file` → tool still runs but with a
/// clear diagnostic that key may be over-broad." Implementation: when
/// `input_files` is empty AND no Path A scan is declared, the run goes
/// through but two distinct content patterns would share a key. Test:
/// two calls with the same args but the *tool's* declared file content
/// changes between them — second still reports cached because we didn't
/// declare it.
#[tokio::test]
async fn missing_input_declaration_is_over_broad_hit() {
    let Some(mut h) = Harness::new().await else {
        return;
    };

    let undeclared = h.work.path().join("undeclared.txt");
    write_file(&undeclared, b"v1");

    let make = || {
        ExecArgs::new(
            h.tool.clone(),
            vec![
                "0".into(),
                undeclared.to_string_lossy().into(),
                "-".into(),
                "-".into(),
            ],
            h.work.path().to_path_buf(),
        )
        // input_files intentionally left empty — caller forgot to declare it.
    };

    let _first = send_exec(&mut h.client, make()).await;

    bump_mtime(&undeclared);
    write_file(&undeclared, b"v2");

    let second = send_exec(&mut h.client, make()).await;
    assert!(
        second.cached,
        "undeclared input mutations are deliberately invisible to the cache; \
         document this as a caller-side over-broad-key foot-gun"
    );

    h.shutdown().await;
}
