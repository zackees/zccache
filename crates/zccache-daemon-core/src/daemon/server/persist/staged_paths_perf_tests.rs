//! Durability-sync cost tests for depfile rewrites (#1729, #1733).
//!
//! Unix only: on Windows the rewrite's own replace/rename (plus file
//! scanning) costs more than a synced write, so comparing the two there
//! measures the platform, not whether a sync was reintroduced.
#![cfg(unix)]

use super::*;
use std::io::Write;

/// Median cost of `op` next to the median cost of the same-sized write
/// that *does* pay a file + directory sync, measured interleaved in the
/// same directory so machine load and the filesystem hit both equally.
/// Returns `None` unless a synced write costs at least 2 ms here. Where
/// sync is cheap (tmpfs, a write-back cache, a hosted runner's disk at
/// ~0.8 ms) the rename and read in the operation itself are the same order
/// as a sync, so the comparison cannot separate them.
fn median_cost_vs_synced_write(
    dir: &Path,
    mut op: impl FnMut(),
) -> Option<(std::time::Duration, std::time::Duration)> {
    const SAMPLES: usize = 31;
    let synced = dir.join("synced-baseline.d");
    let mut op_costs = Vec::with_capacity(SAMPLES);
    let mut sync_costs = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = std::time::Instant::now();
        let mut file = std::fs::File::create(&synced).unwrap();
        file.write_all(b"baseline.o: baseline.c\n").unwrap();
        file.sync_all().unwrap();
        drop(file);
        if let Ok(directory) = std::fs::File::open(dir) {
            let _ = directory.sync_all();
        }
        sync_costs.push(started.elapsed());

        let started = std::time::Instant::now();
        op();
        op_costs.push(started.elapsed());
    }
    op_costs.sort();
    sync_costs.sort();
    let (op, sync) = (op_costs[SAMPLES / 2], sync_costs[SAMPLES / 2]);
    (sync >= std::time::Duration::from_millis(2)).then_some((op, sync))
}

/// Requested depfiles are compiler outputs, not durable cache metadata.
/// Rehydrating one must not pay a file + directory fsync; that used to
/// dominate cold rustc-check misses (#1729: ~1 s per 50 requests).
#[test]
fn depfile_rehydration_does_not_pay_a_durability_sync() {
    let temp = tempfile::tempdir().unwrap();
    let depfile = temp.path().join("fixture.d");
    let output: NormalizedPath = temp.path().join("fixture.rmeta").into();
    let canonical = format!("{STAGED_OUTPUT_REMAP_ROOT}/fixture.rmeta: fixture.rs\n");
    let costs = median_cost_vs_synced_write(temp.path(), || {
        std::fs::write(&depfile, &canonical).unwrap();
        rehydrate_logical_depfile(&depfile, std::slice::from_ref(&output)).unwrap();
    });

    assert_eq!(
        std::fs::read_to_string(&depfile).unwrap(),
        format!("{}: fixture.rs\n", output.to_string_lossy())
    );
    if let Some((rehydrate, synced)) = costs {
        assert!(
            rehydrate * 2 < synced,
            "depfile rehydration median {rehydrate:?} is not clearly below a synced \
             write ({synced:?}); requested outputs must not be synced"
        );
    }
}

/// The private staging directory is not itself a committed cache entry.
/// Publication syncs the copied output and generation before its pointer
/// commit, so canonicalizing the staged depfile must not sync the
/// transient source or its parent directory first (#1733).
#[test]
fn staged_depfile_canonicalization_does_not_pay_a_durability_sync() {
    let temp = tempfile::tempdir().unwrap();
    let private_root = temp.path().join("staged");
    std::fs::create_dir(&private_root).unwrap();
    let staged_depfile = private_root.join("fixture.d");
    let requested_depfile: NormalizedPath = temp.path().join("requested/fixture.d").into();
    let source = format!("{}: fixture.rs\n", staged_depfile.display());
    let costs = median_cost_vs_synced_write(&private_root, || {
        std::fs::write(&staged_depfile, &source).unwrap();
        canonicalize_logical_depfile(&staged_depfile, &private_root, &requested_depfile).unwrap();
    });

    assert!(contains_staged_output_marker(
        &std::fs::read(&staged_depfile).unwrap()
    ));
    if let Some((canonicalize, synced)) = costs {
        assert!(
            canonicalize * 2 < synced,
            "staged depfile canonicalization median {canonicalize:?} is not clearly below \
             a synced write ({synced:?}); transient syncs are redundant"
        );
    }
}
