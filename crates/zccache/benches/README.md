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
`write_payloads`, `read_outputs`, `persist_payloads`, `warm_restore`, `exec`.
Each is declared `harness = false` in `crates/zccache/Cargo.toml`.
