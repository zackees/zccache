//! Unit tests for `cli/` submodules. Originally a single 1.4K-LOC
//! `tests.rs`; split per domain so each file stays well under 1,000 LOC.
//! Each module owns whatever tempfile / fixture helpers its tests use.

mod analyze;
mod args_parsing;
mod cache_ops;
mod daemon;
mod exit_code;
mod session_warnings;
mod snapshot;
mod util;
mod warm_lockfile;
