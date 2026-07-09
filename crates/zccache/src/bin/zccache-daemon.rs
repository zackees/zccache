//! zccache daemon process.
//!
//! Thin shim (issue #997): the daemon `main` lives in the library at
//! [`zccache::daemon::entry`] so it can be invoked both here and from the
//! `zccache` binary's argv[0] multi-call dispatch (issue #998). This file
//! only installs the global allocator (which must live in the final binary,
//! not the library) and delegates.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    zccache::daemon::entry::run();
}
