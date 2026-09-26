//! Per-file delivery for `zccache warm` under `ZCCACHE_MODE` (#1683).

use std::path::Path;

/// Deliver one `zccache warm` payload per `ZCCACHE_MODE` (#1683): reflink,
/// hardlink, then copy, each only where the mode allows it. `file_times` is
/// zccache's LRU recency signal for the cache file, not a cargo freshness
/// signal: a hardlinked output carries it to the shared inode, and an
/// independent output gets the cache file stamped directly.
pub(crate) fn deliver_warm_file(
    src: &Path,
    dst: &Path,
    mode: crate::core::config::MaterializationMode,
    file_times: std::fs::FileTimes,
) -> std::io::Result<()> {
    let tiers = mode.tiers_for_shareable();
    let cloned = tiers.reflink && kernal_api::platform::fs::reflink_file(src, dst).is_ok();
    let linked = !cloned && tiers.hardlink && {
        // A failed clone may leave a partial destination behind.
        let _ = std::fs::remove_file(dst);
        std::fs::hard_link(src, dst).is_ok()
    };
    if !cloned && !linked {
        let _ = std::fs::remove_file(dst);
        std::fs::copy(src, dst)?;
    }
    if !linked {
        kernal_api::platform::fs::set_readonly(dst, false)?;
        if let Ok(cache_file) = std::fs::File::open(src) {
            let _ = cache_file.set_times(file_times);
        }
    }
    if let Ok(output) = std::fs::File::open(dst) {
        let _ = output.set_times(file_times);
    }
    Ok(())
}

#[cfg(test)]
#[path = "tests/warm_delivery.rs"]
mod tests;
