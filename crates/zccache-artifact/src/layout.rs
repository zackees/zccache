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

const STAGED_ROOT: &str = ".staged-v2";
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

/// Resolve and validate a staged-v2 generation without falling back.
pub fn resolve_staged_artifact_files(
    artifact_dir: &Path,
    key_hex: &str,
    expected_sizes: &[u64],
) -> io::Result<Option<Vec<NormalizedPath>>> {
    validate_key(key_hex)?;
    let root = artifact_dir.join(STAGED_ROOT);
    let pointer = root.join(format!("{key_hex}.current"));
    let generation_hex = match fs::read_to_string(pointer) {
        Ok(value) => value.trim().to_string(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    validate_generation(&generation_hex)?;
    let generation = root.join(key_hex).join(&generation_hex);
    let manifest = load_manifest(&generation.join("manifest.bin"), key_hex, &generation_hex)?;
    if generation_digest(key_hex, &manifest.outputs) != generation_hex {
        return Err(invalid_data(
            "staged generation digest does not match its manifest",
        ));
    }
    if manifest.outputs.len() != expected_sizes.len() {
        return Err(invalid_data(
            "staged output count does not match artifact metadata",
        ));
    }

    let mut paths = Vec::with_capacity(manifest.outputs.len());
    let mut seen = vec![false; expected_sizes.len()];
    for output in &manifest.outputs {
        if output.index >= expected_sizes.len()
            || seen[output.index]
            || expected_sizes[output.index] != output.size
        {
            return Err(invalid_data(
                "staged output size does not match artifact metadata",
            ));
        }
        seen[output.index] = true;
        let path: NormalizedPath = generation.join(format!("output-{}", output.index)).into();
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
    if seen.iter().any(|was_seen| !was_seen) {
        return Err(invalid_data("staged manifest has a missing output index"));
    }
    paths.sort_by_key(|(index, _)| *index);
    Ok(Some(paths.into_iter().map(|(_, path)| path).collect()))
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
    let mut hasher = blake3::Hasher::new();
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
    let mut hasher = blake3::Hasher::new();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn payload_bytes(payload: &ResolvedArtifactPayload) -> Vec<u8> {
        match payload {
            ResolvedArtifactPayload::File(path) => fs::read(path).expect("read file payload"),
            ResolvedArtifactPayload::Bytes(bytes) => bytes.as_ref().clone(),
        }
    }

    fn build_pack(payloads: &[&[u8]]) -> Vec<u8> {
        let header_size = 8 + payloads.len() * 16;
        let mut data = Vec::with_capacity(
            header_size + payloads.iter().map(|payload| payload.len()).sum::<usize>(),
        );
        data.extend_from_slice(PACK_MAGIC);
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

    fn seed_staged(
        artifact_dir: &Path,
        key_hex: &str,
        payloads: &[&[u8]],
    ) -> (String, Vec<StagedOutput>) {
        let mut outputs = Vec::with_capacity(payloads.len());
        for (index, payload) in payloads.iter().enumerate() {
            outputs.push(StagedOutput {
                index,
                size: payload.len() as u64,
                digest_hex: blake3::hash(payload).to_hex().to_string(),
            });
        }
        let generation_hex = generation_digest(key_hex, &outputs);
        let generation_dir = artifact_dir
            .join(STAGED_ROOT)
            .join(key_hex)
            .join(&generation_hex);
        fs::create_dir_all(&generation_dir).expect("create generation");
        for (index, payload) in payloads.iter().enumerate() {
            fs::write(generation_dir.join(format!("output-{index}")), payload)
                .expect("write staged output");
        }
        let manifest = StagedManifest {
            version: STAGED_MANIFEST_VERSION,
            key_hex: key_hex.to_string(),
            generation_hex: generation_hex.clone(),
            outputs: outputs.clone(),
        };
        fs::write(
            generation_dir.join("manifest.bin"),
            bincode::serialize(&manifest).expect("serialize manifest"),
        )
        .expect("write manifest");
        fs::write(
            artifact_dir
                .join(STAGED_ROOT)
                .join(format!("{key_hex}.current")),
            &generation_hex,
        )
        .expect("write pointer");
        (generation_hex, outputs)
    }

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
        let (generation, _) = seed_staged(root.path(), &key, &[b"valid"]);
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
