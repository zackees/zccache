//! Startup policy for the persisted depgraph snapshot (#1157 finding 2).
//!
//! Extracted out of `daemon::entry`'s `spawn_blocking` closure so the arms
//! that decide "warm graph or cold world" are reachable from a unit test.
//! Before the extraction the drop paths could only be pinned by re-emitting
//! their events by hand, which cannot catch an arm that stops firing.
//!
//! ## What a reset actually costs, and what it does not
//!
//! An artifact key is `H(logical_context_key, sorted(path → content_hash))`
//! over the source *plus every resolved include* (`zccache-depgraph`,
//! `context::compute_artifact_key`). That include set exists nowhere but the
//! depgraph: the artifact index (`zccache-artifact::ArtifactIndex`) stores
//! output names/sizes/stdout/stderr/exit-code and no input identity at all.
//! So an empty graph means every translation unit reports `CacheVerdict::Cold`
//! and is recompiled once, even though its artifact is still on disk and is
//! re-adopted (same key, deduplicated) the moment the compiler hands the
//! include list back. The blast radius is one recompile per TU — not a cache
//! wipe.
//!
//! That also rules out "keep the artifact-key mapping across the reset"
//! as stated in #1157: preserving the mapping *is* preserving the contexts,
//! the contexts live in the rejected blob, and reading a foreign-version blob
//! needs the old type definitions (the versioned migration this change
//! deliberately does not implement). Reinterpreting it without one would be
//! actively dangerous — a schema bump can be precisely because a new input
//! class started feeding the key (`rustc_env_deps`, #1021), and a resurrected
//! context missing that field could satisfy `check()` and serve an artifact
//! built under different inputs. A wrong hit is catastrophic; a miss is slow.
//!
//! What this module does instead is stop the drop from being *destructive*:
//! the rejected snapshot is moved aside rather than left for the next
//! graceful shutdown to overwrite, and a sidecar written by this build's own
//! schema version is loaded back. That makes the common real-world reset —
//! a machine alternating between two binaries with different
//! `DEPGRAPH_VERSION`s — a one-time cost per version instead of a full cold
//! recompile on every switch, with no byte of foreign-schema data ever
//! interpreted.

use std::path::Path;

use crate::depgraph::{quarantine, DepGraph, DepGraphLoadOutcome};

/// Outcome of the startup load, ready to hand to `DepGraphSetter::install`.
pub(crate) struct StartupLoad {
    /// The graph to install, or `None` to start cold.
    pub graph: Option<DepGraph>,
    /// Operator-visible warning for a degraded load, printed to stderr and
    /// surfaced in the session log.
    pub warning: Option<String>,
}

/// Size of the snapshot at `path`, or `None` when it is not there.
///
/// The distinction matters in the forensics event: `null` means the file
/// vanished between classification and this stat, while `0` means a real,
/// present, empty snapshot. Collapsing them would let a post-incident search
/// claim a file existed when it did not.
pub(crate) fn snapshot_bytes(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|meta| meta.len())
}

/// Classify the snapshot at `path` and decide what to install.
///
/// Emits the durable lifecycle event for every degraded arm before returning;
/// callers only have to print the warning and install the result.
pub(crate) fn load_for_startup(path: &Path) -> StartupLoad {
    let start = std::time::Instant::now();
    let outcome = crate::depgraph::classify_load(path);
    let warning = outcome.warning(path);
    match outcome {
        DepGraphLoadOutcome::Loaded { graph } => {
            let stats = graph.stats();
            let (cold_ctxs, warm_ctxs, stale_ctxs) = graph.state_breakdown();
            let ctxs_with_key = graph.contexts_with_artifact_key();
            tracing::info!(
                contexts = stats.context_count,
                files = stats.file_count,
                cold = cold_ctxs,
                warm = warm_ctxs,
                stale = stale_ctxs,
                with_artifact_key = ctxs_with_key,
                elapsed_ms = start.elapsed().as_millis() as u64,
                "loaded depgraph from disk (background)"
            );
            StartupLoad {
                graph: Some(graph),
                warning: None,
            }
        }
        DepGraphLoadOutcome::Missing => StartupLoad {
            graph: None,
            warning: None,
        },
        DepGraphLoadOutcome::VersionMismatch {
            file_version,
            expected_version,
        } => {
            let bytes = snapshot_bytes(path);
            let quarantined =
                quarantine::quarantine(path, &quarantine::quarantine_path(path, file_version));
            let recovered = quarantine::recover(path);
            tracing::warn!(
                file_version,
                expected_version,
                recovered = recovered.is_some(),
                "depgraph version mismatch — the on-disk snapshot was written by \
                 a different schema version"
            );
            // #1157: a schema bump silently costs every workspace a full cold
            // recompile. A `tracing::warn!` goes to whatever the operator
            // happened to be capturing, so fleet-wide "everyone recompiled
            // today" incidents were unattributable after the fact.
            emit_reset_event(
                crate::daemon::lifecycle::EVENT_VERSION_MISMATCH,
                serde_json::json!({
                    "subsystem": "depgraph",
                    "file_version": file_version,
                    "expected_version": expected_version,
                    "path": path.display().to_string(),
                    "bytes": bytes,
                }),
                path,
                quarantined.as_ref(),
                recovered.as_ref().map(|(_, from)| from),
            );
            finish(recovered, warning)
        }
        DepGraphLoadOutcome::Corrupt { ref message }
        | DepGraphLoadOutcome::IoError { ref message } => {
            let bytes = snapshot_bytes(path);
            // Unlike the version-mismatch arm there is nothing salvageable in
            // these bytes — they failed validation, so they are preserved for
            // forensics in a single slot and never read back.
            let quarantined = quarantine::quarantine(path, &quarantine::corrupt_sidecar_path(path));
            let recovered = quarantine::recover(path);
            tracing::warn!(
                recovered = recovered.is_some(),
                "depgraph load failed: {message}"
            );
            // #1157: the sibling `VersionMismatch` arm already emits a durable
            // event, and this arm has the same blast radius. Leaving it on
            // `tracing::warn!` alone made the corrupt case the one variant a
            // post-incident log search missed.
            emit_reset_event(
                crate::core::lifecycle::EVENT_STATE_CORRUPT,
                serde_json::json!({
                    "subsystem": "depgraph",
                    "message": message,
                    "path": path.display().to_string(),
                    "bytes": bytes,
                }),
                path,
                quarantined.as_ref(),
                recovered.as_ref().map(|(_, from)| from),
            );
            finish(recovered, warning)
        }
    }
}

/// Install the recovered sidecar when there is one, else start cold.
///
/// The warning is kept in both cases: the primary snapshot really was
/// rejected, and an operator investigating a schema bump should see that even
/// when a sidecar softened the blow.
fn finish(
    recovered: Option<(DepGraph, zccache_core::NormalizedPath)>,
    warning: Option<String>,
) -> StartupLoad {
    match recovered {
        Some((graph, from)) => {
            tracing::info!(
                sidecar = %from.as_path().display(),
                contexts = graph.stats().context_count,
                "recovered depgraph from a same-version quarantine sidecar"
            );
            StartupLoad {
                graph: Some(graph),
                warning,
            }
        }
        None => StartupLoad {
            graph: None,
            warning,
        },
    }
}

/// Write the durable drop-path event, adding the disposition fields shared by
/// both degraded arms.
fn emit_reset_event(
    event: &'static str,
    mut payload: serde_json::Value,
    primary: &Path,
    quarantined_to: Option<&zccache_core::NormalizedPath>,
    recovered_from: Option<&zccache_core::NormalizedPath>,
) {
    if let Some(map) = payload.as_object_mut() {
        map.insert(
            "consequence".to_string(),
            serde_json::Value::from(if recovered_from.is_some() {
                "recovered_from_quarantine"
            } else {
                "empty_graph"
            }),
        );
        map.insert(
            "quarantined_to".to_string(),
            match quarantined_to {
                Some(path) => serde_json::Value::from(path.as_path().display().to_string()),
                None => serde_json::Value::Null,
            },
        );
        map.insert(
            "recovered_from".to_string(),
            match recovered_from {
                Some(path) => serde_json::Value::from(path.as_path().display().to_string()),
                None => serde_json::Value::Null,
            },
        );
    }
    crate::daemon::lifecycle::write_event(event, payload);
    quarantine::prune(primary);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::depgraph::DEPGRAPH_VERSION;

    /// Drives the real call site (unlike the earlier hand-rolled payload
    /// tests this replaces): writes a version-skewed snapshot, runs
    /// `load_for_startup`, and reads the event back out of the lifecycle log.
    struct Fixture {
        _temp: tempfile::TempDir,
        cache_root: std::path::PathBuf,
        snapshot: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let cache_root = temp.path().join("cache");
            let snapshot = cache_root.join("depgraph").join("depgraph.bin");
            std::fs::create_dir_all(snapshot.parent().unwrap()).unwrap();
            Self {
                _temp: temp,
                cache_root,
                snapshot,
            }
        }

        /// `write_event` resolves the cache root from the process environment;
        /// point it at this fixture for the duration of one load.
        fn load(&self) -> (StartupLoad, serde_json::Value) {
            let _guard = CacheRootEnv::set(&self.cache_root);
            let load = load_for_startup(&self.snapshot);
            // Resolve the log exactly the way production's `write_event` does,
            // so the test cannot pass by reading a path the daemon never uses.
            let log = crate::core::lifecycle::log_file_path();
            let body = std::fs::read_to_string(log.as_path())
                .expect("a degraded load must write a durable lifecycle event");
            let record = body
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .next_back()
                .expect("the lifecycle log must contain a parseable record");
            (load, record)
        }

        fn write_snapshot(&self, version: u32) {
            let graph = crate::depgraph::DepGraph::new();
            crate::depgraph::save_to_file(&graph, &self.snapshot).unwrap();
            if version != DEPGRAPH_VERSION {
                let mut bytes = std::fs::read(&self.snapshot).unwrap();
                bytes[4..8].copy_from_slice(&version.to_le_bytes());
                std::fs::write(&self.snapshot, &bytes).unwrap();
            }
        }
    }

    struct CacheRootEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
    }

    static CACHE_ROOT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    impl CacheRootEnv {
        fn set(root: &Path) -> Self {
            let lock = CACHE_ROOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var_os("ZCCACHE_CACHE_DIR");
            std::env::set_var("ZCCACHE_CACHE_DIR", root);
            Self {
                _lock: lock,
                previous,
            }
        }
    }

    impl Drop for CacheRootEnv {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("ZCCACHE_CACHE_DIR", value),
                None => std::env::remove_var("ZCCACHE_CACHE_DIR"),
            }
        }
    }

    #[test]
    fn version_mismatch_drives_the_event_and_quarantines_instead_of_clobbering() {
        assert!(
            crate::core::lifecycle::EVENT_ALL
                .contains(&crate::daemon::lifecycle::EVENT_VERSION_MISMATCH),
            "log-audit tooling and operator docs key on the catalog; an event \
             outside it is invisible to them"
        );

        let fixture = Fixture::new();
        let foreign = DEPGRAPH_VERSION + 1;
        fixture.write_snapshot(foreign);
        let size = std::fs::metadata(&fixture.snapshot).unwrap().len();

        let (load, record) = fixture.load();

        assert!(load.graph.is_none(), "a foreign schema must start cold");
        assert!(load.warning.is_some(), "the operator must be told why");
        assert_eq!(
            record["event"],
            crate::daemon::lifecycle::EVENT_VERSION_MISMATCH
        );
        // `subsystem` separates a depgraph schema bump from the daemon-version
        // mismatch that already used this event name.
        assert_eq!(record["subsystem"], "depgraph");
        assert_eq!(record["file_version"], foreign);
        assert_eq!(record["expected_version"], DEPGRAPH_VERSION);
        assert_eq!(record["consequence"], "empty_graph");
        assert_eq!(record["bytes"], size, "the size of what was dropped");
        assert!(record["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("depgraph.bin")));

        let sidecar = crate::depgraph::quarantine::quarantine_path(&fixture.snapshot, foreign);
        assert_eq!(
            record["quarantined_to"].as_str(),
            Some(sidecar.as_path().display().to_string().as_str()),
            "the event must name where the rejected snapshot went"
        );
        assert!(
            sidecar.as_path().exists() && !fixture.snapshot.exists(),
            "the rejected snapshot must be preserved, not left for the next \
             graceful shutdown to overwrite"
        );
        assert_eq!(record["recovered_from"], serde_json::Value::Null);
    }

    /// The oscillation case #1157 actually costs users: two binaries with
    /// different `DEPGRAPH_VERSION`s sharing a cache root. Each side keeps its
    /// own snapshot, so the second switch back is warm rather than a full cold
    /// recompile.
    #[test]
    fn a_same_version_sidecar_is_recovered_after_the_primary_is_rejected() {
        let fixture = Fixture::new();

        // This build's own snapshot, parked by an earlier foreign-version run.
        fixture.write_snapshot(DEPGRAPH_VERSION);
        let mine =
            crate::depgraph::quarantine::quarantine_path(&fixture.snapshot, DEPGRAPH_VERSION);
        std::fs::rename(&fixture.snapshot, mine.as_path()).unwrap();

        // The other binary left its own snapshot as the primary.
        fixture.write_snapshot(DEPGRAPH_VERSION + 1);

        let (load, record) = fixture.load();

        assert!(
            load.graph.is_some(),
            "a sidecar carrying this build's exact schema version must be \
             adopted — it passes the same magic/version/rkyv validation the \
             primary snapshot does"
        );
        assert_eq!(record["consequence"], "recovered_from_quarantine");
        assert_eq!(
            record["recovered_from"].as_str(),
            Some(mine.as_path().display().to_string().as_str())
        );
        assert!(
            load.warning.is_some(),
            "recovery softens the blow but the primary snapshot really was \
             rejected; the operator still needs to see the schema skew"
        );
    }

    #[test]
    fn corrupt_bytes_drive_the_event_and_are_never_read_back() {
        assert!(
            crate::core::lifecycle::EVENT_ALL
                .contains(&crate::core::lifecycle::EVENT_STATE_CORRUPT),
            "an event outside the catalog cannot be given a log-audit rule"
        );

        let fixture = Fixture::new();
        std::fs::write(&fixture.snapshot, b"not a valid snapshot").unwrap();

        let (load, record) = fixture.load();

        assert!(load.graph.is_none());
        assert_eq!(record["event"], crate::core::lifecycle::EVENT_STATE_CORRUPT);
        assert_eq!(record["subsystem"], "depgraph");
        assert_eq!(record["consequence"], "empty_graph");
        assert_eq!(record["bytes"], 20, "the size of the dropped snapshot");
        assert!(record["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("depgraph.bin")));

        let sidecar = crate::depgraph::quarantine::corrupt_sidecar_path(&fixture.snapshot);
        assert_eq!(
            record["quarantined_to"].as_str(),
            Some(sidecar.as_path().display().to_string().as_str())
        );
        assert_eq!(
            std::fs::read(sidecar.as_path()).unwrap(),
            b"not a valid snapshot",
            "damaged bytes are kept for forensics"
        );
        // And the forensic copy is never a load candidate: re-running the load
        // with the primary gone must still start cold, not resurrect it.
        let (again, _) = fixture.load();
        assert!(again.graph.is_none());
    }

    #[test]
    fn snapshot_bytes_separates_a_missing_snapshot_from_an_empty_one() {
        let temp = tempfile::tempdir().unwrap();

        let missing = temp.path().join("never-written.bin");
        assert_eq!(
            snapshot_bytes(&missing),
            None,
            "a snapshot that is not there must report absent, not zero bytes — \
             collapsing the two would make the forensics claim a file existed"
        );

        let empty = temp.path().join("empty.bin");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(
            snapshot_bytes(&empty),
            Some(0),
            "a present-but-empty snapshot is a real, different failure and must \
             be distinguishable from a missing one"
        );

        let populated = temp.path().join("populated.bin");
        std::fs::write(&populated, b"corrupt-but-sizeable").unwrap();
        assert_eq!(snapshot_bytes(&populated), Some(20));
    }

    #[test]
    fn a_missing_snapshot_is_a_plain_cold_start_with_no_event() {
        let fixture = Fixture::new();
        let _guard = CacheRootEnv::set(&fixture.cache_root);
        let load = load_for_startup(&fixture.snapshot);
        assert!(load.graph.is_none());
        assert!(
            load.warning.is_none(),
            "a first run is not a degraded load and must not warn"
        );
    }
}
