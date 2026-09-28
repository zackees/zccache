//! Durability-sync cost test for the pre-write hardlink detach.
//!
//! Unix only: on Windows a file replace/rename (plus file scanning) costs as
//! much as a synced write, so the comparison would measure the platform, not
//! whether a sync was reintroduced.
#![cfg(unix)]

use super::*;
use std::io::Write;

/// Bytes in the detached output: a small object file. Sync cost grows with
/// the dirty bytes, so a realistic size keeps the comparison meaningful.
const OUTPUT_BYTES: usize = 256 * 1024;

/// Median cost of detaching a hardlinked output next to the median cost of
/// writing the same bytes with a file + directory sync, interleaved in the
/// same directory so machine load hits both equally. `None` unless a synced
/// write costs at least 5 ms here: where sync is cheap (tmpfs, a write-back
/// cache, hosted runner disks at 0.8-2.1 ms) the copy itself is the same
/// order under a parallel test run.
fn median_detach_vs_synced_write(dir: &Path) -> Option<(std::time::Duration, std::time::Duration)> {
    const SAMPLES: usize = 21;
    let payload = vec![0x5a_u8; OUTPUT_BYTES];
    let blob = dir.join("cache-blob.o");
    let output = dir.join("output.o");
    let synced = dir.join("synced-baseline.o");
    std::fs::write(&blob, &payload).unwrap();
    let mut detach_costs = Vec::with_capacity(SAMPLES);
    let mut sync_costs = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = std::time::Instant::now();
        let mut file = std::fs::File::create(&synced).unwrap();
        file.write_all(&payload).unwrap();
        file.sync_all().unwrap();
        drop(file);
        if let Ok(directory) = std::fs::File::open(dir) {
            let _ = directory.sync_all();
        }
        sync_costs.push(started.elapsed());

        let _ = std::fs::remove_file(&output);
        std::fs::hard_link(&blob, &output).unwrap();
        let started = std::time::Instant::now();
        break_output_hardlink_before_compile(&output).unwrap();
        detach_costs.push(started.elapsed());
        assert_eq!(
            crate::platform::fs::links::hard_link_count(&output).unwrap(),
            1,
            "detach must leave the output unlinked from the cache blob"
        );
    }
    detach_costs.sort();
    sync_costs.sort();
    let (detach, sync) = (detach_costs[SAMPLES / 2], sync_costs[SAMPLES / 2]);
    (sync >= std::time::Duration::from_millis(5)).then_some((detach, sync))
}

/// The detached copy is a compiler-visible output the compiler (or tool)
/// rewrites next; compilers never fsync their outputs, so the detach must not
/// either. It runs on the request's critical path on every rebuild of an
/// output that an earlier hit delivered as a hardlink.
#[test]
fn hardlink_detach_does_not_pay_a_durability_sync() {
    let temp = tempfile::tempdir().unwrap();
    if let Some((detach, synced)) = median_detach_vs_synced_write(temp.path()) {
        assert!(
            detach * 2 < synced,
            "hardlink detach median {detach:?} is not clearly below a synced \
             {OUTPUT_BYTES}-byte write ({synced:?}); the detached copy must not be synced"
        );
    }
}
