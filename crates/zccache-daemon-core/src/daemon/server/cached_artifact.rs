//! In-memory artifact records and on-disk payload materialization helpers.
//!
//! `CachedArtifact` is the daemon's per-key view of a cached compilation:
//! metadata plus either resident bytes or pointers to on-disk payload files.
//! `ensure_payloads` lazily resolves the payload slice from the artifact dir,
//! and `migrate_meta_files` upgrades legacy `.meta` blobs to the redb-backed
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

struct ArtifactAccess {
    last_used: std::time::Instant,
    last_used_wall: std::time::SystemTime,
    used_in_process: bool,
    last_access_checkpoint: Option<std::time::Instant>,
}

pub(crate) struct ArtifactAccessSnapshot {
    pub(crate) last_used: std::time::Instant,
    pub(crate) last_used_wall: std::time::SystemTime,
    pub(crate) used_in_process: bool,
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
    pub(crate) fn from_cached_payloads(
        meta: ArtifactIndex,
        payloads: Vec<CachedPayload>,
    ) -> Self {
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
            #[cfg(test)]
            last_access_checkpoint: access.last_access_checkpoint,
        }
    }

    /// Record one hit and return updated index metadata when its durable
    /// checkpoint is due.
    pub(crate) fn record_access(
        &self,
        now: std::time::Instant,
    ) -> Option<ArtifactIndex> {
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

/// Load output payloads from `{key}_0`, `{key}_1`, ... files on disk.
///
/// Returns the payload slice, or `None` if any data file is missing
/// (indicating corruption or eviction — caller should treat as cache miss).
pub(super) fn ensure_payloads(
    cached: &CachedArtifact,
    artifact_dir: &Path,
    key_hex: &str,
) -> Option<Arc<[CachedPayload]>> {
    ensure_payloads_with_staged_policy(cached, artifact_dir, key_hex, staged_artifacts_enabled())
}

pub(super) fn ensure_payloads_with_staged_policy(
    cached: &CachedArtifact,
    artifact_dir: &Path,
    key_hex: &str,
    staged_enabled: bool,
) -> Option<Arc<[CachedPayload]>> {
    if let Some(payloads) = cached.payloads.get() {
        return Some(Arc::clone(payloads));
    }

    let loaded: Arc<[CachedPayload]> = if staged_enabled {
        match load_staged_artifact_paths(artifact_dir, key_hex, &cached.meta.output_sizes) {
            Ok(Some(payloads)) => Arc::from(
                payloads
                    .into_iter()
                    .map(CachedPayload::File)
                    .collect::<Vec<_>>(),
            ),
            Ok(None) => load_legacy_payloads(cached, artifact_dir, key_hex)
                .or_else(|| cached.payloads.get().cloned())?,
            Err(_) => return cached.payloads.get().cloned(),
        }
    } else {
        load_legacy_payloads(cached, artifact_dir, key_hex)
            .or_else(|| cached.payloads.get().cloned())?
    };

    // Another request may have completed the same discovery concurrently.
    // Whichever result initializes the cell first is canonical; both are
    // derived from the same immutable artifact index and cache files.
    let _ = cached.payloads.set(loaded);
    cached.payloads.get().cloned()
}

fn load_legacy_payloads(
    cached: &CachedArtifact,
    artifact_dir: &Path,
    key_hex: &str,
) -> Option<Arc<[CachedPayload]>> {
    let mut payloads = Vec::with_capacity(cached.meta.output_names.len());
    for i in 0..cached.meta.output_names.len() {
        let path = artifact_dir.join(format!("{key_hex}_{i}"));
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.is_file()
                && cached
                    .meta
                    .output_sizes
                    .get(i)
                    .is_none_or(|expected| *expected == meta.len())
            {
                payloads.push(CachedPayload::File(path.into()));
                continue;
            }
        }
        // Fallback: artifact may be stored in a `.pack` file (pack mode).
        let bytes = try_load_packed_payload(artifact_dir, key_hex, i)?;
        if let Some(expected) = cached.meta.output_sizes.get(i) {
            if *expected != bytes.len() as u64 {
                return None;
            }
        }
        payloads.push(CachedPayload::Bytes(Arc::new(bytes)));
    }
    Some(Arc::from(payloads))
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

            // Write {key}_0, {key}_1, ... data files if missing.
            // Legacy `.meta` files only ever stored inline bytes, so we
            // only handle the `Bytes` variant here. Any `Path` variant
            // would be a forward-compat artefact that legacy migration
            // can safely skip — caller treats failures as non-cacheable.
            for (i, out) in artifact.outputs.iter().enumerate() {
                let data_path = artifact_dir.join(format!("{stem}_{i}"));
                if !data_path.exists() {
                    if let Some(bytes) = out.payload.as_bytes() {
                        std::fs::write(&data_path, bytes.as_slice()).ok();
                    }
                }
            }

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
        let key = "shared-cell";
        std::fs::write(dir.path().join(format!("{key}_0")), b"payload").unwrap();
        let cached = lazy_artifact(7);
        let owned_clone = cached.clone();

        let first =
            ensure_payloads_with_staged_policy(&cached, dir.path(), key, false).unwrap();
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
        let key = "retry-cell";
        let cached = lazy_artifact(7);

        assert!(
            ensure_payloads_with_staged_policy(&cached, dir.path(), key, false).is_none()
        );
        std::fs::write(dir.path().join(format!("{key}_0")), b"payload").unwrap();
        assert!(
            ensure_payloads_with_staged_policy(&cached, dir.path(), key, false).is_some()
        );
    }
}
