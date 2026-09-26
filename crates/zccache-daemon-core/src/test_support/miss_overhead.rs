//! Per-TU cache-miss overhead fixture (zccache#1670).
//!
//! Drives an in-process daemon through fresh C++ compiles that each miss the
//! cache, and reports the **miss overhead**: wall time from sending the
//! `Compile` request to receiving its `CompileResult`, minus the compiler
//! child's own run time (`PhaseProfiler::compiler_process_ns`, which also
//! spans compile admission; compiles here are serial, so admission never
//! waits). That remainder is what zccache adds to every cold compile:
//! argument parsing, the include scan or depfile parse, blake3 hashing, the
//! staged store, and enqueueing the index write.
//!
//! Shared by the `miss_overhead` criterion bench and the CI budget test so
//! both measure the same thing.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::core::NormalizedPath;
use crate::daemon::server::ProfileHandle;
use crate::daemon::DaemonServer;
use crate::protocol::{Request, Response};

#[cfg(unix)]
type ClientConn = crate::ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = crate::ipc::IpcClientConnection;

/// How the daemon learns a miss's dependencies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencySource {
    /// `-MD -MF -`: the depfile goes to stdout, so the daemon cannot use it
    /// and falls back to its recursive include scan.
    IncludeScan,
    /// `-MD -MF <file>`: the daemon parses the compiler-written depfile,
    /// the normal gcc/clang path since zccache#1668.
    Depfile,
}

/// Header-tree shape: `HEADER_COUNT` headers, each pulling in the next few,
/// sized like ArduinoCore/avr-libc headers (register `#define`s, comments,
/// declarations).
const HEADER_COUNT: usize = 96;
const FAN_OUT: usize = 3;
const DEFINES_PER_HEADER: usize = 40;

/// One measured batch of cache misses.
#[derive(Debug, Clone, Copy)]
pub struct MissOverheadSample {
    /// Compiles in the batch.
    pub compiles: u64,
    /// Client-side wall time of the whole batch.
    pub wall: Duration,
    /// Summed compiler child time the daemon recorded for the batch.
    pub compiler_process: Duration,
}

impl MissOverheadSample {
    /// Batch wall time minus compiler child time.
    #[must_use]
    pub fn overhead(&self) -> Duration {
        self.wall.saturating_sub(self.compiler_process)
    }

    /// Mean overhead per compile.
    #[must_use]
    pub fn overhead_per_compile(&self) -> Duration {
        self.overhead() / u32::try_from(self.compiles.max(1)).unwrap_or(u32::MAX)
    }
}

/// A running daemon plus a generated project whose compiles always miss.
pub struct MissOverheadFixture {
    work: tempfile::TempDir,
    _cache: tempfile::TempDir,
    server: tokio::task::JoinHandle<()>,
    shutdown: std::sync::Arc<kernal_api::async_engine::Notify>,
    profile: ProfileHandle,
    client: ClientConn,
    session_id: String,
    compiler: NormalizedPath,
    source: DependencySource,
    next_unit: u64,
}

impl MissOverheadFixture {
    /// Start a daemon on an isolated cache root and generate the project.
    ///
    /// Returns `None` when no `clang++` is available.
    pub async fn start(source: DependencySource) -> Option<Self> {
        let compiler = super::find_clang()?;
        let work = super::temp_cache_dir().expect("work tempdir");
        let cache = super::temp_cache_dir().expect("cache tempdir");
        write_header_tree(&work.path().join("include"));

        let endpoint = crate::ipc::unique_test_endpoint();
        let mut server = DaemonServer::bind_with_cache_dir(&endpoint, &cache.path().into())
            .expect("bind daemon");
        let profile = server.profile_handle();
        let shutdown = server.shutdown_handle();
        let server = tokio::spawn(async move {
            server.run(0).await.expect("daemon run");
        });
        let mut client = crate::ipc::connect(&endpoint).await.expect("connect");
        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: work.path().into(),
                log_file: None,
                track_stats: true,
                journal_path: None,
                profile: false,
                private_daemon: None,
            })
            .await
            .expect("send SessionStart");
        let session_id = match client.recv::<Response>().await.expect("recv") {
            Some(Response::SessionStarted { session_id, .. }) => session_id,
            other => panic!("expected SessionStarted, got {other:?}"),
        };

        Some(Self {
            work,
            _cache: cache,
            server,
            shutdown,
            profile,
            client,
            session_id,
            compiler,
            source,
            next_unit: 0,
        })
    }

    /// Compile `compiles` never-seen translation units back to back.
    ///
    /// Sources are written before the clock starts, so the sample holds
    /// only request handling and the compiler run.
    pub async fn measure(&mut self, compiles: u64) -> MissOverheadSample {
        let units: Vec<u64> = (0..compiles)
            .map(|_| {
                self.next_unit += 1;
                self.next_unit
            })
            .collect();
        for unit in &units {
            std::fs::write(
                self.work.path().join(format!("unit_{unit}.cpp")),
                format!("#include \"h_0.h\"\nint unit_{unit}(void) {{ return {unit}; }}\n"),
            )
            .expect("write source");
        }

        self.profile.reset();
        let scans_before = crate::depgraph::scanner::scan_includes_calls();
        let start = Instant::now();
        for unit in units {
            self.compile(unit).await;
        }
        let wall = start.elapsed();
        let profile = self.profile.snapshot();
        assert_eq!(profile.miss_count, compiles, "every compile must miss");
        if self.source == DependencySource::IncludeScan {
            // Each new unit's own source is parsed at least once.
            let scans = crate::depgraph::scanner::scan_includes_calls() - scans_before;
            assert!(
                scans >= compiles,
                "include-scan path ran {scans} scans for {compiles} compiles"
            );
        }
        MissOverheadSample {
            compiles,
            wall,
            compiler_process: Duration::from_nanos(
                profile
                    .avg_compiler_process_ns
                    .saturating_mul(profile.miss_count),
            ),
        }
    }

    async fn compile(&mut self, unit: u64) {
        let mut args = vec![
            "-c".to_string(),
            format!("unit_{unit}.cpp"),
            "-o".to_string(),
            format!("unit_{unit}.o"),
            "-Iinclude".to_string(),
            "-O0".to_string(),
        ];
        let depfile = match self.source {
            DependencySource::IncludeScan => "-".to_string(),
            DependencySource::Depfile => format!("unit_{unit}.d"),
        };
        args.extend(["-MD".into(), "-MF".into(), depfile]);
        self.client
            .send(&Request::Compile {
                session_id: self.session_id.clone(),
                args,
                cwd: self.work.path().into(),
                compiler: self.compiler.clone(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .expect("send Compile");
        // CompileProgress heartbeats are non-terminal frames (#1337).
        loop {
            match self.client.recv::<Response>().await.expect("recv") {
                Some(Response::CompileProgress { .. }) => {}
                Some(Response::CompileResult {
                    exit_code,
                    cached,
                    stderr,
                    ..
                }) => {
                    assert_eq!(
                        exit_code,
                        0,
                        "compile failed: {}",
                        String::from_utf8_lossy(&stderr)
                    );
                    assert!(!cached, "unit {unit} unexpectedly hit the cache");
                    return;
                }
                other => panic!("expected CompileResult, got {other:?}"),
            }
        }
    }

    /// Stop the daemon and wait for it to exit.
    pub async fn shutdown(self) {
        self.shutdown.notify_one();
        let _ = self.server.await;
    }
}

/// Write `h_0.h` .. `h_{HEADER_COUNT-1}.h`; `h_0.h` reaches all of them.
fn write_header_tree(dir: &Path) {
    std::fs::create_dir_all(dir).expect("create include dir");
    for id in 0..HEADER_COUNT {
        let mut body = format!(
            "/* Header {id}.\n * Generated miss-overhead fixture.\n */\n#ifndef H_{id}\n#define H_{id}\n"
        );
        for child in (1..=FAN_OUT).map(|k| id * FAN_OUT + k) {
            if child < HEADER_COUNT {
                body.push_str(&format!("#include \"h_{child}.h\"\n"));
            }
        }
        for reg in 0..DEFINES_PER_HEADER {
            body.push_str(&format!(
                "#define REG_{id}_{reg} (*(volatile unsigned char *)0x{:04X}) /* bit {reg} */\n",
                id * DEFINES_PER_HEADER + reg
            ));
        }
        body.push_str(&format!(
            "// Accessor for block {id}.\nunsigned char read_block_{id}(unsigned char offset);\n#endif\n"
        ));
        std::fs::write(dir.join(format!("h_{id}.h")), body).expect("write header");
    }
}
