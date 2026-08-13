# enforce_platform_boundary

This Dylint enforces zccache#1365: host-platform selection and native OS APIs
may only appear inside the `zccache-platform` leaf crate.

Allowed locations:

- `crates/zccache-platform/src/lib.rs`: exactly one `cfg_select!` host selector.
- `platform_win`, `platform_linux`, and `platform_macos` concrete trees: host
  cfg and native APIs.
- Neutral facade files may bridge through private `crate::platform_imp`.

Every other production Rust source denies host cfg predicates, direct
`std::os::{windows,unix}` / `libc` / `windows_sys` paths, and references to
concrete platform modules. Tests, benches, vendored sources, Dylint fixtures,
and the dev-only test-support crate are outside the production boundary.

There is no baseline or allowlist. Every prohibited production occurrence is
an error.

## Running

```bash
uv run python -m ci.lint --dylint-only
soldr rustup run nightly-2026-05-26 cargo test --manifest-path dylints/enforce_platform_boundary/Cargo.toml
```
