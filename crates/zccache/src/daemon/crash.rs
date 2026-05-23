//! Daemon-side façade over [`zccache::core::crash`].
//!
//! Historically this module owned the panic hook + signal handler
//! implementation. Issue #313 moved that code into `zccache::core::crash`
//! so the CLI can install the same coverage with one call. The
//! daemon-only knobs that survived (`install_panic_hook`,
//! `install_minidump_handler`, `check_previous_crashes`,
//! `list_crash_dumps`, `clear_crash_dumps`, `write_crash_dump`) all
//! delegate to the shared core implementation, and the daemon's
//! `main.rs` still installs them in the same order it always has.
//!
//! New code should call `zccache::core::crash::install("zccache-daemon")`
//! once at the top of `main` and rely on the returned `CrashGuard` to
//! keep handlers registered.

use zccache::core::NormalizedPath;

pub use zccache::core::crash::{
    check_previous_crashes, clear_crash_dumps, list_crash_dumps, CrashGuard,
};

/// Install the Rust panic hook with the binary stem `zccache-daemon`.
/// Kept as a free function so the existing crash-trigger fixture (which
/// is shared with the panic-only minidump test) keeps compiling.
pub fn install_panic_hook() {
    let _ = zccache::core::crash::install("zccache-daemon");
}

/// Install OS-level signal/exception handlers. Returns a guard the
/// caller MUST keep alive for the lifetime of the process; dropping it
/// unregisters the handlers.
#[must_use]
pub fn install_minidump_handler() -> Option<MinidumpHandle> {
    // The shared install() in core does panic-hook + signal-handler in
    // one shot. Calling it twice is a no-op (idempotent via OnceLock),
    // so it's safe for the daemon to call install_panic_hook() first
    // and then install_minidump_handler() — they end up wired through
    // the same guard.
    let guard = zccache::core::crash::install("zccache-daemon");
    Some(MinidumpHandle { _guard: guard })
}

/// Opaque RAII guard for the OS-level handler registration. Dropping
/// it unregisters the signal/exception handlers via the underlying
/// `CrashGuard` in core.
pub struct MinidumpHandle {
    #[allow(dead_code)]
    _guard: CrashGuard,
}

/// Write a Rust-panic-style text dump. Kept exported for callers that
/// want to record a synthesised "crash report" (e.g. a graceful shutdown
/// from a recoverable error that still warrants postmortem).
pub fn write_crash_dump(panic_info: &str, backtrace: &str) -> Option<NormalizedPath> {
    let crash_dir = zccache::core::config::crash_dump_dir();
    std::fs::create_dir_all(&crash_dir).ok()?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let filename = format!("crash-{timestamp}-zccache-daemon-panic.txt");
    let path = crash_dir.join(&filename);
    let content = format!(
        "zccache zccache-daemon crash report (panic)\n\
         ===========================================\n\
         Version: {version}\n\
         Binary:  zccache-daemon\n\
         OS:      {os}\n\
         Arch:    {arch}\n\
         PID:     {pid}\n\
         Time:    {timestamp}\n\
         \n\
         Panic:\n\
         {panic_info}\n\
         \n\
         Backtrace:\n\
         {backtrace}\n",
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        pid = std::process::id(),
    );
    std::fs::write(&path, content).ok()?;
    Some(path)
}

// The behaviour of this module is covered end-to-end by the
// `crash_minidump_test` integration test, which spawns the real
// `crash-trigger` binary with `ZCCACHE_CACHE_DIR` pointing at a
// tempdir. Manipulating the env var from a unit test inside the
// daemon's test binary would race with any other test that calls
// `default_cache_dir()` since cargo runs unit tests multi-threaded.
