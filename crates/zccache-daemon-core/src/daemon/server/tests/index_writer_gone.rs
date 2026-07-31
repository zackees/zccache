//! #1177: an index insert that cannot reach the index writer must be observed.
//!
//! The send only fails when the writer task is gone, and that means the daemon
//! has silently stopped recording what it caches — every artifact published
//! from then on is invisible to the next daemon start, so the cache is
//! effectively write-only. `let _ = tx.send(..)` made that indistinguishable
//! from success, which is the whole point of the finding.

use std::sync::atomic::Ordering;

use crate::daemon::server::staged_publish::enqueue_index_insert;

use super::CacheDirEnvGuard;

/// Build metadata whose contents do not matter — the test is about the send
/// path, not the payload.
fn dummy_meta() -> crate::artifact::ArtifactIndex {
    crate::artifact::ArtifactIndex::new(
        vec!["out.o".to_string()],
        vec![1],
        Vec::new(),
        Vec::new(),
        0,
    )
}

fn index_writer_gone_rows(log: &str) -> usize {
    log.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["event"] == crate::core::lifecycle::EVENT_INDEX_WRITER_GONE)
        .count()
}

/// A dead index writer is reported once, loudly — and only once, because the
/// condition is permanent and an event per compile would turn one fault into
/// an unbounded log (the very growth #1165 was about).
#[tokio::test]
async fn a_dead_index_writer_is_reported_once_not_once_per_compile() {
    let root = tempfile::tempdir().unwrap();
    let _cache_env = CacheDirEnvGuard::set(root.path());
    let server = super::bind_isolated_server(root.path());
    let state = std::sync::Arc::clone(&server.state);

    // Drop the receiving half: this is exactly the state the daemon is in
    // after the index-writer task dies.
    drop(server);

    let before = std::fs::read_to_string(crate::core::lifecycle::log_file_path())
        .map(|log| index_writer_gone_rows(&log))
        .unwrap_or(0);

    for index in 0..3 {
        enqueue_index_insert(&state, format!("key-{index}"), dummy_meta());
    }

    assert!(
        state.index_writer_gone.load(Ordering::Acquire),
        "a failed send must latch the degraded flag"
    );
    let log = std::fs::read_to_string(crate::core::lifecycle::log_file_path())
        .expect("the failure must be durably recorded");
    assert_eq!(
        index_writer_gone_rows(&log) - before,
        1,
        "three failed sends must produce exactly one event, not three"
    );
}
