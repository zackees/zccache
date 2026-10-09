//! Tests for cache-hit output delivery: `write_cached_output`,
//! `persist_artifact_output`, `persist_artifact_file`, and
//! `break_output_hardlink_before_compile`. Most of these are regression
//! guards for staleness / cache-poisoning / mtime-preservation bugs that
//! had downstream consequences for cargo's incremental fingerprint.

use super::super::*;
use crate::daemon::server::handle_compile_multi::materialize_multi_hit;

fn seed_persisted_blob(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    write_authoritative_blob_digest(path).unwrap();
}

fn require_hardlink(out: &Path, cache: &Path, test_name: &str) -> bool {
    if crate::platform::fs::identity::same_file(out, cache).unwrap() {
        return true;
    }
    // Skip only where the volume cannot hardlink. A hardlink-capable volume
    // that still delivered an independent file means the test stopped
    // exercising the shared-inode path it guards (#1792 audit).
    assert!(
        !fs_caps_raw(cache, out).hardlink,
        "{test_name}: the volume supports hardlinks but the output was not linked"
    );
    eprintln!("SKIP {test_name}: temporary filesystem does not support same-volume hardlinks");
    false
}

/// Deliver a hardlink-eligible output under LINK, the mode whose shared
/// inode the #197/#1039 guards below protect (AUTO never shares, #1792).
fn deliver_linked(out: &Path, cache: &Path) {
    materialize_cached_file_with_mode(
        out,
        cache,
        crate::compiler::DeliveryPolicy::HardlinkEligible,
        MaterializationMode::Link,
        false,
    )
    .unwrap();
}

mod delivery;
mod failures;
mod integrity;
mod modes;
mod mtime;
