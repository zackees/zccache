# Embedded mixed-language fixture

This dependency-free Cargo package drives explicit Rust, C, C++, and
Emscripten compilation through soldr. `EMBEDDED_LANGUAGE` selects the native
sources compiled by `build.rs`; each build records the compiler command so the
performance harness can prove that soldr injected `zccache-soldr`.
