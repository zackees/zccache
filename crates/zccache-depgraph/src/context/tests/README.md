## context/tests/

`#[cfg(test)]`-only tests for the `context/` module, split per surface:

- `cc.rs` — `CompileContext`, `compute_context_key`, `compute_artifact_key`
  (C/C++).
- `rustc.rs` — `RustcCompileContext`, `compute_rustc_artifact_key`,
  `CARGO_*` env-var filter regressions (issues #139 / #396).
- `mod.rs` — shared test helpers (`make_context`, `make_rustc_context`,
  `make_rustc_context_with_env`).
