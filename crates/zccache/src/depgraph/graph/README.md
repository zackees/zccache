# graph/

Core dependency graph: two DashMap-backed indices (`files`, `contexts`) plus
read/write paths for cache verdicts (`check`, `check_diagnostic`,
`try_fast_hit`, `update`).

- `mod.rs` — public surface + the verdict pipeline.
- `tests.rs` — `#[cfg(test)]` unit tests, kept out of `mod.rs` so the main
  file stays under the LOC guard.
