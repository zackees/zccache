# persist-rust-bench

Tiny Rust crate with a handful of "dummy-linked" deps (serde/regex/url/...) used
as an end-to-end workload for the persist-path improvements landed for
issue #274. Compiling it cold produces ~150–250 `.rmeta`/`.rlib`/`.d` files
totalling a few hundred MB, which is the realistic shape that exercises the
daemon's persist semaphore and background index writer.

Microbench-style measurement of the *daemon's* persist path lives in
`crates/zccache-daemon/tests/persist_pool_bench.rs` (run with `soldr cargo
test --release -p zccache-daemon --test persist_pool_bench -- --nocapture
--ignored`). This crate is the end-to-end shape: invoke `soldr cargo build`
in this directory after clearing `~/.zccache/artifacts/` to measure the
full pipeline.
