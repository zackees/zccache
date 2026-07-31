//! Startup recovery for an unreadable artifact index (#1157, finding 1).
//!
//! `zccache-artifact` owns the corrupt arm but cannot own the recovery: the
//! rebuild scans the artifact directory, needs a wall-clock budget tied to
//! daemon startup, and writes a lifecycle event into the cache root — all
//! daemon concerns, and `zccache-daemon-core` is the crate that depends on
//! `zccache-artifact`, never the reverse. So the store only reports that it
//! started corrupt ([`ArtifactStore::take_started_corrupt`]) and the policy
//! lives here.
//!
//! What gets rebuilt, and why deliberately less than everything on disk, is
//! documented on `zccache_artifact::reconcile`. The short version: output
//! filenames are not recoverable from any on-disk record, they are
//! load-bearing for placing outputs at index >= 1, and a wrong cache hit is
//! catastrophic where a miss is only slow — so only single-output staged
//! generations come back.

use std::path::Path;
use std::time::Duration;

use crate::artifact::{reconcile_index_from_disk, ArtifactStore, DEFAULT_RECONCILE_BUDGET};
use crate::core::lifecycle::{write_event_in_cache_root, EVENT_INDEX_RECONCILED};

/// Overrides the wall-clock cap on the startup rebuild scan, in milliseconds.
///
/// `0` disables reconciliation outright (the scan reports itself truncated
/// and recovers nothing), which is the escape hatch if a pathological cache
/// makes even the capped scan unwelcome on the startup path.
pub(crate) const RECONCILE_BUDGET_ENV: &str = "ZCCACHE_INDEX_RECONCILE_BUDGET_MS";

/// Resolve the rebuild budget from the environment, falling back to
/// [`DEFAULT_RECONCILE_BUDGET`]. An unparseable value is ignored rather than
/// fatal — this runs during startup and must never stop the daemon coming up.
pub(crate) fn reconcile_budget() -> Duration {
    std::env::var(RECONCILE_BUDGET_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map_or(DEFAULT_RECONCILE_BUDGET, Duration::from_millis)
}

/// If `store` reported that its on-disk blob did not parse, rebuild what can
/// be rebuilt from the surviving payloads under `artifact_dir` and insert it.
///
/// Returns the number of entries recovered (`0` when the index loaded fine,
/// which is the overwhelmingly common case and costs one atomic read).
///
/// Existing in-memory entries always win: a request handler may already have
/// stored a *complete* entry — with real output names and captured compiler
/// output — for a key the scan also found, and the reconstructed entry is
/// strictly poorer. Reconciliation fills holes, it never overwrites.
pub(crate) fn reconcile_corrupt_index(
    store: &ArtifactStore,
    artifact_dir: &Path,
    cache_dir: &Path,
    budget: Duration,
) -> usize {
    if !store.take_started_corrupt() {
        return 0;
    }

    let outcome = reconcile_index_from_disk(artifact_dir, budget);
    let mut recovered = 0_usize;
    for (key, meta) in &outcome.entries {
        if store.get(key).is_none() {
            store.insert(key, meta);
            recovered += 1;
        }
    }

    tracing::warn!(
        artifact_dir = %artifact_dir.display(),
        recovered,
        candidates = outcome.candidates,
        skipped_multi_output = outcome.skipped_multi_output,
        skipped_unverifiable = outcome.skipped_unverifiable,
        truncated_by_budget = outcome.truncated_by_budget,
        elapsed_ns = outcome.elapsed_ns,
        "artifact index was unreadable; rebuilt entries from surviving on-disk payloads"
    );
    write_event_in_cache_root(
        cache_dir,
        EVENT_INDEX_RECONCILED,
        serde_json::json!({
            "subsystem": "artifact_index",
            "artifact_dir": artifact_dir.display().to_string(),
            "recovered": recovered,
            "candidates": outcome.candidates,
            "skipped_multi_output": outcome.skipped_multi_output,
            "skipped_unverifiable": outcome.skipped_unverifiable,
            "truncated_by_budget": outcome.truncated_by_budget,
            "budget_ms": budget.as_millis() as u64,
            "elapsed_ns": outcome.elapsed_ns,
        }),
    );
    recovered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::layout_fixtures::seed_staged_generation;
    use crate::artifact::{resolve_artifact_payloads, RECONCILED_OUTPUT_NAME};

    /// A cache root with a corrupt `index.bin` and one healthy single-output
    /// staged generation. Returns `(tempdir, cache_dir, artifact_dir, key)`.
    fn corrupt_index_with_payload(
        payload: &[u8],
        key_byte: char,
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        String,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache_dir = dir.path().to_path_buf();
        let artifact_dir = cache_dir.join("artifacts");
        std::fs::create_dir_all(&artifact_dir).expect("artifact dir");
        std::fs::write(cache_dir.join("index.bin"), b"not valid bincode").expect("corrupt index");
        let key = key_byte.to_string().repeat(64);
        seed_staged_generation(&artifact_dir, &key, &[payload]);
        (dir, cache_dir, artifact_dir, key)
    }

    fn open_store(cache_dir: &Path) -> ArtifactStore {
        ArtifactStore::open(&cache_dir.join("index.bin")).expect("open store")
    }

    #[test]
    fn a_corrupt_index_is_rebuilt_into_re_hittable_entries() {
        let payload = b"cached object bytes";
        let (_dir, cache_dir, artifact_dir, key) = corrupt_index_with_payload(payload, 'a');

        let store = open_store(&cache_dir);
        assert_eq!(store.len(), 0, "the corrupt load still starts empty");

        let recovered =
            reconcile_corrupt_index(&store, &artifact_dir, &cache_dir, DEFAULT_RECONCILE_BUDGET);
        assert_eq!(recovered, 1);

        let entry = store.get(&key).expect("the key must be back in the index");
        assert_eq!(entry.output_sizes, vec![payload.len() as u64]);
        assert_eq!(&*entry.output_names, &[RECONCILED_OUTPUT_NAME.to_string()]);

        // Re-hittable means the resolver the hit path uses finds the payload
        // from the rebuilt metadata alone.
        let payloads = resolve_artifact_payloads(
            &artifact_dir,
            &key,
            &entry.output_sizes,
            true,
            "test::reconciled",
        )
        .expect("resolve")
        .expect("the rebuilt entry must resolve to its surviving payload");
        assert_eq!(payloads.len(), 1);
    }

    #[test]
    fn the_rebuild_emits_a_durable_event_recording_the_outcome() {
        let (_dir, cache_dir, artifact_dir, _key) = corrupt_index_with_payload(b"bytes", 'b');
        let store = open_store(&cache_dir);
        reconcile_corrupt_index(&store, &artifact_dir, &cache_dir, DEFAULT_RECONCILE_BUDGET);

        let log = std::fs::read_to_string(
            cache_dir
                .join("logs")
                .join(crate::core::lifecycle::live_log_filename()),
        )
        .expect("lifecycle log");
        let row = log
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|value| value["event"] == EVENT_INDEX_RECONCILED)
            .expect("a rebuild must be attributable after the fact, not just a warn");
        assert_eq!(row["recovered"], 1);
        assert_eq!(row["candidates"], 1);
        assert_eq!(row["truncated_by_budget"], false);
        assert!(row["elapsed_ns"].is_number());
    }

    #[test]
    fn an_exhausted_budget_falls_back_to_an_empty_index_without_sleeping() {
        let (_dir, cache_dir, artifact_dir, key) = corrupt_index_with_payload(b"bytes", 'c');
        let store = open_store(&cache_dir);

        let recovered = reconcile_corrupt_index(&store, &artifact_dir, &cache_dir, Duration::ZERO);
        assert_eq!(recovered, 0);
        assert!(store.get(&key).is_none());
        assert_eq!(store.len(), 0, "beyond the budget the daemon starts empty");

        let log = std::fs::read_to_string(
            cache_dir
                .join("logs")
                .join(crate::core::lifecycle::live_log_filename()),
        )
        .expect("lifecycle log");
        assert!(
            log.lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .any(|value| value["event"] == EVENT_INDEX_RECONCILED
                    && value["truncated_by_budget"] == true),
            "a truncated scan must say so, or a half-recovered cache looks like a cold one"
        );
    }

    #[test]
    fn a_healthy_index_is_never_scanned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache_dir = dir.path().to_path_buf();
        let artifact_dir = cache_dir.join("artifacts");
        std::fs::create_dir_all(&artifact_dir).expect("artifact dir");
        let key = "d".repeat(64);
        seed_staged_generation(&artifact_dir, &key, &[b"bytes"]);

        let store = open_store(&cache_dir);
        let recovered =
            reconcile_corrupt_index(&store, &artifact_dir, &cache_dir, DEFAULT_RECONCILE_BUDGET);
        assert_eq!(recovered, 0);
        assert!(
            store.get(&key).is_none(),
            "a cold cache must stay cold; reconciliation is only for a corrupt load"
        );
        assert!(
            !cache_dir.join("logs").exists()
                || !std::fs::read_to_string(
                    cache_dir
                        .join("logs")
                        .join(crate::core::lifecycle::live_log_filename())
                )
                .unwrap_or_default()
                .contains(EVENT_INDEX_RECONCILED),
            "no corruption, no event"
        );
    }

    #[test]
    fn the_rebuild_is_idempotent_across_restarts() {
        let payload = b"restart stable bytes";
        let (_dir, cache_dir, artifact_dir, key) = corrupt_index_with_payload(payload, 'e');

        let first = open_store(&cache_dir);
        assert_eq!(
            reconcile_corrupt_index(&first, &artifact_dir, &cache_dir, DEFAULT_RECONCILE_BUDGET),
            1
        );
        let after_first = first.get(&key).expect("entry");
        // The daemon flushes on shutdown; the second boot must not be
        // disturbed by that healthy index existing.
        first.flush().expect("flush");

        // Second boot: corrupt the freshly flushed index again and rebuild.
        std::fs::write(cache_dir.join("index.bin"), b"corrupt again").expect("re-corrupt");
        let second = open_store(&cache_dir);
        assert_eq!(
            reconcile_corrupt_index(&second, &artifact_dir, &cache_dir, DEFAULT_RECONCILE_BUDGET),
            1
        );
        let after_second = second.get(&key).expect("entry");

        assert_eq!(after_first.output_sizes, after_second.output_sizes);
        assert_eq!(after_first.total_size, after_second.total_size);
        assert_eq!(
            after_first.stored_at_secs, after_second.stored_at_secs,
            "a restart loop must not keep resetting retention age for the whole cache"
        );
    }

    #[test]
    fn a_complete_in_memory_entry_is_never_replaced_by_a_poorer_rebuilt_one() {
        let (_dir, cache_dir, artifact_dir, key) = corrupt_index_with_payload(b"bytes", 'f');
        let store = open_store(&cache_dir);

        let complete = crate::artifact::ArtifactIndex::new(
            vec!["real-name.o".to_string()],
            vec![5],
            b"warning: unused variable".to_vec(),
            Vec::new(),
            0,
        );
        store.insert(&key, &complete);

        let recovered =
            reconcile_corrupt_index(&store, &artifact_dir, &cache_dir, DEFAULT_RECONCILE_BUDGET);
        assert_eq!(recovered, 0);
        let kept = store.get(&key).expect("entry");
        assert_eq!(&*kept.output_names, &["real-name.o".to_string()]);
        assert!(
            !kept.stdout.is_empty(),
            "captured compiler output must survive; the rebuilt entry has none"
        );
    }

    /// The budget the daemon actually uses is injected as a parameter (every
    /// test above passes its own, so none of them sleep). This only covers
    /// the operator-facing override that resolves the default.
    #[test]
    fn an_absent_budget_override_resolves_to_the_default() {
        if std::env::var(RECONCILE_BUDGET_ENV).is_ok() {
            return;
        }
        assert_eq!(reconcile_budget(), DEFAULT_RECONCILE_BUDGET);
    }
}
