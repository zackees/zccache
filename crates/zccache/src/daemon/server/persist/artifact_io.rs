//! Atomic artifact writes: tmp-then-rename, error enrichment, and the
//! Windows AV-scanner retry helper.

use super::*;

pub(in crate::daemon::server) fn artifact_persist_tmp_path(cache_path: &Path) -> PathBuf {
    let counter = ARTIFACT_PERSIST_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = cache_path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "artifact".into());
    cache_path.with_file_name(format!(".{name}.tmp-{}-{counter}", std::process::id()))
}

pub(in crate::daemon::server) fn persist_artifact_output(cache_path: &Path, payload: &[u8]) -> std::io::Result<()> {
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
pub(in crate::daemon::server) fn enrich_persist_err(
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
pub(in crate::daemon::server) fn persist_artifact_paths(
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
pub(in crate::daemon::server) fn persist_artifact_paths_with_stats(
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
pub(in crate::daemon::server) struct PersistArtifactFileStats {
    pub(in crate::daemon::server) hardlink_count: u64,
    pub(in crate::daemon::server) copy_count: u64,
    pub(in crate::daemon::server) copy_bytes: u64,
}

pub(in crate::daemon::server) fn persist_artifact_file(
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
pub(in crate::daemon::server) fn replace_artifact_cache_file(
    tmp_path: &Path,
    cache_path: &Path,
) -> std::io::Result<()> {
    std::fs::rename(tmp_path, cache_path)
}

#[cfg(windows)]
pub(in crate::daemon::server) fn replace_artifact_cache_file(
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
