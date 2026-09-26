# Benchmarks

Criterion benchmarks for the `zccache` crate, covering hashing, filesystem
scanning, fingerprinting, payload read/write, warm restore, and generic tool
exec.

Run them all with:

```bash
soldr cargo bench -p zccache
```

or one at a time, e.g. `soldr cargo bench -p zccache --bench hashing`.

Targets: `hashing`, `scan_metadata`, `scan_recursive`, `fingerprint`,
`write_payloads`, `read_outputs`, `persist_payloads`, `warm_restore`, `exec`,
`miss_overhead`.
Each is declared `harness = false` in `crates/zccache/Cargo.toml`.

`miss_overhead` (zccache#1670) reports per-TU cache-miss overhead: request
wall time minus the compiler child's run time, for both the include-scan and
depfile dependency paths. It needs `clang++` and shares its fixture with the
`miss_overhead_budget` CI test in `zccache-daemon-core`.
