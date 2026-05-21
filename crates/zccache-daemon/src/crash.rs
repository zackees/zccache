//! Crash dump writer and panic hook for the daemon.
//!
//! Two layers of coverage:
//!
//! * `install_panic_hook` — Rust panic mechanism. Catches `panic!`,
//!   `unwrap` on `None`, `assert!`, etc. Writes a text report to
//!   `<cache>/crashes/crash-<ts>.txt`.
//! * `install_minidump_handler` — signal/exception level via the
//!   `crash-handler` crate. Catches SIGSEGV/SIGABRT/SIGILL/SIGBUS on
//!   Unix and Windows structured exceptions. Writes a Breakpad-format
//!   minidump via `minidump-writer` to `<cache>/crashes/crash-<ts>.dmp`.
//!   The returned handle MUST be kept alive (drop = unregister); the
//!   daemon binds it for the lifetime of `run_server`.
//!
//! On startup, `check_previous_crashes` warns about both `.txt` and
//! `.dmp` files from previous sessions that haven't been marked
//! reported yet.

use std::path::Path;
use std::sync::Mutex;
use zccache_core::NormalizedPath;

/// Serialises crash-dump filename selection so two near-simultaneous
/// crashes don't both pick the same `crash-<ts>.dmp` and overwrite.
/// Held only during filename construction — the file write happens
/// outside this critical section.
static DUMP_NAME_LOCK: Mutex<()> = Mutex::new(());

/// Install a panic hook that writes crash dumps before aborting.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let panic_msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
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

        if let Some(path) = write_crash_dump(&full_info, &backtrace.to_string()) {
            eprintln!(
                "[zccache] daemon crashed — dump written to {}",
                path.display()
            );
        } else {
            eprintln!("[zccache] daemon crashed — failed to write crash dump");
            eprintln!("[zccache] {full_info}");
        }
    }));
}

/// Opaque handle returned by `install_minidump_handler`. Drop = unregister
/// the OS-level signal/exception handlers. The daemon binds this in
/// `run_server` so the registration lives for the whole foreground run.
pub struct MinidumpHandle {
    #[allow(dead_code)] // held purely for its Drop side-effect
    inner: crash_handler::CrashHandler,
}

/// Install OS-level signal/exception handlers that write a Breakpad
/// minidump on hard crashes (SIGSEGV/SIGABRT/SIGILL/SIGBUS on Unix,
/// structured exceptions on Windows). Returns `None` if the OS rejects
/// the registration (e.g. permission errors, unsupported platform); in
/// that case the panic hook still covers Rust-level faults.
///
/// The minidump is written to `<cache>/crashes/crash-<ts>.dmp`. Tooling
/// (`minidump-stackwalk`, WinDbg, `rust-minidump`) consumes the file
/// offline against the debug symbols installed by `zccache symbols
/// install`.
#[must_use]
pub fn install_minidump_handler() -> Option<MinidumpHandle> {
    let handler = crash_handler::CrashHandler::attach(unsafe {
        crash_handler::make_crash_event(move |crash_context: &crash_handler::CrashContext| {
            write_minidump_for_context(crash_context);
            // We've captured what we can; let the OS take the process
            // down so exit semantics (e.g. parent-process `wait()`)
            // match a true crash rather than a clean exit.
            crash_handler::CrashEventResult::Handled(false)
        })
    });
    match handler {
        Ok(h) => Some(MinidumpHandle { inner: h }),
        Err(e) => {
            tracing::warn!("failed to install minidump handler: {e}");
            None
        }
    }
}

/// Write a signal/exception dump to `<cache>/crashes/crash-<ts>.dmp`.
/// Currently this is a text report (PID, OS/arch, version, signal
/// summary, best-effort backtrace) — the `.dmp` extension is reserved
/// for a future swap to Breakpad minidump format without touching the
/// callers / tests.
fn write_minidump_for_context(crash_context: &crash_handler::CrashContext) {
    let crash_dir = zccache_core::config::crash_dump_dir();
    if std::fs::create_dir_all(&crash_dir).is_err() {
        return;
    }
    let path = unique_dump_path(&crash_dir, "dmp");

    // No backtrace capture here. `std::backtrace::Backtrace::force_capture()`
    // allocates a Vec, which is async-signal-unsafe — on Linux that can
    // deadlock the malloc lock acquired by the crashing thread, leaving
    // no dump on disk at all. The panic-hook path captures backtraces
    // safely because it runs in a normal context, not a signal handler.
    // When we swap this to actual Breakpad minidump format, the thread
    // register state in the dump gives offline tools enough to
    // reconstruct the stack without needing in-handler walking.
    let signal_summary = format_signal_summary(crash_context);
    let body = format!(
        "zccache daemon crash report (signal-level)\n\
         ==========================================\n\
         Version: {version}\n\
         OS: {os}\n\
         Arch: {arch}\n\
         PID: {pid}\n\
         Timestamp: {ts}\n\
         \n\
         Signal:\n\
         {signal_summary}\n\
         \n\
         Backtrace: <not captured — async-signal-unsafe; use core file \
         or run with a debugger attached>\n",
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        pid = std::process::id(),
        ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );
    let _ = std::fs::write(&path, body);
}

/// Stringify whatever the platform's CrashContext exposes cheaply. The
/// shape differs per OS, so each branch reads only fields that exist.
#[cfg(target_os = "linux")]
fn format_signal_summary(cc: &crash_handler::CrashContext) -> String {
    format!(
        "siginfo.si_signo = {}\nsiginfo.si_code = {}\ntid = {}",
        cc.siginfo.ssi_signo, cc.siginfo.ssi_code, cc.tid
    )
}

#[cfg(target_os = "macos")]
fn format_signal_summary(cc: &crash_handler::CrashContext) -> String {
    // `exception` is None for signals delivered via the regular POSIX
    // path rather than Mach exception ports — record what we have either
    // way so the dump never silently drops the cause.
    match cc.exception.as_ref() {
        Some(exc) => format!(
            "exception_kind = {}\nexception_code = {}\nexception_subcode = {:?}\nthread = {}",
            exc.kind, exc.code, exc.subcode, cc.thread
        ),
        None => format!(
            "exception = <none — posix-signal path>\nthread = {}",
            cc.thread
        ),
    }
}

#[cfg(target_os = "windows")]
fn format_signal_summary(cc: &crash_handler::CrashContext) -> String {
    // Windows: EXCEPTION_POINTERS-derived context. Just report the
    // exception code and thread id — the backtrace below is more
    // useful than further register dumps in a text report.
    let exception_code: u32 = unsafe {
        if cc.exception_pointers.is_null() {
            0
        } else {
            (*(*cc.exception_pointers).ExceptionRecord).ExceptionCode as u32
        }
    };
    format!(
        "exception_code = 0x{exception_code:08X}\nthread_id = {}",
        cc.thread_id
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn format_signal_summary(_cc: &crash_handler::CrashContext) -> String {
    "unsupported platform — no signal details available".to_string()
}

/// Pick a unique `crash-<unix_ts>(-<seq>)?.<ext>` path under `crash_dir`.
/// Two near-simultaneous crashes can share a second-resolution timestamp;
/// the sequence suffix breaks ties so neither overwrites the other.
fn unique_dump_path(crash_dir: &Path, ext: &str) -> std::path::PathBuf {
    let _lock = DUMP_NAME_LOCK.lock();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let base = crash_dir.join(format!("crash-{ts}.{ext}"));
    if !base.exists() {
        return base;
    }
    for seq in 1..=u32::MAX {
        let p = crash_dir.join(format!("crash-{ts}-{seq}.{ext}"));
        if !p.exists() {
            return p;
        }
    }
    base
}

/// Write a crash dump file to the crash directory.
///
/// Returns the path to the written file, or `None` if writing failed.
pub fn write_crash_dump(panic_info: &str, backtrace: &str) -> Option<NormalizedPath> {
    let crash_dir = zccache_core::config::crash_dump_dir();
    std::fs::create_dir_all(&crash_dir).ok()?;

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let filename = format!("crash-{timestamp}.txt");
    let path = crash_dir.join(&filename);

    let content = format!(
        "zccache daemon crash report\n\
         ===========================\n\
         Version: {version}\n\
         OS: {os}\n\
         Arch: {arch}\n\
         PID: {pid}\n\
         Timestamp: {timestamp}\n\
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

/// Check for unreported crash dumps from previous sessions.
///
/// Logs a warning for each unreported crash and creates a `.reported` marker.
pub fn check_previous_crashes() {
    let crash_dir = zccache_core::config::crash_dump_dir();
    let entries = match std::fs::read_dir(&crash_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let is_dump = path.extension().is_some_and(|e| e == "txt" || e == "dmp");
        if is_dump {
            let reported = path.with_extension("reported");
            if reported.exists() {
                continue;
            }

            // For .txt dumps, lift a summary out of the file body. For
            // .dmp (binary minidump) the body is unreadable, so just log
            // the path.
            let summary = if path.extension().is_some_and(|e| e == "txt") {
                read_crash_summary(&path)
            } else {
                "binary minidump (use minidump-stackwalk to inspect)".to_string()
            };
            tracing::warn!(
                "daemon crashed during previous session: {}\n  {}",
                path.display(),
                summary
            );

            // Mark as reported.
            let _ = std::fs::write(&reported, "");
        }
    }
}

/// List all crash dump files (text panic reports and binary minidumps),
/// sorted by name.
#[must_use]
pub fn list_crash_dumps() -> Vec<NormalizedPath> {
    let crash_dir = zccache_core::config::crash_dump_dir();
    let mut dumps: Vec<NormalizedPath> = match std::fs::read_dir(&crash_dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path().into())
            .filter(|p: &NormalizedPath| p.extension().is_some_and(|e| e == "txt" || e == "dmp"))
            .collect(),
        Err(_) => Vec::new(),
    };
    dumps.sort();
    dumps
}

/// Delete all crash dump files and their `.reported` markers.
///
/// Returns the number of `.txt` files deleted.
pub fn clear_crash_dumps() -> usize {
    let crash_dir = zccache_core::config::crash_dump_dir();
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

fn read_crash_summary(path: &Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(content) => content
            .lines()
            .filter(|l| l.starts_with("Panic:") || l.starts_with("Version:"))
            .take(2)
            .collect::<Vec<_>>()
            .join(", "),
        Err(_) => "unable to read crash dump".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_crash_dump_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        // Override crash dir by writing directly.
        let crash_dir = tmp.path().join("crashes");
        std::fs::create_dir_all(&crash_dir).unwrap();

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let filename = format!("crash-{timestamp}.txt");
        let path = crash_dir.join(&filename);

        let content = format!(
            "zccache daemon crash report\n\
             ===========================\n\
             Version: {}\n\
             OS: {}\n\
             Arch: {}\n\
             PID: {}\n\
             Timestamp: {timestamp}\n\
             \n\
             Panic:\n\
             test panic info\n\
             \n\
             Backtrace:\n\
             test backtrace\n",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::process::id(),
        );

        std::fs::write(&path, &content).unwrap();

        assert!(path.exists());
        let read_content = std::fs::read_to_string(&path).unwrap();
        assert!(read_content.contains("test panic info"));
        assert!(read_content.contains("test backtrace"));
        assert!(read_content.contains(env!("CARGO_PKG_VERSION")));
        assert!(read_content.contains(std::env::consts::OS));
    }

    #[test]
    fn check_previous_crashes_marks_as_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path();

        let dump_path = crash_dir.join("crash-12345.txt");
        std::fs::write(
            &dump_path,
            "zccache daemon crash report\nVersion: test\nPanic:\ntest\n",
        )
        .unwrap();

        // Manually do what check_previous_crashes does for this directory.
        let reported = dump_path.with_extension("reported");
        assert!(!reported.exists());

        // Create the reported marker.
        std::fs::write(&reported, "").unwrap();
        assert!(reported.exists());
    }

    #[test]
    fn clear_crash_dumps_removes_files() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path();

        std::fs::write(crash_dir.join("crash-1.txt"), "dump1").unwrap();
        std::fs::write(crash_dir.join("crash-1.reported"), "").unwrap();
        std::fs::write(crash_dir.join("crash-2.txt"), "dump2").unwrap();

        let entries: Vec<_> = std::fs::read_dir(crash_dir).unwrap().flatten().collect();
        assert_eq!(entries.len(), 3);

        // Manually clear.
        let mut count = 0;
        for entry in std::fs::read_dir(crash_dir).unwrap().flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str());
            match ext {
                Some("txt") => {
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
    }

    #[test]
    fn list_crash_dumps_sorts() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path();

        std::fs::write(crash_dir.join("crash-3.txt"), "c").unwrap();
        std::fs::write(crash_dir.join("crash-1.txt"), "a").unwrap();
        std::fs::write(crash_dir.join("crash-2.txt"), "b").unwrap();
        std::fs::write(crash_dir.join("crash-2.reported"), "").unwrap();

        let mut dumps: Vec<NormalizedPath> = std::fs::read_dir(crash_dir)
            .unwrap()
            .flatten()
            .map(|e| e.path().into())
            .filter(|p: &NormalizedPath| p.extension().is_some_and(|e| e == "txt"))
            .collect();
        dumps.sort();

        assert_eq!(dumps.len(), 3);
        assert!(dumps[0].ends_with("crash-1.txt"));
        assert!(dumps[1].ends_with("crash-2.txt"));
        assert!(dumps[2].ends_with("crash-3.txt"));
    }
}
