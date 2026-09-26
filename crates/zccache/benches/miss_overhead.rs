//! Criterion bench for per-TU compile cache MISS overhead, excluding the
//! compiler child process.
//!
//! On a miss, zccache does more than spawn the compiler: it recursively scans
//! the TU's `#include` graph, hashes every discovered file, and stages the
//! artifact + index into the cache store. This bench isolates those steps on a
//! tempdir fixture (a `.cpp` that pulls in a ~100-header quoted tree, same
//! builder shape as `scan_recursive.rs`):
//!
//! - `miss_overhead/scan` — `scan_recursive` on the TU.
//! - `miss_overhead/hash` — blake3-hash every discovered file
//!   (`zccache::hash::hash_bytes` on the file bytes, as in `hashing.rs`).
//! - `miss_overhead/store_persist` — staged write-then-rename of the object
//!   payload plus an index file, mirroring `persist_payloads.rs`.
//! - `miss_overhead/total` — all three in sequence.
//!
//! Run with: `soldr cargo bench -p zccache --bench miss_overhead`.
//! Compare against a pre-change commit with `-- --save-baseline pre` /
//! `-- --baseline pre`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use zccache::depgraph::scanner::{scan_recursive, ScanResult};
use zccache::depgraph::search_paths::IncludeSearchPaths;
use zccache::hash::ContentHash;

const OBJECT_SIZE: usize = 64 * 1024;

/// Build a flat tree of `#include "h_<id>.h"` headers (see
/// `scan_recursive.rs`). Returns the root header id and header count.
fn build_tree(root: &Path, depth: usize, fan_out: usize) -> (u32, usize) {
    std::fs::create_dir_all(root).unwrap();
    let mut counter: u32 = 0;

    fn rec(dir: &Path, depth: usize, fan_out: usize, counter: &mut u32) -> u32 {
        *counter += 1;
        let my_id = *counter;
        let my_path = dir.join(format!("h_{my_id:05}.h"));
        let filler = (0..30)
            .map(|j| format!("static int filler_{my_id}_{j} = {j};\n"))
            .collect::<String>();
        let mut content = String::new();
        if depth > 0 {
            for _ in 0..fan_out {
                let child_id = rec(dir, depth - 1, fan_out, counter);
                content.push_str(&format!("#include \"h_{child_id:05}.h\"\n"));
            }
        }
        content.push_str(&filler);
        std::fs::write(&my_path, content).unwrap();
        my_id
    }

    let root_id = rec(root, depth, fan_out, &mut counter);
    (root_id, counter as usize)
}

struct Fixture {
    _tmp: tempfile::TempDir,
    tu: PathBuf,
    search: IncludeSearchPaths,
    store_dir: PathBuf,
    object: Vec<u8>,
    header_count: usize,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("src");
        // depth=3, fan_out=4 -> 1+4+16+64 = 85 headers; plus a second
        // depth-2 root pulled in from the TU, ~100 headers total.
        let (root_a, n_a) = build_tree(&src, 3, 4);
        let tu = src.join("main.cpp");
        let mut body = format!("#include \"h_{root_a:05}.h\"\n");
        let sub = src.join("sub");
        let (root_b, n_b) = build_tree(&sub, 2, 4);
        let _ = writeln!(body, "#include \"sub/h_{root_b:05}.h\"");
        body.push_str("int main() { return 0; }\n");
        std::fs::write(&tu, body).unwrap();

        let store_dir = tmp.path().join("store");
        std::fs::create_dir_all(&store_dir).unwrap();
        Self {
            _tmp: tmp,
            tu,
            search: IncludeSearchPaths::default(),
            store_dir,
            object: vec![0x5a; OBJECT_SIZE],
            header_count: n_a + n_b,
        }
    }
}

fn step_scan(fx: &Fixture) -> ScanResult {
    scan_recursive(&fx.tu, &fx.search)
}

fn step_hash(tu: &Path, scan: &ScanResult) -> Vec<(PathBuf, ContentHash)> {
    let mut out = Vec::with_capacity(scan.resolved.len() + 1);
    let bytes = std::fs::read(tu).unwrap();
    out.push((tu.to_path_buf(), zccache::hash::hash_bytes(&bytes)));
    for p in &scan.resolved {
        let bytes = std::fs::read(p.as_path()).unwrap();
        out.push((p.as_path().to_path_buf(), zccache::hash::hash_bytes(&bytes)));
    }
    out
}

/// Mirrors `persist_payloads.rs`: write to a tmp path, then rename into
/// place. Persists the object payload and an index listing every input hash.
fn step_store_persist(fx: &Fixture, hashes: &[(PathBuf, ContentHash)]) {
    let mut index = String::with_capacity(hashes.len() * 96);
    for (path, h) in hashes {
        let _ = writeln!(index, "{} {}", h.to_hex(), path.display());
    }
    let key_hex = zccache::hash::hash_bytes(index.as_bytes()).to_hex();

    let obj_tmp = fx.store_dir.join(format!(".{key_hex}.o.tmp"));
    let obj_final = fx.store_dir.join(format!("{key_hex}.o"));
    std::fs::write(&obj_tmp, &fx.object).unwrap();
    std::fs::rename(&obj_tmp, &obj_final).unwrap();

    let idx_tmp = fx.store_dir.join(format!(".{key_hex}.idx.tmp"));
    let idx_final = fx.store_dir.join(format!("{key_hex}.idx"));
    std::fs::write(&idx_tmp, index.as_bytes()).unwrap();
    std::fs::rename(&idx_tmp, &idx_final).unwrap();
}

fn reset_store(fx: &Fixture) {
    for entry in std::fs::read_dir(&fx.store_dir).unwrap() {
        let _ = std::fs::remove_file(entry.unwrap().path());
    }
}

fn bench_miss_overhead(c: &mut Criterion) {
    let fx = Fixture::new();
    let scan = step_scan(&fx);
    assert!(
        scan.resolved.len() >= fx.header_count,
        "fixture should resolve every header ({} < {})",
        scan.resolved.len(),
        fx.header_count
    );
    let hashes = step_hash(&fx.tu, &scan);

    let mut group = c.benchmark_group("miss_overhead");
    group.sample_size(20);

    group.bench_function("scan", |b| {
        b.iter(|| black_box(step_scan(black_box(&fx))));
    });
    group.bench_function("hash", |b| {
        b.iter(|| black_box(step_hash(black_box(&fx.tu), black_box(&scan))));
    });
    group.bench_function("store_persist", |b| {
        b.iter(|| {
            reset_store(&fx);
            step_store_persist(black_box(&fx), black_box(&hashes));
        });
    });
    group.bench_function("total", |b| {
        b.iter(|| {
            reset_store(&fx);
            let s = step_scan(black_box(&fx));
            let h = step_hash(black_box(&fx.tu), black_box(&s));
            step_store_persist(black_box(&fx), black_box(&h));
            black_box((s, h));
        });
    });

    group.finish();
}

criterion_group!(benches, bench_miss_overhead);
criterion_main!(benches);
