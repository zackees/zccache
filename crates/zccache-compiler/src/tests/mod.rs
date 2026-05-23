//! Per-topic test modules for `zccache-compiler`.
//!
//! Split out of `lib.rs` so each focused file stays under 1,000 LOC and
//! groups by what it exercises (C/C++ parsing, rustc, clang-cl, etc.).
//! Tests can still touch `pub(crate)` helpers because they live inside
//! the crate as `#[cfg(test)] mod tests;`.

pub mod clang_cl;
pub mod clippy_driver;
pub mod cpp_output;
pub mod cpp_parse;
pub mod detect;
pub mod modules;
pub mod rustc;

/// Shared helper: lift a `&[&str]` literal into the `&[String]` that
/// `parse_invocation` expects.
pub(crate) fn args(s: &[&str]) -> Vec<String> {
    s.iter().map(|x| x.to_string()).collect()
}
