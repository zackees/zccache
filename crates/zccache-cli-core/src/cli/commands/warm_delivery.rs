//! Per-file delivery for `zccache warm` under `ZCCACHE_MODE` (#1683).

use std::path::Path;

/// Deliver one `zccache warm` payload per `ZCCACHE_MODE` (#1683).
///
/// `zccache warm` has always linked first, and AUTO keeps that order: a clone
/// attempt would add a failed syscall per file on every non-reflink volume.
/// LINK, COPY and REFLINK follow [`MaterializationMode::tiers_for_shareable`].
///
/// `now` is zccache's LRU recency stamp for the cache file, not a cargo
/// freshness signal. A hardlinked output carries it to the shared inode; an
/// independent output gets the cache file stamped directly, through
/// `set_file_mtime`, which (unlike a read-only handle on Windows) also works
/// on a read-only cache file.
///
/// [`MaterializationMode::tiers_for_shareable`]: crate::core::config::MaterializationMode::tiers_for_shareable
pub(crate) fn deliver_warm_file(
    src: &Path,
    dst: &Path,
    mode: crate::core::config::MaterializationMode,
    now: std::time::SystemTime,
) -> std::io::Result<()> {
    use crate::core::config::{MaterializationMode, MaterializationTiers};
    let tiers = match mode {
        MaterializationMode::Auto => MaterializationTiers {
            reflink: false,
            hardlink: true,
        },
        _ => mode.tiers_for_shareable(),
    };
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
    let stamp = kernal_api::platform::fs::FileTime::from_system_time(now);
    if !linked {
        kernal_api::platform::fs::set_readonly(dst, false)?;
        let _ = kernal_api::platform::fs::set_file_mtime(src, stamp);
    }
    let _ = kernal_api::platform::fs::set_file_mtime(dst, stamp);
    Ok(())
}

#[cfg(test)]
#[path = "tests/warm_delivery.rs"]
mod tests;
