//! Quarantine and recovery for depgraph snapshots rejected at load (#1157).
//!
//! Before this module, a snapshot the running binary could not read — either
//! because the schema version moved or because the bytes were damaged — was
//! left in place and then *overwritten* by the next graceful shutdown. Two
//! things followed from that:
//!
//! 1. The rejected bytes were destroyed, so a later migration (or simply
//!    going back to the binary that wrote them) could never recover them.
//! 2. A workspace alternating between two binaries with different
//!    [`DEPGRAPH_VERSION`]s destroyed the other side's graph on every switch,
//!    so *both* sides cold-recompiled the world forever.
//!
//! Quarantine fixes both without any reinterpretation of foreign bytes: a
//! rejected snapshot is *moved aside* to a version-tagged sidecar, and a
//! sidecar written by a binary with this build's exact `DEPGRAPH_VERSION` may
//! be loaded back through the ordinary [`super::load_from_file`] path.
//!
//! ## Why this cannot produce a wrong cache hit
//!
//! Nothing here decodes, migrates, or partially salvages a snapshot whose
//! version tag differs from this build's. A recovered sidecar goes through
//! exactly the same magic + version + rkyv validation as `depgraph.bin`
//! itself, so the only new claim is "a snapshot written by *this* schema
//! version against *this* cache root is as trustworthy as the primary
//! snapshot" — which is true by construction, and is anyway re-verified
//! per-context against live file hashes by `DepGraph::check`.

use std::path::Path;

use zccache_core::NormalizedPath;

use super::DEPGRAPH_VERSION;

/// Most version-tagged sidecars kept in the depgraph directory. Steady state
/// for a workspace oscillating between two binaries is exactly two (each
/// version's own snapshot); anything beyond that is a version this machine
/// has stopped running, and its snapshot is dead weight.
const MAX_QUARANTINED_SNAPSHOTS: usize = 2;

/// Sidecar filename prefix shared by every version-tagged quarantine file.
const QUARANTINE_PREFIX: &str = "depgraph.v";

/// Sidecar filename suffix shared by every version-tagged quarantine file.
const QUARANTINE_SUFFIX: &str = ".bin";

/// Single-slot sidecar for snapshots that failed *validation* rather than the
/// version check. Kept for forensics only — never loaded back.
const CORRUPT_SIDECAR_NAME: &str = "depgraph.corrupt.bin";

/// Sidecar path holding the snapshot written by schema `version`.
#[must_use]
pub fn quarantine_path(primary: &Path, version: u32) -> NormalizedPath {
    sidecar(
        primary,
        &format!("{QUARANTINE_PREFIX}{version}{QUARANTINE_SUFFIX}"),
    )
}

/// Sidecar path for a snapshot that failed payload validation.
#[must_use]
pub fn corrupt_sidecar_path(primary: &Path) -> NormalizedPath {
    sidecar(primary, CORRUPT_SIDECAR_NAME)
}

fn sidecar(primary: &Path, name: &str) -> NormalizedPath {
    match primary.parent() {
        Some(dir) => NormalizedPath::new(dir).join(name),
        None => NormalizedPath::new(name),
    }
}

/// Move `primary` aside to `dest`, replacing whatever was there.
///
/// Returns the destination on success and `None` if the move failed (the
/// snapshot then stays where it is; the caller still starts cold, which is
/// the pre-existing behaviour). A failure here must never be fatal: this is
/// best-effort preservation, not a correctness dependency.
#[must_use]
pub fn quarantine(primary: &Path, dest: &NormalizedPath) -> Option<NormalizedPath> {
    // Windows `rename` does not overwrite an existing destination.
    let _ = std::fs::remove_file(dest.as_path());
    match std::fs::rename(primary, dest.as_path()) {
        Ok(()) => Some(dest.clone()),
        Err(_) => None,
    }
}

/// Load the sidecar written by this build's schema version, if one exists.
///
/// Deliberately routed through [`super::classify_load`] so a sidecar gets the
/// identical magic/version/rkyv validation the primary snapshot gets. Any
/// outcome other than `Loaded` yields `None` — a damaged sidecar is dropped
/// silently rather than escalated, because the caller is *already* in the
/// degraded path and a second warning about a backup file would not change
/// what an operator does.
#[must_use]
pub fn recover(primary: &Path) -> Option<(super::super::graph::DepGraph, NormalizedPath)> {
    let path = quarantine_path(primary, DEPGRAPH_VERSION);
    if !path.as_path().exists() {
        return None;
    }
    super::classify_load(path.as_path())
        .into_graph()
        .map(|graph| (graph, path))
}

/// Parse the schema version out of a quarantine sidecar's file name.
fn quarantined_version(name: &str) -> Option<u32> {
    name.strip_prefix(QUARANTINE_PREFIX)?
        .strip_suffix(QUARANTINE_SUFFIX)?
        .parse()
        .ok()
}

/// Delete quarantined sidecars beyond the retention cap, oldest
/// first. The sidecar matching this build's [`DEPGRAPH_VERSION`] is never a
/// pruning candidate — it is the one [`recover`] reads.
///
/// Returns the number of files removed.
pub fn prune(primary: &Path) -> usize {
    let Some(dir) = primary.parent() else {
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut candidates: Vec<(std::time::SystemTime, NormalizedPath)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let version = quarantined_version(name.to_str()?)?;
            if version == DEPGRAPH_VERSION {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, NormalizedPath::new(entry.path())))
        })
        .collect();
    if candidates.len() <= MAX_QUARANTINED_SNAPSHOTS {
        return 0;
    }
    // Newest first, so the tail past the cap is the oldest.
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    let mut removed = 0;
    for (_, path) in candidates.drain(MAX_QUARANTINED_SNAPSHOTS..) {
        if std::fs::remove_file(path.as_path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_snapshot_with_version(path: &Path, version: u32) {
        let graph = crate::graph::DepGraph::new();
        super::super::save_to_file(&graph, path).unwrap();
        if version != DEPGRAPH_VERSION {
            let mut bytes = std::fs::read(path).unwrap();
            bytes[4..8].copy_from_slice(&version.to_le_bytes());
            std::fs::write(path, &bytes).unwrap();
        }
    }

    #[test]
    fn quarantine_moves_the_rejected_snapshot_aside_instead_of_leaving_it_to_be_clobbered() {
        let tmp = tempfile::tempdir().unwrap();
        let primary = tmp.path().join("depgraph.bin");
        write_snapshot_with_version(&primary, DEPGRAPH_VERSION + 1);
        let original = std::fs::read(&primary).unwrap();

        let dest = quarantine_path(&primary, DEPGRAPH_VERSION + 1);
        let moved = quarantine(&primary, &dest).expect("quarantine must succeed");

        assert!(
            !primary.exists(),
            "the rejected snapshot must be moved, not copied — leaving it in \
             place is what let the next graceful shutdown destroy it"
        );
        assert_eq!(
            std::fs::read(moved.as_path()).unwrap(),
            original,
            "the quarantined bytes must be preserved verbatim so a future \
             migration has something to migrate"
        );
    }

    #[test]
    fn quarantine_overwrites_a_stale_sidecar_for_the_same_version() {
        let tmp = tempfile::tempdir().unwrap();
        let primary = tmp.path().join("depgraph.bin");
        let dest = quarantine_path(&primary, DEPGRAPH_VERSION + 1);
        std::fs::write(dest.as_path(), b"stale").unwrap();

        write_snapshot_with_version(&primary, DEPGRAPH_VERSION + 1);
        let fresh = std::fs::read(&primary).unwrap();
        quarantine(&primary, &dest).expect("quarantine must replace the stale sidecar");

        assert_eq!(std::fs::read(dest.as_path()).unwrap(), fresh);
    }

    #[test]
    fn recover_returns_only_a_sidecar_this_build_can_validate() {
        let tmp = tempfile::tempdir().unwrap();
        let primary = tmp.path().join("depgraph.bin");

        assert!(
            recover(&primary).is_none(),
            "no sidecar means no recovery, not a panic"
        );

        // A sidecar tagged with a foreign version must never be adopted: this
        // is the exact byte-reinterpretation that would risk a wrong hit.
        let foreign = quarantine_path(&primary, DEPGRAPH_VERSION);
        write_snapshot_with_version(&primary, DEPGRAPH_VERSION + 1);
        std::fs::rename(&primary, foreign.as_path()).unwrap();
        assert!(
            recover(&primary).is_none(),
            "a sidecar whose embedded version tag is foreign must be rejected \
             even though its file name claims the current version"
        );

        // A truncated sidecar fails payload validation and is dropped.
        std::fs::write(foreign.as_path(), b"ZCDG").unwrap();
        assert!(recover(&primary).is_none());

        // The real thing round-trips.
        write_snapshot_with_version(&primary, DEPGRAPH_VERSION);
        std::fs::rename(&primary, foreign.as_path()).unwrap();
        let (_graph, from) = recover(&primary).expect("a valid same-version sidecar must load");
        assert_eq!(from.as_path(), foreign.as_path());
    }

    #[test]
    fn prune_keeps_the_cap_and_never_touches_this_builds_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let primary = tmp.path().join("depgraph.bin");

        let mine = quarantine_path(&primary, DEPGRAPH_VERSION);
        std::fs::write(mine.as_path(), b"mine").unwrap();
        let mut foreign = Vec::new();
        for offset in 1..=4u32 {
            let path = quarantine_path(&primary, DEPGRAPH_VERSION + offset);
            std::fs::write(path.as_path(), b"foreign").unwrap();
            // Distinct mtimes so "oldest first" is well-defined on filesystems
            // with coarse timestamps.
            filetime::set_file_mtime(
                path.as_path(),
                filetime::FileTime::from_unix_time(1_700_000_000 + i64::from(offset), 0),
            )
            .unwrap();
            foreign.push(path);
        }

        assert_eq!(prune(&primary), 4 - MAX_QUARANTINED_SNAPSHOTS);
        assert!(
            mine.as_path().exists(),
            "pruning must never delete the sidecar recover() reads"
        );
        assert!(!foreign[0].as_path().exists(), "oldest goes first");
        assert!(!foreign[1].as_path().exists());
        assert!(foreign[2].as_path().exists());
        assert!(foreign[3].as_path().exists());
        assert_eq!(prune(&primary), 0, "pruning is idempotent");
    }
}
