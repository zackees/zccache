//! Independent requested-path materialization and physical-work observations.

use super::copy_output_with;
use std::fs;
use std::io;
use std::path::Path;

#[derive(Clone, Copy, Debug, Default)]
pub(in crate::daemon::server) struct StagedMaterializationStats {
    pub(in crate::daemon::server) reflink_count: u64,
    pub(in crate::daemon::server) hardlink_count: u64,
    pub(in crate::daemon::server) copy_count: u64,
    pub(in crate::daemon::server) copy_bytes: u64,
}

impl StagedMaterializationStats {
    pub(in crate::daemon::server) fn add(&mut self, other: Self) {
        self.reflink_count = self.reflink_count.saturating_add(other.reflink_count);
        self.hardlink_count = self.hardlink_count.saturating_add(other.hardlink_count);
        self.copy_count = self.copy_count.saturating_add(other.copy_count);
        self.copy_bytes = self.copy_bytes.saturating_add(other.copy_bytes);
    }
}

#[derive(Debug)]
struct StagedMaterializationError {
    source: io::Error,
    progress: StagedMaterializationStats,
}

impl std::fmt::Display for StagedMaterializationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for StagedMaterializationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

pub(in crate::daemon::server) fn materialization_error(
    source: io::Error,
    progress: StagedMaterializationStats,
) -> io::Error {
    io::Error::new(
        source.kind(),
        StagedMaterializationError { source, progress },
    )
}

pub(in crate::daemon::server) fn materialization_error_progress(
    error: &io::Error,
) -> StagedMaterializationStats {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<StagedMaterializationError>())
        .map_or_else(StagedMaterializationStats::default, |error| error.progress)
}

/// Deliver a private staged output to its requested path under an explicit
/// `ZCCACHE_MODE`: every mode delivers an independent file here; `COPY`
/// skips the reflink attempt (#1683).
pub(in crate::daemon::server) fn materialize_independent_with_mode(
    source: &Path,
    destination: &Path,
    mode: crate::core::config::MaterializationMode,
) -> io::Result<StagedMaterializationStats> {
    #[cfg(test)]
    super::hook::pause(destination, super::StagedHookPoint::MaterializeOutput);
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
    }

    // The copy lands in a unique sibling temporary that is renamed over the
    // requested path, so readers never observe a partial output (#1563). That
    // rename does NOT make the output safe to execute on its own: `rename(2)`
    // keeps the inode, and a child forked by this process while the copy's
    // write descriptor was open inherits that descriptor until its own
    // `execve`. Cargo hard-links `build-script-build` to the published inode
    // and execs it, and `ETXTBSY` is evaluated per inode, so the exec fails
    // with `Text file busy` for the child's fork-to-exec window even though
    // this process closed its descriptor before publishing (zccache#1562).
    // The exclusive guard below keeps every daemon child spawn out of the
    // open-write-close + rename window; see `daemon::spawn_exclusion`. A
    // spawn outside that lock is waited out after publishing (soldr#3350).
    let temporary = super::temporary_path(destination, "materialize");
    let result = (|| {
        let _materialize_guard = crate::daemon::spawn_exclusion::materialize_exclusive();
        let (reflink, copy_bytes) = copy_output_with(source, &temporary, mode)?;
        #[cfg(test)]
        {
            // Test seam: keep a write descriptor on the temporary open while
            // paused, modelling the descriptor `copy_output` holds mid-copy.
            let open_for_write = fs::OpenOptions::new().write(true).open(&temporary)?;
            super::hook::pause(
                destination,
                super::StagedHookPoint::MaterializeTemporaryOpen,
            );
            drop(open_for_write);
        }
        #[cfg(test)]
        super::hook::pause(destination, super::StagedHookPoint::MaterializePublish);
        if fs::metadata(destination).is_ok() {
            let _ = crate::platform::fs::permissions::set_readonly(destination, false);
        }
        super::replace_staged_path(&temporary, destination)?;
        let _ = crate::platform::fs::permissions::set_readonly(destination, false);
        Ok(StagedMaterializationStats {
            reflink_count: u64::from(reflink),
            hardlink_count: 0,
            copy_count: u64::from(!reflink),
            copy_bytes,
        })
    })();
    match &result {
        Ok(_) => crate::daemon::spawn_exclusion::await_publishable(destination),
        Err(_) => {
            let _ = fs::remove_file(&temporary);
        }
    }
    result
}

#[cfg(test)]
#[path = "materialize_tests.rs"]
mod tests;
