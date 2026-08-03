//! In-memory artifact records and on-disk payload materialization helpers.
//!
//! `CachedArtifact` is the daemon's per-key view of a cached compilation:
//! metadata plus either resident bytes or pointers to on-disk payload files.
//! `ensure_payloads` lazily resolves the payload slice from the artifact dir,
//! and `migrate_meta_files` upgrades legacy `.meta` blobs into the bincode
//! `ArtifactStore` on first startup after an upgrade.

use super::*;
use std::sync::OnceLock;

#[derive(Clone)]
pub(crate) enum CachedPayload {
    /// Payload bytes already resident in memory.
    Bytes(Arc<Vec<u8>>),
    /// Payload bytes are available in a cache file.
    File(NormalizedPath),
}

/// Payloads resolved for one requested-output delivery.
///
/// Staged file payloads carry a shared store-lock guard. Its ownership is tied
/// to this value so callers cannot retain generation paths while accidentally
/// dropping the lock before reflink/hardlink/copy or directory unpacking.
pub(crate) struct MaterializationPayloads {
    payloads: Arc<[CachedPayload]>,
    staged_guard: Option<StagedMaterializationGuard>,
}

impl std::ops::Deref for MaterializationPayloads {
    type Target = [CachedPayload];

    fn deref(&self) -> &Self::Target {
        &self.payloads
    }
}

impl MaterializationPayloads {
    pub(crate) fn staged_lock_timings(&self) -> Option<(u64, u64)> {
        self.staged_guard
            .as_ref()
            .map(StagedMaterializationGuard::timings_ns)
    }

    pub(crate) fn record_staged_lock_timings(
        &self,
        profiler: &crate::daemon::staged_stats::StagedProfiler,
    ) {
        use crate::daemon::staged_stats::StagedTiming;

        if let Some((wait_ns, hold_ns)) = self.staged_lock_timings() {
            profiler.timing(StagedTiming::HitStoreLockWait, wait_ns);
            profiler.timing(StagedTiming::HitStoreLockHold, hold_ns);
        }
    }
}

struct ArtifactAccess {
    last_used: std::time::Instant,
    last_used_wall: std::time::SystemTime,
    used_in_process: bool,
    published_in_process: bool,
    last_access_checkpoint: Option<std::time::Instant>,
}

pub(crate) struct ArtifactAccessSnapshot {
    pub(crate) last_used: std::time::Instant,
    pub(crate) last_used_wall: std::time::SystemTime,
    pub(crate) used_in_process: bool,
    pub(crate) published_in_process: bool,
    #[cfg(test)]
    pub(crate) last_access_checkpoint: Option<std::time::Instant>,
}

/// Cached compilation artifact with lazy payload loading.
///
/// Metadata (output names, sizes, stdout, stderr, exit code) is always in
/// memory after startup. Output payloads are either already in memory or are
/// represented by cache files so hits can hardlink without eager reads.
#[derive(Clone)]
pub(crate) struct CachedArtifact {
    inner: Arc<CachedArtifactInner>,
}

/// Immutable artifact body plus the two lazily-mutated per-artifact cells.
///
/// Keeping the complete body behind one `Arc` makes cloning a live-map entry
/// allocation-free even though `ArtifactIndex` contains an output-size `Vec`.
pub(crate) struct CachedArtifactInner {
    pub(crate) meta: ArtifactIndex,
    /// Arc-wrapped stdout/stderr for cheap IPC response clones.
    pub(crate) stdout: Arc<Vec<u8>>,
    pub(crate) stderr: Arc<Vec<u8>>,
    /// Lazily-resolved output payloads shared by owned cache-entry clones.
    ///
    /// The enclosing artifact `Arc` lets lookup release its DashMap read guard
    /// immediately. Filesystem discovery then initializes this cell without
    /// holding a map shard lock.
    payloads: OnceLock<Arc<[CachedPayload]>>,
    /// Per-artifact access state used by durable retention.
    ///
    /// This lock is independent of the DashMap shard, so one hot artifact
    /// cannot block unrelated keys that happen to hash into the same shard.
    access: std::sync::Mutex<ArtifactAccess>,
}

impl std::ops::Deref for CachedArtifact {
    type Target = CachedArtifactInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl CachedArtifact {
    fn with_payloads(meta: ArtifactIndex, payloads: Arc<[CachedPayload]>) -> Self {
        let stdout = Arc::clone(&meta.stdout);
        let stderr = Arc::clone(&meta.stderr);
        let now = std::time::Instant::now();
        Self {
            inner: Arc::new(CachedArtifactInner {
                meta,
                stdout,
                stderr,
                payloads: OnceLock::from(payloads),
                access: std::sync::Mutex::new(ArtifactAccess {
                    last_used: now,
                    last_used_wall: std::time::SystemTime::now(),
                    used_in_process: true,
                    published_in_process: true,
                    last_access_checkpoint: Some(now),
                }),
            }),
        }
    }

    /// Create from a freshly compiled `ArtifactData`. Payload mapping is
    /// 1:1 between the protocol `ArtifactPayload` enum and the internal
    /// `CachedPayload` enum.
    pub(super) fn from_artifact_data(artifact: &ArtifactData) -> Self {
        let meta = ArtifactIndex::new(
            artifact.outputs.iter().map(|o| o.name.clone()).collect(),
            artifact
                .outputs
                .iter()
                .map(|o| o.payload.size_bytes())
                .collect(),
            Arc::clone(&artifact.stdout),
            Arc::clone(&artifact.stderr),
            artifact.exit_code,
        );
        Self::with_payloads(
            meta,
            Arc::from(
                artifact
                    .outputs
                    .iter()
                    .map(|o| match &o.payload {
                        ArtifactPayload::Bytes(b) => CachedPayload::Bytes(Arc::clone(b)),
                        ArtifactPayload::Path(p) => CachedPayload::File(p.clone()),
                    })
                    .collect::<Vec<_>>(),
            ),
        )
    }

    /// Create from index metadata and already-created payload files.
    pub(super) fn from_file_payloads(meta: ArtifactIndex, payloads: Vec<NormalizedPath>) -> Self {
        Self::with_payloads(
            meta,
            Arc::from(
                payloads
                    .into_iter()
                    .map(CachedPayload::File)
                    .collect::<Vec<_>>(),
            ),
        )
    }

    /// Create from index metadata and an already-resolved payload list.
    #[cfg(test)]
    pub(crate) fn from_cached_payloads(meta: ArtifactIndex, payloads: Vec<CachedPayload>) -> Self {
        Self::with_payloads(meta, Arc::from(payloads))
    }

    /// Create from index metadata (lazy payloads not loaded yet).
    pub(super) fn from_index(meta: ArtifactIndex) -> Self {
        let stdout = Arc::clone(&meta.stdout);
        let stderr = Arc::clone(&meta.stderr);
        let stored_at = std::time::UNIX_EPOCH
            .checked_add(std::time::Duration::from_secs(meta.stored_at_secs))
            .unwrap_or(std::time::UNIX_EPOCH);
        Self {
            inner: Arc::new(CachedArtifactInner {
                meta,
                stdout,
                stderr,
                payloads: OnceLock::new(),
                access: std::sync::Mutex::new(ArtifactAccess {
                    last_used: std::time::Instant::now(),
                    // Keep durable age in wall-clock form. Reconstructing it as an
                    // Instant fails when the entry predates system uptime on Windows.
                    last_used_wall: stored_at.min(std::time::SystemTime::now()),
                    used_in_process: false,
                    published_in_process: false,
                    last_access_checkpoint: None,
                }),
            }),
        }
    }

    fn access(&self) -> std::sync::MutexGuard<'_, ArtifactAccess> {
        self.access
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn access_snapshot(&self) -> ArtifactAccessSnapshot {
        let access = self.access();
        ArtifactAccessSnapshot {
            last_used: access.last_used,
            last_used_wall: access.last_used_wall,
            used_in_process: access.used_in_process,
            published_in_process: access.published_in_process,
            #[cfg(test)]
            last_access_checkpoint: access.last_access_checkpoint,
        }
    }

    /// Record one hit and return updated index metadata when its durable
    /// checkpoint is due.
    pub(crate) fn record_access(&self, now: std::time::Instant) -> Option<ArtifactIndex> {
        const PERSIST_INTERVAL: Duration = Duration::from_secs(60 * 60);

        let mut access = self.access();
        access.last_used = now;
        access.used_in_process = true;
        if access
            .last_access_checkpoint
            .is_some_and(|checkpoint| now.saturating_duration_since(checkpoint) < PERSIST_INTERVAL)
        {
            return None;
        }
        access.last_access_checkpoint = Some(now);
        let wall_now = std::time::SystemTime::now();
        access.last_used_wall = wall_now;
        drop(access);

        let mut meta = self.meta.clone();
        meta.stored_at_secs = wall_now
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Some(meta)
    }
}

/// Resolve payloads for materialization, translating a resolver failure or a
/// missing-blob outcome into a typed [`MaterializationFailure`] the caller can
/// report and (for genuine blob loss) use as depgraph-invalidation evidence.
pub(super) fn ensure_payloads_for_materialization(
    cached: &CachedArtifact,
    artifact_dir: &Path,
    key_hex: &str,
) -> MaterializationResult<MaterializationPayloads> {
    let existing = cached.payloads.get().cloned();
    let existing_is_staged = existing.as_ref().is_some_and(|payloads| {
        payloads.iter().any(
            |payload| matches!(payload, CachedPayload::File(path) if is_staged_artifact_path(path)),
        )
    });
    let mut staged_guard = if existing_is_staged {
        Some(
            acquire_staged_materialization_guard_for_cached_path(artifact_dir)
                .map_err(|error| cache_read_failure(artifact_dir, error))?,
        )
    } else if existing.is_none() && staged_artifacts_enabled() {
        acquire_staged_materialization_guard_if_present(artifact_dir, key_hex)
            .map_err(|error| cache_read_failure(artifact_dir, error))?
    } else {
        None
    };

    let resolved = if let Some(payloads) = existing {
        Some(payloads)
    } else {
        ensure_payloads_with_staged_policy_result(
            cached,
            artifact_dir,
            key_hex,
            staged_guard.is_some(),
        )
        .map_err(|error| cache_read_failure(artifact_dir, error))?
    };

    match resolved {
        Some(payloads) => {
            let is_staged = payloads.iter().any(
                |payload| matches!(payload, CachedPayload::File(path) if is_staged_artifact_path(path)),
            );
            if is_staged && staged_guard.is_none() {
                // A concurrent resolver may have initialized the shared cell
                // with staged paths after this request chose its lookup lane.
                // Acquire ownership before those canonical paths can escape.
                staged_guard = Some(
                    acquire_staged_materialization_guard_for_cached_path(artifact_dir)
                        .map_err(|error| cache_read_failure(artifact_dir, error))?,
                );
            } else if !is_staged {
                // A pointer may have disappeared before the shared lock was
                // acquired, causing the resolver to fall back to pack/v1.
                // Non-staged payloads must not retain the staged-store lock.
                staged_guard = None;
            }
            Ok(MaterializationPayloads {
                payloads,
                staged_guard,
            })
        }
        None => Err(cache_blob_missing(
            &artifact_dir.join(key_hex),
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "cached artifact metadata or payload is unavailable",
            ),
        )),
    }
}

#[cfg(test)]
pub(super) fn ensure_payloads_with_staged_policy(
    cached: &CachedArtifact,
    artifact_dir: &Path,
    key_hex: &str,
    staged_enabled: bool,
) -> Option<Arc<[CachedPayload]>> {
    ensure_payloads_with_staged_policy_result(cached, artifact_dir, key_hex, staged_enabled)
        .ok()
        .flatten()
}

/// Resolve output payloads through the shared, layout-aware resolver
/// (staged-v2 -> pack -> legacy flat-v1; see `zccache_artifact::layout`), then
/// cache the result in the artifact's lazily-initialized `OnceLock`.
///
/// Keeping resolution behind the immutable `&CachedArtifact` (rather than
/// `&mut`) preserves the short-lock/owned-clone model: a lookup releases its
/// DashMap shard guard before this runs, and concurrent discoveries race
/// harmlessly to initialize the same cell (#1180).
fn ensure_payloads_with_staged_policy_result(
    cached: &CachedArtifact,
    artifact_dir: &Path,
    key_hex: &str,
    staged_enabled: bool,
) -> std::io::Result<Option<Arc<[CachedPayload]>>> {
    if let Some(payloads) = cached.payloads.get() {
        return Ok(Some(Arc::clone(payloads)));
    }

    let Some(resolved) = crate::artifact::resolve_artifact_payloads(
        artifact_dir,
        key_hex,
        &cached.meta.output_sizes,
        staged_enabled,
        "daemon::cached_artifact::ensure_payloads",
    )?
    else {
        return Ok(cached.payloads.get().cloned());
    };

    let loaded: Arc<[CachedPayload]> = Arc::from(
        resolved
            .into_iter()
            .map(|payload| match payload {
                crate::artifact::ResolvedArtifactPayload::File(path) => CachedPayload::File(path),
                crate::artifact::ResolvedArtifactPayload::Bytes(bytes) => {
                    CachedPayload::Bytes(bytes)
                }
            })
            .collect::<Vec<_>>(),
    );

    // Another request may have completed the same discovery concurrently.
    // Whichever result initializes the cell first is canonical; both are
    // derived from the same immutable artifact index and cache files.
    let _ = cached.payloads.set(loaded);
    Ok(cached.payloads.get().cloned())
}

/// Migrate legacy `.meta` files to the in-memory artifact index.
/// Called once on first startup after upgrade.
pub(super) fn migrate_meta_files(
    artifact_dir: &Path,
    artifacts: &DashMap<String, CachedArtifact>,
    store: &ArtifactStore,
) -> usize {
    use rayon::prelude::*;

    // Collect .meta file paths first.
    let meta_paths: Vec<NormalizedPath> = match std::fs::read_dir(artifact_dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path().into())
            .filter(|p: &NormalizedPath| p.extension().and_then(|e| e.to_str()) == Some("meta"))
            .collect(),
        Err(_) => return 0,
    };

    if meta_paths.is_empty() {
        return 0;
    }

    // Parallel phase: read, deserialize, and write data files.
    // Each .meta file is fully independent for I/O.
    let migrated: Vec<(String, CachedArtifact, NormalizedPath)> = meta_paths
        .par_iter()
        .filter_map(|path| {
            let data = std::fs::read(path).ok()?;
            let artifact = bincode::deserialize::<ArtifactData>(&data).ok()?;
            let stem: String = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            // Legacy `.meta` files only ever stored inline bytes. Publish
            // them through the active layout; a forward-compatible `Path`
            // payload makes this entry non-migratable and safe to skip.
            crate::artifact::record_legacy_artifact_access(
                path,
                &stem,
                0,
                crate::artifact::LegacyArtifactAccessPurpose::Migration,
                "cached_artifact::migrate_meta_files:meta_source",
            );
            let payloads: Vec<Arc<Vec<u8>>> = artifact
                .outputs
                .iter()
                .map(|output| output.payload.as_bytes().cloned())
                .collect::<Option<_>>()?;
            persist_migrated_artifact_payloads(artifact_dir, &stem, &payloads).ok()?;

            let cached = CachedArtifact::from_artifact_data(&artifact);
            Some((stem, cached, path.clone()))
        })
        .collect();

    // Sequential phase: insert into the in-memory store and DashMap,
    // then delete the legacy .meta files.
    let count = migrated.len();
    for (stem, cached, meta_path) in migrated {
        store.insert(&stem, &cached.meta);
        artifacts.insert(stem, cached);
        std::fs::remove_file(&meta_path).ok();
    }

    if count > 0 {
        tracing::info!(count, "migrated legacy .meta files to artifact index");
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lazy_artifact(payload_size: u64) -> CachedArtifact {
        CachedArtifact::from_index(ArtifactIndex::new(
            vec!["output.o".to_string()],
            vec![payload_size],
            Arc::new(Vec::new()),
            Arc::new(Vec::new()),
            0,
        ))
    }

    #[test]
    fn owned_clones_share_lazy_payload_initialization() {
        let dir = tempfile::tempdir().unwrap();
        // Artifact keys are validated as bounded hex strings by the central
        // resolver (`zccache_artifact::layout::validate_key`); a
        // human-readable slug like "shared-cell" is rejected before the
        // resolver even looks at disk, which used to be masked by the
        // pre-#1180 ad-hoc path formatting this test predates.
        let key = "1".repeat(64);
        let key = key.as_str();
        std::fs::write(dir.path().join(format!("{key}_0")), b"payload").unwrap();
        let cached = lazy_artifact(7);
        let owned_clone = cached.clone();

        let first = ensure_payloads_with_staged_policy(&cached, dir.path(), key, false).unwrap();
        let second =
            ensure_payloads_with_staged_policy(&owned_clone, dir.path(), key, false).unwrap();

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn perf_owned_clone_is_one_arc_refcount_and_does_not_pin_dashmap() {
        let artifacts = DashMap::new();
        artifacts.insert("key", lazy_artifact(7));
        let owned = {
            let entry = artifacts.get("key").unwrap();
            entry.value().clone()
        };
        let before = Arc::strong_count(&owned.inner);
        let clones: Vec<_> = (0..1_024).map(|_| owned.clone()).collect();

        assert_eq!(Arc::strong_count(&owned.inner), before + clones.len());
        assert!(
            clones
                .iter()
                .all(|clone| Arc::ptr_eq(&owned.inner, &clone.inner)),
            "owned lookup clones must not duplicate ArtifactIndex vectors"
        );
        assert!(
            artifacts.remove("key").is_some(),
            "an owned clone must not retain a DashMap shard guard"
        );
    }

    #[test]
    fn owned_clones_share_access_updates() {
        let cached = lazy_artifact(0);
        let owned_clone = cached.clone();
        assert!(!cached.access_snapshot().used_in_process);

        let now = std::time::Instant::now();
        let persisted = owned_clone.record_access(now);

        assert!(persisted.is_some());
        let access = cached.access_snapshot();
        assert!(access.used_in_process);
        assert_eq!(access.last_used, now);
        assert!(access.last_access_checkpoint.is_some());
    }

    #[test]
    fn failed_payload_discovery_can_be_retried() {
        let dir = tempfile::tempdir().unwrap();
        // See `owned_clones_share_lazy_payload_initialization` above: the
        // key must be a bounded hex string to satisfy `layout::validate_key`.
        let key = "2".repeat(64);
        let key = key.as_str();
        let cached = lazy_artifact(7);

        assert!(ensure_payloads_with_staged_policy(&cached, dir.path(), key, false).is_none());
        std::fs::write(dir.path().join(format!("{key}_0")), b"payload").unwrap();
        assert!(ensure_payloads_with_staged_policy(&cached, dir.path(), key, false).is_some());
    }

    #[test]
    fn staged_materialization_lease_blocks_clear_until_delivery_finishes() {
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("artifacts");
        std::fs::create_dir_all(&artifact_dir).unwrap();
        let source = dir.path().join("source.rlib");
        let bytes = b"staged materialization ownership";
        std::fs::write(&source, bytes).unwrap();
        let key = "3".repeat(64);
        persist_staged_artifact_paths(&artifact_dir, &key, &[source.into()]).unwrap();
        let cached = lazy_artifact(bytes.len() as u64);

        let payloads = ensure_payloads_for_materialization(&cached, &artifact_dir, &key).unwrap();
        assert!(
            payloads.staged_lock_timings().is_some(),
            "a staged payload must carry its store-lock lease"
        );
        let staged_generation_path = match &payloads[0] {
            CachedPayload::File(path) => path.clone(),
            CachedPayload::Bytes(_) => panic!("staged resolver returned an inline payload"),
        };

        let clear_hook =
            StagedHookGuard::arm(&artifact_dir, StagedHookPoint::MaintenanceStoreLockPending);
        let (clear_done_tx, clear_done_rx) = mpsc::sync_channel(1);
        let clear_artifact_dir = artifact_dir.clone();
        let clear = std::thread::spawn(move || {
            let result = clear_staged_artifacts(&clear_artifact_dir);
            let _ = clear_done_tx.send(result);
        });
        clear_hook.wait_until_reached();
        clear_hook.resume();
        assert!(
            matches!(
                clear_done_rx.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "exclusive clear must wait while requested-output delivery owns the shared lease"
        );

        let destination: NormalizedPath = dir.path().join("restored.rlib").into();
        write_payloads_par_observed(std::slice::from_ref(&destination), &payloads).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), bytes);

        drop(payloads);
        clear_done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("clear did not resume after the materialization lease was released")
            .unwrap();
        clear.join().unwrap();
        assert!(
            !staged_generation_path.exists(),
            "clear must proceed promptly after the lease is released"
        );
    }

    #[test]
    fn staged_materialization_lease_blocks_snapshot_eviction_until_delivery_finishes() {
        use std::collections::HashMap;
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("artifacts");
        std::fs::create_dir_all(&artifact_dir).unwrap();
        let source = dir.path().join("source.rmeta");
        let bytes = b"snapshot eviction ownership";
        std::fs::write(&source, bytes).unwrap();
        let key = "6".repeat(64);
        persist_staged_artifact_paths(&artifact_dir, &key, &[source.into()]).unwrap();
        let cached = lazy_artifact(bytes.len() as u64);
        let payloads = ensure_payloads_for_materialization(&cached, &artifact_dir, &key).unwrap();
        let generation = std::fs::read_to_string(
            artifact_dir
                .join(".staged-v2")
                .join(format!("{key}.current")),
        )
        .unwrap();
        let expected = HashMap::from([(key.clone(), Some(generation.trim().to_string()))]);

        let eviction_hook =
            StagedHookGuard::arm(&artifact_dir, StagedHookPoint::MaintenanceStoreLockPending);
        let (eviction_done_tx, eviction_done_rx) = mpsc::sync_channel(1);
        let eviction_artifact_dir = artifact_dir.clone();
        let eviction = std::thread::spawn(move || {
            let result = evict_staged_artifact_keys_if_unchanged(&eviction_artifact_dir, &expected);
            let _ = eviction_done_tx.send(result);
        });
        eviction_hook.wait_until_reached();
        eviction_hook.resume();
        assert!(
            matches!(
                eviction_done_rx.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "snapshot eviction must wait while a hit owns the staged generation"
        );

        let destination: NormalizedPath = dir.path().join("restored.rmeta").into();
        write_payloads_par_observed(std::slice::from_ref(&destination), &payloads).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), bytes);
        drop(payloads);

        let removed = eviction_done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("eviction did not resume after the materialization lease was released")
            .unwrap();
        assert_eq!(removed, std::collections::HashSet::from([key]));
        eviction.join().unwrap();
    }

    #[test]
    fn non_staged_payloads_do_not_take_the_staged_store_lock() {
        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("artifacts");
        std::fs::create_dir_all(&artifact_dir).unwrap();
        let key = "4".repeat(64);
        let legacy = artifact_dir.join(format!("{key}_0"));
        std::fs::write(&legacy, b"legacy").unwrap();
        let cached = lazy_artifact(6);

        let payloads = ensure_payloads_for_materialization(&cached, &artifact_dir, &key).unwrap();
        assert!(
            payloads.staged_lock_timings().is_none(),
            "legacy file payloads must not acquire the staged-store lock"
        );

        let inline = CachedArtifact::from_cached_payloads(
            ArtifactIndex::new(
                vec!["inline.o".to_string()],
                vec![6],
                Arc::new(Vec::new()),
                Arc::new(Vec::new()),
                0,
            ),
            vec![CachedPayload::Bytes(Arc::new(b"inline".to_vec()))],
        );
        let inline_payloads =
            ensure_payloads_for_materialization(&inline, &artifact_dir, &"5".repeat(64)).unwrap();
        assert!(
            inline_payloads.staged_lock_timings().is_none(),
            "byte-backed payloads must not acquire the staged-store lock"
        );
    }

    #[test]
    fn every_cache_hit_delivery_path_uses_the_typed_materialization_lease() {
        let consumers = [
            (
                "single compile",
                include_str!("handle_compile/cached_hit.rs"),
            ),
            ("multi compile", include_str!("handle_compile_multi.rs")),
            ("exact exec", include_str!("handle_exec.rs")),
            ("link and directory bundle", include_str!("handle_link.rs")),
        ];
        for (name, source) in consumers {
            assert!(
                source.contains("ensure_payloads_for_materialization("),
                "{name} bypasses the typed staged materialization lease"
            );
        }
        assert!(
            include_str!("handle_link.rs")
                .contains("materialize_directory_payload(&payloads[0], &output_path)"),
            "directory-bundle unpacking must consume payloads from the leased resolver"
        );
    }
}
