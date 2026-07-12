//! Default-on immutable artifact generations for the staged-output rollout (#1056).
//!
//! This is the storage half of the staged compiler-output design. The compiler
//! writes supported outputs into private staging before publication. The
//! persisted generation is always independent: reflink when the filesystem can
//! provide true COW, otherwise a byte copy. Unsupported plans fall back before
//! spawn, and `ZCCACHE_STAGED_ARTIFACTS=off` is the rollout kill switch.

use super::*;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

mod maintenance;
pub(crate) use maintenance::{
    evict_staged_artifact_keys, scan_staged_disk_artifacts, StagedDiskArtifact,
};

pub(in crate::daemon::server) const STAGED_ARTIFACTS_ENV: &str = "ZCCACHE_STAGED_ARTIFACTS";

const STAGED_ROOT: &str = ".staged-v2";
const STAGED_MANIFEST_VERSION: u32 = 1;
const PUBLISH_LOCK: &str = ".publish.lock";
const STORE_LOCK: &str = ".store.lock";

static STAGED_ARTIFACT_TMP_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Deserialize, Serialize)]
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

pub(in crate::daemon::server) fn staged_artifacts_enabled() -> bool {
    std::env::var(STAGED_ARTIFACTS_ENV).map_or(true, |value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "off" | "no"
        )
    })
}

pub(in crate::daemon::server) fn staged_lane_enabled(
    family: crate::compiler::CompilerFamily,
) -> bool {
    let Ok(value) = std::env::var(STAGED_ARTIFACTS_ENV) else {
        return true;
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "all" | "1" | "true" | "yes" | "on" => true,
        "rust" => family == crate::compiler::CompilerFamily::Rustc,
        "c" | "cc" | "c-cpp" | "cpp" => matches!(
            family,
            crate::compiler::CompilerFamily::Gcc
                | crate::compiler::CompilerFamily::Clang
                | crate::compiler::CompilerFamily::Msvc
        ),
        _ => false,
    }
}

pub(in crate::daemon::server) fn staged_link_lane_enabled() -> bool {
    std::env::var(STAGED_ARTIFACTS_ENV).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "all" | "1" | "true" | "yes" | "on"
        )
    })
}

pub(in crate::daemon::server) fn staged_key_supported(key_hex: &str) -> bool {
    !key_hex.is_empty()
        && key_hex.len() <= 128
        && key_hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(in crate::daemon::server) fn is_staged_artifact_path(path: &Path) -> bool {
    let components = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let Some(window) = components.windows(4).last() else {
        return false;
    };
    let root_matches = if cfg!(windows) {
        window[0].eq_ignore_ascii_case(STAGED_ROOT)
    } else {
        window[0] == STAGED_ROOT
    };
    let key_matches = window[1].len() <= 128
        && !window[1].is_empty()
        && window[1].bytes().all(|byte| byte.is_ascii_hexdigit());
    let generation_matches =
        window[2].len() == 64 && window[2].bytes().all(|byte| byte.is_ascii_hexdigit());
    let output_matches = window[3]
        .strip_prefix("output-")
        .is_some_and(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()));
    root_matches && key_matches && generation_matches && output_matches
}

pub(in crate::daemon::server) fn is_staged_artifact_root(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        if cfg!(windows) {
            name.to_string_lossy().eq_ignore_ascii_case(STAGED_ROOT)
        } else {
            name == STAGED_ROOT
        }
    })
}

fn staged_root(artifact_dir: &Path) -> PathBuf {
    artifact_dir.join(STAGED_ROOT)
}

fn open_store_lock(root: &Path) -> io::Result<File> {
    fs::create_dir_all(root)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(STORE_LOCK))
}

fn validate_key(key_hex: &str) -> io::Result<()> {
    if !staged_key_supported(key_hex) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "staged artifact key must be a bounded hexadecimal string",
        ));
    }
    Ok(())
}

fn validate_generation(generation_hex: &str) -> io::Result<()> {
    if generation_hex.len() != 64 || !generation_hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "staged artifact generation is not a blake3 digest",
        ));
    }
    Ok(())
}

fn pointer_path(artifact_dir: &Path, key_hex: &str) -> PathBuf {
    staged_root(artifact_dir).join(format!("{key_hex}.current"))
}

fn generation_dir(artifact_dir: &Path, key_hex: &str, generation_hex: &str) -> PathBuf {
    staged_root(artifact_dir).join(key_hex).join(generation_hex)
}

fn output_path(generation_dir: &Path, index: usize) -> PathBuf {
    generation_dir.join(format!("output-{index}"))
}

fn manifest_path(generation_dir: &Path) -> PathBuf {
    generation_dir.join("manifest.bin")
}

fn temporary_path(path: &Path, suffix: &str) -> PathBuf {
    let nonce = STAGED_ARTIFACT_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(
        ".{}.{}.{}-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        nonce,
        suffix
    ))
}

fn sync_file(path: &Path) -> io::Result<()> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?
        .sync_all()
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = temporary_path(path, "tmp");
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        replace_staged_path(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn replace_staged_path(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        fs::rename(source, destination)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };

        let source_wide: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let destination_wide: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let result = unsafe {
            MoveFileExW(
                source_wide.as_ptr(),
                destination_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if result == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

fn copy_independent(source: &Path, destination: &Path) -> io::Result<(bool, u64)> {
    if reflink_copy::reflink(source, destination).is_ok() {
        return Ok((true, 0));
    }
    // A failed reflink probe may leave a partial destination, including
    // platform-specific attributes. Remove it before attempting the copy tier.
    if fs::metadata(destination).is_ok() {
        let _ = set_readonly(destination, false);
    }
    let _ = fs::remove_file(destination);
    let bytes = fs::copy(source, destination)?;
    Ok((false, bytes))
}

fn copy_output(source: &Path, destination: &Path) -> io::Result<(bool, u64)> {
    let source_metadata = fs::metadata(source)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let result = copy_independent(source, destination);
    if result.is_err() {
        let _ = fs::remove_file(destination);
        return result;
    }
    // Keep the destination writable while restoring timestamps. On Windows,
    // setting mtime on a read-only file fails with ERROR_ACCESS_DENIED.
    make_writable(destination)?;
    let mtime = filetime::FileTime::from_last_modification_time(&source_metadata);
    filetime::set_file_mtime(destination, mtime)?;
    set_readonly(destination, true)?;
    result
}

/// Materialize a staged compiler output without sharing a writable inode with
/// the private compiler file or the published backend generation.
pub(in crate::daemon::server) fn materialize_independent(
    source: &Path,
    destination: &Path,
) -> io::Result<()> {
    if let Ok(metadata) = fs::metadata(destination) {
        if metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!(
                    "output destination is a directory: {}",
                    destination.display()
                ),
            ));
        }
        let _ = set_readonly(destination, false);
        fs::remove_file(destination)?;
    }
    copy_output(source, destination).map(|_| {
        let _ = set_readonly(destination, false);
    })
}

fn digest_file(path: &Path) -> io::Result<(u64, String)> {
    let mut file = File::open(path)?;
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

fn load_manifest(
    path: &Path,
    expected_key: &str,
    expected_generation: &str,
) -> io::Result<StagedManifest> {
    let manifest: StagedManifest = bincode::deserialize(&fs::read(path)?).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid staged manifest: {error}"),
        )
    })?;
    if manifest.version != STAGED_MANIFEST_VERSION
        || manifest.key_hex != expected_key
        || manifest.generation_hex != expected_generation
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "staged manifest identity/version mismatch",
        ));
    }
    Ok(manifest)
}

fn validate_staged_generation(
    artifact_dir: &Path,
    key_hex: &str,
    generation_hex: &str,
) -> io::Result<()> {
    validate_generation(generation_hex)?;
    let generation = generation_dir(artifact_dir, key_hex, generation_hex);
    let manifest = load_manifest(&manifest_path(&generation), key_hex, generation_hex)?;
    if generation_digest(key_hex, &manifest.outputs) != generation_hex {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "staged generation digest does not match its manifest",
        ));
    }
    let mut seen = vec![false; manifest.outputs.len()];
    for output in &manifest.outputs {
        if output.index >= seen.len() || seen[output.index] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "staged manifest has a duplicate or invalid output index",
            ));
        }
        seen[output.index] = true;
        let path = output_path(&generation, output.index);
        if fs::metadata(&path)?.len() != output.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "staged output size does not match its manifest",
            ));
        }
        let (_, digest_hex) = digest_file(&path)?;
        if digest_hex != output.digest_hex {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "staged output digest does not match its manifest",
            ));
        }
    }
    if seen.iter().any(|was_seen| !was_seen) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "staged manifest has a missing output index",
        ));
    }
    Ok(())
}

pub(in crate::daemon::server) fn persist_staged_artifact_paths(
    artifact_dir: &Path,
    key_hex: &str,
    sources: &[NormalizedPath],
) -> io::Result<PersistArtifactFileStats> {
    let publish_started = std::time::Instant::now();
    validate_key(key_hex)?;
    if sources.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot publish an empty staged artifact",
        ));
    }

    let root = staged_root(artifact_dir);
    let store_lock = open_store_lock(&root)?;
    fs2::FileExt::lock_shared(&store_lock)?;
    let key_root = root.join(key_hex);
    fs::create_dir_all(&key_root)?;
    let publish_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(key_root.join(PUBLISH_LOCK))?;
    fs2::FileExt::lock_exclusive(&publish_lock)?;
    let temporary_generation = key_root.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        STAGED_ARTIFACT_TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&temporary_generation)?;

    let result = (|| {
        let mut outputs = Vec::with_capacity(sources.len());
        let mut stats = PersistArtifactFileStats::default();
        for (index, source) in sources.iter().enumerate() {
            let destination = output_path(&temporary_generation, index);
            let (reflink, copied_bytes) =
                copy_output(source.as_path(), &destination).map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!(
                            "staged output copy failed: {} -> {}: {error}",
                            source.display(),
                            destination.display()
                        ),
                    )
                })?;
            let (size, digest_hex) = digest_file(&destination).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "staged output hash failed: {}: {error}",
                        destination.display()
                    ),
                )
            })?;
            write_authoritative_blob_digest_for(&destination, &destination).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "staged output durable digest failed: {}: {error}",
                        destination.display()
                    ),
                )
            })?;
            outputs.push(StagedOutput {
                index,
                size,
                digest_hex,
            });
            if reflink {
                stats.reflink_count += 1;
            } else {
                stats.copy_count += 1;
                stats.copy_bytes += copied_bytes;
            }
        }

        let generation_hex = generation_digest(key_hex, &outputs);
        validate_generation(&generation_hex)?;
        let pointer = pointer_path(artifact_dir, key_hex);
        let mut invalid_generation_to_remove = None;
        if let Ok(previous_value) = fs::read_to_string(&pointer) {
            let previous = previous_value.trim();
            if !previous.is_empty() && previous != generation_hex {
                if validate_staged_generation(artifact_dir, key_hex, previous).is_ok() {
                    crate::core::lifecycle::write_event(
                        "staged_publication_conflict",
                        serde_json::json!({
                            "cache_key": key_hex,
                            "existing_generation": previous,
                            "candidate_generation": generation_hex,
                            "elapsed_ns": publish_started.elapsed().as_nanos() as u64,
                        }),
                    );
                    tracing::error!(
                        event = "staged_publication_conflict",
                        cache_key = key_hex,
                        existing_generation = previous,
                        candidate_generation = generation_hex,
                        "same cache key produced a different complete output generation"
                    );
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "same cache key produced different staged output bytes",
                    ));
                }
                crate::core::lifecycle::write_event(
                    "staged_publication_replaces_invalid_generation",
                    serde_json::json!({
                        "cache_key": key_hex,
                        "invalid_generation": previous,
                        "replacement_generation": generation_hex,
                    }),
                );
                if validate_generation(previous).is_ok() {
                    invalid_generation_to_remove = Some(previous.to_string());
                }
            }
        }
        let output_count = outputs.len();
        let final_generation = generation_dir(artifact_dir, key_hex, &generation_hex);
        let manifest = StagedManifest {
            version: STAGED_MANIFEST_VERSION,
            key_hex: key_hex.to_string(),
            generation_hex: generation_hex.clone(),
            outputs,
        };
        let manifest_bytes = bincode::serialize(&manifest).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("staged manifest encode failed: {error}"),
            )
        })?;
        let temporary_manifest = manifest_path(&temporary_generation);
        fs::write(&temporary_manifest, manifest_bytes).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("staged manifest write failed: {error}"),
            )
        })?;
        sync_file(&temporary_manifest).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("staged manifest sync failed: {error}"),
            )
        })?;
        sync_directory(&temporary_generation).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("staged generation sync failed: {error}"),
            )
        })?;

        match fs::rename(&temporary_generation, &final_generation) {
            Ok(()) => {}
            Err(error) if final_generation.exists() => {
                let _ = error;
                let _ = fs::remove_dir_all(&temporary_generation);
            }
            Err(error) => return Err(error),
        }
        sync_directory(&key_root).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("staged generation parent sync failed: {error}"),
            )
        })?;

        atomic_write(&pointer, generation_hex.as_bytes()).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("staged generation pointer publish failed: {error}"),
            )
        })?;
        if let Some(parent) = pointer.parent() {
            sync_directory(parent).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("staged pointer parent sync failed: {error}"),
                )
            })?;
        }
        if let Some(invalid_generation) = invalid_generation_to_remove {
            let invalid_path = generation_dir(artifact_dir, key_hex, &invalid_generation);
            if let Err(error) = remove_staged_tree(&invalid_path) {
                tracing::warn!(
                    path = %invalid_path.display(),
                    %error,
                    "failed to remove replaced invalid staged generation"
                );
            }
        }
        tracing::info!(
            event = "staged_publication_complete",
            cache_key = key_hex,
            generation = generation_hex,
            output_count,
            reflink_count = stats.reflink_count,
            copy_count = stats.copy_count,
            copy_bytes = stats.copy_bytes,
            elapsed_ns = publish_started.elapsed().as_nanos() as u64,
            "published immutable staged artifact generation"
        );
        Ok(stats)
    })();

    if result.is_err() {
        let _ = fs::remove_dir_all(&temporary_generation);
    }
    result
}

pub(in crate::daemon::server) fn load_staged_artifact_paths(
    artifact_dir: &Path,
    key_hex: &str,
    expected_sizes: &[u64],
) -> io::Result<Option<Vec<NormalizedPath>>> {
    validate_key(key_hex)?;
    let pointer = pointer_path(artifact_dir, key_hex);
    let generation_hex = match fs::read_to_string(pointer) {
        Ok(value) => value.trim().to_string(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    validate_generation(&generation_hex)?;
    let generation = generation_dir(artifact_dir, key_hex, &generation_hex);
    let manifest = load_manifest(&manifest_path(&generation), key_hex, &generation_hex)?;
    if generation_digest(key_hex, &manifest.outputs) != generation_hex {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "staged generation digest does not match its manifest",
        ));
    }
    if manifest.outputs.len() != expected_sizes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
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
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "staged output size does not match artifact metadata",
            ));
        }
        seen[output.index] = true;
        let path = output_path(&generation, output.index);
        let metadata = fs::metadata(&path)?;
        if metadata.len() != output.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "staged output size does not match its manifest",
            ));
        }
        let (_, digest_hex) = digest_file(&path)?;
        if digest_hex != output.digest_hex {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "staged output digest does not match its manifest",
            ));
        }
        paths.push((output.index, path.into()));
    }
    if seen.iter().any(|was_seen| !was_seen) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "staged manifest has a missing output index",
        ));
    }
    paths.sort_by_key(|(index, _)| *index);
    Ok(Some(paths.into_iter().map(|(_, path)| path).collect()))
}

pub(in crate::daemon::server) fn cleanup_staged_artifact_temps(
    artifact_dir: &Path,
) -> io::Result<usize> {
    let root = staged_root(artifact_dir);
    if !root.is_dir() {
        return Ok(0);
    }
    let store_lock = open_store_lock(&root)?;
    fs2::FileExt::lock_exclusive(&store_lock)?;
    let entries = fs::read_dir(&root)?;
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with(".") && name != STORE_LOCK
            })
        {
            fs::remove_file(path)?;
            removed += 1;
            continue;
        }
        if path.is_dir() {
            let key = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned());
            let current = key
                .as_deref()
                .and_then(|key| {
                    fs::read_to_string(root.join(format!("{key}.current")).as_path()).ok()
                })
                .map(|value| value.trim().to_string());
            for child in fs::read_dir(&path)?.flatten() {
                let child_path = child.path();
                let child_name = child_path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let remove = child_name != PUBLISH_LOCK
                    && (child_name.starts_with(".tmp-")
                        || (child_path.is_dir()
                            && current.as_deref() != Some(child_name.as_str())));
                if remove {
                    if child_path.is_dir() {
                        fs::remove_dir_all(child_path)?;
                    } else {
                        fs::remove_file(child_path)?;
                    }
                    removed += 1;
                }
            }
        }
    }
    Ok(removed)
}

fn remove_staged_tree(path: &Path) -> io::Result<u64> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        fs::remove_file(path)?;
        return Ok(0);
    }
    if metadata.is_dir() {
        let mut removed = 0;
        for entry in fs::read_dir(path)?.flatten() {
            removed += remove_staged_tree(&entry.path())?;
        }
        fs::remove_dir(path)?;
        return Ok(removed);
    }
    let size = metadata.len();
    remove_registered_blob(path)?;
    Ok(size)
}

pub(in crate::daemon::server) fn clear_staged_artifacts(artifact_dir: &Path) -> io::Result<u64> {
    let root = staged_root(artifact_dir);
    if !root.is_dir() {
        return Ok(0);
    }
    let store_lock = open_store_lock(&root)?;
    fs2::FileExt::lock_exclusive(&store_lock)?;
    let mut bytes_removed = 0;
    for entry in fs::read_dir(&root)?.flatten() {
        if entry.file_name() == STORE_LOCK {
            continue;
        }
        bytes_removed += remove_staged_tree(&entry.path())?;
    }
    Ok(bytes_removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn source_files(dir: &Path) -> Vec<NormalizedPath> {
        let first = dir.join("source-a.rlib");
        let second = dir.join("source-b.rmeta");
        fs::write(&first, b"first immutable payload").unwrap();
        fs::write(&second, b"second immutable payload").unwrap();
        vec![first.into(), second.into()]
    }

    #[test]
    fn staged_generation_is_independent_and_hash_addressed() {
        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let sources = source_files(dir.path());
        let stats =
            persist_staged_artifact_paths(&artifact_dir, &"a".repeat(64), &sources).unwrap();

        assert_eq!(stats.hardlink_count, 0);
        assert_eq!(stats.reflink_count + stats.copy_count, 2);

        let payloads = load_staged_artifact_paths(&artifact_dir, &"a".repeat(64), &[23, 24])
            .unwrap()
            .unwrap();
        assert_eq!(payloads.len(), 2);
        assert_eq!(fs::read(&payloads[0]).unwrap(), b"first immutable payload");
        assert_eq!(fs::read(&payloads[1]).unwrap(), b"second immutable payload");
        assert!(!same_file(sources[0].as_path(), payloads[0].as_path()));
        assert!(fs::metadata(&payloads[0]).unwrap().permissions().readonly());

        fs::write(&sources[0], b"mutated compiler output").unwrap();
        assert_eq!(fs::read(&payloads[0]).unwrap(), b"first immutable payload");

        let pointer = artifact_dir
            .join(STAGED_ROOT)
            .join(format!("{}.current", "a".repeat(64)));
        let generation = fs::read_to_string(pointer).unwrap();
        assert_eq!(generation.trim().len(), 64);
        assert!(generation
            .trim()
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit()));
        assert!(!is_staged_artifact_path(
            &artifact_dir
                .join(STAGED_ROOT)
                .join("not-a-generation")
                .join("file")
        ));
        assert!(is_staged_artifact_path(&payloads[0]));
    }

    #[test]
    fn staged_publication_rejects_nondeterministic_same_key_output() {
        let dir = tempfile::tempdir().unwrap();
        let _cache_dir = crate::daemon::server::tests::CacheDirEnvGuard::set(dir.path());
        let artifact_dir = dir.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let sources = source_files(dir.path());
        let key = "e".repeat(64);
        persist_staged_artifact_paths(&artifact_dir, &key, &sources).unwrap();

        make_writable(&sources[0]).unwrap();
        fs::write(&sources[0], b"replacement immutable payload").unwrap();
        let error = persist_staged_artifact_paths(&artifact_dir, &key, &sources).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);

        let payloads = load_staged_artifact_paths(&artifact_dir, &key, &[23, 24])
            .unwrap()
            .unwrap();
        assert_eq!(fs::read(&payloads[0]).unwrap(), b"first immutable payload");
        assert_eq!(fs::read(&payloads[1]).unwrap(), b"second immutable payload");
        assert!(!same_file(sources[0].as_path(), payloads[0].as_path()));

        let log = fs::read_to_string(crate::core::lifecycle::log_file_path()).unwrap();
        let event: serde_json::Value = log
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .find(|event: &serde_json::Value| {
                event["event"] == "staged_publication_conflict" && event["cache_key"] == key
            })
            .expect("durable staged publication conflict event");
        assert_ne!(event["existing_generation"], event["candidate_generation"]);
        assert!(event["elapsed_ns"].as_u64().is_some());
    }

    #[test]
    fn staged_publication_can_replace_a_proven_corrupt_generation() {
        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let sources = source_files(dir.path());
        let key = "9".repeat(64);
        persist_staged_artifact_paths(&artifact_dir, &key, &sources).unwrap();
        let old_generation = fs::read_to_string(pointer_path(&artifact_dir, &key)).unwrap();
        let payloads = load_staged_artifact_paths(&artifact_dir, &key, &[23, 24])
            .unwrap()
            .unwrap();
        make_writable(&payloads[0]).unwrap();
        fs::write(&payloads[0], b"corrupt").unwrap();

        fs::write(&sources[0], b"replacement immutable payload").unwrap();
        persist_staged_artifact_paths(&artifact_dir, &key, &sources).unwrap();
        let replacement = load_staged_artifact_paths(&artifact_dir, &key, &[29, 24])
            .unwrap()
            .unwrap();
        assert_eq!(
            fs::read(&replacement[0]).unwrap(),
            b"replacement immutable payload"
        );
        assert!(!generation_dir(&artifact_dir, &key, old_generation.trim()).exists());
    }

    #[test]
    fn concurrent_same_key_publishers_never_overwrite_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let source_a: NormalizedPath = dir.path().join("a.o").into();
        let source_b: NormalizedPath = dir.path().join("b.o").into();
        fs::write(&source_a, b"generation-a").unwrap();
        fs::write(&source_b, b"generation-b").unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let key = "8".repeat(64);

        let publishers: Vec<_> = [source_a, source_b]
            .into_iter()
            .map(|source| {
                let barrier = std::sync::Arc::clone(&barrier);
                let artifact_dir = artifact_dir.clone();
                let key = key.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    persist_staged_artifact_paths(&artifact_dir, &key, &[source])
                })
            })
            .collect();
        barrier.wait();
        let results: Vec<_> = publishers
            .into_iter()
            .map(|publisher| publisher.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| result
                    .as_ref()
                    .is_err_and(|error| error.kind() == io::ErrorKind::AlreadyExists))
                .count(),
            1
        );
        let payloads = load_staged_artifact_paths(&artifact_dir, &key, &[12])
            .unwrap()
            .unwrap();
        assert!(matches!(
            fs::read(&payloads[0]).unwrap().as_slice(),
            b"generation-a" | b"generation-b"
        ));
    }

    #[test]
    fn staged_generation_rejects_same_size_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let sources = source_files(dir.path());
        let key = "b".repeat(64);
        persist_staged_artifact_paths(&artifact_dir, &key, &sources).unwrap();
        let payloads = load_staged_artifact_paths(&artifact_dir, &key, &[23, 24])
            .unwrap()
            .unwrap();

        make_writable(&payloads[0]).unwrap();
        let mut corrupted = fs::read(&payloads[0]).unwrap();
        corrupted[0] ^= 0xff;
        fs::write(&payloads[0], corrupted).unwrap();
        assert_eq!(
            load_staged_artifact_paths(&artifact_dir, &key, &[23, 24])
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn staged_generation_pointer_never_selects_partial_set() {
        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let sources = source_files(dir.path());
        let key = "c".repeat(64);
        persist_staged_artifact_paths(&artifact_dir, &key, &sources).unwrap();

        let pointer = artifact_dir
            .join(STAGED_ROOT)
            .join(format!("{key}.current"));
        let generation = fs::read_to_string(pointer).unwrap();
        let generation_dir = artifact_dir
            .join(STAGED_ROOT)
            .join(&key)
            .join(generation.trim());
        make_writable(&generation_dir.join("output-1")).unwrap();
        fs::remove_file(generation_dir.join("output-1")).unwrap();
        assert!(load_staged_artifact_paths(&artifact_dir, &key, &[23, 24]).is_err());
    }

    #[test]
    fn staged_generation_cleans_abandoned_temporary_directories() {
        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("artifacts");
        let key_root = artifact_dir.join(STAGED_ROOT).join("d".repeat(64));
        fs::create_dir_all(&key_root).unwrap();
        fs::create_dir(key_root.join(".tmp-crashed")).unwrap();
        fs::create_dir(key_root.join("stable-generation")).unwrap();
        fs::create_dir(key_root.join("orphan-generation")).unwrap();
        fs::write(key_root.join(PUBLISH_LOCK), b"").unwrap();
        fs::write(
            artifact_dir
                .join(STAGED_ROOT)
                .join(format!("{}.current", "d".repeat(64))),
            "stable-generation",
        )
        .unwrap();

        assert_eq!(cleanup_staged_artifact_temps(&artifact_dir).unwrap(), 2);
        assert!(!key_root.join(".tmp-crashed").exists());
        assert!(key_root.join("stable-generation").exists());
        assert!(!key_root.join("orphan-generation").exists());
        assert!(key_root.join(PUBLISH_LOCK).exists());
    }

    #[test]
    fn staged_clear_removes_every_visible_generation_but_keeps_coordination_lock() {
        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let sources = source_files(dir.path());
        let key = "7".repeat(64);
        persist_staged_artifact_paths(&artifact_dir, &key, &sources).unwrap();
        #[cfg(unix)]
        let outside = {
            let outside = dir.path().join("outside-clear-boundary");
            fs::create_dir(&outside).unwrap();
            fs::write(outside.join("must-survive"), b"outside").unwrap();
            std::os::unix::fs::symlink(
                &outside,
                artifact_dir.join(STAGED_ROOT).join("hostile-symlink"),
            )
            .unwrap();
            outside
        };

        assert!(clear_staged_artifacts(&artifact_dir).unwrap() > 0);
        #[cfg(unix)]
        assert_eq!(fs::read(outside.join("must-survive")).unwrap(), b"outside");
        assert!(load_staged_artifact_paths(&artifact_dir, &key, &[23, 24])
            .unwrap()
            .is_none());
        let remaining: Vec<_> = fs::read_dir(artifact_dir.join(STAGED_ROOT))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(remaining, vec![std::ffi::OsString::from(STORE_LOCK)]);
    }

    #[test]
    fn mutable_page_writer_never_shares_backend_inode() {
        // This is intentionally a database-shaped page writer rather than
        // the sqlite-link compile fixture: it exercises truncate, same-size
        // page replacement, and a journal-like sibling file.
        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let backend = dir.path().join("backend.db");
        let journal = dir.path().join("backend.db-wal");
        fs::write(&backend, vec![0x11_u8; 4096]).unwrap();
        fs::write(&journal, b"journal-before-checkpoint").unwrap();
        let sources = vec![backend.clone().into(), journal.clone().into()];
        persist_staged_artifact_paths(&artifact_dir, &"f".repeat(64), &sources).unwrap();
        let journal_size = fs::metadata(&journal).unwrap().len();
        let payloads =
            load_staged_artifact_paths(&artifact_dir, &"f".repeat(64), &[4096, journal_size])
                .unwrap()
                .unwrap();
        let destination = dir.path().join("work.db");
        materialize_independent(&payloads[0], &destination).unwrap();
        let mut page = vec![0x22_u8; 4096];
        page[37] = 0x99;
        fs::write(&destination, page).unwrap();
        assert_eq!(fs::read(&backend).unwrap(), vec![0x11_u8; 4096]);
        assert_ne!(fs::read(&destination).unwrap(), fs::read(&backend).unwrap());
        assert!(!same_file(&payloads[0], &destination));
    }
}
