//! Criterion bench for `scan_recursive` on synthetic header trees.
//!
//! `scan_recursive` is the entrypoint for recursive `#include` scanning
//! during compile cache MISSES (see crates/zccache-depgraph/src/scanner.rs).
//! Two fixtures exercise it:
//!
//! - `small_120_files` — ~120 unique headers, regression guard for small TUs.
//! - `large_1300_files` — ~1300 unique headers, mimics STL-heavy C++ TUs where
//!   parallelizing the read+parse step is the win.
//!
//! All headers live in a single flat directory and use quoted includes
//! (`#include "h_<id>.h"`) so they resolve via the including file's own
//! directory (no `-I` flag needed) — see `resolve_include` in scanner.rs.
//!
//! Run with: `soldr cargo bench -p zccache-depgraph --bench scan_recursive`.
//! Save a baseline on the pre-change commit with `-- --save-baseline pre`,
//! then re-run after the impl change with `-- --baseline pre`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::path::{Path, PathBuf};

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use zccache::depgraph::scanner::scan_recursive;
use zccache::depgraph::search_paths::IncludeSearchPaths;

/// Build a tree of `#include "h_<id>.h"` headers in a single flat directory.
///
/// Returns the path of the root header (which transitively pulls in every
/// other header via quoted includes). Quoted includes are resolved against
/// the including file's own directory, so a flat layout means every
/// `#include` resolves without needing any `-I` search path.
///
/// Header bodies include enough lines (~30) of pretend declarations to
/// approximate the size of a small real-world header — pure-`#include`
/// files would be unrealistically cheap to read.
fn build_tree(root: &Path, depth: usize, fan_out: usize) -> (PathBuf, usize) {
    std::fs::create_dir_all(root).unwrap();
    let mut counter: u32 = 0;

    fn next(counter: &mut u32) -> u32 {
        *counter += 1;
        *counter
    }

    fn rec(dir: &Path, depth: usize, fan_out: usize, counter: &mut u32) -> u32 {
        let my_id = next(counter);
        let my_path = dir.join(format!("h_{my_id:05}.h"));

        let filler = (0..30)
            .map(|j| format!("static int filler_{my_id}_{j} = {j};\n"))
            .collect::<String>();

        if depth == 0 {
            let body = format!("// leaf {my_id}\n{filler}");
            std::fs::write(&my_path, body).unwrap();
            return my_id;
        }

        let mut content = String::new();
        for _ in 0..fan_out {
            let child_id = rec(dir, depth - 1, fan_out, counter);
            content.push_str(&format!("#include \"h_{child_id:05}.h\"\n"));
        }
        content.push_str(&filler);
        std::fs::write(&my_path, content).unwrap();
        my_id
    }

    let root_id = rec(root, depth, fan_out, &mut counter);
    let root_path = root.join(format!("h_{root_id:05}.h"));
    (root_path, counter as usize)
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root_header: PathBuf,
    search: IncludeSearchPaths,
    file_count: usize,
}

impl Fixture {
    fn new(depth: usize, fan_out: usize) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let inc_dir = tmp.path().join("inc");
        let (root_header, file_count) = build_tree(&inc_dir, depth, fan_out);
        // Quoted includes resolve via the including file's dir, so the search
        // paths are intentionally empty.
        let search = IncludeSearchPaths::default();
        Self {
            _tmp: tmp,
            root_header,
            search,
            file_count,
        }
    }
}

fn bench_scan_recursive(c: &mut Criterion) {
    let mut group = c.benchmark_group("scan_recursive");
    group.sample_size(20);

    // depth=3, fan_out=3 → ~40 headers (1+3+9+27 = 40). Regression guard.
    let small = Fixture::new(3, 3);
    let small_name = format!("small_{}_files", small.file_count);
    group.bench_function(&small_name, |b| {
        b.iter(|| {
            let r = scan_recursive(black_box(&small.root_header), black_box(&small.search));
            black_box(r);
        });
    });

    // depth=5, fan_out=4 → ~1300 headers. Heavy C++ TU workload.
    let large = Fixture::new(5, 4);
    let large_name = format!("large_{}_files", large.file_count);
    group.bench_function(&large_name, |b| {
        b.iter(|| {
            let r = scan_recursive(black_box(&large.root_header), black_box(&large.search));
            black_box(r);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_scan_recursive);
criterion_main!(benches);
