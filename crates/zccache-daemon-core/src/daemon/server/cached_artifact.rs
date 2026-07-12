//! In-memory artifact records and on-disk payload materialization helpers.
//!
//! `CachedArtifact` is the daemon's per-key view of a cached compilation:
//! metadata plus either resident bytes or pointers to on-disk payload files.
//! `ensure_payloads` lazily resolves the payload slice from the artifact dir,
//! and `migrate_meta_files` upgrades legacy `.meta` blobs to the redb-backed
//! `ArtifactStore` on first startup after an upgrade.

use super::*;

#[derive(Clone)]
pub(crate) enum CachedPayload {
    /// Payload bytes already resident in memory.
    Bytes(Arc<Vec<u8>>),
    /// Payload bytes are available in a cache file.
    File(NormalizedPath),
}

#[derive(Clone)]
/// Cached compilation artifact with lazy payload loading.
///
/// Metadata (output names, sizes, stdout, stderr, exit code) is always in
/// memory after startup. Output payloads are either already in memory or are
/// represented by cache files so hits can hardlink without eager reads.
pub(crate) struct CachedArtifact {
    pub(crate) meta: ArtifactIndex,
    /// Arc-wrapped stdout/stderr for cheap IPC response clones.
    pub(crate) stdout: Arc<Vec<u8>>,
    pub(crate) stderr: Arc<Vec<u8>>,
    /// Lazily-resolved output payloads. `None` = not yet checked on disk.
    /// Arc-wrapped so cache-hit clones are O(1) refcount bumps.
    pub(crate) payloads: Option<Arc<[CachedPayload]>>,
    /// When this artifact was last used (inserted or returned as a hit).
    pub(crate) last_used: std::time::Instant,
}

impl CachedArtifact {
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
        Self {
            meta,
            stdout: Arc::clone(&artifact.stdout),
            stderr: Arc::clone(&artifact.stderr),
            payloads: Some(Arc::from(
                artifact
                    .outputs
                    .iter()
                    .map(|o| match &o.payload {
                        ArtifactPayload::Bytes(b) => CachedPayload::Bytes(Arc::clone(b)),
                        ArtifactPayload::Path(p) => CachedPayload::File(p.clone()),
                    })
                    .collect::<Vec<_>>(),
            )),
            last_used: std::time::Instant::now(),
        }
    }

    /// Create from index metadata and already-created payload files.
    pub(super) fn from_file_payloads(meta: ArtifactIndex, payloads: Vec<NormalizedPath>) -> Self {
        let stdout = Arc::clone(&meta.stdout);
        let stderr = Arc::clone(&meta.stderr);
        Self {
            meta,
            stdout,
            stderr,
            payloads: Some(Arc::from(
                payloads
                    .into_iter()
                    .map(CachedPayload::File)
                    .collect::<Vec<_>>(),
            )),
            last_used: std::time::Instant::now(),
        }
    }

    /// Create from index metadata (lazy payloads not loaded yet).
    pub(super) fn from_index(meta: ArtifactIndex) -> Self {
        let stdout = Arc::clone(&meta.stdout);
        let stderr = Arc::clone(&meta.stderr);
        Self {
            meta,
            stdout,
            stderr,
            payloads: None,
            last_used: std::time::Instant::now(),
        }
    }
}

/// Load output payloads from `{key}_0`, `{key}_1`, ... files on disk.
///
/// Returns the payload slice, or `None` if any data file is missing
/// (indicating corruption or eviction — caller should treat as cache miss).
pub(super) fn ensure_payloads<'a>(
    cached: &'a mut CachedArtifact,
    artifact_dir: &Path,
    key_hex: &str,
) -> Option<&'a [CachedPayload]> {
    ensure_payloads_with_staged_policy(cached, artifact_dir, key_hex, staged_artifacts_enabled())
}

pub(super) fn ensure_payloads_with_staged_policy<'a>(
    cached: &'a mut CachedArtifact,
    artifact_dir: &Path,
    key_hex: &str,
    staged_enabled: bool,
) -> Option<&'a [CachedPayload]> {
    if cached.payloads.is_none() {
        if staged_enabled {
            match load_staged_artifact_paths(artifact_dir, key_hex, &cached.meta.output_sizes) {
                Ok(Some(payloads)) => {
                    cached.payloads = Some(Arc::from(
                        payloads
                            .into_iter()
                            .map(CachedPayload::File)
                            .collect::<Vec<_>>(),
                    ));
                    return cached.payloads.as_deref();
                }
                Ok(None) => {}
                Err(_) => return None,
            }
        }
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
        cached.payloads = Some(Arc::from(payloads));
    }
    cached.payloads.as_deref()
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
