//! Materialization tests. Kept out of `materialize.rs` so the Linux-only
//! ETXTBSY regression test (zccache#1562) can use native APIs without
//! crossing the production platform boundary (enforce_platform_boundary).
use super::super::{
    load_staged_artifact_paths, persist_staged_artifact_paths, StagedFaultGuard, StagedFaultPoint,
    StagedHookGuard, StagedHookPoint,
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
        persist_staged_artifact_paths(&artifact_dir, &"9".repeat(64), &[source.into()]).unwrap();
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
    let reflink_fault = StagedFaultGuard::arm(dir.path(), [StagedFaultPoint::MaterializeReflink]);
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
        let mut cmd = std::process::Command::new("true");
        // SAFETY: the closure only sleeps; it is async-signal-safe enough
        // for a test and touches no locks or allocations.
        unsafe {
            cmd.pre_exec(|| {
                std::thread::sleep(std::time::Duration::from_millis(500));
                Ok(())
            });
        }
        // The production spawn boundary holds exactly this shared guard
        // across the native fork/exec through its `SpawnAdmission`
        // (`process::tests::semantic_session_admission_holds_materialization_lock_through_native_spawn`).
        // The facade offers no pre-exec seam to stretch that window, so the
        // same protocol is exercised here on a std spawn.
        let child = {
            let _spawn_guard = crate::daemon::spawn_exclusion::spawn_shared();
            cmd.spawn()
        };
        child.and_then(std::process::Child::wait_with_output)
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

/// zackees/soldr#3350: the spawn/materialize lock only excludes spawns that
/// take it. Embedded in soldr's daemon, zccache shares the process with code
/// that forks under a different lock or none, and such a child still
/// inherits the copy's write descriptor across the rename. Materialization
/// must not return until no process holds the published output open for
/// writing, whoever forked it.
#[cfg(target_os = "linux")]
#[test]
fn materialized_executable_runs_after_a_foreign_fork_inherits_the_copy() {
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

    // A spawner outside zccache's lock forks while the copy's write
    // descriptor is open; its fork-to-exec window is stretched to 500 ms.
    let spawner = std::thread::spawn(|| {
        let mut cmd = std::process::Command::new("true");
        // SAFETY: the closure only sleeps.
        unsafe {
            cmd.pre_exec(|| {
                std::thread::sleep(std::time::Duration::from_millis(500));
                Ok(())
            });
        }
        cmd.spawn().and_then(std::process::Child::wait_with_output)
    });
    std::thread::sleep(std::time::Duration::from_millis(100));

    hook.resume();
    materialize.join().unwrap().unwrap();

    let status = std::process::Command::new(&destination)
        .status()
        .unwrap_or_else(|error| {
            panic!(
                "published output {} must be executable once materialization \
                 returns, even after a foreign fork: {error}",
                destination.display()
            )
        });
    assert!(status.success());
    assert!(spawner.join().unwrap().unwrap().status.success());
}
