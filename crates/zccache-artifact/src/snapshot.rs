//! Immutable compiler-store snapshots for external cache transport.
//! The backend retains its index and staged layout; callers provide only a
//! compatibility identity and a new publication path, never internal paths.

use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

use crate::{ArtifactStore, StagedReadGuard};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotManifest {
    schema: u32,
    compatibility: String,
    index_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    depgraph_digest: Option<String>,
}

/// Completed backend publication, separate from test-result evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SnapshotReceipt {
    pub entries: usize,
    pub outputs: usize,
}

/// Export a complete store to a new immutable snapshot directory.
/// The caller must quiesce writers and flush or supply the authoritative index.
/// A failed export must leave every previously committed snapshot intact.
pub fn export_snapshot(
    store: &ArtifactStore,
    artifact_dir: &Path,
    compatibility: &str,
    destination: &Path,
) -> io::Result<SnapshotReceipt> {
    validate_compatibility(compatibility)?;
    let _guard = StagedReadGuard::acquire_if_present(artifact_dir)?;
    publish_snapshot(
        store,
        artifact_dir,
        compatibility,
        destination,
        crate::layout::SnapshotLayout::NormalizeLegacy,
        None,
    )
}

/// Export a flushed store from disk, refusing a live daemon's writer lease.
pub fn export_store_snapshot(
    cache_root: &Path,
    compatibility: &str,
    destination: &Path,
) -> io::Result<SnapshotReceipt> {
    validate_compatibility(compatibility)?;
    let _writer = zccache_core::cache_root_lock::CacheRootWriterLock::acquire(cache_root)?;
    let index = cache_root.join("index.bin");
    let store = ArtifactStore::from_snapshot(&index, &fs::read(&index)?)?;
    let graph = match fs::read(cache_root.join("depgraph/depgraph.bin")) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let artifacts = cache_root.join("artifacts");
    let _guard = StagedReadGuard::acquire_if_present(&artifacts)?;
    publish_snapshot(
        &store,
        &artifacts,
        compatibility,
        destination,
        crate::layout::SnapshotLayout::NormalizeLegacy,
        graph.as_deref(),
    )
}

fn publish_snapshot(
    store: &ArtifactStore,
    artifact_dir: &Path,
    compatibility: &str,
    destination: &Path,
    layout: crate::layout::SnapshotLayout,
    depgraph: Option<&[u8]>,
) -> io::Result<SnapshotReceipt> {
    crate::staged_lock::validate_staged_root_path(
        crate::staged_lock::staged_root(artifact_dir).as_path(),
    )?;
    // Caller-controlled destination parent; never replace a committed snapshot.
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "snapshot already exists",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error),
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let pending = tempfile::Builder::new()
        .prefix(".compiler-snapshot-")
        .tempdir_in(parent)?;
    let depgraph_digest = if let Some(bytes) = depgraph {
        let directory = pending.path().join("depgraph");
        fs::create_dir(&directory)?;
        let path = directory.join("depgraph.bin");
        let mut file = fs::File::create(&path)?;
        std::io::Write::write_all(&mut file, bytes)?;
        file.sync_all()?;
        drop(file);
        // The dependency-graph owner validates its format. Transport does
        // not duplicate its decoder, version handling or context semantics.
        zccache_depgraph::load_from_file(&path).map_err(invalid)?;
        kernal_api::platform::fs::sync_directory_if_supported(&directory)?;
        Some(kernal_api::hash::blake3_bytes(bytes).to_hex().to_string())
    } else {
        None
    };
    let rows = store.load_all();
    let copied = ArtifactStore::open_empty(&pending.path().join("index.bin"));
    let target = pending.path().join("artifacts");
    fs::create_dir_all(&target)?;
    let mut outputs = 0;
    for (key, meta) in &rows {
        if meta.output_names.len() != meta.output_sizes.len()
            || (!meta.output_modes.is_empty() && meta.output_modes.len() != meta.output_sizes.len())
        {
            return Err(invalid("compiler output metadata count mismatch"));
        }
        crate::layout::copy_artifact_generation(
            artifact_dir,
            key,
            &meta.output_sizes,
            &target,
            &layout,
        )?;
        copied.insert(key, meta);
        outputs += meta.output_sizes.len();
    }
    copied.flush()?;
    let index = pending.path().join("index.bin");
    fs::OpenOptions::new()
        .write(true)
        .open(&index)?
        .sync_all()?;
    let manifest = SnapshotManifest {
        schema: 1,
        compatibility: compatibility.into(),
        index_digest: index_digest(&index)?,
        depgraph_digest,
    };
    let manifest_path = pending.path().join("snapshot.json");
    let mut manifest_file = fs::File::create(&manifest_path)?;
    std::io::Write::write_all(
        &mut manifest_file,
        &serde_json::to_vec(&manifest).map_err(invalid)?,
    )?;
    manifest_file.sync_all()?;
    // Windows refuses a directory rename while a child file is still open.
    drop(manifest_file);
    kernal_api::platform::fs::sync_directory_if_supported(&target)?;
    kernal_api::platform::fs::sync_directory_if_supported(pending.path())?;
    kernal_api::platform::fs::replacement::rename_generation(pending.path(), destination)?;
    kernal_api::platform::fs::sync_directory_if_supported(parent)?;
    Ok(SnapshotReceipt {
        entries: rows.len(),
        outputs,
    })
}

/// Validate every incoming payload before importing into a quiesced store.
/// Local rows win collisions. A writer lease excludes daemons; publication
/// adds payloads first, the union graph second, and the union index last.
pub fn import_snapshot(
    source: &Path,
    compatibility: &str,
    destination: &Path,
) -> io::Result<SnapshotReceipt> {
    import_snapshot_before_commit(source, compatibility, destination, || Ok(()))
}

fn import_snapshot_before_commit(
    source: &Path,
    compatibility: &str,
    destination: &Path,
    before_commit: impl FnOnce() -> io::Result<()>,
) -> io::Result<SnapshotReceipt> {
    validate_compatibility(compatibility)?;
    let manifest: SnapshotManifest =
        serde_json::from_slice(&fs::read(source.join("snapshot.json"))?).map_err(invalid)?;
    if manifest.schema != 1 || manifest.compatibility != compatibility {
        return Err(invalid("snapshot schema or compatibility mismatch"));
    }
    let index = source.join("index.bin");
    let bytes = fs::read(&index)?;
    if kernal_api::hash::blake3_bytes(&bytes).to_hex().as_str() != manifest.index_digest.as_str() {
        return Err(invalid("snapshot index digest mismatch"));
    }
    let store = ArtifactStore::from_snapshot(&index, &bytes)?;
    let depgraph = manifest
        .depgraph_digest
        .map(|expected| {
            let bytes = fs::read(source.join("depgraph/depgraph.bin"))?;
            if kernal_api::hash::blake3_bytes(&bytes).to_hex().as_str() != expected.as_str() {
                return Err(invalid("snapshot dependency-context digest mismatch"));
            }
            Ok(bytes)
        })
        .transpose()?;
    // Completed transport snapshots are immutable and have no maintenance
    // writer. Do not create/open a writable store lock in the source snapshot.
    let parent = destination.parent().filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    validate_import_destination(source, destination)?;
    fs::create_dir_all(parent)?;
    let pending = tempfile::Builder::new().prefix(".compiler-import-").tempdir_in(parent)?;
    let validated = pending.path().join("validated");
    let receipt = publish_snapshot(
        &store,
        &source.join("artifacts"),
        compatibility,
        &validated,
        crate::layout::SnapshotLayout::StagedOnly,
        depgraph.as_deref(),
    )?;
    let _writer = zccache_core::cache_root_lock::CacheRootWriterLock::acquire(destination)?;
    let index = destination.join("index.bin");
    let local = match fs::read(&index) {
        Ok(bytes) => ArtifactStore::from_snapshot(&index, &bytes)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => ArtifactStore::open_empty(&index),
        Err(error) => return Err(error),
    };
    let existing: std::collections::HashSet<_> = local.load_all().into_iter()
        .map(|(key, _)| key).collect();
    let added: Vec<_> = store.load_all().into_iter()
        .filter(|(key, _)| !existing.contains(key)).collect();
    if added.is_empty() {
        if !index.exists() {
            local.flush()?;
        }
        return Ok(receipt);
    }
    let graph_dir = destination.join("depgraph");
    let graph_path = graph_dir.join("depgraph.bin");
    let graph = merge_store_graph(&validated, &graph_path, &added)?;
    let artifacts = destination.join("artifacts");
    let _staged = StagedReadGuard::acquire(&artifacts)?;
    for (key, meta) in &added {
        crate::layout::copy_artifact_generation(
            &validated.join("artifacts"), key, &meta.output_sizes, &artifacts,
            &crate::layout::SnapshotLayout::StagedOnly,
        )?;
        local.insert(key, meta);
    }
    if let Some(graph) = graph {
        let prepared = pending.path().join("union-graph.bin");
        let options = zccache_depgraph::snapshot::SaveOptions {
            ttl: std::time::Duration::MAX,
            budget_bytes: u64::MAX,
            ..Default::default()
        };
        zccache_depgraph::snapshot::save_to_file_with(&graph, &prepared, &options).map_err(invalid)?;
        fs::OpenOptions::new().write(true).open(&prepared)?.sync_all()?;
        fs::create_dir_all(&graph_dir)?;
        kernal_api::platform::fs::replacement::atomic_replace(&prepared, &graph_path)?;
        kernal_api::platform::fs::sync_directory_if_supported(&graph_dir)?;
    }
    // Until this atomic index commit, interrupted imports leave the old
    // indexed objects and contexts intact. Unindexed additions only miss.
    before_commit()?;
    local.flush()?;
    Ok(receipt)
}

fn validate_import_destination(source: &Path, destination: &Path) -> io::Result<()> {
    if destination.components().any(|component| matches!(component, std::path::Component::ParentDir)) {
        return Err(invalid("compiler store destination contains a parent component"));
    }
    let source = fs::canonicalize(source)?;
    let mut ancestor = destination;
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) => {
                if ancestor == destination && !meta.is_dir() {
                    return Err(invalid("compiler store destination is not a directory"));
                }
                let resolved = fs::canonicalize(ancestor)?;
                if resolved.starts_with(&source)
                    || (ancestor == destination && source.starts_with(&resolved))
                {
                    return Err(invalid("snapshot source and compiler store overlap"));
                }
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                ancestor = ancestor.parent().filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
            }
            Err(error) => return Err(error),
        }
    }
}

fn merge_store_graph(
    source: &Path,
    destination: &Path,
    added: &[(String, crate::ArtifactIndex)],
) -> io::Result<Option<zccache_depgraph::DepGraph>> {
    use zccache_depgraph::{snapshot::{load_from_file_with, LoadOptions}, DepGraph};
    let options = LoadOptions { ttl: std::time::Duration::MAX, ..Default::default() };
    let incoming_path = source.join("depgraph/depgraph.bin");
    if !incoming_path.exists() {
        return Ok(None);
    }
    let incoming = load_from_file_with(&incoming_path, &options).map_err(invalid)?;
    let local = if destination.exists() {
        load_from_file_with(destination, &options).map_err(invalid)?
    } else {
        DepGraph::new()
    };
    let keys: std::collections::HashSet<_> = added.iter().map(|(key, _)| key.as_str()).collect();
    Ok(Some(local.merge_missing(&incoming, |key| {
        keys.contains(zccache_hash::ContentHash::from_bytes(*key).to_hex().as_str())
    })))
}

fn validate_compatibility(identity: &str) -> io::Result<()> {
    if identity.len() != 64
        || !identity
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return Err(invalid(
            "compatibility identity must be 64 lowercase hex digits",
        ));
    }
    Ok(())
}

fn index_digest(path: &Path) -> io::Result<String> {
    let mut digest = kernal_api::hash::Blake3Hasher::new();
    digest.update(&fs::read(path)?);
    Ok(digest.finalize().to_hex().to_string())
}

fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod tests;
