//! Artifact pack format, atomic writes, hardlinking, cached output materialization.

use super::*;

pub(super) fn artifact_persist_tmp_path(cache_path: &Path) -> PathBuf {
    let counter = ARTIFACT_PERSIST_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = cache_path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "artifact".into());
    cache_path.with_file_name(format!(".{name}.tmp-{}-{counter}", std::process::id()))
}

pub(super) fn persist_artifact_output(cache_path: &Path, payload: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = artifact_persist_tmp_path(cache_path);
    let result = (|| {
        std::fs::write(&tmp_path, payload)?;
        replace_artifact_cache_file(&tmp_path, cache_path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

// ─── Artifact-pack format (experimental, env-gated) ───────────────────────────
//
// Layout of `{key_hex}.pack`:
//
//   [magic: 4 bytes = b"ZCPK"]
//   [num_payloads: u32 le]
//   [(offset: u64 le, size: u64 le)] * num_payloads
//   [payload_0 bytes]
//   [payload_1 bytes]
//   ...
//
// Why: each `std::fs::write` of a fresh file under Windows Defender pays a
// per-file scan cost. Packing N payloads of one cache miss into a single
// `.pack` collapses the per-file overhead by N. Bench measured 2.6× wall-clock
// improvement at 5 payloads per artifact (see `tests/persist_pool_bench.rs`).
//
// Trade-off: hit path can't hardlink — it must slice the pack and write the
// extracted bytes. Gated by `ZCCACHE_PACK_ARTIFACTS` until the read-path cost
// is measured against the write-path win on real workloads.

pub(super) const PACK_MAGIC: &[u8; 4] = b"ZCPK";

pub(super) fn pack_mode_enabled() -> bool {
    std::env::var("ZCCACHE_PACK_ARTIFACTS")
        .ok()
        .is_some_and(|v| !v.is_empty() && v != "0")
}

pub(super) fn pack_path_for(artifact_dir: &Path, key_hex: &str) -> PathBuf {
    artifact_dir.join(format!("{key_hex}.pack"))
}

pub(super) fn build_pack(payloads: &[Arc<Vec<u8>>]) -> Vec<u8> {
    let n = payloads.len();
    let header_size = 4 + 4 + n * 16;
    let body_size: usize = payloads.iter().map(|p| p.len()).sum();
    let mut buf = Vec::with_capacity(header_size + body_size);
    buf.extend_from_slice(PACK_MAGIC);
    buf.extend_from_slice(&(n as u32).to_le_bytes());
    let mut offset = header_size as u64;
    for p in payloads {
        buf.extend_from_slice(&offset.to_le_bytes());
        buf.extend_from_slice(&(p.len() as u64).to_le_bytes());
        offset += p.len() as u64;
    }
    for p in payloads {
        buf.extend_from_slice(p);
    }
    buf
}

pub(super) fn parse_pack_header(data: &[u8]) -> std::io::Result<Vec<(u64, u64)>> {
    if data.len() < 8 || &data[..4] != PACK_MAGIC {
        return Err(std::io::Error::other("not a zccache pack file"));
    }
    let n = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
    let needed = 8 + n * 16;
    if data.len() < needed {
        return Err(std::io::Error::other("pack header truncated"));
    }
    let mut entries = Vec::with_capacity(n);
    for i in 0..n {
        let base = 8 + i * 16;
        let offset = u64::from_le_bytes(data[base..base + 8].try_into().unwrap());
        let size = u64::from_le_bytes(data[base + 8..base + 16].try_into().unwrap());
        entries.push((offset, size));
    }
    Ok(entries)
}

/// Try to extract the i-th payload from `{key_hex}.pack`. Returns None if the
/// pack file is missing, corrupt, or doesn't have that many payloads.
pub(super) fn try_load_packed_payload(
    artifact_dir: &Path,
    key_hex: &str,
    idx: usize,
) -> Option<Vec<u8>> {
    let pack_path = pack_path_for(artifact_dir, key_hex);
    let data = std::fs::read(&pack_path).ok()?;
    let entries = parse_pack_header(&data).ok()?;
    let &(offset, size) = entries.get(idx)?;
    let start = offset as usize;
    let end = start.checked_add(size as usize)?;
    if end > data.len() {
        return None;
    }
    Some(data[start..end].to_vec())
}

/// Persist all payloads of one artifact, either as N individual files
/// (today's layout) or as a single `.pack` file (env-gated). Wraps every
/// inner `std::fs::write` in `persist_artifact_output`'s tmp-then-rename
/// atomicity.
pub(super) fn persist_artifact_payloads(
    artifact_dir: &Path,
    key_hex: &str,
    payloads: &[Arc<Vec<u8>>],
) -> std::io::Result<()> {
    if pack_mode_enabled() {
        let pack = build_pack(payloads);
        return persist_artifact_output(&pack_path_for(artifact_dir, key_hex), &pack);
    }
    // Run inline for small N — rayon dispatch cost is comparable to the
    // syscalls themselves below the threshold (same break-even as
    // `write_payloads_par`). Empirically tuned in
    // `crates/zccache-daemon/benches/persist_payloads.rs`.
    if payloads.len() < PAR_WRITE_THRESHOLD {
        for (i, payload) in payloads.iter().enumerate() {
            let cache_path = artifact_dir.join(format!("{key_hex}_{i}"));
            persist_artifact_output(&cache_path, payload)?;
        }
        return Ok(());
    }
    use rayon::prelude::*;
    // `reduce` preserves the prior "return first error" semantics:
    // `a.and(b)` returns the first `Err` it sees and otherwise `Ok(())`.
    payloads
        .par_iter()
        .enumerate()
        .map(|(i, payload)| {
            let cache_path = artifact_dir.join(format!("{key_hex}_{i}"));
            persist_artifact_output(&cache_path, payload)
        })
        .reduce(|| Ok(()), |a, b| a.and(b))
}

/// Persist artifact payloads when the daemon already has them on disk — typical
/// for the rustc multi-compile miss path where the compiler just wrote outputs
/// to `target/.../<name>` and the daemon would otherwise `std::fs::read` them
/// into RAM before writing them back to the cache.
///
/// Each cache file is created via `persist_artifact_file` — `std::fs::hard_link`
/// with a same-volume requirement and a copy fallback for cross-volume cases.
/// Net effect on the cold-write path: one disk write per output instead of two,
/// halving the per-file overhead Defender real-time scanning pays on Windows.
///
/// Pack mode (`ZCCACHE_PACK_ARTIFACTS=1`) still needs the bytes contiguous, so
/// it materialises each path via `std::fs::read` and falls through to the
/// existing `persist_artifact_output`. The hardlink win only applies when pack
/// mode is off (the default).
pub(super) fn persist_artifact_paths(
    artifact_dir: &Path,
    key_hex: &str,
    sources: &[NormalizedPath],
) -> std::io::Result<()> {
    if pack_mode_enabled() {
        let bytes: Vec<Arc<Vec<u8>>> = sources
            .iter()
            .map(|p| std::fs::read(p.as_path()).map(Arc::new))
            .collect::<std::io::Result<_>>()?;
        let pack = build_pack(&bytes);
        return persist_artifact_output(&pack_path_for(artifact_dir, key_hex), &pack);
    }
    if sources.len() < PAR_WRITE_THRESHOLD {
        for (i, source) in sources.iter().enumerate() {
            let cache_path = artifact_dir.join(format!("{key_hex}_{i}"));
            persist_artifact_file(&cache_path, source.as_path())?;
        }
        return Ok(());
    }
    use rayon::prelude::*;
    sources
        .par_iter()
        .enumerate()
        .map(|(i, source)| {
            let cache_path = artifact_dir.join(format!("{key_hex}_{i}"));
            persist_artifact_file(&cache_path, source.as_path()).map(|_| ())
        })
        .reduce(|| Ok(()), |a, b| a.and(b))
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct PersistArtifactFileStats {
    pub(super) hardlink_count: u64,
    pub(super) copy_count: u64,
    pub(super) copy_bytes: u64,
}

pub(super) fn persist_artifact_file(
    cache_path: &Path,
    source_path: &Path,
) -> std::io::Result<PersistArtifactFileStats> {
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp_path = artifact_persist_tmp_path(cache_path);
    let result = (|| match std::fs::hard_link(source_path, &tmp_path) {
        Ok(()) => {
            replace_artifact_cache_file(&tmp_path, cache_path)?;
            Ok(PersistArtifactFileStats {
                hardlink_count: 1,
                ..PersistArtifactFileStats::default()
            })
        }
        Err(_) => {
            let copy_bytes = std::fs::copy(source_path, &tmp_path)?;
            replace_artifact_cache_file(&tmp_path, cache_path)?;
            Ok(PersistArtifactFileStats {
                copy_count: 1,
                copy_bytes,
                ..PersistArtifactFileStats::default()
            })
        }
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

#[cfg(not(windows))]
pub(super) fn replace_artifact_cache_file(
    tmp_path: &Path,
    cache_path: &Path,
) -> std::io::Result<()> {
    std::fs::rename(tmp_path, cache_path)
}

#[cfg(windows)]
pub(super) fn replace_artifact_cache_file(
    tmp_path: &Path,
    cache_path: &Path,
) -> std::io::Result<()> {
    match std::fs::rename(tmp_path, cache_path) {
        Ok(()) => Ok(()),
        Err(_) if cache_path.exists() => {
            std::fs::remove_file(cache_path)?;
            std::fs::rename(tmp_path, cache_path)
        }
        Err(err) => Err(err),
    }
}

/// Write cached output to disk. Optimized syscall sequence:
/// 1. Try hardlink directly (1 syscall — common case when output doesn't exist)
/// 2. If output already exists: check if it's the same file (skip if so)
/// 3. Remove existing output and retry hardlink (2 syscalls)
/// 4. Fall back to fs::write from memory (1 syscall)
///
/// After writing, the output's mtime is set to the current time. This is
/// critical for build system compatibility: cargo, make, and ninja use mtime
/// to determine if an output is fresh relative to its dependencies. Without
/// this, hardlinked outputs inherit the cache file's old mtime, causing
/// build systems to consider them stale and triggering unnecessary rebuilds.
/// See issue #15 for the full root cause analysis.
///
/// The hardlink-first order optimizes for the rebuild scenario where outputs
/// don't exist yet (1 syscall). For incremental builds where outputs exist
/// as hardlinks, the failed hardlink + same_file check is still fast.
pub(super) fn write_cached_output(
    out_path: &Path,
    cache_file: &Path,
    data: &[u8],
) -> std::io::Result<()> {
    // Fast path: hardlink directly (works when out_path doesn't exist yet).
    // This is the cheapest path — one kernel call when no output exists.
    if std::fs::hard_link(cache_file, out_path).is_ok() {
        touch_mtime(out_path);
        return Ok(());
    }
    // Hardlink failed — output probably exists. Check if it's already
    // the same file (hardlinked from a previous hit). Compare file
    // identity (inode/volume+index), NOT file size — two different
    // compilations can produce .o files with identical sizes but
    // different content (alignment, padding).
    if same_file(out_path, cache_file) {
        touch_mtime(out_path);
        return Ok(());
    }
    // Output exists but is different — remove and retry
    let _ = std::fs::remove_file(out_path);
    if std::fs::hard_link(cache_file, out_path).is_ok() {
        touch_mtime(out_path);
        return Ok(());
    }
    // Hardlink failed entirely (cross-device, no cache file) — copy from memory.
    // fs::write creates a new file with current mtime, so no touch needed.
    std::fs::write(out_path, data)
}

pub(super) fn write_cached_file(out_path: &Path, cache_file: &Path) -> std::io::Result<()> {
    if std::fs::hard_link(cache_file, out_path).is_ok() {
        touch_mtime(out_path);
        return Ok(());
    }
    if same_file(out_path, cache_file) {
        touch_mtime(out_path);
        return Ok(());
    }
    let _ = std::fs::remove_file(out_path);
    if std::fs::hard_link(cache_file, out_path).is_ok() {
        touch_mtime(out_path);
        return Ok(());
    }
    std::fs::copy(cache_file, out_path)?;
    touch_mtime(out_path);
    Ok(())
}

pub(super) fn write_cached_payload(
    out_path: &Path,
    cache_file: &Path,
    payload: &CachedPayload,
) -> std::io::Result<()> {
    match payload {
        CachedPayload::Bytes(data) => write_cached_output(out_path, cache_file, data),
        CachedPayload::File(path) => write_cached_file(out_path, path),
    }
}

/// Write a batch of cached payloads to their target paths in parallel.
///
/// `targets[i]` is the `(out_path, cache_file)` pair for `payloads[i]`. The
/// parent of each `out_path` is created if it does not exist (matches the
/// per-site behavior of the link-hit path). Returns `true` iff every write
/// succeeded; `false` on first failure (callers either fall through to a
/// fallback path or ignore the result, mirroring the prior serial loops).
///
/// Threshold: rayon is only used when `targets.len() >= 4`. For N ≤ 3 the
/// per-iteration thread-pool dispatch cost (~300 µs) is comparable to the
/// hardlink syscalls themselves, so a serial loop is faster. The
/// `benches/write_payloads.rs` micro-benchmark sets this cut-off
/// empirically on the Windows / NTFS host.
pub(super) const PAR_WRITE_THRESHOLD: usize = 4;

pub(super) fn write_payloads_par<P, Q>(targets: &[(P, Q)], payloads: &[CachedPayload]) -> bool
where
    P: AsRef<Path> + Sync,
    Q: AsRef<Path> + Sync,
{
    debug_assert_eq!(targets.len(), payloads.len());
    let write_one = |out: &Path, cache: &Path, payload: &CachedPayload| -> bool {
        if let Some(parent) = out.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        write_cached_payload(out, cache, payload).is_ok()
    };
    if targets.len() < PAR_WRITE_THRESHOLD {
        return targets
            .iter()
            .zip(payloads.iter())
            .all(|((out, cache), payload)| write_one(out.as_ref(), cache.as_ref(), payload));
    }
    use rayon::prelude::*;
    targets
        .par_iter()
        .zip(payloads.par_iter())
        .all(|((out, cache), payload)| write_one(out.as_ref(), cache.as_ref(), payload))
}

pub(super) fn break_output_hardlink_before_compile(path: &Path) -> std::io::Result<()> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }

    if hard_link_count(path)? <= 1 {
        return Ok(());
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("output"))
        .to_string_lossy();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();

    let mut last_err = None;
    for attempt in 0..32 {
        let tmp_path = parent.join(format!(
            ".zccache-detach-{pid}-{nonce}-{attempt}-{file_name}"
        ));
        let copy_result = (|| {
            let mut src = std::fs::File::open(path)?;
            let mut dst = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)?;
            std::io::copy(&mut src, &mut dst)?;
            dst.sync_all()?;
            let permissions = src.metadata()?.permissions();
            std::fs::set_permissions(&tmp_path, permissions)?;
            Ok::<(), std::io::Error>(())
        })();

        match copy_result {
            Ok(()) => {
                if let Err(e) = std::fs::remove_file(path) {
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(e);
                }
                if let Err(e) = std::fs::rename(&tmp_path, path) {
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(e);
                }
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_err = Some(e);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(e);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "failed to create hardlink detach temp file",
        )
    }))
}

#[cfg(unix)]
pub(super) fn hard_link_count(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;

    Ok(std::fs::metadata(path)?.nlink())
}

#[cfg(windows)]
pub(super) fn hard_link_count(path: &Path) -> std::io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_NORMAL,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();

    unsafe {
        let handle = CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        );
        if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }

        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        let ok = GetFileInformationByHandle(handle, &mut info);
        let close_result = CloseHandle(handle);

        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if close_result == 0 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(info.nNumberOfLinks as u64)
    }
}

/// No-op kept as a named seam so callers express intent ("the output
/// is now in place"). Previously this stamped `mtime = now()` to make
/// make/ninja treat the artifact as freshly produced. That broke
/// cargo's incremental fingerprint: cargo records the artifact's mtime
/// when rustc first produces it, and a hardlinked cache hit that bumps
/// mtime to `now` looks "externally touched" → cargo invalidates the
/// downstream graph, paying re-link/re-fingerprint overhead that
/// negates the cache savings. Iter7 of the cold-tar-untar-warm OODA
/// loop measured a 2.5s warm-time regression when this fired on `bin`
/// outputs. Skipping the touch preserves the cache file's stored
/// mtime, which is what cargo expects to see. make/ninja workflows
/// rely on content/depfile freshness as well, so this trade-off is
/// considered net-positive across consumers.
pub(super) fn touch_mtime(_path: &Path) {}

/// Check if two paths refer to the same file (hardlink check).
///
/// Returns `false` if either file doesn't exist or the check fails.
#[cfg(unix)]
pub(super) fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(ma), Ok(mb)) => ma.dev() == mb.dev() && ma.ino() == mb.ino(),
        _ => false,
    }
}

#[cfg(windows)]
pub(super) fn same_file(a: &Path, b: &Path) -> bool {
    get_file_id(a)
        .zip(get_file_id(b))
        .map(|(ia, ib)| ia == ib)
        .unwrap_or(false)
}

/// Returns (volume_serial, file_index_high, file_index_low) for a path.
#[cfg(windows)]
pub(super) fn get_file_id(path: &Path) -> Option<(u32, u32, u32)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_NORMAL,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();

    unsafe {
        let handle = CreateFileW(
            wide.as_ptr(),
            0, // no access needed, just metadata
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        );
        if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return None;
        }

        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        let ok = GetFileInformationByHandle(handle, &mut info);
        CloseHandle(handle);

        if ok == 0 {
            return None;
        }

        Some((
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
        ))
    }
}
