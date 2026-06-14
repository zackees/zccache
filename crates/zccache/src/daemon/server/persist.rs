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
        std::fs::create_dir_all(parent).map_err(|e| enrich_persist_err(e, None, cache_path))?;
    }
    let tmp_path = artifact_persist_tmp_path(cache_path);
    let result = (|| {
        std::fs::write(&tmp_path, payload)?;
        replace_artifact_cache_file(&tmp_path, cache_path)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(enrich_persist_err(e, None, cache_path));
    }
    Ok(())
}

// Issue #728: failed cache writes used to surface as a bare io::Error with no
// path context, leaving us unable to tell whether the source file vanished
// mid-flight (TOCTOU against ninja), whether the destination dir was wrong,
// or whether Defender quarantined the file. The error returned from
// `persist_artifact_file` / `persist_artifact_output` now embeds:
//   src=, dst=, errno=, src_exists_now=, src_size_now=
// so the WARN at the call site can distinguish those cases without plumbing
// extra fields through.
//
// Pass `src = None` for payload writes (the bytes came from RAM — there is
// no source file to stat). The `src_exists_now=` / `src_size_now=` fields
// are then omitted.
pub(super) fn enrich_persist_err(
    orig: std::io::Error,
    src: Option<&Path>,
    dst: &Path,
) -> std::io::Error {
    let errno = orig.raw_os_error();
    let kind = orig.kind();
    let mut msg = String::new();
    if let Some(src) = src {
        use std::fmt::Write as _;
        let (exists_now, size_now) = match std::fs::metadata(src) {
            Ok(meta) => (true, Some(meta.len())),
            Err(_) => (false, None),
        };
        let _ = write!(msg, "src={}", src.display());
        let _ = write!(msg, " src_exists_now={exists_now}");
        match size_now {
            Some(size) => {
                let _ = write!(msg, " src_size_now={size}");
            }
            None => {
                let _ = write!(msg, " src_size_now=?");
            }
        }
        msg.push(' ');
    }
    use std::fmt::Write as _;
    let _ = write!(msg, "dst={}", dst.display());
    let _ = write!(msg, " errno={errno:?}");
    let _ = write!(msg, ": {orig}");
    std::io::Error::new(kind, msg)
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
    persist_artifact_paths_with_stats(artifact_dir, key_hex, sources).map(|_| ())
}

/// Same as `persist_artifact_paths`, plus aggregate hardlink/copy/copy-bytes
/// stats summed across every source. Lets the rustc miss path use the same
/// serial-vs-rayon threshold without re-implementing the loop. Pack mode
/// returns default stats — its single packed write doesn't yield per-source
/// hardlink/copy attribution.
pub(super) fn persist_artifact_paths_with_stats(
    artifact_dir: &Path,
    key_hex: &str,
    sources: &[NormalizedPath],
) -> std::io::Result<PersistArtifactFileStats> {
    if pack_mode_enabled() {
        let bytes: Vec<Arc<Vec<u8>>> = sources
            .iter()
            .map(|p| std::fs::read(p.as_path()).map(Arc::new))
            .collect::<std::io::Result<_>>()?;
        let pack = build_pack(&bytes);
        persist_artifact_output(&pack_path_for(artifact_dir, key_hex), &pack)?;
        return Ok(PersistArtifactFileStats::default());
    }
    if sources.len() < PAR_WRITE_THRESHOLD {
        let mut stats = PersistArtifactFileStats::default();
        for (i, source) in sources.iter().enumerate() {
            let cache_path = artifact_dir.join(format!("{key_hex}_{i}"));
            let one = persist_artifact_file(&cache_path, source.as_path())?;
            stats.hardlink_count += one.hardlink_count;
            stats.copy_count += one.copy_count;
            stats.copy_bytes += one.copy_bytes;
        }
        return Ok(stats);
    }
    use rayon::prelude::*;
    sources
        .par_iter()
        .enumerate()
        .map(|(i, source)| {
            let cache_path = artifact_dir.join(format!("{key_hex}_{i}"));
            persist_artifact_file(&cache_path, source.as_path())
        })
        .reduce(
            || Ok(PersistArtifactFileStats::default()),
            |a, b| match (a, b) {
                (Ok(x), Ok(y)) => Ok(PersistArtifactFileStats {
                    hardlink_count: x.hardlink_count + y.hardlink_count,
                    copy_count: x.copy_count + y.copy_count,
                    copy_bytes: x.copy_bytes + y.copy_bytes,
                }),
                (Err(e), _) | (_, Err(e)) => Err(e),
            },
        )
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
        std::fs::create_dir_all(parent)
            .map_err(|e| enrich_persist_err(e, Some(source_path), cache_path))?;
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
    match result {
        Ok(stats) => Ok(stats),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            Err(enrich_persist_err(e, Some(source_path), cache_path))
        }
    }
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
    av_scan_retry(|| match std::fs::rename(tmp_path, cache_path) {
        Ok(()) => Ok(()),
        Err(_) if cache_path.exists() => {
            std::fs::remove_file(cache_path)?;
            std::fs::rename(tmp_path, cache_path)
        }
        Err(err) => Err(err),
    })
}

// ── Windows AV-scanner retry (issue #490) ──────────────────────────────────
//
// Defender / EDR tools open just-written files for an inline scan with a
// restrictive share mode and no `FILE_SHARE_DELETE`, so any `MoveFileExW` /
// `DeleteFileW` against the target during the scan window fails with
// `ERROR_ACCESS_DENIED` (5) or `ERROR_SHARING_VIOLATION` (32). The scan window
// is short — typically tens to a few hundred milliseconds — so a bounded
// back-off retry absorbs the race without papering over real ACL failures
// (those persist past the budget and surface to the caller unchanged).

#[cfg(windows)]
const AV_SCAN_RETRY_DELAYS_MS: &[u64] = &[50, 100, 250, 500];

#[cfg(windows)]
fn is_av_scan_transient(err: &std::io::Error) -> bool {
    if matches!(err.kind(), std::io::ErrorKind::PermissionDenied) {
        return true;
    }
    matches!(err.raw_os_error(), Some(5) | Some(32))
}

#[cfg(windows)]
fn av_scan_retry<T, F>(mut op: F) -> std::io::Result<T>
where
    F: FnMut() -> std::io::Result<T>,
{
    for &delay in AV_SCAN_RETRY_DELAYS_MS {
        match op() {
            Ok(value) => return Ok(value),
            Err(err) if is_av_scan_transient(&err) => {
                std::thread::sleep(std::time::Duration::from_millis(delay));
            }
            Err(err) => return Err(err),
        }
    }
    op()
}

/// Write cached output to disk. Optimized syscall sequence:
/// 1. Try hardlink directly (1 syscall — common case when output doesn't exist)
/// 2. If output already exists: check if it's the same file (skip if so)
/// 3. Remove existing output and retry hardlink (2 syscalls)
/// 4. Fall back to fs::write from memory (1 syscall)
///
/// **Mtime policy**: the output inherits the cache file's stored mtime by
/// default. We deliberately do NOT stamp `now()` — that was the pre-iter7
/// behaviour and it caused cargo's incremental fingerprint to mark
/// hardlinked artifacts as "externally modified", invalidating the
/// downstream graph (measured 5.9 ms → 2.8 ms per-hit + recovery of the
/// `bin`-cell recompile cascade in the cold-tar-untar-warm scenario).
/// Preservation is also the cheapest possible policy: zero extra syscalls.
///
/// The only exception is the sibling-floor refinement in [`touch_mtime`],
/// which bumps the artifact's mtime UP **only when** an existing sibling
/// artifact in the same directory already has a higher mtime — required to
/// keep cargo's "dep_mtime ≤ my_mtime" check from misfiring on
/// out-of-order materialization (issues #466 / #467). In isolation (no
/// siblings, or all siblings older), the floor is a no-op and the cache
/// mtime is preserved verbatim — the fast path.
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
        CachedPayload::PendingFile { source_path } => {
            // Try the cache file first; if the async persist hasn't
            // completed yet, fall back to the rustc-output source path.
            // Once persist finishes both paths hardlink to the same
            // inode so subsequent hits see no difference (issue #632).
            if cache_file.exists() {
                write_cached_file(out_path, cache_file)
            } else {
                write_cached_file(out_path, source_path.as_path())
            }
        }
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

pub(super) fn write_payloads_par_with_mtime_floor<P, Q, R>(
    targets: &[(P, Q)],
    payloads: &[CachedPayload],
    floor_paths: &[R],
) -> bool
where
    P: AsRef<Path> + Sync,
    Q: AsRef<Path> + Sync,
    R: AsRef<Path>,
{
    if !write_payloads_par(targets, payloads) {
        return false;
    }
    let batch_floor = std::time::SystemTime::now();
    floor_materialized_outputs_to_input_max(
        targets.iter().map(|(out, _)| out.as_ref()),
        floor_paths.iter().map(|path| path.as_ref()),
        batch_floor,
    );
    true
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

/// **Preserve** the cache file's stored mtime on the materialized artifact
/// by default, only bumping UP to the max of existing sibling compilation
/// artifacts (`*.rlib` / `*.rmeta` / `*.so` / `*.dylib` / `*.dll` /
/// `*.exe` / `*.a` / `*.lib`) in the same directory when that is needed
/// to satisfy cargo's "dep_mtime ≤ my_mtime" fingerprint invariant.
/// Preservation is the fast path. The floor is a corrective.
///
/// **Why preservation is the default** (iter7): stamping `now()` made
/// cargo treat hardlinked cache hits as "externally modified", invalidating
/// the downstream graph and paying re-link / re-fingerprint cost that
/// fully cancelled the cache savings. Measured 5.9 ms → 2.8 ms per-hit,
/// and recovery of the `bin`-cell recompile cascade (cold-tar-untar-warm,
/// warm 11.6 s → 9.8 s). Preservation is also the cheapest policy.
///
/// **Why the floor exception exists** (issues #466 / #467): cargo's
/// `Fingerprint::check_filesystem` emits `FsStatusOutdated::StaleDependency`
/// when any dep's artifact mtime is strictly greater than the dependent's
/// (`dep_mtime > my_mtime → stale`). Cache files for transitively-
/// dependent crates are not guaranteed to have correctly-ordered mtimes:
/// archive truncation, parallel cache stores, and out-of-order
/// re-materialization all break the dep-before-dependent invariant. The
/// GH bench measured this as 31 crates recompiling on every "warm with
/// target intact" build, taking ~3 s vs sccache's 215 ms baseline. The
/// floor closes that gap without re-introducing the iter7 regression
/// because it only ever increases mtime to a *stable sibling-derived
/// value* (not `now()`), and the value is idempotent across rebuilds:
/// 1. The next hit on the same artifact takes the hardlink / same-file
///    fast path and returns without re-flooring.
/// 2. If we do re-floor, sibling mtimes only ever grow (cache hits
///    materialise with their cache mtime; the floor floors UP from
///    there), so the value converges.
///
/// **Cost**: in isolation (no siblings, or all siblings older than the
/// cache mtime), the floor is a pure no-op after one `read_dir` + N
/// stats — ~50 µs on a `target/debug/deps/` with 300 entries. When the
/// floor actually bumps, the amortised cost is hidden behind the cargo
/// recompilation it prevents (~3 s saved for 50 µs of stat work).
///
/// Disable via `ZCCACHE_DISABLE_MTIME_FLOOR=1` if the floor causes
/// problems with a specific build system (this also disables the
/// preservation guarantee's enforcement, so the cache file's mtime is
/// what survives — still not `now()`).
pub(super) fn touch_mtime(path: &Path) {
    if mtime_floor_disabled() {
        return;
    }
    let _ = floor_artifact_mtime_to_sibling_max(path);
}

fn floor_materialized_outputs_to_input_max<'a>(
    output_paths: impl IntoIterator<Item = &'a Path>,
    input_paths: impl IntoIterator<Item = &'a Path>,
    minimum_mtime: std::time::SystemTime,
) {
    if mtime_floor_disabled() {
        return;
    }

    let outputs: Vec<&Path> = output_paths.into_iter().collect();
    if outputs.is_empty() {
        return;
    }

    let mut max_mtime = minimum_mtime;
    for path in outputs.iter().copied().chain(input_paths) {
        let Ok(mtime) = std::fs::metadata(path).and_then(|metadata| metadata.modified()) else {
            continue;
        };
        if mtime > max_mtime {
            max_mtime = mtime;
        }
    }

    let ft = filetime::FileTime::from_system_time(max_mtime);
    for path in outputs {
        let Ok(current) = std::fs::metadata(path).and_then(|metadata| metadata.modified()) else {
            continue;
        };
        if current < max_mtime {
            let _ = filetime::set_file_mtime(path, ft);
        }
    }
}

fn mtime_floor_disabled() -> bool {
    use std::sync::OnceLock;
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("ZCCACHE_DISABLE_MTIME_FLOOR")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0")
    })
}

fn floor_artifact_mtime_to_sibling_max(path: &Path) -> std::io::Result<()> {
    let parent = match path.parent() {
        Some(p) => p,
        None => return Ok(()),
    };
    let my_mtime = std::fs::metadata(path)?.modified()?;
    let mut max_mtime = my_mtime;
    for entry in std::fs::read_dir(parent)?.flatten() {
        let p = entry.path();
        // Skip self — comparing against our own mtime is a no-op but
        // would waste a stat.
        if p == path {
            continue;
        }
        // Filter to artifact extensions cargo's `Fingerprint::outputs`
        // tracks. Other entries (.d depfiles, .json metadata,
        // .fingerprint state) don't participate in the StaleDependency
        // comparison.
        let ext = match p.extension().and_then(|s| s.to_str()) {
            Some(e) => e,
            None => continue,
        };
        if !matches!(
            ext,
            "rlib" | "rmeta" | "so" | "dylib" | "dll" | "exe" | "a" | "lib"
        ) {
            continue;
        }
        if let Ok(m) = entry.metadata().and_then(|md| md.modified()) {
            if m > max_mtime {
                max_mtime = m;
            }
        }
    }
    if max_mtime > my_mtime {
        let ft = filetime::FileTime::from_system_time(max_mtime);
        let _ = filetime::set_file_mtime(path, ft);
    }
    Ok(())
}

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

#[cfg(test)]
mod tests {
    //! Tests for `floor_artifact_mtime_to_sibling_max` (issues #466 / #467).
    //!
    //! These exercise the dep-mtime-ordering fix in isolation, without
    //! standing up a full daemon. The function is private; the tests live
    //! in the same module so they can call it directly.

    use super::*;
    use std::time::{Duration, SystemTime};

    fn write_with_mtime(path: &Path, contents: &[u8], mtime: SystemTime) {
        std::fs::write(path, contents).unwrap();
        let ft = filetime::FileTime::from_system_time(mtime);
        filetime::set_file_mtime(path, ft).unwrap();
    }

    fn mtime_of(path: &Path) -> SystemTime {
        std::fs::metadata(path).unwrap().modified().unwrap()
    }

    fn epoch_plus(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn floor_noop_when_target_dir_is_empty() {
        // Single artifact, no siblings — mtime must be preserved (iter7
        // invariant). The floor must not invent a value out of thin air.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("only.rlib");
        let before = epoch_plus(1_000_000);
        write_with_mtime(&target, b"x", before);

        floor_artifact_mtime_to_sibling_max(&target).unwrap();

        assert_eq!(mtime_of(&target), before);
    }

    #[test]
    fn floor_noop_when_already_newest() {
        // Target artifact already has the highest mtime among siblings —
        // floor must not lower it (this is the "fresh build" case).
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("newer.rlib");
        let older = dir.path().join("older.rlib");
        write_with_mtime(&target, b"t", epoch_plus(2_000_000));
        write_with_mtime(&older, b"o", epoch_plus(1_000_000));

        floor_artifact_mtime_to_sibling_max(&target).unwrap();

        assert_eq!(mtime_of(&target), epoch_plus(2_000_000));
    }

    #[test]
    fn floor_bumps_when_sibling_is_newer() {
        // The "cache hit out of order" case: zccache materialised the
        // dependent first (older cache mtime), the dep second (newer cache
        // mtime). Cargo's strict `dep_mtime > my_mtime → stale` would fire.
        // After floor, `my_mtime == dep_mtime`, satisfying `dep > my == false`.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dependent.rlib");
        let dep = dir.path().join("dep.rlib");
        write_with_mtime(&target, b"t", epoch_plus(1_000_000));
        write_with_mtime(&dep, b"d", epoch_plus(2_000_000));

        floor_artifact_mtime_to_sibling_max(&target).unwrap();

        // Floored UP to the dep's mtime — cargo's check passes.
        assert_eq!(mtime_of(&target), epoch_plus(2_000_000));
        // Dep was not touched.
        assert_eq!(mtime_of(&dep), epoch_plus(2_000_000));
    }

    #[test]
    fn floor_ignores_non_artifact_files() {
        // Cargo's StaleDependency check looks at output artifacts only
        // (rlib/rmeta/so/dylib/dll/exe/a/lib). The floor must skip
        // depfiles (.d), fingerprint state, JSON sidecars, etc., so a
        // newer .d file doesn't artificially bump the artifact's mtime.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("art.rlib");
        let dep_file = dir.path().join("dep.d");
        let json_sidecar = dir.path().join("meta.json");
        write_with_mtime(&target, b"t", epoch_plus(1_000_000));
        write_with_mtime(&dep_file, b"d", epoch_plus(5_000_000));
        write_with_mtime(&json_sidecar, b"j", epoch_plus(5_000_000));

        floor_artifact_mtime_to_sibling_max(&target).unwrap();

        // .d and .json are filtered out — target mtime stays at its
        // original value.
        assert_eq!(mtime_of(&target), epoch_plus(1_000_000));
    }

    #[test]
    fn floor_idempotent_under_repeated_application() {
        // Subsequent cache hits for the same artifact must converge to a
        // stable mtime — otherwise cargo's "externally modified" check
        // (the original iter7 concern) would fire on repeat builds.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("art.rlib");
        let dep = dir.path().join("dep.rlib");
        write_with_mtime(&target, b"t", epoch_plus(1_000_000));
        write_with_mtime(&dep, b"d", epoch_plus(2_000_000));

        floor_artifact_mtime_to_sibling_max(&target).unwrap();
        let first = mtime_of(&target);
        floor_artifact_mtime_to_sibling_max(&target).unwrap();
        let second = mtime_of(&target);
        floor_artifact_mtime_to_sibling_max(&target).unwrap();
        let third = mtime_of(&target);

        assert_eq!(first, epoch_plus(2_000_000));
        assert_eq!(second, first);
        assert_eq!(third, first);
    }

    #[test]
    fn batch_floor_bumps_build_script_output_to_extern_mtime() {
        // Issue #599: build-script binaries live in target/debug/build/*,
        // while their rustc extern dependencies live in target/debug/deps.
        // The same-directory floor never saw those extern artifacts.
        let dir = tempfile::tempdir().unwrap();
        let build_dir = dir.path().join("target/debug/build/blake3-abc");
        let deps_dir = dir.path().join("target/debug/deps");
        std::fs::create_dir_all(&build_dir).unwrap();
        std::fs::create_dir_all(&deps_dir).unwrap();

        let cache = dir.path().join("cache/build-script-cache");
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(&cache, b"build script exe").unwrap();
        let old_time = filetime::FileTime::from_unix_time(1_000_000, 0);
        filetime::set_file_mtime(&cache, old_time).unwrap();

        let extern_dep = deps_dir.join("libcc-new.rlib");
        write_with_mtime(
            &extern_dep,
            b"cc rlib",
            SystemTime::UNIX_EPOCH + Duration::new(2_000_000, 123_456_700),
        );
        let dep_mtime = mtime_of(&extern_dep);

        let output = build_dir.join("build-script-build");
        let targets = vec![(output.clone(), cache.clone())];
        let payloads = vec![CachedPayload::File(cache.clone().into())];
        let floor_paths = vec![extern_dep.clone()];

        assert!(write_payloads_par_with_mtime_floor(
            &targets,
            &payloads,
            &floor_paths,
        ));

        let output_mtime = mtime_of(&output);
        assert!(
            output_mtime >= dep_mtime,
            "extensionless build-script output must be at least as new as extern dependency; \
             output={output_mtime:?}, dep={dep_mtime:?}",
        );
    }

    #[test]
    fn batch_floor_freshens_materialized_outputs_without_floor_paths() {
        // Issue #599: a compile cache hit is still a rustc invocation from
        // Cargo's perspective. If zccache hardlinks an old cache artifact and
        // preserves that old mtime, Cargo records stale output mtimes and the
        // next no-op build recompiles the graph. The batch materializer uses
        // one fresh floor for all outputs from that hit.
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache/libcrate-cache.rlib");
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(&cache, b"rlib").unwrap();
        let old_mtime = epoch_plus(1_000_000);
        filetime::set_file_mtime(&cache, filetime::FileTime::from_system_time(old_mtime)).unwrap();

        let output = dir.path().join("target/debug/deps/libcrate.rlib");
        let targets = vec![(output.clone(), cache.clone())];
        let payloads = vec![CachedPayload::File(cache.clone().into())];
        let floor_paths: Vec<PathBuf> = Vec::new();

        assert!(write_payloads_par_with_mtime_floor(
            &targets,
            &payloads,
            &floor_paths,
        ));

        let output_mtime = mtime_of(&output);
        assert!(
            output_mtime > old_mtime,
            "compile-hit output must not inherit the stale cache mtime; \
             output={output_mtime:?}, old_cache={old_mtime:?}",
        );
    }
}
