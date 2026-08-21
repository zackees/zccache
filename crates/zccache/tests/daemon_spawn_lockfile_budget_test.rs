//! Regression tests for the daemon spawn-to-lockfile budget from #784/#800.
//!
//! The daemon readiness contract is the lockfile, not full background startup:
//! clients poll for it with a 10 second grace period. Large persisted cache
//! state must therefore be loaded after `write_lock_file(pid)`, or Windows
//! Defender plus concurrent builds can push clients into
//! `no daemon lockfile observed within 10s`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use zccache::artifact::{ArtifactIndex, ArtifactStore};
use zccache::core::{config, NormalizedPath};
use zccache::depgraph::SystemIncludeCache;
use zccache::fscache::{Confidence, FileMetadata, MetadataCache};

/// How much longer the large-state spawn may take than the small-state
/// control before we call it a synchronous load.
///
/// A *differential*, not an absolute budget. The absolute 8s budget this
/// replaces failed roughly half of `main`'s Integration runs (#1446) for a
/// simple reason: it was measuring machine speed. Clean spawn latency is
/// ~1.8s on a dev box and ~8.2s on a hosted `ubuntu` runner, so a threshold
/// tight enough to mean anything on the dev box sat on top of the runner's
/// clean baseline. That baseline cancels in a difference.
///
/// Be clear about what this can detect. Measured 2026-08-21 on a dev box,
/// over the fixtures below: clean delta is 0ms / 130ms / 0ms across three
/// runs, and restoring the #784 loads (`MetadataCache::load_from_disk` plus
/// `ArtifactStore::open`) moves it to ~520ms. That is a real separation but
/// a small absolute signal, and hosted-runner spawn jitter is measured in
/// seconds. So this budget is deliberately set well above the signal: it is
/// a coarse backstop for a *catastrophic* synchronous load, not the primary
/// detector.
///
/// The primary detector is
/// `lockfile_window_has_no_synchronous_persisted_state_loads` below, which
/// is deterministic and machine-independent. Each guard is assigned the job
/// it can actually do.
const STATE_LOAD_DELTA_BUDGET: Duration = Duration::from_secs(3);

/// Per-spawn hang detector, deliberately far above any real latency. The
/// assertion is the delta above; this only stops the poll loop from
/// spinning forever if a daemon never writes a lockfile at all.
const HARD_CAP: Duration = Duration::from_secs(60);

const TEST_DAEMON_NAMESPACE_LARGE: &str = "lockfile-budget";
const TEST_DAEMON_NAMESPACE_SMALL: &str = "lockfile-budget-control";

const METADATA_MIN_BYTES: u64 = 100 * 1024 * 1024;
const ARTIFACT_INDEX_ENTRIES: usize = 10_000;
const SYSTEM_INCLUDE_ENTRIES: usize = 50;

// Control fixture. Same code paths and same file names, negligible payload,
// so the only thing that differs between the two spawns is how much state
// there is to load.
const CONTROL_ARTIFACT_INDEX_ENTRIES: usize = 8;
const CONTROL_SYSTEM_INCLUDE_ENTRIES: usize = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StateFixture {
    /// Near-empty snapshots: the machine-speed control.
    Control,
    /// FastLED-scale snapshots: what a reintroduced synchronous load reads.
    Large,
}

#[test]
fn daemon_writes_lockfile_without_waiting_for_large_persisted_state() {
    // Control first: a near-empty state dir. Running it first also means the
    // daemon binary is already in page cache for the large run, so binary
    // load cost is excluded from the difference rather than inflating it.
    let control = measure_lockfile_latency(StateFixture::Control);
    let large = measure_lockfile_latency(StateFixture::Large);

    let delta = large.saturating_sub(control);
    assert!(
        delta <= STATE_LOAD_DELTA_BUDGET,
        "daemon took {large:?} to write its lockfile with a large persisted          state but only {control:?} with a near-empty one -- {delta:?} longer,          exceeding the {STATE_LOAD_DELTA_BUDGET:?} budget. A synchronous          persisted-state load was likely added before `write_lock_file`.          See zackees/zccache#784, #800 and #1446."
    );
}

/// Spawn a daemon over `fixture` and return how long it took to write its
/// readiness lockfile.
fn measure_lockfile_latency(fixture: StateFixture) -> Duration {
    let daemon_bin = env!("CARGO_BIN_EXE_zccache-daemon");
    let namespace = match fixture {
        StateFixture::Control => TEST_DAEMON_NAMESPACE_SMALL,
        StateFixture::Large => TEST_DAEMON_NAMESPACE_LARGE,
    };
    let tmp = tempfile::tempdir().expect("create tempdir");
    let cache_dir = NormalizedPath::new(tmp.path().join("cache"));
    let effective_cache_dir = config::effective_cache_root_from_top_level(&cache_dir);
    let daemon_state_dir = config::daemon_state_dir_from_cache_dir_with_namespace(
        &effective_cache_dir,
        Some(namespace.to_owned()),
    );
    std::fs::create_dir_all(daemon_state_dir.as_path()).expect("create daemon state dir");
    install_persisted_state(&daemon_state_dir, fixture);

    if fixture == StateFixture::Large {
        assert_large_fixture_is_still_large(&daemon_state_dir);
    }

    let endpoint = zccache::ipc::unique_test_endpoint();
    let lockfile = lock_file_path_for_cache_dir(cache_dir.as_path(), namespace);
    let _ = std::fs::remove_file(lockfile.as_path());

    let spawn_at = Instant::now();
    let mut child = Command::new(daemon_bin)
        .args(["--foreground", "--endpoint", &endpoint])
        .env("ZCCACHE_CACHE_DIR", cache_dir.as_path())
        .env("ZCCACHE_DAEMON_STATE_DIR", daemon_state_dir.as_path())
        // Development binaries synthesize a hash namespace when this is
        // absent. Supply an explicit isolated identity so the parent and
        // daemon agree on the readiness lockfile (#1404).
        .env("ZCCACHE_DAEMON_NAMESPACE", namespace)
        .env_remove("ZCCACHE_COLOCATE")
        .env("ZCCACHE_NO_UNLOCK", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daemon");

    let stderr = child.stderr.take().expect("take child stderr");
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.take(64 * 1024).read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    });

    let mut observed_at: Option<Duration> = None;
    while spawn_at.elapsed() < HARD_CAP {
        if lockfile.exists() {
            observed_at = Some(spawn_at.elapsed());
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(lockfile.as_path());

    let captured_stderr = stderr_handle
        .join()
        .unwrap_or_else(|_| String::from("<stderr thread panicked>"));

    match observed_at {
        Some(d) => d,
        None => {
            let strays = find_stray_lockfiles(cache_dir.as_path(), 4);
            panic!(
                "daemon ({fixture:?} fixture) did not write lockfile at `{}` within {:?};                  cache_dir={}, endpoint={}
                 stray daemon*.lock files under cache_dir: {:?}
                 daemon stderr (first 64K):
{}",
                lockfile.display(),
                HARD_CAP,
                cache_dir.display(),
                endpoint,
                strays,
                captured_stderr.trim(),
            );
        }
    }
}

/// The large fixture only tests anything while it stays large; a silent
/// shrink would turn the differential into a comparison of two empty states.
fn assert_large_fixture_is_still_large(daemon_state_dir: &NormalizedPath) {
    let metadata_path = daemon_state_dir.join("metadata.bin");
    assert!(
        metadata_path.as_path().metadata().unwrap().len() >= METADATA_MIN_BYTES,
        "metadata fixture must remain large enough to catch sync reads"
    );
    assert_eq!(
        ArtifactStore::open(daemon_state_dir.join("index.bin").as_path())
            .unwrap()
            .len(),
        ARTIFACT_INDEX_ENTRIES,
        "artifact index fixture must remain populated"
    );
    assert_eq!(
        SystemIncludeCache::load_from_disk(daemon_state_dir.join("system_includes.bin").as_path(),)
            .unwrap()
            .len(),
        SYSTEM_INCLUDE_ENTRIES,
        "system include fixture must remain populated"
    );
}

#[test]
fn lockfile_window_has_no_synchronous_persisted_state_loads() {
    // Post-#997/#1019 layout: `src/bin/zccache-daemon.rs` is a thin shim and
    // the daemon subsystem lives in `zccache-daemon-core`; scan the real
    // startup source there (issue #1030).
    let lifecycle =
        workspace_source_file("crates/zccache-daemon-core/src/daemon/server/lifecycle.rs");
    let bind_window = slice_between(
        &lifecycle,
        "let listener = IpcListener::bind(endpoint)?;",
        "Ok(Self {",
    );
    assert_no_sync_loads_in_window("bind_with_cache_dir", &lifecycle, bind_window);

    let daemon = workspace_source_file("crates/zccache-daemon-core/src/daemon/entry.rs");
    // Marker is the binding name, not the full expression — rustfmt is free
    // to reflow the spawn_blocking closure across lines.
    let startup_window = slice_between(
        &daemon,
        "let bind_result =",
        "crate::ipc::write_lock_file(pid)",
    );
    assert!(
        startup_window.contains("crate::daemon::DaemonServer::bind(&bind_endpoint)"),
        "daemon bind-to-lockfile window must include endpoint bind"
    );
    assert_no_sync_loads_in_window("daemon bind-to-lockfile", &daemon, startup_window);
}

fn install_persisted_state(daemon_state_dir: &NormalizedPath, fixture: StateFixture) {
    let (pad_metadata_to, index_entries, include_entries) = match fixture {
        StateFixture::Control => (
            None,
            CONTROL_ARTIFACT_INDEX_ENTRIES,
            CONTROL_SYSTEM_INCLUDE_ENTRIES,
        ),
        StateFixture::Large => (
            Some(METADATA_MIN_BYTES),
            ARTIFACT_INDEX_ENTRIES,
            SYSTEM_INCLUDE_ENTRIES,
        ),
    };
    write_metadata_snapshot(daemon_state_dir, pad_metadata_to);
    write_artifact_index(daemon_state_dir, index_entries);
    write_system_includes_snapshot(daemon_state_dir, include_entries);
    write_compiler_hash_placeholder(daemon_state_dir);
}

fn write_metadata_snapshot(daemon_state_dir: &NormalizedPath, pad_to: Option<u64>) {
    let metadata = MetadataCache::new();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    for i in 0..256 {
        metadata.insert(
            NormalizedPath::from(format!("/fixture/source_{i:04}.cc")),
            FileMetadata {
                mtime: now,
                size: 1024 + i as u64,
                confidence: Confidence::High,
                last_verified: Instant::now(),
                content_hash: Some([i as u8; 32]),
            },
        );
    }

    let path = daemon_state_dir.join("metadata.bin");
    metadata.save_to_disk(path.as_path()).unwrap();

    // The public writer keeps snapshots compact. Pad the real metadata file so
    // a regression that reintroduces a synchronous `MetadataCache::load_from_disk`
    // must read a FastLED-scale blob before the lockfile is written. The
    // control fixture skips the padding -- that size difference is the
    // independent variable the differential measures.
    if let Some(len) = pad_to {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path.as_path())
            .unwrap();
        file.set_len(len).unwrap();
    }
}

fn write_artifact_index(daemon_state_dir: &NormalizedPath, entries: usize) {
    let index_path = daemon_state_dir.join("index.bin");
    let store = ArtifactStore::open_empty(index_path.as_path());
    let stdout = Arc::new(Vec::new());
    let stderr = Arc::new(Vec::new());
    let rows = (0..entries).map(|i| {
        let key = format!("{i:064x}");
        let meta = ArtifactIndex::new(
            vec![format!("obj_{i:05}.o")],
            vec![128 + (i % 1024) as u64],
            Arc::clone(&stdout),
            Arc::clone(&stderr),
            0,
        );
        (key, meta)
    });
    assert_eq!(store.insert_many(rows), entries);
    store.flush().unwrap();
}

fn write_system_includes_snapshot(daemon_state_dir: &NormalizedPath, entries: usize) {
    let compiler_dir = daemon_state_dir.join("fixture-compilers");
    std::fs::create_dir_all(compiler_dir.as_path()).unwrap();

    let mut cache = SystemIncludeCache::new();
    for i in 0..entries {
        let compiler = compiler_dir.join(format!("cc_{i:03}"));
        std::fs::write(compiler.as_path(), format!("compiler-{i}")).unwrap();
        cache.insert(
            compiler,
            vec![
                NormalizedPath::from(format!("/usr/include/fixture/{i}")),
                NormalizedPath::from(format!("/opt/sdk/include/{i}")),
            ],
        );
    }

    cache
        .save_to_disk(daemon_state_dir.join("system_includes.bin").as_path())
        .unwrap();
}

fn write_compiler_hash_placeholder(daemon_state_dir: &NormalizedPath) {
    let path = daemon_state_dir.join("compiler_hash.bin");
    let mut file = std::fs::File::create(path.as_path()).unwrap();
    for i in 0..100 {
        writeln!(file, "compiler-hash-fixture-entry-{i:03}").unwrap();
    }
    file.flush().unwrap();
}

fn lock_file_path_for_cache_dir(cache_dir: &Path, namespace: &str) -> PathBuf {
    let prev_cache_dir = std::env::var_os("ZCCACHE_CACHE_DIR");
    let prev_namespace = std::env::var_os("ZCCACHE_DAEMON_NAMESPACE");
    let prev_colocate = std::env::var_os("ZCCACHE_COLOCATE");
    unsafe {
        std::env::set_var("ZCCACHE_CACHE_DIR", cache_dir);
        std::env::set_var("ZCCACHE_DAEMON_NAMESPACE", namespace);
        std::env::remove_var("ZCCACHE_COLOCATE");
    }
    let lockfile = zccache::ipc::lock_file_path().as_path().to_path_buf();
    unsafe {
        restore_env("ZCCACHE_CACHE_DIR", prev_cache_dir);
        restore_env("ZCCACHE_DAEMON_NAMESPACE", prev_namespace);
        restore_env("ZCCACHE_COLOCATE", prev_colocate);
    }
    lockfile
}

unsafe fn restore_env(key: &str, value: Option<std::ffi::OsString>) {
    match value {
        Some(v) => unsafe { std::env::set_var(key, v) },
        None => unsafe { std::env::remove_var(key) },
    }
}

/// Read a source file by workspace-root-relative path. The scanned daemon
/// startup sources moved out of this crate in the #1019 split, so resolve
/// from the workspace root (two levels above this crate's manifest).
fn workspace_source_file(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

fn slice_between<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let start_idx = source
        .find(start)
        .unwrap_or_else(|| panic!("missing `{start}`"));
    let end_idx = source[start_idx..]
        .find(end)
        .map(|idx| start_idx + idx)
        .unwrap_or_else(|| panic!("missing `{end}` after `{start}`"));
    &source[start_idx..end_idx]
}

const FORBIDDEN_SYNC_LOADS: [&str; 4] = [
    "::load_from_disk",
    "ArtifactStore::open(",
    "std::fs::read(",
    "std::fs::read_to_end(",
];

fn assert_no_sync_loads(label: &str, source: &str) {
    assert_no_sync_loads_in(label, source);
}

/// As [`assert_no_sync_loads`], but also follows calls made inside the
/// window into functions defined in `file`, one level deep.
///
/// The literal-only scan has a blind spot that matters: a load moved behind
/// a helper -- `preload_state(dir)` with the read one frame down -- shows no
/// forbidden substring in the window and passes. That is not hypothetical;
/// it was demonstrated on #1446 by injecting exactly that shape, and *both*
/// guards in this file missed it. The wall-clock guard cannot close the gap
/// (see `STATE_LOAD_DELTA_BUDGET` -- the signal is ~0.5s against seconds of
/// runner jitter), so it is closed here instead, where the check is
/// deterministic.
///
/// One level, not transitive: it covers the realistic refactor -- extracting
/// startup steps into named helpers -- without turning a test into a call
/// graph analysis. Deeper nesting stays uncovered, deliberately.
fn assert_no_sync_loads_in_window(label: &str, file: &str, window: &str) {
    assert_no_sync_loads_in(label, window);

    for callee in called_fn_names(window) {
        if let Some(body) = fn_body(file, &callee) {
            assert_no_sync_loads_in(&format!("{label} -> fn {callee}()"), &body);
        }
    }
}

fn assert_no_sync_loads_in(label: &str, source: &str) {
    for forbidden in FORBIDDEN_SYNC_LOADS {
        assert!(
            !source.contains(forbidden),
            "{label} must not contain `{forbidden}` before readiness lockfile"
        );
    }
}

/// Bare `name(` call sites in `window`. Method calls (`.name(`) and paths
/// (`::name(`) are skipped: those resolve outside this file's free
/// functions, which is all `fn_body` can look up.
fn called_fn_names(window: &str) -> Vec<String> {
    let bytes = window.as_bytes();
    let mut names = Vec::new();
    let mut start = None;
    for (i, ch) in window.char_indices() {
        let is_ident = ch.is_ascii_alphanumeric() || ch == '_';
        match (start, is_ident) {
            (None, true) => start = Some(i),
            (Some(s), false) => {
                if ch == '(' {
                    let preceded_by_path_or_dot = s
                        .checked_sub(1)
                        .is_some_and(|j| bytes[j] == b'.' || bytes[j] == b':');
                    let name = &window[s..i];
                    if !preceded_by_path_or_dot
                        && name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
                        && !names.iter().any(|n: &String| n == name)
                    {
                        names.push(name.to_owned());
                    }
                }
                start = None;
            }
            _ => {}
        }
    }
    names
}

/// Body of `fn <name>(` in `file`, brace-matched. `None` when the file
/// defines no such free function.
fn fn_body(file: &str, name: &str) -> Option<String> {
    let needle = format!("fn {name}(");
    let at = file.find(&needle)?;
    let open = file[at..].find('{').map(|i| at + i)?;
    let mut depth = 0usize;
    for (i, ch) in file[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(file[open..=open + i].to_owned());
                }
            }
            _ => {}
        }
    }
    None
}

fn find_stray_lockfiles(dir: &Path, max_depth: usize) -> Vec<String> {
    let mut out = Vec::new();
    walk(dir, max_depth, &mut out);
    out
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with("daemon") && name.ends_with(".lock") {
                out.push(path.display().to_string());
            }
        }
        if path.is_dir() {
            walk(&path, depth - 1, out);
        }
    }
}
