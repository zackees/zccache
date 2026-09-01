# ban_registered_env_read

Requires the registered zccache-owned boolean switches to be read through
their typed accessors in `zccache_core::config`. The policy registry owns the
allowlist grammar (`1` or `true`; unknown values are false), so direct
`std::env::var` / `var_os` reads would allow parser and lookup policy to drift.

The lint deliberately covers only the names in this coherent registry. It does
not rewrite the workspace's unrelated environment access, foreign-variable
denylist handling, strict parsers whose unknown values are errors, or
presence-only diagnostics.

`crates/zccache-core/src/config/env_policy.rs` is the sole lookup owner.
