## context/

Compile-context types and cache-key computation. `mod.rs` exposes the public
API (`ContextKey`, `ArtifactKey`, `CompileContext`, `RustcCompileContext`, and
context-key helpers). `artifact_keys.rs` owns generic C/C++ artifact identity;
`rustc_keys.rs` owns rustc artifact, verdict, and env-dependency identity. The
module also carries the
`VOLATILE_CARGO_ENV_VARS` allow-list that pins which `CARGO_*` env vars must
not contribute to cache identity. `tests/` (cfg(test)-only) splits per surface
— `cc` for C/C++, `rustc` for rustc.
