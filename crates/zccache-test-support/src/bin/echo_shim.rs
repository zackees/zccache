//! `echo_shim` — deterministic stdio reflector for wrapper round-trip tests.
//!
//! Reads every byte from stdin until EOF, writes a marker to stdout, then
//! writes a different marker followed by the raw stdin bytes to stderr.
//! Exits with `argv[1]` parsed as i32 (default 0).
//!
//! Used by `crates/zccache-cli/tests/wrapper_passthrough.rs` to prove the
//! parent → CLI → IPC → daemon → child → daemon → IPC → CLI → parent
//! stdio path is byte-lossless on every supported OS.

use std::io::{Read, Write};

const STDOUT_MARKER: &[u8] = b"ZCCACHE_PASSTHROUGH_STDOUT_MARKER\n";
const STDERR_MARKER: &[u8] = b"ZCCACHE_PASSTHROUGH_STDERR_MARKER\n";

fn main() {
    let exit_code: i32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let mut stdin_bytes = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut stdin_bytes);

    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let _ = stdout.write_all(STDOUT_MARKER);
    let _ = stdout.flush();

    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    let _ = stderr.write_all(STDERR_MARKER);
    let _ = stderr.write_all(&stdin_bytes);
    let _ = stderr.flush();

    std::process::exit(exit_code);
}
