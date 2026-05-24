//! Criterion benchmark for `zccache exec` (issue #272).
//!
//! Measures cache-overhead costs that are independent of the tool's own
//! work, using the deterministic `exec_test_tool` fixture as a "tiny shell
//! script that hashes its first argument". Three scenarios:
//!
//! - `exec_warm_hit`     — repeated identical request, expects sub-ms
//!   IPC roundtrip and zero tool spawns
//! - `exec_cold_miss`    — fresh input on every iteration, dominated by
//!   the tool's spawn cost (a few ms) plus daemon hashing
//! - `exec_one_input_changed` — same request shape, single input file
//!   rewritten between iterations, exercises the partial-invalidate path
//!
//! The daemon and IPC client are constructed once and reused; criterion
//! drives the inner request loop only.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use tempfile::TempDir;

use zccache::core::NormalizedPath;
use zccache::daemon::DaemonServer;
use zccache::protocol::{ExecCachePolicy, ExecOutputStreams, Request, Response};

#[cfg(unix)]
type ClientConn = zccache::ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache::ipc::IpcClientConnection;

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
        eprintln!("exec_test_tool not found at {p:?} — skipping exec benches");
        None
    }
}

struct BenchHarness {
    _cache: TempDir,
    work: TempDir,
    _server: tokio::task::JoinHandle<()>,
    rt: tokio::runtime::Runtime,
    client: ClientConn,
    tool: PathBuf,
}

impl BenchHarness {
    fn new() -> Option<Self> {
        let tool = find_test_tool()?;
        let cache = tempfile::tempdir().ok()?;
        let work = tempfile::tempdir().ok()?;
        std::env::set_var("ZCCACHE_CACHE_DIR", cache.path());
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .ok()?;

        let (server, client) = rt.block_on(async {
            let endpoint = zccache::ipc::unique_test_endpoint();
            let mut server = DaemonServer::bind(&endpoint).expect("bind");
            let _shutdown = server.shutdown_handle();
            let server_task = tokio::spawn(async move {
                server.run(0).await.expect("daemon run");
            });
            let client = zccache::ipc::connect(&endpoint).await.expect("connect");
            (server_task, client)
        });

        Some(Self {
            _cache: cache,
            work,
            _server: server,
            rt,
            client,
            tool,
        })
    }

    fn build_request(&self, args: Vec<String>, input_files: Vec<NormalizedPath>) -> Request {
        Request::GenericToolExec {
            tool: NormalizedPath::from(self.tool.as_path()),
            args,
            cwd: NormalizedPath::from(self.work.path()),
            env: vec![],
            input_files,
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
        }
    }

    fn run(&mut self, req: &Request) -> Response {
        let client = &mut self.client;
        self.rt.block_on(async move {
            client.send(req).await.unwrap();
            client.recv::<Response>().await.unwrap().unwrap()
        })
    }
}

fn write_input(path: &Path, content: &[u8]) {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).ok();
    }
    std::fs::write(path, content).unwrap();
}

fn bench_warm_hit(c: &mut Criterion) {
    let Some(mut h) = BenchHarness::new() else {
        return;
    };
    let input = h.work.path().join("warm_input.txt");
    write_input(&input, b"warm-hit-bytes");

    let req = h.build_request(
        vec![
            "0".into(),
            input.to_string_lossy().into(),
            "-".into(),
            "-".into(),
        ],
        vec![NormalizedPath::from(input.as_path())],
    );
    // Prime the cache so the bench measures the hit path only.
    let primed = h.run(&req);
    if let Response::GenericToolExecResult { cached, .. } = &primed {
        assert!(!cached, "first call primes the cache");
    }
    let warm = h.run(&req);
    if let Response::GenericToolExecResult { cached, .. } = &warm {
        assert!(cached, "second call must be a warm hit");
    }

    c.bench_function("exec_warm_hit", |b| {
        b.iter(|| {
            let r = h.run(&req);
            match r {
                Response::GenericToolExecResult { cached: true, .. } => {}
                other => panic!("expected warm hit, got {other:?}"),
            }
        });
    });
}

fn bench_cold_miss(c: &mut Criterion) {
    let Some(mut h) = BenchHarness::new() else {
        return;
    };
    let work_root: PathBuf = h.work.path().to_path_buf();
    let mut counter: u64 = 0;

    c.bench_function("exec_cold_miss", |b| {
        b.iter_batched(
            || {
                counter = counter.wrapping_add(1);
                let path = work_root.join(format!("cold_{counter}.txt"));
                write_input(&path, format!("cold-{counter}").as_bytes());
                path
            },
            |input| {
                let req = h.build_request(
                    vec![
                        "0".into(),
                        input.to_string_lossy().into(),
                        "-".into(),
                        "-".into(),
                    ],
                    vec![NormalizedPath::from(input.as_path())],
                );
                let r = h.run(&req);
                match r {
                    Response::GenericToolExecResult { cached: false, .. } => {}
                    other => panic!("expected cold miss, got {other:?}"),
                }
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_one_input_changed(c: &mut Criterion) {
    let Some(mut h) = BenchHarness::new() else {
        return;
    };

    let stable: PathBuf = h.work.path().join("stable.txt");
    write_input(&stable, b"stable-content-stays-cached");
    let churn: PathBuf = h.work.path().join("churn.txt");
    let churn_for_setup = churn.clone();
    let mut counter: u64 = 0;

    c.bench_function("exec_one_input_changed", |b| {
        b.iter_batched(
            || {
                counter = counter.wrapping_add(1);
                write_input(&churn_for_setup, format!("v{counter}").as_bytes());
            },
            |()| {
                let req = h.build_request(
                    vec![
                        "0".into(),
                        churn.to_string_lossy().into(),
                        "-".into(),
                        "-".into(),
                    ],
                    vec![
                        NormalizedPath::from(stable.as_path()),
                        NormalizedPath::from(churn.as_path()),
                    ],
                );
                // Measure the path-changed case; the daemon may serve a
                // hit from a prior iteration when criterion's adaptive
                // batching repeats the same content. The bench's role is
                // to bound the latency of the partial-invalidate route,
                // not to assert cache outcome.
                let _ = h.run(&req);
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(
    name = exec;
    config = Criterion::default()
        .sample_size(20)
        .warm_up_time(std::time::Duration::from_millis(500))
        .measurement_time(std::time::Duration::from_secs(3));
    targets = bench_warm_hit, bench_cold_miss, bench_one_input_changed
);
criterion_main!(exec);
