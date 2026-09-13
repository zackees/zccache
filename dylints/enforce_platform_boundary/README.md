# enforce_platform_boundary

This Dylint enforces zccache#1365: host-platform selection and native OS APIs
may only appear in the narrow zccache product adapters around kernal-api.

Allowed locations:

- `crates/zccache-ipc/src/platform.rs`: endpoint spelling, peer admission, and
  protected socket/pipe adaptation.
- `crates/zccache-daemon-core/src/platform.rs`: daemon exit/materialization
  policy around canonical process and filesystem operations.
- `crates/zccache-cli-core/src/platform.rs`: CLI stack and presentation policy.

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
