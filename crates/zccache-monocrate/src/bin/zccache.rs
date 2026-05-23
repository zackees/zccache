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

#[cfg(unix)]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(windows)]
#[global_allocator]
static GLOBAL_WIN: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::process::ExitCode;

fn main() -> ExitCode {
    // Crash coverage first thing: panic hook + native signal/SEH
    // handler so a fault inside arg parsing or symbol install still
    // leaves a dump under `~/.zccache/crashes/`. Guard stays alive
    // until main returns. See issue #313.
    let _crash_guard = zccache_monocrate::core::crash::install("zccache");
    zccache_monocrate::core::crash::note_previous_crashes();

    zccache_monocrate::cli::commands::run()
}
