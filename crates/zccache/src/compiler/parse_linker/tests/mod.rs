//! Unit tests for the linker parser, split by domain.

mod compiler_driver;
mod detection;
mod gnu_ld;
mod implib;
mod is_link;
mod msvc_link;

/// Shared helper used by every test module to build a `Vec<String>` from a
/// list of `&str` literals.
fn args(s: &[&str]) -> Vec<String> {
    s.iter().map(|x| (*x).to_string()).collect()
}
