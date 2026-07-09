## context/

Compile-context types and cache-key computation. `mod.rs` exposes the public
API (`ContextKey`, `ArtifactKey`, `CompileContext`, `RustcCompileContext`,
`compute_context_key`, `compute_artifact_key`, `compute_rustc_artifact_key`,
`compute_rustc_artifact_key_with_root`) along with the
`VOLATILE_CARGO_ENV_VARS` allow-list that pins which `CARGO_*` env vars must
not contribute to cache identity. `tests/` (cfg(test)-only) splits per surface
— `cc` for C/C++, `rustc` for rustc.
