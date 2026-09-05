//! Independent requested-path materialization and physical-work observations.

use super::copy_output;
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

pub(in crate::daemon::server) fn materialize_independent_with_stats(
    source: &Path,
    destination: &Path,
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
    // open-write-close + rename window; see `daemon::spawn_exclusion`.
    let temporary = super::temporary_path(destination, "materialize");
    let result = (|| {
        let _materialize_guard = crate::daemon::spawn_exclusion::materialize_exclusive();
        let (reflink, copy_bytes) = copy_output(source, &temporary)?;
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
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::super::{
        load_staged_artifact_paths, persist_staged_artifact_paths, StagedFaultGuard,
        StagedFaultPoint, StagedHookGuard, StagedHookPoint,
    };
    use super::*;

    #[test]
    fn staged_persist_and_materialization_report_physical_work() {
        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let source = dir.path().join("source.rlib");
        fs::write(&source, b"observable staged payload").unwrap();

        let persisted =
            persist_staged_artifact_paths(&artifact_dir, &"9".repeat(64), &[source.into()])
                .unwrap();
        assert!(persisted.staged);
        assert_eq!(persisted.reflink_count + persisted.copy_count, 1);

        let payload = load_staged_artifact_paths(&artifact_dir, &"9".repeat(64), &[25])
            .unwrap()
            .unwrap()
            .remove(0);
        let destination = dir.path().join("restored.rlib");
        let materialized = materialize_independent_with_stats(&payload, &destination).unwrap();
        assert_eq!(materialized.reflink_count + materialized.copy_count, 1);
        assert_eq!(fs::read(destination).unwrap(), b"observable staged payload");
    }

    #[test]
    fn independent_materialization_faults_fall_back_or_fail_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.rlib");
        fs::write(&source, b"independent materialization payload").unwrap();

        let fallback = dir.path().join("fallback.rlib");
        let reflink_fault =
            StagedFaultGuard::arm(dir.path(), [StagedFaultPoint::MaterializeReflink]);
        let observed = materialize_independent_with_stats(&source, &fallback).unwrap();
        assert_eq!(observed.reflink_count, 0);
        assert_eq!(observed.copy_count, 1);
        assert_eq!(observed.copy_bytes, 35);
        assert_eq!(
            fs::read(&fallback).unwrap(),
            b"independent materialization payload"
        );
        reflink_fault.assert_all_consumed();

        let failed = dir.path().join("failed.rlib");
        let all_faults = StagedFaultGuard::arm(
            dir.path(),
            [
                StagedFaultPoint::MaterializeReflink,
                StagedFaultPoint::MaterializeCopy,
            ],
        );
        materialize_independent_with_stats(&source, &failed).unwrap_err();
        assert!(
            !failed.exists(),
            "failed copy tier left a partial destination"
        );
        all_faults.assert_all_consumed();
    }

    #[test]
    fn independent_materialization_publishes_only_complete_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.bin");
        let destination = dir.path().join("destination.bin");
        fs::write(&source, b"new complete output").unwrap();
        fs::write(&destination, b"old complete output").unwrap();

        let hook = StagedHookGuard::arm(&destination, StagedHookPoint::MaterializePublish);
        let source_for_thread = source.clone();
        let destination_for_thread = destination.clone();
        let materialize = std::thread::spawn(move || {
            materialize_independent_with_stats(&source_for_thread, &destination_for_thread)
        });

        hook.wait_until_reached();
        assert_eq!(fs::read(&destination).unwrap(), b"old complete output");
        hook.resume();
        materialize.join().unwrap().unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"new complete output");
    }

    /// zccache#1562: a child forked while the materialization copy held a
    /// write descriptor on the sibling temporary inherits that descriptor
    /// across the rename, so executing the published output fails with
    /// `ETXTBSY` until the child execs. The spawn/materialize lock must keep
    /// the fork out of the copy window. Disabling the exclusive guard in
    /// `materialize_independent_with_stats` makes this test fail.
    #[cfg(target_os = "linux")]
    #[test]
    fn materialized_executable_runs_while_a_child_is_between_fork_and_exec() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("build_script_build-source");
        let destination = dir.path().join("build-script-build");
        fs::write(&source, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();

        let hook = StagedHookGuard::arm(&destination, StagedHookPoint::MaterializeTemporaryOpen);
        let source_for_thread = source.clone();
        let destination_for_thread = destination.clone();
        let materialize = std::thread::spawn(move || {
            materialize_independent_with_stats(&source_for_thread, &destination_for_thread)
        });
        hook.wait_until_reached();

        // Another daemon task spawns a compiler child. Its fork-to-exec window
        // is stretched to 500 ms so the inherited descriptor demonstrably
        // outlives the rename below.
        let spawner = std::thread::spawn(|| {
            let mut cmd = tokio::process::Command::new("true");
            // SAFETY: the closure only sleeps; it is async-signal-safe enough
            // for a test and touches no locks or allocations.
            unsafe {
                cmd.as_std_mut().pre_exec(|| {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    Ok(())
                });
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                crate::daemon::process::tokio_leaf_command_output_with_priority(
                    &mut cmd,
                    crate::daemon::process::CompilePriority::Normal,
                )
                .await
            })
        });
        // Give the spawner time to reach the lock (or, without the lock, to
        // fork while the temporary's write descriptor is still open).
        std::thread::sleep(std::time::Duration::from_millis(100));

        hook.resume();
        materialize.join().unwrap().unwrap();

        let status = std::process::Command::new(&destination)
            .status()
            .unwrap_or_else(|error| {
                panic!(
                    "published output {} must be executable immediately after \
                     materialization returns: {error}",
                    destination.display()
                )
            });
        assert!(status.success());
        let output = spawner.join().unwrap().unwrap();
        assert!(output.status.success());
    }
}
