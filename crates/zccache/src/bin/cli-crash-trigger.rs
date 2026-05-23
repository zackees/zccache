//! Test-fixture binary used by `tests/cli_crash_test.rs`.
//!
//! Installs the shared crash coverage under the binary stem `zccache`
//! (matching the production CLI) and then deliberately faults so the
//! integration test can assert the resulting dump filename includes
//! both `zccache` and the kind label.

fn main() {
    let _guard = zccache::core::crash::install("zccache");
    let mode = std::env::args().nth(1).unwrap_or_default();
    match mode.as_str() {
        "panic" => panic!("intentional test panic from cli-crash-trigger"),
        "sigsegv" => unsafe { sadness_generator::raise_segfault() },
        "sigabrt" => unsafe { sadness_generator::raise_abort() },
        other => {
            eprintln!("cli-crash-trigger: unknown mode '{other}' (expected panic|sigsegv|sigabrt)");
            std::process::exit(2);
        }
    }
}
