//! Test-fixture binary used by `tests/crash_minidump_test.rs`.
//!
//! Installs the shared crash handlers (panic + signal/exception via
//! `zccache-core::crash::install`) under the binary stem
//! `zccache-daemon` so the resulting dump filename matches what the
//! production daemon would produce, then deliberately triggers a
//! specific kind of crash so the test runner can assert a dump
//! appeared on disk.
//!
//! Invocation: `crash-trigger <mode>` where `<mode>` is one of
//! `panic`, `sigsegv`, `sigabrt`, `stack-overflow`, `illegal-instruction`.

fn main() {
    // Single install call covers both layers (panic hook + native
    // signal/SEH handler). Guard must outlive the deliberate crash.
    let _guard = zccache::core::crash::install("zccache-daemon");

    let mode = std::env::args().nth(1).unwrap_or_default();
    match mode.as_str() {
        "panic" => panic!("intentional test panic from crash-trigger"),
        "sigsegv" => unsafe { sadness_generator::raise_segfault() },
        "sigabrt" => unsafe { sadness_generator::raise_abort() },
        "stack-overflow" => unsafe { sadness_generator::raise_stack_overflow() },
        "illegal-instruction" => unsafe { sadness_generator::raise_illegal_instruction() },
        other => {
            eprintln!(
                "crash-trigger: unknown mode '{other}' (expected panic|sigsegv|sigabrt|stack-overflow|illegal-instruction)"
            );
            std::process::exit(2);
        }
    }
}
