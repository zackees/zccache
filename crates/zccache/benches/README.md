# Benchmarks

Criterion benchmarks for the `zccache` crate, covering hashing, filesystem
scanning, fingerprinting, payload read/write, warm restore, and generic tool
exec.

Run them all with:

```bash
soldr cargo bench -p zccache
```

or one at a time, e.g. `soldr cargo bench -p zccache --bench hashing`.

Targets: `hashing`, `scan_metadata`, `scan_recursive`, `scan_includes`,
`miss_overhead`, `fingerprint`, `write_payloads`, `read_outputs`,
`persist_payloads`, `warm_restore`, `exec`.
Each is declared `harness = false` in `crates/zccache/Cargo.toml`.

`scan_includes` measures `scan_includes_str` throughput (bytes/s) over an
in-memory corpus of ~60 avr-libc / ArduinoCore-style headers; the acceptance
target for the single-pass scanner (zackees/zccache#1670) is >=5x over the
pre-change scanner (`-- --save-baseline pre`, then `-- --baseline pre`).
`miss_overhead` measures per-TU cache-miss overhead excluding the compiler:
recursive include scan, blake3 hashing of every discovered file, and the
staged object + index persist, reported as `miss_overhead/scan`, `/hash`,
`/store_persist` and `/total`.
