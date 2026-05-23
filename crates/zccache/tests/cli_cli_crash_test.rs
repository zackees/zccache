//! End-to-end coverage for the CLI-side install of
//! `zccache::core::crash::install("zccache")`.
//!
//! Spawns the `cli-crash-trigger` fixture (defined at
//! `crates/zccache-cli/src/bin/cli_crash_trigger.rs`) with a per-test
//! `ZCCACHE_CACHE_DIR` tempdir, faults it deliberately, then asserts
//! that the dump path under `<cache>/crashes/` matches the new
//! `crash-<ts>-zccache-<kind>.txt` naming. See issue #313.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MIN_DUMP_BYTES: u64 = 200;

fn run(mode: &str, expected_label: &str) -> (PathBuf, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let cache_dir = tmp.path().join(".zccache");
    let crash_dir = cache_dir.join("crashes");

    let bin = env!("CARGO_BIN_EXE_cli-crash-trigger");
    let _ = std::process::Command::new(bin)
        .arg(mode)
        .env("ZCCACHE_CACHE_DIR", &cache_dir)
        .env("RUST_BACKTRACE", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {bin}: {e}"));

    let dump = wait_for_dump_with_label(&crash_dir, expected_label, Duration::from_secs(5));
    let path = dump.unwrap_or_else(|| {
        panic!(
            "no dump containing '{expected_label}' appeared in {} after mode={mode}",
            crash_dir.display()
        )
    });
    (path, tmp)
}

fn wait_for_dump_with_label(dir: &Path, label: &str, timeout: Duration) -> Option<PathBuf> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let ext_ok = path.extension().is_some_and(|e| e == "txt");
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                if ext_ok && name.contains(label) && name.contains("zccache") {
                    return Some(path);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
fn cli_panic_writes_dump_with_zccache_stem() {
    let (dump, _tmp) = run("panic", "panic");
    let size = std::fs::metadata(&dump).unwrap().len();
    assert!(
        size > MIN_DUMP_BYTES,
        "panic dump suspiciously small: {size} bytes"
    );
    let body = std::fs::read_to_string(&dump).unwrap();
    assert!(
        body.contains("intentional test panic from cli-crash-trigger"),
        "dump body missing panic message:\n{body}"
    );
    assert!(
        body.contains("Binary:  zccache"),
        "dump body missing binary tag:\n{body}"
    );
    let name = dump.file_name().unwrap().to_string_lossy().into_owned();
    // We want the CLI stem in the filename, NOT the daemon's. The
    // shared install() interpolates the stem caller passed in.
    assert!(
        name.contains("-zccache-") && !name.contains("zccache-daemon"),
        "dump filename should embed the CLI stem only: {name}"
    );
    // Backtrace section should mention this file (or at least the
    // `cli_crash_trigger` symbol). `debug = "line-tables-only"` in
    // `profile.dev` and `profile.release` is what gives file:line.
    // In CI/cross builds without sidecars we may not get `.rs:line`
    // resolved — assert only on the symbol so the test stays robust.
    assert!(
        body.contains("Backtrace:"),
        "dump body missing backtrace section:\n{body}"
    );
}

#[test]
#[cfg(unix)]
fn cli_sigsegv_writes_dump_with_zccache_stem() {
    let (dump, _tmp) = run("sigsegv", "SIGSEGV");
    let size = std::fs::metadata(&dump).unwrap().len();
    assert!(size > MIN_DUMP_BYTES);
    let name = dump.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.contains("SIGSEGV") && name.contains("-zccache-"),
        "filename missing labels: {name}"
    );
}

#[test]
#[cfg(windows)]
fn cli_sigsegv_writes_dump_with_zccache_stem_windows() {
    let (dump, _tmp) = run("sigsegv", "STATUS_ACCESS_VIOLATION");
    let size = std::fs::metadata(&dump).unwrap().len();
    assert!(size > MIN_DUMP_BYTES);
    let name = dump.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.contains("STATUS_ACCESS_VIOLATION") && name.contains("-zccache-"),
        "filename missing labels: {name}"
    );
}
