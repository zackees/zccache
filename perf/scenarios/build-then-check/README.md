scenario that runs a cold `cargo build --release` and then an immediate
`cargo check --release` over an unchanged source tree. red here means the
check pass cannot reuse the metadata the build pass already cached: zccache
keys on rustc's `--emit`, so `check` and `build` land on different keys and
check misses even though the metadata it needs is already present
(soldr#758).
