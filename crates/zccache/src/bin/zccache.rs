//! zccache CLI -- command-line interface for the compiler cache.
//!
//! Usage modes:
//!
//! 1. Subcommand mode:
//!    zccache session-start --compiler /path/to/clang++
//!    zccache session-end `<id>`
//!    zccache status
//!
//! 2. Compiler wrapper mode (auto-detected):
//!    ZCCACHE_SESSION_ID=42 zccache clang++ -c foo.cpp -o foo.o
//!
//!    If the first arg isn't a known subcommand, zccache treats
//!    the entire command line as a compiler invocation and forwards
//!    it to the daemon via the session from ZCCACHE_SESSION_ID.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::process::ExitCode;

#[cfg(windows)]
fn main() -> ExitCode {
    match std::thread::Builder::new()
        .name("zccache-cli".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(run_main)
    {
        Ok(handle) => match handle.join() {
            Ok(code) => code,
            Err(_) => ExitCode::FAILURE,
        },
        Err(err) => {
            eprintln!("zccache: failed to start CLI thread: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() -> ExitCode {
    run_main()
}

fn run_main() -> ExitCode {
    // #998: argv[0] multi-call dispatch. When this binary is invoked under the
    // daemon's name (the CLI self-copies to `…/v<VERSION>/zccache-daemon[.exe]`
    // and runs it — #999), run the daemon and exit. Any other/unknown argv[0]
    // falls through to the standard CLI below. The daemon installs its own
    // crash guard, so this runs before the CLI's.
    if zccache::cli::multicall::invoked_as_daemon() {
        zccache::daemon::entry::run();
        return ExitCode::SUCCESS;
    }

    // Crash coverage first thing: panic hook + native signal/SEH
    // handler so a fault inside arg parsing or symbol install still
    // leaves a dump under `~/.zccache/crashes/`. Guard stays alive
    // until main returns. See issue #313.
    let _crash_guard = zccache::core::crash::install("zccache");
    zccache::core::crash::note_previous_crashes();

    zccache::cli::commands::run()
}
