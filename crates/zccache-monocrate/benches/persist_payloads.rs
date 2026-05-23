//! Micro-benchmark for cache-populate artifact persistence.
//!
//! `persist_artifact_payloads` writes N output payloads to the artifact
//! directory using an atomic write-then-rename pattern (see
//! `persist_artifact_output` in crates/zccache-daemon/src/server.rs).
//! It runs off the hot path (inside `tokio::spawn` -> `spawn_blocking`),
//! but it holds `state.persist_semaphore` for the full serial duration,
//! throttling concurrent cache populates. Parallelizing the inner loop
//! shortens the semaphore-permit hold time.
//!
//! This bench mirrors the production syscall sequence
//! (`fs::write` to tmp, `fs::rename` to final, on a per-payload basis)
//! and compares serial vs `rayon::par_iter` for `N ∈ {1, 3, 5, 10}`.

use std::path::PathBuf;
use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use rayon::prelude::*;

const PAYLOAD_SIZE: usize = 1024 * 1024; // 1 MiB
const COUNTS: &[usize] = &[1, 3, 5, 10];

struct Fixture {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    payloads: Vec<Arc<Vec<u8>>>,
    key_hex: String,
}

impl Fixture {
    fn new(n: usize) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("artifacts");
        std::fs::create_dir_all(&dir).unwrap();
        let payloads: Vec<Arc<Vec<u8>>> = (0..n)
            .map(|i| Arc::new(vec![(i as u8).wrapping_add(0x42); PAYLOAD_SIZE]))
            .collect();
        Self {
            _tmp: tmp,
            dir,
            payloads,
            key_hex: "deadbeef".to_string(),
        }
    }

    /// Clear out the artifact files from a previous iteration so the
    /// rename step exercises the create path consistently.
    fn reset(&self) {
        for i in 0..self.payloads.len() {
            let _ = std::fs::remove_file(self.dir.join(format!("{}_{i}", self.key_hex)));
        }
    }
}

/// Mirrors `persist_artifact_output` in server.rs: write to a tmp path,
/// then rename atomically into place.
fn persist_one(dir: &std::path::Path, key_hex: &str, i: usize, payload: &[u8]) {
    let cache_path = dir.join(format!("{key_hex}_{i}"));
    let tmp_path = dir.join(format!(".{key_hex}_{i}.tmp"));
    std::fs::write(&tmp_path, payload).unwrap();
    std::fs::rename(&tmp_path, &cache_path).unwrap();
}

fn persist_serial(fx: &Fixture) {
    for (i, payload) in fx.payloads.iter().enumerate() {
        persist_one(&fx.dir, &fx.key_hex, i, payload);
    }
}

fn persist_parallel(fx: &Fixture) {
    fx.payloads.par_iter().enumerate().for_each(|(i, payload)| {
        persist_one(&fx.dir, &fx.key_hex, i, payload);
    });
}

fn bench_persist_payloads(c: &mut Criterion) {
    let mut group = c.benchmark_group("persist_payloads");
    for &n in COUNTS {
        let fx = Fixture::new(n);
        group.bench_with_input(BenchmarkId::new("serial", n), &n, |b, _| {
            b.iter(|| {
                fx.reset();
                persist_serial(black_box(&fx));
            });
        });
        group.bench_with_input(BenchmarkId::new("parallel", n), &n, |b, _| {
            b.iter(|| {
                fx.reset();
                persist_parallel(black_box(&fx));
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_persist_payloads);
criterion_main!(benches);
