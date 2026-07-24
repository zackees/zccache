//! Aggregate cache-layout validation for issue #1152.
//!
//! This is intentionally ignored: it starts real daemon instances and uses
//! the host clang and ar. `./test --integration` runs ignored integration
//! coverage in the Linux validation image.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::unwrap_in_result
)]

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::JoinHandle;
use zccache_daemon_core::artifact::{
    resolve_artifact_payloads, ResolvedArtifactPayload, LEGACY_PATH_VALIDATE_ENV,
};
use zccache_daemon_core::audit::{audit_cache_root, AuditOptions, LogAuditContext};
use zccache_daemon_core::core::NormalizedPath;
use zccache_daemon_core::daemon::DaemonServer;
use zccache_daemon_core::ipc::IpcConnection;
use zccache_daemon_core::protocol::{
    ArtifactData, ArtifactOutput, ArtifactPayload, ExecCachePolicy, ExecOutputStreams, Request,
    Response,
};

const STAGED_ENV: &str = "ZCCACHE_STAGED_ARTIFACTS";
const PACK_ENV: &str = "ZCCACHE_PACK_ARTIFACTS";
const CACHE_ENV: &str = "ZCCACHE_CACHE_DIR";

struct EnvGuard {
    values: Vec<(&'static str, Option<OsString>)>,
}

impl EnvGuard {
    fn capture(names: &[&'static str]) -> Self {
        Self {
            values: names
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect(),
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.values {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

struct Daemon {
    client: IpcConnection,
    task: JoinHandle<()>,
    shutdown: Arc<Notify>,
}

impl Daemon {
    async fn start(cache_root: &Path) -> Self {
        let endpoint = zccache_daemon_core::ipc::unique_test_endpoint();
        let cache_root = NormalizedPath::new(cache_root);
        let mut server =
            DaemonServer::bind_with_cache_dir(&endpoint, &cache_root).expect("bind daemon");
        // Mirror the production daemon's startup path (`daemon::entry`):
        // restore the persisted depgraph snapshot from disk *before*
        // `run()` starts accepting compile requests. Without this, the
        // in-memory depgraph starts empty on every `Daemon::start` call
        // (including the post-restart "warm" daemon), so every C/C++
        // compile after restart evaluates to `CacheVerdict::Cold` and the
        // warm-hit assertions below deterministically fail — the depgraph
        // check is the only cross-restart hit path (`try_fast_hit` /
        // `try_request_cache_hit` are in-memory-only). See
        // `crates/zccache/tests/daemon_rustc_restore_test.rs` for the same
        // pattern in another harness.
        let depgraph_path = zccache_daemon_core::depgraph::depgraph_file_path();
        let depgraph_load = zccache_daemon_core::depgraph::classify_load(&depgraph_path);
        if let zccache_daemon_core::depgraph::DepGraphLoadOutcome::Loaded { graph } = depgraph_load
        {
            server.set_dep_graph(graph);
        }
        let shutdown = server.shutdown_handle();
        let task = tokio::spawn(async move {
            server.run(0).await.expect("run daemon");
        });
        let client = zccache_daemon_core::ipc::connect(&endpoint)
            .await
            .expect("connect daemon");
        Self {
            client,
            task,
            shutdown,
        }
    }

    async fn request(&mut self, request: &Request) -> Response {
        self.client.send(request).await.expect("send request");
        self.client
            .recv()
            .await
            .expect("receive response")
            .expect("daemon response")
    }

    async fn stop(mut self) {
        let response = self.request(&Request::Shutdown).await;
        assert!(matches!(response, Response::ShuttingDown));
        self.shutdown.notify_one();
        tokio::time::timeout(Duration::from_secs(10), self.task)
            .await
            .expect("daemon shutdown timeout")
            .expect("daemon task");
    }
}

struct LegacyValidation {
    cache_root: PathBuf,
    finished: bool,
}

impl LegacyValidation {
    fn new(cache_root: &Path) -> Self {
        Self {
            cache_root: cache_root.to_path_buf(),
            finished: false,
        }
    }

    fn finish(mut self) {
        self.finished = true;
        let violations = legacy_violations(&self.cache_root);
        assert!(
            violations.is_empty(),
            "strict legacy-path validation found {} violation(s):\n{}",
            violations.len(),
            violations.join("\n")
        );
    }
}

impl Drop for LegacyValidation {
    fn drop(&mut self) {
        if self.finished || std::thread::panicking() {
            return;
        }
        let violations = legacy_violations(&self.cache_root);
        if !violations.is_empty() {
            panic!(
                "strict legacy-path validation found {} violation(s):\n{}",
                violations.len(),
                violations.join("\n")
            );
        }
    }
}

fn legacy_violations(cache_root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    collect_files(cache_root, &mut files);
    let mut violations = Vec::new();
    for file in files {
        let Ok(contents) = std::fs::read_to_string(&file) else {
            continue;
        };
        for (offset, line) in contents.lines().enumerate() {
            let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if row.get("event").and_then(|value| value.as_str())
                != Some("legacy_artifact_path_accessed")
                || row.get("purpose").and_then(|value| value.as_str()) == Some("migration")
            {
                continue;
            }
            violations.push(format!(
                "{}:{} path={} call_site={} purpose={}",
                file.display(),
                offset + 1,
                row.get("path")
                    .and_then(|value| value.as_str())
                    .unwrap_or("<missing>"),
                row.get("call_site")
                    .and_then(|value| value.as_str())
                    .unwrap_or("<missing>"),
                row.get("purpose")
                    .and_then(|value| value.as_str())
                    .unwrap_or("<missing>")
            ));
        }
    }
    violations
}

fn collect_files(root: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            collect_files(&entry.path(), files);
        } else if kind.is_file() {
            files.push(entry.path());
        }
    }
}

fn find_tool(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

fn write_executable(path: &Path, contents: &str) {
    std::fs::write(path, contents).expect("write executable fixture");
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

fn write_counting_clang(root: &Path, clang: &Path, count_file: &Path) -> PathBuf {
    let wrapper = root.join("clang");
    write_executable(
        &wrapper,
        &format!(
            "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = \"-c\" ]; then\n    printf 'compile: %s\\n' \"$*\" >> {}\n    break\n  fi\ndone\nexec {} \"$@\"\n",
            shell_quote(count_file),
            shell_quote(clang)
        ),
    );
    wrapper
}

fn write_counting_exec(root: &Path, count_file: &Path) -> PathBuf {
    let tool = root.join("layout-exec");
    write_executable(
        &tool,
        &format!(
            "#!/bin/sh\nprintf 'exec\\n' >> {}\nprintf '%s' \"$2\" > \"$1\"\nprintf 'exec-stdout\\n'\n",
            shell_quote(count_file)
        ),
    );
    tool
}

fn line_count(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|contents| contents.lines().count())
        .unwrap_or_default()
}

fn build_pack(payloads: &[&[u8]]) -> Vec<u8> {
    let header_size = 8 + payloads.len() * 16;
    let mut bytes =
        Vec::with_capacity(header_size + payloads.iter().map(|p| p.len()).sum::<usize>());
    bytes.extend_from_slice(b"ZCPK");
    bytes.extend_from_slice(&(payloads.len() as u32).to_le_bytes());
    let mut offset = header_size as u64;
    for payload in payloads {
        bytes.extend_from_slice(&offset.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        offset += payload.len() as u64;
    }
    for payload in payloads {
        bytes.extend_from_slice(payload);
    }
    bytes
}

fn resolved_bytes(payload: &ResolvedArtifactPayload) -> Vec<u8> {
    match payload {
        ResolvedArtifactPayload::File(path) => std::fs::read(path).unwrap(),
        ResolvedArtifactPayload::Bytes(bytes) => bytes.as_ref().clone(),
    }
}

fn exercise_compatibility_resolver(root: &Path) {
    let artifacts = root.join("artifacts");
    std::fs::create_dir_all(&artifacts).unwrap();
    let legacy_key = "1".repeat(64);
    let pack_key = "2".repeat(64);
    std::fs::write(artifacts.join(format!("{legacy_key}_0")), b"legacy").unwrap();
    std::fs::write(
        artifacts.join(format!("{pack_key}.pack")),
        build_pack(&[b"packed"]),
    )
    .unwrap();

    for call_site in [
        "compatibility_fixture:first",
        "compatibility_fixture:restart",
    ] {
        let legacy = resolve_artifact_payloads(&artifacts, &legacy_key, &[6], true, call_site)
            .unwrap()
            .unwrap();
        let packed = resolve_artifact_payloads(&artifacts, &pack_key, &[6], true, call_site)
            .unwrap()
            .unwrap();
        assert_eq!(resolved_bytes(&legacy[0]), b"legacy");
        assert_eq!(resolved_bytes(&packed[0]), b"packed");
    }
}

fn artifact(bytes: &[u8]) -> ArtifactData {
    ArtifactData {
        outputs: vec![ArtifactOutput {
            name: "migrated.o".to_string(),
            payload: ArtifactPayload::Bytes(Arc::new(bytes.to_vec())),
        }],
        stdout: Arc::new(Vec::new()),
        stderr: Arc::new(Vec::new()),
        exit_code: 0,
    }
}

async fn session_start(daemon: &mut Daemon, work: &Path) -> String {
    match daemon
        .request(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: work.into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
            profile: false,
            private_daemon: None,
        })
        .await
    {
        Response::SessionStarted { session_id, .. } => session_id,
        response => panic!("expected SessionStarted, got {response:?}"),
    }
}

fn compile_request(session_id: &str, compiler: &Path, work: &Path, args: Vec<String>) -> Request {
    Request::Compile {
        session_id: session_id.to_string(),
        args,
        cwd: work.into(),
        compiler: compiler.into(),
        env: None,
        stdin: Vec::new(),
    }
}

fn link_request(ar: &Path, work: &Path, archive: &Path, input: &Path) -> Request {
    Request::LinkEphemeral {
        client_pid: std::process::id(),
        tool: ar.into(),
        args: vec![
            "rcsD".to_string(),
            archive.to_string_lossy().into_owned(),
            input.to_string_lossy().into_owned(),
        ],
        cwd: work.into(),
        env: None,
    }
}

fn exec_request(tool: &Path, work: &Path, output: &Path) -> Request {
    Request::GenericToolExec {
        tool: tool.into(),
        args: vec![
            output.to_string_lossy().into_owned(),
            "exec-payload".to_string(),
        ],
        cwd: work.into(),
        env: Vec::new(),
        input_files: Vec::new(),
        input_extra: Arc::new(b"layout-validation-v1".to_vec()),
        output_streams: ExecOutputStreams::default(),
        output_files: vec![output.into()],
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

/// Print the compile journal when the test panics: each row carries the
/// #1155 `miss_reason`, turning a bare warm-phase `cached: false` failure
/// into a self-explaining CI log.
struct JournalDumpOnPanic {
    journal: PathBuf,
}

impl Drop for JournalDumpOnPanic {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        match std::fs::read_to_string(&self.journal) {
            Ok(contents) => {
                eprintln!("--- compile_journal.jsonl ({}) ---", self.journal.display());
                for line in contents.lines() {
                    eprintln!("{line}");
                }
                eprintln!("--- end compile_journal.jsonl ---");
            }
            Err(error) => {
                eprintln!(
                    "could not read compile journal {}: {error}",
                    self.journal.display()
                );
            }
        }
    }
}

#[track_caller]
fn assert_compile(response: Response, cached: bool) {
    match response {
        Response::CompileResult {
            exit_code,
            cached: actual,
            stderr,
            ..
        } => {
            assert_eq!(
                exit_code,
                0,
                "compiler stderr: {}",
                String::from_utf8_lossy(&stderr)
            );
            assert_eq!(actual, cached);
        }
        response => panic!("expected CompileResult, got {response:?}"),
    }
}

#[track_caller]
fn assert_link(response: Response, cached: bool) {
    assert!(
        matches!(
            response,
            Response::LinkResult {
                exit_code: 0,
                cached: actual,
                ..
            } if actual == cached
        ),
        "expected successful LinkResult with cached={cached}"
    );
}

#[track_caller]
fn assert_exec(response: Response, cached: bool) {
    assert!(
        matches!(
            response,
            Response::GenericToolExecResult {
                exit_code: 0,
                cached: actual,
                ..
            } if actual == cached
        ),
        "expected successful GenericToolExecResult with cached={cached}"
    );
}

async fn wait_for(path: &Path, description: &str) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {description}: {}", path.display()));
}

// NOTE (#1161 durability contract): this harness previously carried two
// cold-phase quiesce helpers here — `wait_for_staged_publication` (polling
// stable `.staged-v2/*.current` pointer counts) and
// `wait_for_depgraph_contexts` (polling `DaemonStatus::dep_graph_contexts`).
// Both were present in the runs that still failed on the 2-core runner,
// because neither observable actually guaranteed durability:
//
// * Per-unit publication is synchronous with the compile response. Both the
//   staged multi path (`handle_compile_multi_staged.rs`: inline
//   `persist_artifact_paths` + `dep_graph.update` + durable-index row send,
//   all under a publication guard) and the non-staged multi path
//   (`handle_compile_multi.rs`: inline hardlink persist + `update` inside
//   the joined miss tasks) complete before `Response::CompileResult` is
//   sent — so there was nothing request-side left to poll for.
// * `dep_graph_contexts` counts contexts in ANY state, including
//   freshly-registered Cold entries with no `artifact_key` yet, so the old
//   wait could be (and was) satisfied before durability.
// * The one genuinely asynchronous durability step was the index-writer WAL:
//   the durable `index.bin` row rides `state.index_writer_tx` and was lost
//   whenever shutdown aborted the writer after its old 2 s bound on a slow
//   host. `DaemonServer::run`'s Shutdown arm now drains it
//   deterministically (flush-ack, then join — `daemon/server/run.rs`), so
//   `Daemon::stop()` returning after the daemon-task join IS the durability
//   barrier this harness relies on. No test-side quiesce is needed.

fn flat_artifact_files(artifact_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(artifact_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let (_, index) = name.rsplit_once('_')?;
            index.parse::<usize>().ok().map(|_| entry.path())
        })
        .collect()
}

#[tokio::test]
#[ignore = "integration: real clang/ar, daemon restart, and full cache-log audit"]
async fn strict_layout_validation_aggregates_all_runtime_flows() {
    let Some(clang) = find_tool("clang") else {
        eprintln!("skipping strict layout validation: clang not found");
        return;
    };
    let Some(ar) = find_tool("ar") else {
        eprintln!("skipping strict layout validation: ar not found");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::capture(&[STAGED_ENV, PACK_ENV, CACHE_ENV, LEGACY_PATH_VALIDATE_ENV]);

    std::env::remove_var(LEGACY_PATH_VALIDATE_ENV);
    exercise_compatibility_resolver(&temp.path().join("compatibility-cache"));

    let cache_root = temp.path().join("strict-cache");
    let artifact_dir = cache_root.join("artifacts");
    let work = temp.path().join("work");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::env::set_var(CACHE_ENV, &cache_root);
    std::env::set_var(STAGED_ENV, "all");
    std::env::remove_var(PACK_ENV);
    std::env::set_var(LEGACY_PATH_VALIDATE_ENV, "1");
    let validation = LegacyValidation::new(&cache_root);

    let migration_key = "3".repeat(64);
    std::fs::write(
        artifact_dir.join(format!("{migration_key}.meta")),
        bincode::serialize(&artifact(b"migrated")).unwrap(),
    )
    .unwrap();

    let compile_count = temp.path().join("compile-count.txt");
    let exec_count = temp.path().join("exec-count.txt");
    let compiler = write_counting_clang(temp.path(), &clang, &compile_count);
    let exec_tool = write_counting_exec(temp.path(), &exec_count);
    let single_source = work.join("single.c");
    let multi_a = work.join("multi_a.c");
    let multi_b = work.join("multi_b.c");
    let single_output = work.join("single.o");
    let multi_a_output = work.join("multi_a.o");
    let multi_b_output = work.join("multi_b.o");
    let link_input = work.join("link-input.o");
    let archive = work.join("liblayout.a");
    let exec_output = work.join("exec-output.bin");
    std::fs::write(&single_source, "int single(void) { return 1; }\n").unwrap();
    std::fs::write(&multi_a, "int multi_a(void) { return 2; }\n").unwrap();
    std::fs::write(&multi_b, "int multi_b(void) { return 3; }\n").unwrap();
    std::fs::write(&link_input, b"not-a-real-object-but-valid-ar-member").unwrap();

    let single_args = vec![
        "-c".to_string(),
        single_source.to_string_lossy().into_owned(),
        "-o".to_string(),
        single_output.to_string_lossy().into_owned(),
    ];
    let multi_args = vec![
        "-c".to_string(),
        multi_a.to_string_lossy().into_owned(),
        multi_b.to_string_lossy().into_owned(),
    ];

    let mut cold = Daemon::start(&cache_root).await;
    let cold_session = session_start(&mut cold, &work).await;
    assert_compile(
        cold.request(&compile_request(
            &cold_session,
            &compiler,
            &work,
            single_args.clone(),
        ))
        .await,
        false,
    );
    assert_compile(
        cold.request(&compile_request(
            &cold_session,
            &compiler,
            &work,
            multi_args.clone(),
        ))
        .await,
        false,
    );
    assert_link(
        cold.request(&link_request(&ar, &work, &archive, &link_input))
            .await,
        false,
    );
    assert_exec(
        cold.request(&exec_request(&exec_tool, &work, &exec_output))
            .await,
        false,
    );
    wait_for(
        &artifact_dir
            .join(".staged-v2")
            .join(format!("{migration_key}.current")),
        "staged migration pointer",
    )
    .await;
    wait_for(
        &cache_root.join(".disk-maintenance-last-full-v1"),
        "startup eviction scan",
    )
    .await;
    // No cold-phase quiesce waits: per-unit publication (artifact bytes,
    // depgraph update, durable-index row send) completes before each compile
    // response, and `stop()` relies on the #1161 shutdown drain in
    // `DaemonServer::run` — after the Shutdown response and the daemon-task
    // join below, `index.bin` and `depgraph.bin` are durable. See the module
    // NOTE above for why the old pointer-count / dep_graph_contexts waits
    // guaranteed nothing.
    cold.stop().await;

    let cold_compile_count = line_count(&compile_count);
    let cold_exec_count = line_count(&exec_count);
    // 3 = one single-file compile + one per unit of the two-source multi
    // compile. A cold multi-source miss is executed per unit by design: each
    // unit is compiled into its own staging generation with an explicit `-o`
    // and its own `-MD -MF <staging>/unit.d -MT <cwd output>` depfile so the
    // artifacts publish independently (a single clang invocation could not
    // emit per-unit staged outputs — it accepts only one `-o`).
    assert_eq!(
        cold_compile_count,
        3,
        "single + per-unit multi cold compiler runs; log:\n{}",
        std::fs::read_to_string(&compile_count).unwrap_or_default()
    );
    assert_eq!(cold_exec_count, 1, "one cold generic-tool run");

    for output in [
        &single_output,
        &multi_a_output,
        &multi_b_output,
        &archive,
        &exec_output,
    ] {
        std::fs::remove_file(output).unwrap();
    }

    // On any warm-phase panic, dump the compile journal so CI logs carry
    // the concrete #1155 miss_reason instead of a bare `cached: false`.
    let _journal_dump = JournalDumpOnPanic {
        journal: cache_root.join("logs").join("compile_journal.jsonl"),
    };
    let mut warm = Daemon::start(&cache_root).await;
    let warm_session = session_start(&mut warm, &work).await;
    assert_compile(
        warm.request(&compile_request(
            &warm_session,
            &compiler,
            &work,
            single_args,
        ))
        .await,
        true,
    );
    assert_compile(
        warm.request(&compile_request(
            &warm_session,
            &compiler,
            &work,
            multi_args,
        ))
        .await,
        true,
    );
    assert_link(
        warm.request(&link_request(&ar, &work, &archive, &link_input))
            .await,
        true,
    );
    assert_exec(
        warm.request(&exec_request(&exec_tool, &work, &exec_output))
            .await,
        true,
    );
    warm.stop().await;

    assert_eq!(
        line_count(&compile_count),
        cold_compile_count,
        "restart-warm single/multi restores must not execute clang"
    );
    assert_eq!(
        line_count(&exec_count),
        cold_exec_count,
        "restart-warm exec restore must not execute the tool"
    );
    assert!(single_output.is_file());
    assert!(multi_a_output.is_file());
    assert!(multi_b_output.is_file());
    assert!(archive.is_file());
    assert_eq!(std::fs::read(&exec_output).unwrap(), b"exec-payload");
    assert!(
        flat_artifact_files(&artifact_dir).is_empty(),
        "all strict-mode writes, including migration, must use staged-v2"
    );

    let migrated = resolve_artifact_payloads(
        &artifact_dir,
        &migration_key,
        &[8],
        true,
        "migration_fixture",
    )
    .unwrap()
    .unwrap();
    assert_eq!(resolved_bytes(&migrated[0]), b"migrated");

    validation.finish();
    let report = audit_cache_root(
        &cache_root,
        LogAuditContext::Integration,
        &AuditOptions::default(),
    )
    .unwrap();
    assert!(
        report.passed(),
        "aggregate cache-log audit failed:\n{}",
        report.format_human()
    );
}
