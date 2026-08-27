//! Capability probing and per-volume-pair materialization policy (#1039).

use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

pub(in crate::daemon::server) const DISABLE_REFLINK_ENV: &str = "ZCCACHE_DISABLE_REFLINK";
pub(in crate::daemon::server) const COW_READONLY_ENV: &str = "ZCCACHE_COW_READONLY";
const CAPS_CACHE_LIMIT: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(in crate::daemon::server) enum FileIdWidth {
    Bits64,
    Bits128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::daemon::server) struct VolumeCaps {
    pub(in crate::daemon::server) reflink: bool,
    pub(in crate::daemon::server) hardlink: bool,
    pub(in crate::daemon::server) readonly_enforced: bool,
    pub(in crate::daemon::server) file_id: FileIdWidth,
    pub(in crate::daemon::server) hardlink_limit: u64,
}

impl VolumeCaps {
    fn copy_only() -> Self {
        Self {
            reflink: false,
            hardlink: false,
            readonly_enforced: false,
            file_id: if crate::platform::fs::volume::file_id_width() > 64 {
                FileIdWidth::Bits128
            } else {
                FileIdWidth::Bits64
            },
            hardlink_limit: 0,
        }
    }

    fn effective(mut self) -> Self {
        self = apply_reflink_switch(self, env_flag(DISABLE_REFLINK_ENV, false));
        self
    }
}

pub(in crate::daemon::server) fn apply_reflink_switch(
    mut caps: VolumeCaps,
    disabled: bool,
) -> VolumeCaps {
    if disabled {
        caps.reflink = false;
    }
    caps
}

// The path ancestor is part of the key, not just the raw volume id.
// Ephemeral disk-image / loop-mount volumes (macOS hdiutil, Linux
// losetup) reliably recycle their device id after unmount — a later,
// unrelated filesystem can attach at the SAME `st_dev`. A cache keyed
// on volume id alone would then hand out a stale capability probed
// against the *previous* filesystem at that id (e.g. HFS+'s
// hardlink=true reused for a freshly-mounted exFAT volume, which has
// no hardlink support at all — `std::fs::hard_link` then fails with
// ENOTSUP despite the cached capability). The probe ancestor path is
// stable for repeat calls into the *same* real mount (so legitimate
// caching across many blobs in one cache directory is preserved) but
// differs across distinct mounts, including ones that reuse a device
// id, because mount points are never reused verbatim.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct VolumePair(u128, u128, NormalizedPath);

static CAPS: OnceLock<dashmap::DashMap<VolumePair, VolumeCaps>> = OnceLock::new();
static CAPS_INSERT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static PROBE_COUNT: AtomicU64 = AtomicU64::new(0);

fn cache() -> &'static dashmap::DashMap<VolumePair, VolumeCaps> {
    CAPS.get_or_init(dashmap::DashMap::new)
}

pub(in crate::daemon::server) fn readonly_enabled() -> bool {
    env_flag(COW_READONLY_ENV, true)
}

pub(in crate::daemon::server) fn hardlink_below_limit(caps: VolumeCaps, link_count: u64) -> bool {
    caps.hardlink && link_count < caps.hardlink_limit
}

fn env_flag(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| {
            !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "false" | "off" | "no"
            )
        })
        .unwrap_or(default)
}

pub(in crate::daemon::server) fn fs_caps(src: &Path, dst: &Path) -> VolumeCaps {
    let Some(src_volume) = crate::platform::fs::volume::volume_identity_u128(src) else {
        return VolumeCaps::copy_only();
    };
    let Some(dst_probe) = existing_path(dst.parent().unwrap_or(dst)) else {
        return VolumeCaps::copy_only();
    };
    let Some(dst_volume) = crate::platform::fs::volume::volume_identity_u128(&dst_probe) else {
        return VolumeCaps::copy_only();
    };
    if src_volume != dst_volume {
        return VolumeCaps::copy_only();
    }
    let key = VolumePair(src_volume, dst_volume, dst_probe);
    if let Some(caps) = cache().get(&key) {
        return (*caps).effective();
    }
    let caps = probe_caps(src, dst);
    #[cfg(test)]
    record_probe(&key);
    let _insert_guard = CAPS_INSERT_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if cache().len() >= CAPS_CACHE_LIMIT {
        // Coarse bound on unbounded growth (issue #1042): the cache key
        // includes a normalized destination parent so distinct build-output
        // directories never collide, but a daemon servicing many thousands
        // of distinct directories over its lifetime would otherwise grow
        // this map without limit. A full clear is simpler than LRU
        // bookkeeping and self-corrects — the next fs_caps() call just
        // re-probes and re-populates, which is cheap (one create_dir_all
        // + two tiny reflink/hardlink probe files).
        cache().clear();
    }
    cache().insert(key, caps);
    caps.effective()
}

/// Per-volume-pair probe tally, for tests only.
///
/// A process-wide total cannot answer "was this pair re-probed?" while ~770
/// other tests run in parallel and probe their own directories -- measured at
/// 102 unrelated probes during one suite run. Counting per key isolates the
/// question from that noise.
#[cfg(test)]
static PROBES_BY_KEY: OnceLock<dashmap::DashMap<VolumePair, u64>> = OnceLock::new();

#[cfg(test)]
fn record_probe(key: &VolumePair) {
    *PROBES_BY_KEY
        .get_or_init(dashmap::DashMap::new)
        .entry(key.clone())
        .or_insert(0) += 1;
}

/// The highest number of probes any single volume pair has incurred.
///
/// The cache promises one probe per pair, so this stays at 1 no matter how many
/// deliveries or concurrent tests run. A regression that re-probes per delivery
/// pushes exactly one key up to the delivery count, which is visible here
/// without needing to know which key belongs to which test.
#[cfg(test)]
pub(in crate::daemon::server) fn max_probes_for_any_volume_pair() -> u64 {
    PROBES_BY_KEY
        .get_or_init(dashmap::DashMap::new)
        .iter()
        .map(|entry| *entry.value())
        .max()
        .unwrap_or(0)
}

fn probe_caps(src: &Path, dst: &Path) -> VolumeCaps {
    PROBE_COUNT.fetch_add(1, Ordering::Relaxed);
    let Some(parent) = dst.parent() else {
        return VolumeCaps::copy_only();
    };
    if std::fs::create_dir_all(parent).is_err() {
        return VolumeCaps::copy_only();
    }
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        PROBE_COUNT.load(Ordering::Relaxed)
    );
    let reflink_probe = parent.join(format!(".zccache-reflink-probe-{nonce}"));
    let hardlink_probe = parent.join(format!(".zccache-hardlink-probe-{nonce}"));
    let reflink = reflink_copy::reflink(src, &reflink_probe).is_ok();
    let _ = std::fs::remove_file(&reflink_probe);
    let hardlink = std::fs::hard_link(src, &hardlink_probe).is_ok();
    let _ = std::fs::remove_file(&hardlink_probe);
    VolumeCaps {
        reflink,
        hardlink,
        readonly_enforced: hardlink,
        file_id: if crate::platform::fs::volume::file_id_width() > 64 {
            FileIdWidth::Bits128
        } else {
            FileIdWidth::Bits64
        },
        hardlink_limit: crate::platform::fs::volume::hard_link_limit(),
    }
}

fn existing_path(path: &Path) -> Option<NormalizedPath> {
    let mut candidate = path;
    loop {
        if candidate.exists() {
            return Some(candidate.into());
        }
        candidate = candidate.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_pair_normalizes_equivalent_probe_paths() {
        let canonical = VolumePair(1, 1, NormalizedPath::new("probe/out"));
        let equivalent = VolumePair(1, 1, NormalizedPath::new("probe/./out"));

        assert_eq!(canonical, equivalent);
    }
}
