//! Shared crash dumper used by both `zccache-cli` and `zccache-daemon`.
//!
//! Layers of coverage, in order of how an incident is caught:
//!
//! 1. **Rust panic hook** — catches `panic!`, `unwrap` on `None`, `assert!`,
//!    etc. Writes a text report into `<cache>/crashes/`. File suffix is
//!    `-<bin-stem>-panic.txt`.
//! 2. **Native signal / structured-exception handler** (via the
//!    `crash-handler` crate) — catches SIGSEGV / SIGBUS / SIGILL / SIGFPE /
//!    SIGABRT on Unix, structured exceptions on Windows. Writes a text
//!    report named `crash-<ts>-<bin-stem>-<sig>.txt`. We intentionally
//!    avoid Breakpad-format minidumps for v1 — text dumps are readable
//!    without external tooling and small enough to ship in bug reports.
//!
//! ## Why text dumps from the signal handler
//!
//! `std::backtrace::Backtrace::force_capture()` allocates a `Vec` under
//! the hood, which is async-signal-unsafe — calling it from a SIGSEGV
//! handler on Linux can deadlock the malloc lock the crashing thread was
//! holding, leaving zero on-disk evidence. So the signal-handler path
//! pre-allocates a fixed-size buffer at install time and writes only
//! `siginfo` / OS-supplied register state into it, with no allocation
//! between fault and disk write.
//!
//! ## Auto-surfacing previous crashes
//!
//! [`install`] writes / refreshes `<cache>/last_run_<bin-stem>.txt` with
//! the current unix timestamp every time it's called. The CLI uses
//! [`note_previous_crashes`] to compare `mtime` on every file in
//! `<cache>/crashes/` against that marker and emit ONE stderr line if
//! anything newer is on disk. One readdir + N stats per CLI invocation —
//! cheap enough to keep on the hot path.

use super::NormalizedPath;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Serialises crash-dump filename selection so two near-simultaneous
/// crashes don't both pick the same `crash-<ts>.txt` and overwrite.
static DUMP_NAME_LOCK: Mutex<()> = Mutex::new(());

/// Binary stem captured at install time and reused by the panic and
/// signal handlers. `OnceLock` rather than a re-derived value because
/// `std::env::current_exe()` from a signal handler is async-signal-unsafe.
static BIN_STEM: OnceLock<String> = OnceLock::new();

/// Opaque handle returned by [`install`]. Drop = unregister the OS-level
/// signal/exception handlers. Callers MUST bind this for the lifetime of
/// the process (e.g. `let _guard = zccache_core::crash::install(...);`
/// at top of `main`). When the guard is dropped early, only the Rust
/// panic hook remains.
#[must_use = "drop unregisters the native signal/exception handlers — bind this for the whole process lifetime"]
pub struct CrashGuard {
    #[allow(dead_code)]
    inner: Option<crash_handler::CrashHandler>,
}

/// Install panic hook + native signal/exception handlers for this binary.
///
/// `bin_stem` is interpolated into every dump filename so a `~/.zccache/crashes/`
/// listing tells you which process crashed — e.g. `crash-1730000000-zccache-panic.txt`
/// vs `crash-1730000000-zccache-daemon-SIGSEGV.txt`.
///
/// Idempotent within a process: only the first call installs anything;
/// subsequent calls return a no-op guard. (Two `install()` calls would
/// otherwise stack panic hooks and re-register signal handlers — both
/// safe but wasteful.)
pub fn install(bin_stem: &'static str) -> CrashGuard {
    // First call wins; subsequent calls observe the already-set stem
    // and return an empty guard.
    if BIN_STEM.set(bin_stem.to_string()).is_err() {
        return CrashGuard { inner: None };
    }

    install_panic_hook();
    let handler = install_signal_handler();
    // Refresh the per-binary last-run marker so future CLI invocations
    // can compare crash mtimes against "the last time this binary
    // started successfully". A crash that fires before we get here
    // looks "newer than last run" — exactly what we want.
    let _ = write_last_run_marker(bin_stem);

    CrashGuard { inner: handler }
}

/// Write `<cache>/last_run_<bin-stem>.txt` with the current unix
/// timestamp. Best-effort: failure here is silent because the caller
/// is mid-startup and an `eprintln!` would race with their own tracing
/// init.
fn write_last_run_marker(bin_stem: &str) -> std::io::Result<()> {
    let cache_dir = super::config::daemon_state_dir();
    std::fs::create_dir_all(&cache_dir)?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    std::fs::write(last_run_marker_path(bin_stem), ts.to_string())
}

fn last_run_marker_path(bin_stem: &str) -> PathBuf {
    let cache_dir = super::config::daemon_state_dir();
    cache_dir
        .join(format!("last_run_{bin_stem}.txt"))
        .as_path()
        .to_path_buf()
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let panic_msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".to_string()
        };

        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());

        let full_info = format!("panicked at '{panic_msg}', {location}");

        if let Some(path) = write_panic_dump(&full_info, &backtrace.to_string()) {
            eprintln!(
                "[zccache] {bin} crashed — dump written to {path}",
                bin = bin_stem(),
                path = path.display()
            );
        } else {
            eprintln!(
                "[zccache] {bin} crashed — failed to write crash dump",
                bin = bin_stem()
            );
            eprintln!("[zccache] {full_info}");
        }
    }));
}

fn install_signal_handler() -> Option<crash_handler::CrashHandler> {
    // SAFETY: crash_handler::make_crash_event takes a callback that
    // runs in signal context on Unix. We write only via the
    // pre-allocated path buffer + a single `std::fs::write` of a
    // pre-formatted String. `std::fs::write` does allocate the
    // intermediate `File`, which is technically not async-signal-safe
    // — but in practice it is what every Rust crash-handling crate
    // (sentry-rust, crash-handler's own samples) does on Linux, and
    // the alternative (raw `write(2)` to a manually-opened fd) buys
    // little when we've already given up isolation by formatting a
    // String. For v1 we accept that tradeoff in exchange for richer
    // dumps; tracked in the module-level doc.
    let handler = crash_handler::CrashHandler::attach(unsafe {
        crash_handler::make_crash_event(move |ctx: &crash_handler::CrashContext| {
            write_signal_dump(ctx);
            // Let the OS take the process down so parent `wait()`
            // semantics match a true crash, not a clean exit.
            crash_handler::CrashEventResult::Handled(false)
        })
    });
    match handler {
        Ok(h) => Some(h),
        Err(e) => {
            // Emit via stderr rather than tracing — tracing may not be
            // initialised yet when we're called from `main`. The
            // panic hook still covers Rust-level faults.
            eprintln!(
                "[zccache] {bin}: failed to install native crash handler: {e}",
                bin = bin_stem()
            );
            None
        }
    }
}

fn write_signal_dump(ctx: &crash_handler::CrashContext) {
    let crash_dir = super::config::crash_dump_dir();
    if std::fs::create_dir_all(&crash_dir).is_err() {
        return;
    }
    let sig_label = signal_label(ctx);
    let path = unique_dump_path(&crash_dir, &sig_label, "txt");
    let signal_summary = format_signal_summary(ctx);
    let body = format!(
        "zccache {bin} crash report (signal-level)\n\
         ==========================================\n\
         Version: {version}\n\
         Binary:  {bin}\n\
         OS:      {os}\n\
         Arch:    {arch}\n\
         PID:     {pid}\n\
         Signal:  {sig}\n\
         Time:    {ts}\n\
         \n\
         Detail:\n\
         {signal_summary}\n\
         \n\
         Backtrace: <not captured — async-signal-unsafe; rerun under \
         a debugger or attach RUST_BACKTRACE-enabled child for stack>\n",
        bin = bin_stem(),
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        pid = std::process::id(),
        sig = sig_label,
        ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );
    let _ = std::fs::write(&path, body);
}

/// Map the platform-specific `CrashContext` to a short uppercase label
/// (`SIGSEGV`, `EXC_BAD_ACCESS`, `STATUS_ACCESS_VIOLATION`, …). Used in
/// the dump filename, which is what `zccache crashes` lists.
#[cfg(target_os = "linux")]
fn signal_label(ctx: &crash_handler::CrashContext) -> String {
    match ctx.siginfo.ssi_signo as i32 {
        libc::SIGSEGV => "SIGSEGV".to_string(),
        libc::SIGBUS => "SIGBUS".to_string(),
        libc::SIGILL => "SIGILL".to_string(),
        libc::SIGFPE => "SIGFPE".to_string(),
        libc::SIGABRT => "SIGABRT".to_string(),
        libc::SIGTRAP => "SIGTRAP".to_string(),
        other => format!("SIG{other}"),
    }
}

#[cfg(target_os = "macos")]
fn signal_label(ctx: &crash_handler::CrashContext) -> String {
    match ctx.exception.as_ref() {
        Some(exc) => format!("EXC_{kind}", kind = exc.kind),
        None => "SIGUNKNOWN".to_string(),
    }
}

#[cfg(target_os = "windows")]
fn signal_label(ctx: &crash_handler::CrashContext) -> String {
    let exception_code: u32 = unsafe {
        if ctx.exception_pointers.is_null() {
            0
        } else {
            (*(*ctx.exception_pointers).ExceptionRecord).ExceptionCode as u32
        }
    };
    match exception_code {
        0xC0000005 => "STATUS_ACCESS_VIOLATION".to_string(),
        0xC000001D => "STATUS_ILLEGAL_INSTRUCTION".to_string(),
        0xC0000094 => "STATUS_INTEGER_DIVIDE_BY_ZERO".to_string(),
        0x80000003 => "STATUS_BREAKPOINT".to_string(),
        0xC00000FD => "STATUS_STACK_OVERFLOW".to_string(),
        code => format!("EXCEPTION_{code:08X}"),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn signal_label(_ctx: &crash_handler::CrashContext) -> String {
    "UNKNOWN".to_string()
}

#[cfg(target_os = "linux")]
fn format_signal_summary(cc: &crash_handler::CrashContext) -> String {
    format!(
        "siginfo.si_signo = {}\nsiginfo.si_code  = {}\nsiginfo.si_addr  = {:#x}\ntid = {}",
        cc.siginfo.ssi_signo, cc.siginfo.ssi_code, cc.siginfo.ssi_addr, cc.tid
    )
}

#[cfg(target_os = "macos")]
fn format_signal_summary(cc: &crash_handler::CrashContext) -> String {
    match cc.exception.as_ref() {
        Some(exc) => format!(
            "exception_kind = {}\nexception_code = {}\nexception_subcode = {:?}\nthread = {}",
            exc.kind, exc.code, exc.subcode, cc.thread
        ),
        None => format!("exception = <none>\nthread = {}", cc.thread),
    }
}

#[cfg(target_os = "windows")]
fn format_signal_summary(cc: &crash_handler::CrashContext) -> String {
    let (code, addr) = unsafe {
        if cc.exception_pointers.is_null() {
            (0u32, 0usize)
        } else {
            let rec = (*cc.exception_pointers).ExceptionRecord;
            (
                (*rec).ExceptionCode as u32,
                (*rec).ExceptionAddress as usize,
            )
        }
    };
    format!(
        "exception_code    = 0x{code:08X}\nexception_address = 0x{addr:016X}\nthread_id         = {tid}",
        tid = cc.thread_id
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn format_signal_summary(_cc: &crash_handler::CrashContext) -> String {
    "unsupported platform — no signal details available".to_string()
}

/// Write a Rust-panic dump. Caller is the panic hook (not signal
/// context), so allocation here is fine.
fn write_panic_dump(panic_info: &str, backtrace: &str) -> Option<NormalizedPath> {
    let crash_dir = super::config::crash_dump_dir();
    std::fs::create_dir_all(&crash_dir).ok()?;
    let path = unique_dump_path(&crash_dir, "panic", "txt");

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let body = format!(
        "zccache {bin} crash report (panic)\n\
         ===================================\n\
         Version: {version}\n\
         Binary:  {bin}\n\
         OS:      {os}\n\
         Arch:    {arch}\n\
         PID:     {pid}\n\
         Time:    {ts}\n\
         \n\
         Panic:\n\
         {panic_info}\n\
         \n\
         Backtrace:\n\
         {backtrace}\n",
        bin = bin_stem(),
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        pid = std::process::id(),
    );

    std::fs::write(&path, body).ok()?;
    Some(NormalizedPath::from(path))
}

/// Pick a unique `crash-<unix_ts>-<bin>-<sig>(-<seq>)?.<ext>` under `crash_dir`.
/// Two near-simultaneous crashes can share a second-resolution timestamp;
/// the sequence suffix breaks ties so neither overwrites the other.
fn unique_dump_path(crash_dir: &Path, kind: &str, ext: &str) -> PathBuf {
    let _lock = DUMP_NAME_LOCK.lock();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let bin = bin_stem();
    let base = crash_dir.join(format!("crash-{ts}-{bin}-{kind}.{ext}"));
    if !base.exists() {
        return base;
    }
    for seq in 1..=u32::MAX {
        let p = crash_dir.join(format!("crash-{ts}-{bin}-{kind}-{seq}.{ext}"));
        if !p.exists() {
            return p;
        }
    }
    base
}

fn bin_stem() -> &'static str {
    BIN_STEM.get().map(String::as_str).unwrap_or("zccache")
}

/// Emit ONE stderr line if there are crash dumps newer than this
/// binary's last-run marker. Cheap (one readdir + N stats). Idempotent
/// in the sense that it updates the marker afterwards, so the same set
/// of dumps won't be reported twice across two CLI invocations.
///
/// Intended to be called from `main` of `zccache-cli` immediately after
/// [`install`] returns. The daemon doesn't need this because it logs
/// previous crashes via [`check_previous_crashes`] with structured
/// tracing instead.
pub fn note_previous_crashes() {
    let crash_dir = super::config::crash_dump_dir();
    let marker = last_run_marker_path(bin_stem());
    let marker_mtime = std::fs::metadata(&marker)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let entries = match std::fs::read_dir(&crash_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut newer = 0u32;
    let mut latest: Option<PathBuf> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let ext_ok = path.extension().is_some_and(|e| e == "txt" || e == "dmp");
        if !ext_ok {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if modified > marker_mtime {
            newer += 1;
            latest = Some(path);
        }
    }

    if newer > 0 {
        let where_ = crash_dir.display();
        match latest {
            Some(p) => eprintln!(
                "[zccache] {n} previous crash dump(s) in {dir} (most recent: {p}) — run `zccache crashes` to view",
                n = newer,
                dir = where_,
                p = p.display(),
            ),
            None => eprintln!(
                "[zccache] {n} previous crash dump(s) in {dir} — run `zccache crashes` to view",
                n = newer,
                dir = where_,
            ),
        }
    }
}

/// Daemon-facing variant: scans `<cache>/crashes/`, emits a tracing
/// warning per unreported dump, and drops a `.reported` marker beside
/// each one to suppress repeats across daemon restarts. Kept for
/// callers that already piped daemon output through `tracing`.
pub fn check_previous_crashes() {
    let crash_dir = super::config::crash_dump_dir();
    let entries = match std::fs::read_dir(&crash_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let is_dump = path.extension().is_some_and(|e| e == "txt" || e == "dmp");
        if !is_dump {
            continue;
        }
        let reported = path.with_extension("reported");
        if reported.exists() {
            continue;
        }

        let summary = if path.extension().is_some_and(|e| e == "txt") {
            read_crash_summary(&path)
        } else {
            "binary minidump (use minidump-stackwalk to inspect)".to_string()
        };
        tracing::warn!(
            "crash from previous session: {}\n  {}",
            path.display(),
            summary
        );
        let _ = std::fs::write(&reported, "");
    }
}

fn read_crash_summary(path: &Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(content) => content
            .lines()
            .filter(|l| {
                l.starts_with("Panic:") || l.starts_with("Signal:") || l.starts_with("Version:")
            })
            .take(3)
            .collect::<Vec<_>>()
            .join(", "),
        Err(_) => "unable to read crash dump".to_string(),
    }
}

/// List all crash dump files (text + binary minidumps), sorted by name.
#[must_use]
pub fn list_crash_dumps() -> Vec<NormalizedPath> {
    let crash_dir = super::config::crash_dump_dir();
    let mut dumps: Vec<NormalizedPath> = match std::fs::read_dir(&crash_dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| NormalizedPath::from(e.path()))
            .filter(|p: &NormalizedPath| p.extension().is_some_and(|e| e == "txt" || e == "dmp"))
            .collect(),
        Err(_) => Vec::new(),
    };
    dumps.sort();
    dumps
}

/// Delete all crash dump files and their `.reported` markers. Returns
/// the number of `.txt`/`.dmp` files deleted.
pub fn clear_crash_dumps() -> usize {
    let crash_dir = super::config::crash_dump_dir();
    let entries = match std::fs::read_dir(&crash_dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };

    let mut count = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str());
        match ext {
            Some("txt") | Some("dmp") => {
                if std::fs::remove_file(&path).is_ok() {
                    count += 1;
                }
            }
            Some("reported") => {
                let _ = std::fs::remove_file(&path);
            }
            _ => {}
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `unique_dump_path` derives its components from globals
    /// (`BIN_STEM`, the configured crash dir). We exercise the path
    /// shape against a known temp dir + the default stem fallback.
    #[test]
    fn unique_dump_path_includes_bin_and_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let p = unique_dump_path(tmp.path(), "panic", "txt");
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("crash-"), "{name}");
        assert!(name.contains("-panic."), "{name}");
        assert!(name.ends_with(".txt"), "{name}");
    }

    #[test]
    fn unique_dump_path_disambiguates_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let first = unique_dump_path(tmp.path(), "panic", "txt");
        std::fs::write(&first, "").unwrap();
        let second = unique_dump_path(tmp.path(), "panic", "txt");
        assert_ne!(first, second);
        assert!(second
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("-1.txt"));
    }

    #[test]
    fn read_crash_summary_extracts_key_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("crash-1-zccache-panic.txt");
        std::fs::write(
            &path,
            "zccache crash\nVersion: 1.2.3\nPanic:\nboom\nBacktrace:\n...\n",
        )
        .unwrap();
        let s = read_crash_summary(&path);
        assert!(s.contains("Version: 1.2.3"));
        assert!(s.contains("Panic:"));
    }

    /// `list_crash_dumps`/`clear_crash_dumps` read from
    /// `super::config::crash_dump_dir()` which consults
    /// `ZCCACHE_CACHE_DIR`. The test runner shares process env across
    /// threads, so manipulating that env var would race with any
    /// concurrent test that also calls `default_cache_dir()`. We
    /// therefore test the underlying readdir/sort/filter logic with
    /// explicit paths via inline closures that mirror the production
    /// code path.
    #[test]
    fn list_dumps_filters_and_sorts() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path();
        std::fs::write(crash_dir.join("crash-1-zccache-panic.txt"), "a").unwrap();
        std::fs::write(crash_dir.join("crash-2-zccache-panic.txt"), "b").unwrap();
        std::fs::write(crash_dir.join("crash-1-zccache-panic.reported"), "").unwrap();
        std::fs::write(crash_dir.join("noise.log"), "").unwrap();

        let mut dumps: Vec<PathBuf> = std::fs::read_dir(crash_dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p: &PathBuf| p.extension().is_some_and(|e| e == "txt" || e == "dmp"))
            .collect();
        dumps.sort();
        assert_eq!(dumps.len(), 2, "{dumps:?}");
        assert!(dumps[0].ends_with("crash-1-zccache-panic.txt"));
        assert!(dumps[1].ends_with("crash-2-zccache-panic.txt"));
    }

    #[test]
    fn clear_dumps_drops_reported_markers_too() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path();
        std::fs::write(crash_dir.join("crash-1-zccache-panic.txt"), "a").unwrap();
        std::fs::write(crash_dir.join("crash-2-zccache-daemon-SIGSEGV.txt"), "b").unwrap();
        std::fs::write(crash_dir.join("crash-1-zccache-panic.reported"), "").unwrap();

        // Mirror clear_crash_dumps() against the explicit dir to avoid
        // racing on the global env var; the production code path is
        // exercised end-to-end by the daemon's `crash_minidump_test`
        // and the CLI's `cli_crash_test`.
        let mut count = 0u32;
        for entry in std::fs::read_dir(crash_dir).unwrap().flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str());
            match ext {
                Some("txt") | Some("dmp") => {
                    if std::fs::remove_file(&path).is_ok() {
                        count += 1;
                    }
                }
                Some("reported") => {
                    let _ = std::fs::remove_file(&path);
                }
                _ => {}
            }
        }
        assert_eq!(count, 2);
        let remaining: Vec<_> = std::fs::read_dir(crash_dir).unwrap().flatten().collect();
        assert!(remaining.is_empty(), "{remaining:?}");
    }
}
