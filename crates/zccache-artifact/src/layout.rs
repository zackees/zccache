//! Shared read resolver for every on-disk artifact layout.
//!
//! The daemon and offline `zccache warm` command both consume this module so
//! runtime callers never reconstruct flat-v1 filenames or partially parse a
//! staged generation.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;
use zccache_core::NormalizedPath;

use crate::staged_lock::STAGED_ROOT;

const STAGED_MANIFEST_VERSION: u32 = 1;
const PACK_MAGIC: &[u8; 4] = b"ZCPK";
pub const LEGACY_PATH_VALIDATE_ENV: &str = "ZCCACHE_LEGACY_PATH_VALIDATE";

#[derive(Clone, Copy, Debug)]
pub enum LegacyArtifactAccessPurpose {
    CompatibilityRead,
    LegacyWrite,
    Migration,
    EvictionScan,
}

impl LegacyArtifactAccessPurpose {
    const fn as_str(self) -> &'static str {
        match self {
            Self::CompatibilityRead => "compatibility_read",
            Self::LegacyWrite => "legacy_write",
            Self::Migration => "migration",
            Self::EvictionScan => "eviction_scan",
        }
    }
}

#[derive(Clone, Debug)]
pub enum ResolvedArtifactPayload {
    File(NormalizedPath),
    Bytes(Arc<Vec<u8>>),
}

#[derive(Debug, Deserialize, Serialize)]
struct StagedManifest {
    version: u32,
    key_hex: String,
    generation_hex: String,
    outputs: Vec<StagedOutput>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StagedOutput {
    index: usize,
    size: u64,
    digest_hex: String,
}

/// Resolve the payloads for one artifact, preferring staged-v2 and retaining
/// pack and flat-v1 compatibility.
pub fn resolve_artifact_payloads(
    artifact_dir: &Path,
    key_hex: &str,
    expected_sizes: &[u64],
    include_staged: bool,
    call_site: &'static str,
) -> io::Result<Option<Vec<ResolvedArtifactPayload>>> {
    validate_key(key_hex)?;
    if include_staged {
        if let Some(paths) = resolve_staged_artifact_files(artifact_dir, key_hex, expected_sizes)? {
            return Ok(Some(
                paths
                    .into_iter()
                    .map(ResolvedArtifactPayload::File)
                    .collect(),
            ));
        }
    }

    // A pack is a complete artifact representation. Resolve it before
    // constructing any flat-v1 candidates so a healthy pack-only cache never
    // enters the legacy compatibility branch (or trips strict legacy-path
    // validation).
    if let Some(payloads) = resolve_pack_payloads(artifact_dir, key_hex, expected_sizes)? {
        return Ok(Some(payloads));
    }

    let mut payloads = Vec::with_capacity(expected_sizes.len());
    for (index, expected_size) in expected_sizes.iter().copied().enumerate() {
        let legacy: NormalizedPath = artifact_dir.join(format!("{key_hex}_{index}")).into();
        record_legacy_artifact_access(
            &legacy,
            key_hex,
            index,
            LegacyArtifactAccessPurpose::CompatibilityRead,
            call_site,
        );
        match fs::metadata(&legacy) {
            Ok(metadata) if metadata.is_file() && metadata.len() == expected_size => {
                payloads.push(ResolvedArtifactPayload::File(legacy));
            }
            _ => return Ok(None),
        }
    }
    Ok(Some(payloads))
}

/// Record a reviewed flat-v1 construction/access in strict validation mode.
pub fn record_legacy_artifact_access(
    path: &Path,
    key_hex: &str,
    index: usize,
    purpose: LegacyArtifactAccessPurpose,
    call_site: &'static str,
) {
    if legacy_path_validation_enabled() {
        let event = serde_json::json!({
            "path": path.display().to_string(),
            "artifact_key": key_hex,
            "output_index": index,
            "purpose": purpose.as_str(),
            "call_site": call_site,
        });
        if let Some(cache_root) = path.parent().and_then(Path::parent) {
            zccache_core::lifecycle::write_event_in_cache_root(
                cache_root,
                zccache_core::lifecycle::EVENT_LEGACY_ARTIFACT_PATH_ACCESSED,
                event,
            );
        } else {
            zccache_core::lifecycle::write_event(
                zccache_core::lifecycle::EVENT_LEGACY_ARTIFACT_PATH_ACCESSED,
                event,
            );
        }
    }
}

fn legacy_path_validation_enabled() -> bool {
    std::env::var(LEGACY_PATH_VALIDATE_ENV)
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
}

/// The staged-v2 generation currently published for one artifact key, with
/// its manifest already checked for identity, version, and self-consistency
/// (the generation hex is a digest over the manifest's own output rows).
///
/// The payload files themselves are *not* verified yet — that is
/// [`verify_generation_outputs`], which is the expensive part (a full blake3
/// read of every output).
struct PublishedGeneration {
    /// Normalized rather than `std::path::PathBuf`: `ban_std_pathbuf` is a
    /// workspace lint, and the paths derived from this field are already
    /// normalized at their use sites.
    dir: NormalizedPath,
    outputs: Vec<StagedOutput>,
}

/// Read `<artifact_dir>/.staged-v2/<key>.current` and the manifest it points
/// at, rejecting anything whose identity or self-digest does not check out.
///
/// `Ok(None)` means "this key has no published staged generation" — the
/// caller falls back to pack / flat-v1. Every other failure is an error, so a
/// tampered or truncated manifest is loud rather than silently skipped.
fn load_published_generation(
    artifact_dir: &Path,
    key_hex: &str,
) -> io::Result<Option<PublishedGeneration>> {
    validate_key(key_hex)?;
    let root = artifact_dir.join(STAGED_ROOT);
    let pointer = root.join(format!("{key_hex}.current"));
    let generation_hex = match fs::read_to_string(pointer) {
        Ok(value) => value.trim().to_string(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    validate_generation(&generation_hex)?;
    let dir: NormalizedPath = root.join(key_hex).join(&generation_hex).into();
    let manifest = load_manifest(&dir.join("manifest.bin"), key_hex, &generation_hex)?;
    if generation_digest(key_hex, &manifest.outputs) != generation_hex {
        return Err(invalid_data(
            "staged generation digest does not match its manifest",
        ));
    }
    Ok(Some(PublishedGeneration {
        dir,
        outputs: manifest.outputs,
    }))
}

/// Verify every payload file in a published generation against its manifest
/// row (size then blake3 digest) and return the output paths in index order.
///
/// Also enforces that the manifest's indices are exactly `0..outputs.len()`
/// with no repeats, so a caller can index the returned slice positionally.
fn verify_generation_outputs(generation: &PublishedGeneration) -> io::Result<Vec<NormalizedPath>> {
    let mut seen = vec![false; generation.outputs.len()];
    let mut paths = Vec::with_capacity(generation.outputs.len());
    for output in &generation.outputs {
        if output.index >= seen.len() || seen[output.index] {
            return Err(invalid_data(
                "staged manifest has a duplicate or out-of-range output index",
            ));
        }
        seen[output.index] = true;
        // `dir` is already a `NormalizedPath`, so `join` yields one — the
        // `.into()` this replaced became a no-op when the field changed type.
        let path = generation.dir.join(format!("output-{}", output.index));
        if fs::metadata(&path)?.len() != output.size {
            return Err(invalid_data(
                "staged output size does not match its manifest",
            ));
        }
        let (_, digest_hex) = digest_file(&path)?;
        if digest_hex != output.digest_hex {
            return Err(invalid_data(
                "staged output digest does not match its manifest",
            ));
        }
        paths.push((output.index, path));
    }
    paths.sort_by_key(|(index, _)| *index);
    Ok(paths.into_iter().map(|(_, path)| path).collect())
}

/// Resolve and validate a staged-v2 generation without falling back.
pub fn resolve_staged_artifact_files(
    artifact_dir: &Path,
    key_hex: &str,
    expected_sizes: &[u64],
) -> io::Result<Option<Vec<NormalizedPath>>> {
    Ok(validated_staged_generation(artifact_dir, key_hex, expected_sizes)?.map(|(_, paths)| paths))
}

fn validated_staged_generation(
    artifact_dir: &Path,
    key_hex: &str,
    expected_sizes: &[u64],
) -> io::Result<Option<(PublishedGeneration, Vec<NormalizedPath>)>> {
    let Some(generation) = load_published_generation(artifact_dir, key_hex)? else {
        return Ok(None);
    };
    if generation.outputs.len() != expected_sizes.len() {
        return Err(invalid_data(
            "staged output count does not match artifact metadata",
        ));
    }
    for output in &generation.outputs {
        if output.index >= expected_sizes.len() || expected_sizes[output.index] != output.size {
            return Err(invalid_data(
                "staged output size does not match artifact metadata",
            ));
        }
    }
    let paths = verify_generation_outputs(&generation)?;
    Ok(Some((generation, paths)))
}

pub(crate) enum SnapshotLayout {
    NormalizeLegacy,
    StagedOnly,
}

/// Snapshot all supported layouts into verified staged generations. Legacy
/// decoding stays in the shared resolver; no transport-specific parser exists.
pub(crate) fn copy_artifact_generation(
    source: &Path,
    key: &str,
    sizes: &[u64],
    destination: &Path,
    layout: &SnapshotLayout,
) -> io::Result<()> {
    if copy_staged_generation(source, key, sizes, destination)? {
        return Ok(());
    }
    if matches!(layout, SnapshotLayout::StagedOnly) {
        return Err(invalid_data(
            "transport snapshot is missing its staged generation",
        ));
    }
    let payloads = resolve_artifact_payloads(source, key, sizes, false, "snapshot::export")?
        .ok_or_else(|| invalid_data("snapshot payload is missing or invalid"))?;
    let root = destination.join(STAGED_ROOT);
    fs::create_dir_all(&root)?;
    let pending = tempfile::Builder::new()
        .prefix(".snapshot-generation-")
        .tempdir_in(&root)?;
    let pack_mtime = if payloads
        .iter()
        .any(|p| matches!(p, ResolvedArtifactPayload::Bytes(_)))
    {
        Some(zccache_core::mtime::FileTime::from_last_modification_time(
            &fs::metadata(source.join(format!("{key}.pack")))?,
        ))
    } else {
        None
    };
    let mut outputs = Vec::with_capacity(payloads.len());
    for (index, payload) in payloads.iter().enumerate() {
        let target = pending.path().join(format!("output-{index}"));
        match payload {
            ResolvedArtifactPayload::File(path) => copy_snapshot_file(path, &target)?,
            ResolvedArtifactPayload::Bytes(bytes) => {
                let mut file = fs::File::create(&target)?;
                std::io::Write::write_all(&mut file, bytes)?;
                if let Some(mtime) = pack_mtime {
                    zccache_core::mtime::stamp_mtime(&target, mtime)?;
                }
                file.sync_all()?;
            }
        }
        let (size, digest_hex) = digest_file(&target)?;
        if size != sizes[index] {
            return Err(invalid_data("snapshot payload changed size"));
        }
        outputs.push(StagedOutput {
            index,
            size,
            digest_hex,
        });
    }
    let generation_hex = generation_digest(key, &outputs);
    let manifest = StagedManifest {
        version: STAGED_MANIFEST_VERSION,
        key_hex: key.into(),
        generation_hex: generation_hex.clone(),
        outputs,
    };
    let mut file = fs::File::create(pending.path().join("manifest.bin"))?;
    std::io::Write::write_all(
        &mut file,
        &bincode::serialize(&manifest).map_err(|e| invalid_data(e.to_string()))?,
    )?;
    file.sync_all()?;
    // Close the manifest before publishing its containing generation.
    drop(file);
    kernal_api::platform::fs::sync_directory_if_supported(pending.path())?;
    let target = root.join(key).join(&generation_hex);
    fs::create_dir_all(root.join(key))?;
    kernal_api::platform::fs::replacement::rename_generation(pending.path(), &target)?;
    publish_snapshot_pointer(destination, key, &generation_hex, sizes)
}

/// Copy one captured, verified generation without re-reading its mutable
/// current pointer. The caller holds the staged read guard during copying.
fn copy_staged_generation(
    source: &Path,
    key: &str,
    sizes: &[u64],
    destination: &Path,
) -> io::Result<bool> {
    let Some((generation, paths)) = validated_staged_generation(source, key, sizes)? else {
        return Ok(false);
    };
    let generation_name = generation
        .dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_data("staged generation has no name"))?;
    let root = destination.join(STAGED_ROOT);
    let key_dir = root.join(key);
    let target = key_dir.join(generation_name);
    fs::create_dir_all(&key_dir)?;
    if target.exists() {
        // An earlier import may have published this immutable generation
        // before its index commit. Validate and reuse it, including read-only
        // outputs, instead of truncating already committed payload files.
        let manifest = load_manifest(&target.join("manifest.bin"), key, generation_name)?;
        if generation_digest(key, &manifest.outputs) != generation_name {
            return Err(invalid_data("existing generation manifest digest mismatch"));
        }
        verify_generation_outputs(&PublishedGeneration {
            dir: target.into(), outputs: manifest.outputs,
        })?;
        publish_snapshot_pointer(destination, key, generation_name, sizes)?;
        return Ok(true);
    }
    let pending = tempfile::Builder::new().prefix(".snapshot-generation-").tempdir_in(&key_dir)?;
    let manifest = generation.dir.join("manifest.bin");
    for path in paths
        .iter()
        .map(AsRef::<Path>::as_ref)
        .chain(std::iter::once(manifest.as_ref()))
    {
        let name = path
            .file_name()
            .ok_or_else(|| invalid_data("staged file has no name"))?;
        let copied = pending.path().join(name);
        copy_snapshot_file(path, &copied)?;
    }
    // Never expose a partially copied generation. A crash before rename
    // leaves only an unreferenced temporary directory; a retry uses a new one.
    kernal_api::platform::fs::sync_directory_if_supported(pending.path())?;
    kernal_api::platform::fs::replacement::rename_generation(pending.path(), &target)?;
    kernal_api::platform::fs::sync_directory_if_supported(&key_dir)?;
    publish_snapshot_pointer(destination, key, generation_name, sizes)?;
    Ok(true)
}

fn copy_snapshot_file(source: &Path, destination: &Path) -> io::Result<()> {
    let metadata = fs::metadata(source)?;
    let mut input = fs::File::open(source)?;
    let mut output = fs::File::create(destination)?;
    io::copy(&mut input, &mut output)?;
    zccache_core::mtime::stamp_mtime(
        destination,
        zccache_core::mtime::FileTime::from_last_modification_time(&metadata),
    )?;
    fs::set_permissions(destination, metadata.permissions())?;
    output.sync_all()
}

fn publish_snapshot_pointer(
    destination: &Path,
    key: &str,
    generation: &str,
    sizes: &[u64],
) -> io::Result<()> {
    let root = destination.join(STAGED_ROOT);
    let key_dir = root.join(key);
    let target = key_dir.join(generation);
    // Revalidate copied bytes; an unsupported concurrent payload mutation must
    // fail publication, never produce a ready snapshot with an invalid digest.
    let pointer = root.join(format!("{key}.current"));
    let mut pointer_file = fs::File::create(&pointer)?;
    std::io::Write::write_all(&mut pointer_file, generation.as_bytes())?;
    pointer_file.sync_all()?;
    let (verified, _) = validated_staged_generation(destination, key, sizes)?
        .ok_or_else(|| invalid_data("snapshot generation disappeared"))?;
    // Runtime materialization requires durable integrity sidecars as well as
    // the manifest. Derive them only from the just-verified copied outputs;
    // never trust or transport a stale source sidecar. This destination is
    // private until the containing snapshot is published.
    for output in verified.outputs {
        let blob = verified.dir.join(format!("output-{}", output.index));
        let digest = kernal_api::hash::Blake3Digest::from_hex(&output.digest_hex)
            .map_err(|error| invalid_data(error.to_string()))?;
        let mut sidecar = fs::File::create(crate::blob_digest::sidecar_path(&blob))?;
        std::io::Write::write_all(&mut sidecar, digest.as_bytes())?;
        sidecar.sync_all()?;
    }
    for directory in [&target, &key_dir, &root] {
        kernal_api::platform::fs::sync_directory_if_supported(directory)?;
    }
    Ok(())
}

/// Every artifact key that currently has a published staged-v2 generation
/// pointer, discovered by listing `<artifact_dir>/.staged-v2/*.current`.
///
/// Used by index reconciliation (#1157) to enumerate rebuild candidates when
/// `index.bin` is unreadable and there is no key list to iterate. A missing
/// staged root is an empty list, not an error.
pub fn published_staged_keys(artifact_dir: &Path) -> io::Result<Vec<String>> {
    let root = artifact_dir.join(STAGED_ROOT);
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut keys = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(key) = name.strip_suffix(".current") else {
            continue;
        };
        if validate_key(key).is_ok() {
            keys.push(key.to_string());
        }
    }
    keys.sort();
    Ok(keys)
}

/// Fully verified per-output sizes for the published staged generation of
/// `key_hex`, derived from the manifest alone.
///
/// This is [`resolve_staged_artifact_files`] with the index cross-check
/// removed, for the one caller that has no index entry to cross-check
/// against: rebuilding a corrupt `index.bin` from disk (#1157). The manifest
/// is the authoritative record of the output count and sizes, and every
/// payload is blake3-verified before a size is reported, so a reconstructed
/// entry can never claim a size the bytes on disk do not back.
///
/// Also returns the manifest's mtime so the rebuilt entry can carry a stable
/// `stored_at_secs` instead of `now()` — otherwise every restart would reset
/// retention age for the whole cache.
pub fn verified_staged_generation(
    artifact_dir: &Path,
    key_hex: &str,
) -> io::Result<Option<(Vec<u64>, std::time::SystemTime)>> {
    let Some(generation) = load_published_generation(artifact_dir, key_hex)? else {
        return Ok(None);
    };
    verify_generation_outputs(&generation)?;
    let mut sizes = vec![0_u64; generation.outputs.len()];
    for output in &generation.outputs {
        // `verify_generation_outputs` already proved the indices are exactly
        // `0..len` with no repeats, so this cannot panic or overwrite.
        sizes[output.index] = output.size;
    }
    let stored_at = fs::metadata(generation.dir.join("manifest.bin"))?
        .modified()
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    Ok(Some((sizes, stored_at)))
}

fn load_manifest(
    path: &Path,
    expected_key: &str,
    expected_generation: &str,
) -> io::Result<StagedManifest> {
    let manifest: StagedManifest = bincode::deserialize(&fs::read(path)?)
        .map_err(|error| invalid_data(format!("invalid staged manifest: {error}")))?;
    if manifest.version != STAGED_MANIFEST_VERSION
        || manifest.key_hex != expected_key
        || manifest.generation_hex != expected_generation
    {
        return Err(invalid_data("staged manifest identity/version mismatch"));
    }
    Ok(manifest)
}

fn digest_file(path: &Path) -> io::Result<(u64, String)> {
    let mut file = fs::File::open(path)?;
    let mut hasher = kernal_api::hash::Blake3Hasher::new();
    let mut buffer = [0_u8; 1024 * 1024];
    let mut size = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size = size.saturating_add(read as u64);
    }
    Ok((size, hasher.finalize().to_hex().to_string()))
}

fn generation_digest(key_hex: &str, outputs: &[StagedOutput]) -> String {
    let mut hasher = kernal_api::hash::Blake3Hasher::new();
    hasher.update(key_hex.as_bytes());
    for output in outputs {
        hasher.update(&output.index.to_le_bytes());
        hasher.update(&output.size.to_le_bytes());
        hasher.update(output.digest_hex.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn load_pack(artifact_dir: &Path, key_hex: &str) -> io::Result<Option<Vec<u8>>> {
    match fs::read(artifact_dir.join(format!("{key_hex}.pack"))) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn resolve_pack_payloads(
    artifact_dir: &Path,
    key_hex: &str,
    expected_sizes: &[u64],
) -> io::Result<Option<Vec<ResolvedArtifactPayload>>> {
    let Some(data) = load_pack(artifact_dir, key_hex)? else {
        return Ok(None);
    };
    let Some(count) = packed_payload_count(&data) else {
        return Ok(None);
    };
    if count != expected_sizes.len() {
        return Ok(None);
    }
    let mut payloads = Vec::with_capacity(count);
    for (index, expected_size) in expected_sizes.iter().copied().enumerate() {
        let Some(bytes) = packed_payload(&data, index) else {
            return Ok(None);
        };
        if bytes.len() as u64 != expected_size {
            return Ok(None);
        }
        payloads.push(ResolvedArtifactPayload::Bytes(Arc::new(bytes.to_vec())));
    }
    Ok(Some(payloads))
}

fn packed_payload_count(data: &[u8]) -> Option<usize> {
    if data.len() < 8 || &data[..4] != PACK_MAGIC {
        return None;
    }
    let count = u32::from_le_bytes(data[4..8].try_into().ok()?) as usize;
    let header_size = 8_usize.checked_add(count.checked_mul(16)?)?;
    (data.len() >= header_size).then_some(count)
}

fn packed_payload(data: &[u8], index: usize) -> Option<&[u8]> {
    let count = packed_payload_count(data)?;
    if index >= count {
        return None;
    }
    let base = 8 + index * 16;
    let offset = usize::try_from(u64::from_le_bytes(data[base..base + 8].try_into().ok()?)).ok()?;
    let size = usize::try_from(u64::from_le_bytes(
        data[base + 8..base + 16].try_into().ok()?,
    ))
    .ok()?;
    let header_size = 8_usize.checked_add(count.checked_mul(16)?)?;
    if offset < header_size {
        return None;
    }
    data.get(offset..offset.checked_add(size)?)
}

fn validate_key(key_hex: &str) -> io::Result<()> {
    if key_hex.is_empty()
        || key_hex.len() > 128
        || !key_hex.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "artifact key must be a bounded hexadecimal string",
        ));
    }
    Ok(())
}

fn validate_generation(generation_hex: &str) -> io::Result<()> {
    if generation_hex.len() != 64 || !generation_hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid_data(
            "staged artifact generation is not a blake3 digest",
        ));
    }
    Ok(())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Fixture writers for the on-disk layouts this module reads.
///
/// Behind the `test-support` feature (and always available inside this
/// crate's own tests) so crates above `zccache-artifact` can seed a genuine
/// `.staged-v2` generation instead of hand-rolling the manifest encoding and
/// drifting from it.
#[cfg(any(test, feature = "test-support"))]
// Fixture setup: a filesystem error here means the test's premise never got
// established, so panicking with the failed step named is the correct and
// most debuggable outcome. Never compiled into a shipping binary.
#[allow(clippy::expect_used)]
pub mod fixtures {
    use super::{
        generation_digest, StagedManifest, StagedOutput, STAGED_MANIFEST_VERSION, STAGED_ROOT,
    };
    use std::fs;
    use std::path::Path;

    /// Seed the existing pack encoding for shared layout-conformance tests.
    pub fn seed_pack(artifact_dir: &Path, key: &str, payloads: &[&[u8]]) {
        fs::create_dir_all(artifact_dir).expect("create pack fixture directory");
        fs::write(
            artifact_dir.join(format!("{key}.pack")),
            build_pack(payloads),
        )
        .expect("write pack fixture");
    }

    pub(super) fn build_pack(payloads: &[&[u8]]) -> Vec<u8> {
        let header_size = 8 + payloads.len() * 16;
        let mut data = Vec::with_capacity(
            header_size + payloads.iter().map(|payload| payload.len()).sum::<usize>(),
        );
        data.extend_from_slice(super::PACK_MAGIC);
        data.extend_from_slice(&(payloads.len() as u32).to_le_bytes());
        let mut offset = header_size as u64;
        for payload in payloads {
            data.extend_from_slice(&offset.to_le_bytes());
            data.extend_from_slice(&(payload.len() as u64).to_le_bytes());
            offset += payload.len() as u64;
        }
        for payload in payloads {
            data.extend_from_slice(payload);
        }
        data
    }

    /// Write a complete, self-consistent staged-v2 generation for `key_hex`
    /// under `artifact_dir` and publish it via the `.current` pointer.
    /// Returns the generation hex.
    pub fn seed_staged_generation(
        artifact_dir: &Path,
        key_hex: &str,
        payloads: &[&[u8]],
    ) -> String {
        let outputs: Vec<StagedOutput> = payloads
            .iter()
            .enumerate()
            .map(|(index, payload)| StagedOutput {
                index,
                size: payload.len() as u64,
                digest_hex: kernal_api::hash::blake3_bytes(payload).to_hex().to_string(),
            })
            .collect();
        let generation_hex = generation_digest(key_hex, &outputs);
        let generation_dir = artifact_dir
            .join(STAGED_ROOT)
            .join(key_hex)
            .join(&generation_hex);
        fs::create_dir_all(&generation_dir).expect("create staged generation dir");
        for (index, payload) in payloads.iter().enumerate() {
            fs::write(generation_dir.join(format!("output-{index}")), payload)
                .expect("write staged output");
        }
        let manifest = StagedManifest {
            version: STAGED_MANIFEST_VERSION,
            key_hex: key_hex.to_string(),
            generation_hex: generation_hex.clone(),
            outputs,
        };
        fs::write(
            generation_dir.join("manifest.bin"),
            bincode::serialize(&manifest).expect("serialize staged manifest"),
        )
        .expect("write staged manifest");
        fs::write(
            artifact_dir
                .join(STAGED_ROOT)
                .join(format!("{key_hex}.current")),
            &generation_hex,
        )
        .expect("write staged pointer");
        generation_hex
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload_bytes(payload: &ResolvedArtifactPayload) -> Vec<u8> {
        match payload {
            ResolvedArtifactPayload::File(path) => fs::read(path).expect("read file payload"),
            ResolvedArtifactPayload::Bytes(bytes) => bytes.as_ref().clone(),
        }
    }

    use super::fixtures::build_pack;

    use super::fixtures::seed_staged_generation as seed_staged;

    #[test]
    fn resolves_legacy_pack_and_staged_layouts() {
        let root = tempfile::tempdir().expect("tempdir");
        let legacy_key = "1".repeat(64);
        let pack_key = "2".repeat(64);
        let staged_key = "3".repeat(64);
        fs::write(root.path().join(format!("{legacy_key}_0")), b"legacy").expect("write legacy");
        fs::write(
            root.path().join(format!("{pack_key}.pack")),
            build_pack(&[b"packed-a", b"packed-b"]),
        )
        .expect("write pack");
        seed_staged(root.path(), &staged_key, &[b"staged"]);

        let legacy =
            resolve_artifact_payloads(root.path(), &legacy_key, &[6], true, "test::legacy")
                .expect("resolve legacy")
                .expect("legacy payload");
        let packed = resolve_artifact_payloads(root.path(), &pack_key, &[8, 8], true, "test::pack")
            .expect("resolve pack")
            .expect("pack payloads");
        let staged =
            resolve_artifact_payloads(root.path(), &staged_key, &[6], true, "test::staged")
                .expect("resolve staged")
                .expect("staged payload");

        assert!(matches!(&legacy[0], ResolvedArtifactPayload::File(_)));
        assert!(packed
            .iter()
            .all(|payload| matches!(payload, ResolvedArtifactPayload::Bytes(_))));
        assert!(matches!(&staged[0], ResolvedArtifactPayload::File(_)));
        assert_eq!(payload_bytes(&legacy[0]), b"legacy");
        assert_eq!(payload_bytes(&packed[0]), b"packed-a");
        assert_eq!(payload_bytes(&packed[1]), b"packed-b");
        assert_eq!(payload_bytes(&staged[0]), b"staged");
    }

    #[test]
    fn staged_pointer_survives_a_fresh_resolver_call() {
        let root = tempfile::tempdir().expect("tempdir");
        let key = "a".repeat(64);
        seed_staged(root.path(), &key, &[b"restart-readable"]);

        let first = resolve_staged_artifact_files(root.path(), &key, &[16])
            .expect("first resolve")
            .expect("first payload");
        let second = resolve_staged_artifact_files(root.path(), &key, &[16])
            .expect("restart-style resolve")
            .expect("second payload");
        assert_eq!(first, second);
        assert_eq!(
            fs::read(&second[0]).expect("read output"),
            b"restart-readable"
        );
    }

    #[test]
    fn rejects_corrupt_staged_manifest_without_legacy_fallback() {
        let root = tempfile::tempdir().expect("tempdir");
        let key = "b".repeat(64);
        let generation = seed_staged(root.path(), &key, &[b"valid"]);
        fs::write(
            root.path()
                .join(STAGED_ROOT)
                .join(&key)
                .join(generation)
                .join("output-0"),
            b"corrupt",
        )
        .expect("corrupt output");
        fs::write(root.path().join(format!("{key}_0")), b"valid").expect("write legacy");

        let error = resolve_artifact_payloads(root.path(), &key, &[5], true, "test::corrupt")
            .expect_err("corrupt selected generation must be loud");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_incomplete_or_size_mismatched_pack() {
        let root = tempfile::tempdir().expect("tempdir");
        let key = "c".repeat(64);
        fs::write(
            root.path().join(format!("{key}.pack")),
            build_pack(&[b"only-one"]),
        )
        .expect("write pack");

        assert!(
            resolve_artifact_payloads(root.path(), &key, &[8, 1], true, "test::pack-count")
                .expect("resolve")
                .is_none()
        );
        assert!(
            resolve_artifact_payloads(root.path(), &key, &[7], true, "test::pack-size")
                .expect("resolve")
                .is_none()
        );
    }
}
