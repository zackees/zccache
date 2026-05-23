//! End-to-end tests for the signal/exception crash handler.
//!
//! Each test spawns the `crash-trigger` fixture binary (defined at
//! `crates/zccache-daemon/src/bin/crash_trigger.rs`) with a mode that
//! deliberately causes a specific kind of fault, then asserts that a
//! crash dump file appeared in the configured crash directory. The
//! fixture inherits `ZCCACHE_CACHE_DIR` so each test gets an isolated
//! tempdir that doesn't pollute the real `~/.zccache`.
//!
//! After issue #313 every dump (panic or signal) is plain text and the
//! filename embeds the binary stem + signal/panic label, e.g.
//! `crash-1730000000-zccache-daemon-SIGSEGV.txt`. Tests no longer
//! discriminate by extension; they match on the substring inside the
//! filename.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Spawn `crash-trigger <mode>` with a per-test cache dir, wait for it
/// to exit, then assert that exactly one dump containing
/// `expected_label` in its filename appeared in `<cache>/crashes/`.
/// Returns the dump path AND the `TempDir` guard — the caller must keep
/// the guard alive while it touches the path, since dropping the guard
/// deletes the tempdir.
fn run_crash_scenario(mode: &str, expected_label: &str) -> (PathBuf, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let cache_dir = tmp.path().join(".zccache");
    let crash_dir = cache_dir.join("crashes");

    let bin = env!("CARGO_BIN_EXE_crash-trigger");
    let output = std::process::Command::new(bin)
        .arg(mode)
        .env("ZCCACHE_CACHE_DIR", &cache_dir)
        // RUST_BACKTRACE is force_capture()d inside the handler regardless,
        // but be explicit so a developer running the test by hand with
        // RUST_BACKTRACE=0 doesn't get a surprising empty backtrace.
        .env("RUST_BACKTRACE", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {bin}: {e}"));

    // We expect a non-zero exit OR a crash signal — but the panic
    // case may unwind cleanly and exit(0), so don't assert on the
    // exit status. The disk evidence is the real assertion.
    let _ = output;

    let dump = wait_for_dump_with_label(&crash_dir, expected_label, Duration::from_secs(5));
    let path = dump.unwrap_or_else(|| {
        panic!(
            "no dump containing label '{expected_label}' appeared in {} after mode={mode}",
            crash_dir.display()
        )
    });
    (path, tmp)
}

/// Poll the directory for a `.txt` whose filename contains `label`.
/// The handler writes synchronously, but on Windows there's a tiny
/// window between the dump write and process exit where the directory
/// listing may not yet include the file — poll for a few seconds to
/// absorb that.
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
                if ext_ok && name.contains(label) {
                    return Some(path);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Sanity floor on dump size — a truncated handler that managed to
/// `create` the file but failed to write content would otherwise pass
/// the "file appeared" check. Our text dumps are ~1-4 KB.
const MIN_DUMP_BYTES: u64 = 200;

#[test]
fn panic_writes_text_dump() {
    let (dump, _tmp) = run_crash_scenario("panic", "panic");
    let size = std::fs::metadata(&dump).unwrap().len();
    assert!(
        size > MIN_DUMP_BYTES,
        "panic dump suspiciously small: {size} bytes at {}",
        dump.display()
    );
    let body = std::fs::read_to_string(&dump).unwrap();
    assert!(
        body.contains("intentional test panic from crash-trigger"),
        "panic dump didn't include the panic message:\n{body}"
    );
    assert!(
        body.contains("Binary:  zccache-daemon"),
        "panic dump didn't tag binary stem:\n{body}"
    );
    let name = dump.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.contains("zccache-daemon"),
        "dump filename missing binary stem: {name}"
    );
}

#[test]
#[cfg(unix)]
fn sigsegv_writes_signal_dump() {
    let (dump, _tmp) = run_crash_scenario("sigsegv", "SIGSEGV");
    let size = std::fs::metadata(&dump).unwrap().len();
    assert!(
        size > MIN_DUMP_BYTES,
        "sigsegv dump suspiciously small: {size} bytes at {}",
        dump.display()
    );
    let name = dump.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.contains("SIGSEGV") && name.contains("zccache-daemon"),
        "dump filename missing labels: {name}"
    );
}

#[test]
#[cfg(unix)]
fn sigabrt_writes_signal_dump() {
    let (dump, _tmp) = run_crash_scenario("sigabrt", "SIGABRT");
    let size = std::fs::metadata(&dump).unwrap().len();
    assert!(size > MIN_DUMP_BYTES);
}

// On Windows the same low-level fault paths land via SEH; the
// crash-handler crate maps them to STATUS_ACCESS_VIOLATION etc.
#[test]
#[cfg(windows)]
fn windows_segfault_writes_dump() {
    let (dump, _tmp) = run_crash_scenario("sigsegv", "STATUS_ACCESS_VIOLATION");
    let size = std::fs::metadata(&dump).unwrap().len();
    assert!(size > MIN_DUMP_BYTES);
}

#[test]
#[ignore = "stack overflow needs sigaltstack (Unix) or specific guard-page \
            handling (Windows); the handler runs on an already-exhausted \
            stack and the dump is currently not produced. Tracked separately \
            from the rest of the crash-handler coverage."]
fn stack_overflow_writes_signal_dump() {
    let (dump, _tmp) = run_crash_scenario("stack-overflow", "STACK_OVERFLOW");
    assert!(dump.exists());
}

// SIGILL behaviour on macOS goes through Mach exception ports in ways
// that crash-handler documents as more fragile than the Unix-signal
// path. Skip there until we either add the Mach-port workaround or
// drop down to `sigaction(SIGILL)` directly.
#[test]
#[cfg(all(unix, not(target_os = "macos")))]
fn illegal_instruction_writes_signal_dump() {
    let (dump, _tmp) = run_crash_scenario("illegal-instruction", "SIGILL");
    let size = std::fs::metadata(&dump).unwrap().len();
    assert!(size > MIN_DUMP_BYTES);
}
