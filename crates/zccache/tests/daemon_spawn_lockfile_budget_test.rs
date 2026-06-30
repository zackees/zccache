//! Regression tests for the daemon spawn-to-lockfile budget from #784/#800.
//!
//! The daemon readiness contract is the lockfile, not full background startup:
//! clients poll for it with a 10 second grace period. Large persisted cache
//! state must therefore be loaded after `write_lock_file(pid)`, or Windows
//! Defender plus concurrent builds can push clients into
//! `no daemon lockfile observed within 10s`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::panic_in_result_fn, clippy::unwrap_in_result)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use zccache::artifact::{ArtifactIndex, ArtifactStore};
use zccache::core::{config, NormalizedPath};
use zccache::depgraph::SystemIncludeCache;
use zccache::fscache::{Confidence, FileMetadata, MetadataCache};

const LOCKFILE_BUDGET: Duration = Duration::from_secs(8);
const HARD_CAP: Duration = Duration::from_secs(9);

const METADATA_MIN_BYTES: u64 = 100 * 1024 * 1024;
const ARTIFACT_INDEX_ENTRIES: usize = 10_000;
const SYSTEM_INCLUDE_ENTRIES: usize = 50;

#[test]
fn daemon_writes_lockfile_within_budget_with_large_persisted_state() {
    let daemon_bin = env!("CARGO_BIN_EXE_zccache-daemon");
    let tmp = tempfile::tempdir().expect("create tempdir");
    let cache_dir = NormalizedPath::new(tmp.path().join("cache"));
    std::fs::create_dir_all(cache_dir.as_path()).expect("create cache dir");
    install_large_persisted_state(&cache_dir);

    let metadata_path = config::metadata_path_from_cache_dir(&cache_dir);
    assert!(
        metadata_path.as_path().metadata().unwrap().len() >= METADATA_MIN_BYTES,
        "metadata fixture must remain large enough to catch sync reads"
    );
    assert_eq!(
        ArtifactStore::open(config::index_path_from_cache_dir(&cache_dir).as_path())
            .unwrap()
            .len(),
        ARTIFACT_INDEX_ENTRIES,
        "artifact index fixture must remain populated"
    );
    assert_eq!(
        SystemIncludeCache::load_from_disk(
            config::system_includes_cache_path_from_cache_dir(&cache_dir).as_path(),
        )
        .unwrap()
        .len(),
        SYSTEM_INCLUDE_ENTRIES,
        "system include fixture must remain populated"
    );

    let endpoint = zccache::ipc::unique_test_endpoint();
    let lockfile = lock_file_path_for_cache_dir(cache_dir.as_path());
    let _ = std::fs::remove_file(lockfile.as_path());

    let spawn_at = Instant::now();
    let mut child = Command::new(daemon_bin)
        .args(["--foreground", "--endpoint", &endpoint])
        .env("ZCCACHE_CACHE_DIR", cache_dir.as_path())
        .env_remove("ZCCACHE_DAEMON_NAMESPACE")
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
        if lockfile.as_path().is_file() {
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

    let observed = match observed_at {
        Some(d) => d,
        None => {
            let strays = find_stray_lockfiles(cache_dir.as_path(), 4);
            panic!(
                "daemon did not write lockfile at `{}` within {:?}; \
                 cache_dir={}, endpoint={}\n\
                 stray daemon*.lock files under cache_dir: {:?}\n\
                 daemon stderr (first 64K):\n{}",
                lockfile.display(),
                HARD_CAP,
                cache_dir.display(),
                endpoint,
                strays,
                captured_stderr.trim(),
            );
        }
    };

    assert!(
        observed <= LOCKFILE_BUDGET,
        "daemon wrote lockfile `{}` after {:?}, exceeding the {:?} budget. \
         A synchronous persisted-state load was likely added before \
         `write_lock_file`. See zackees/zccache#784 and #800.",
        lockfile.display(),
        observed,
        LOCKFILE_BUDGET,
    );
}

#[test]
fn lockfile_window_has_no_synchronous_persisted_state_loads() {
    let lifecycle = source_file("src/daemon/server/lifecycle.rs");
    let bind_window = slice_between(
        &lifecycle,
        "let listener = IpcListener::bind(endpoint)?;",
        "Ok(Self {",
    );
    assert_no_sync_loads("bind_with_cache_dir", bind_window);

    let daemon = source_file("src/bin/zccache-daemon.rs");
    let startup_window = slice_between(
        &daemon,
        "let bind_result = tokio::task::spawn_blocking(move || {",
        "zccache::ipc::write_lock_file(pid)",
    );
    assert!(
        startup_window.contains("zccache::daemon::DaemonServer::bind(&bind_endpoint)"),
        "daemon bind-to-lockfile window must include endpoint bind"
    );
    assert_no_sync_loads("daemon bind-to-lockfile", startup_window);
}

fn install_large_persisted_state(cache_dir: &NormalizedPath) {
    write_large_metadata_snapshot(cache_dir);
    write_artifact_index(cache_dir);
    write_system_includes_snapshot(cache_dir);
    write_compiler_hash_placeholder(cache_dir);
}

fn write_large_metadata_snapshot(cache_dir: &NormalizedPath) {
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

    let path = config::metadata_path_from_cache_dir(cache_dir);
    metadata.save_to_disk(path.as_path()).unwrap();

    // The public writer keeps snapshots compact. Pad the real metadata file so
    // a regression that reintroduces a synchronous `MetadataCache::load_from_disk`
    // must read a FastLED-scale blob before the lockfile is written.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path.as_path())
        .unwrap();
    file.set_len(METADATA_MIN_BYTES).unwrap();
}

fn write_artifact_index(cache_dir: &NormalizedPath) {
    let index_path = config::index_path_from_cache_dir(cache_dir);
    let store = ArtifactStore::open_empty(index_path.as_path());
    let stdout = Arc::new(Vec::new());
    let stderr = Arc::new(Vec::new());
    let rows = (0..ARTIFACT_INDEX_ENTRIES).map(|i| {
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
    assert_eq!(store.insert_many(rows), ARTIFACT_INDEX_ENTRIES);
    store.flush().unwrap();
}

fn write_system_includes_snapshot(cache_dir: &NormalizedPath) {
    let compiler_dir = cache_dir.join("fixture-compilers");
    std::fs::create_dir_all(compiler_dir.as_path()).unwrap();

    let mut cache = SystemIncludeCache::new();
    for i in 0..SYSTEM_INCLUDE_ENTRIES {
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
        .save_to_disk(config::system_includes_cache_path_from_cache_dir(cache_dir).as_path())
        .unwrap();
}

fn write_compiler_hash_placeholder(cache_dir: &NormalizedPath) {
    let path = config::compiler_hash_cache_path_from_cache_dir(cache_dir);
    let mut file = std::fs::File::create(path.as_path()).unwrap();
    for i in 0..100 {
        writeln!(file, "compiler-hash-fixture-entry-{i:03}").unwrap();
    }
    file.flush().unwrap();
}

fn lock_file_path_for_cache_dir(cache_dir: &Path) -> PathBuf {
    let prev_cache_dir = std::env::var_os("ZCCACHE_CACHE_DIR");
    let prev_namespace = std::env::var_os("ZCCACHE_DAEMON_NAMESPACE");
    let prev_colocate = std::env::var_os("ZCCACHE_COLOCATE");
    unsafe {
        std::env::set_var("ZCCACHE_CACHE_DIR", cache_dir);
        std::env::remove_var("ZCCACHE_DAEMON_NAMESPACE");
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

fn source_file(relative: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)).unwrap()
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

fn assert_no_sync_loads(label: &str, source: &str) {
    for forbidden in [
        "::load_from_disk",
        "ArtifactStore::open(",
        "std::fs::read(",
        "std::fs::read_to_end(",
    ] {
        assert!(
            !source.contains(forbidden),
            "{label} must not contain `{forbidden}` before readiness lockfile"
        );
    }
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
