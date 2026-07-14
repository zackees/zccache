//! Transaction, race, migration, cleanup, and deterministic fault tests.
use super::*;

#[test]
fn staged_rollout_defaults_on_and_preserves_compatibility_switches() {
    use crate::compiler::CompilerFamily::{Clang, Rustc};

    assert!(staged_artifacts_enabled_for(None));
    assert!(staged_lane_enabled_for(None, Rustc));
    assert!(staged_lane_enabled_for(None, Clang));
    assert!(!staged_link_lane_enabled_for(None));
    assert!(!staged_exec_lane_enabled_for(None));

    for disabled in ["", "0", "false", "off", "no", " OFF "] {
        assert!(!staged_artifacts_enabled_for(Some(disabled)));
        assert!(!staged_lane_enabled_for(Some(disabled), Rustc));
        assert!(!staged_lane_enabled_for(Some(disabled), Clang));
        assert!(!staged_link_lane_enabled_for(Some(disabled)));
        assert!(!staged_exec_lane_enabled_for(Some(disabled)));
    }

    assert!(staged_lane_enabled_for(Some("rust"), Rustc));
    assert!(!staged_lane_enabled_for(Some("rust"), Clang));
    assert!(!staged_lane_enabled_for(Some("c-cpp"), Rustc));
    assert!(staged_lane_enabled_for(Some("c-cpp"), Clang));
    assert!(staged_link_lane_enabled_for(Some("all")));
    assert!(staged_exec_lane_enabled_for(Some("all")));
    assert!(staged_exec_lane_enabled_for(Some("exec")));
}
use std::fs;

fn source_files(dir: &Path) -> Vec<NormalizedPath> {
    let first = dir.join("source-a.rlib");
    let second = dir.join("source-b.rmeta");
    fs::write(&first, b"first immutable payload").unwrap();
    fs::write(&second, b"second immutable payload").unwrap();
    vec![first.into(), second.into()]
}

#[test]
fn publication_timing_is_exclusive_of_hashing() {
    let hashing_ns = 37;
    let publication_ns = exclusive_publication_ns(100, hashing_ns);
    assert_eq!(publication_ns, 63);
    assert_eq!(hashing_ns + publication_ns, 100);
    assert_eq!(exclusive_publication_ns(10, 11), 0);
}

#[test]
fn staged_generation_is_independent_and_hash_addressed() {
    let dir = tempfile::tempdir().unwrap();
    let artifact_dir = dir.path().join("artifacts");
    fs::create_dir_all(&artifact_dir).unwrap();
    let sources = source_files(dir.path());
    let stats = persist_staged_artifact_paths(&artifact_dir, &"a".repeat(64), &sources).unwrap();

    assert_eq!(stats.hardlink_count, 0);
    assert_eq!(stats.reflink_count + stats.copy_count, 2);

    let payloads = load_staged_artifact_paths(&artifact_dir, &"a".repeat(64), &[23, 24])
        .unwrap()
        .unwrap();
    assert_eq!(payloads.len(), 2);
    assert_eq!(fs::read(&payloads[0]).unwrap(), b"first immutable payload");
    assert_eq!(fs::read(&payloads[1]).unwrap(), b"second immutable payload");
    assert!(!same_file(sources[0].as_path(), payloads[0].as_path()));
    assert!(fs::metadata(&payloads[0]).unwrap().permissions().readonly());

    fs::write(&sources[0], b"mutated compiler output").unwrap();
    assert_eq!(fs::read(&payloads[0]).unwrap(), b"first immutable payload");

    let pointer = artifact_dir
        .join(STAGED_ROOT)
        .join(format!("{}.current", "a".repeat(64)));
    let generation = fs::read_to_string(pointer).unwrap();
    assert_eq!(generation.trim().len(), 64);
    assert!(generation
        .trim()
        .bytes()
        .all(|byte| byte.is_ascii_hexdigit()));
    assert!(!is_staged_artifact_path(
        &artifact_dir
            .join(STAGED_ROOT)
            .join("not-a-generation")
            .join("file")
    ));
    assert!(is_staged_artifact_path(&payloads[0]));
}

#[cfg(windows)]
#[test]
fn staged_generation_publishes_beyond_legacy_max_path() {
    let dir = tempfile::tempdir().unwrap();
    let mut artifact_dir = dir.path().join("artifacts");
    while artifact_dir.as_os_str().len() < 220 {
        artifact_dir = artifact_dir.join("long-staged-cache-segment");
    }
    fs::create_dir_all(&artifact_dir).unwrap();
    let source = dir.path().join("source.rlib");
    fs::write(&source, b"long-path immutable payload").unwrap();
    let key = "b".repeat(64);
    assert!(pointer_path(&artifact_dir, &key).as_os_str().len() > 260);

    persist_staged_artifact_paths(&artifact_dir, &key, &[source.into()])
        .expect("staged digest publication must support Win32 paths beyond MAX_PATH");
    let payloads = load_staged_artifact_paths(&artifact_dir, &key, &[27])
        .unwrap()
        .unwrap();
    assert_eq!(
        fs::read(&payloads[0]).unwrap(),
        b"long-path immutable payload"
    );
}

#[test]
fn staged_publication_rejects_nondeterministic_same_key_output() {
    let dir = tempfile::tempdir().unwrap();
    let _cache_dir = crate::daemon::server::tests::CacheDirEnvGuard::set(dir.path());
    let artifact_dir = dir.path().join("artifacts");
    fs::create_dir_all(&artifact_dir).unwrap();
    let sources = source_files(dir.path());
    let key = "e".repeat(64);
    persist_staged_artifact_paths(&artifact_dir, &key, &sources).unwrap();

    make_writable(&sources[0]).unwrap();
    fs::write(&sources[0], b"replacement immutable payload").unwrap();
    let error = persist_staged_artifact_paths(&artifact_dir, &key, &sources).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);

    let payloads = load_staged_artifact_paths(&artifact_dir, &key, &[23, 24])
        .unwrap()
        .unwrap();
    assert_eq!(fs::read(&payloads[0]).unwrap(), b"first immutable payload");
    assert_eq!(fs::read(&payloads[1]).unwrap(), b"second immutable payload");
    assert!(!same_file(sources[0].as_path(), payloads[0].as_path()));

    let log = fs::read_to_string(crate::core::lifecycle::log_file_path()).unwrap();
    let event: serde_json::Value = log
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .find(|event: &serde_json::Value| {
            event["event"] == "staged_publication_conflict" && event["cache_key"] == key
        })
        .expect("durable staged publication conflict event");
    assert_ne!(event["existing_generation"], event["candidate_generation"]);
    assert!(event["elapsed_ns"].as_u64().is_some());
}

#[test]
fn publication_fault_matrix_never_exposes_partial_generation() {
    let cases = [
        (
            StagedFaultPoint::GenerationCreate,
            StagedPublishFailure::StoreSetup,
            "publication",
        ),
        (
            StagedFaultPoint::OutputCopy(1),
            StagedPublishFailure::OutputCopy,
            "publication_output_copy",
        ),
        (
            StagedFaultPoint::OutputHash(1),
            StagedPublishFailure::Hash,
            "hashing",
        ),
        (
            StagedFaultPoint::DurableDigest(1),
            StagedPublishFailure::DurableDigest,
            "durable_digest",
        ),
        (
            StagedFaultPoint::ManifestWrite,
            StagedPublishFailure::Manifest,
            "manifest",
        ),
        (
            StagedFaultPoint::ManifestSync,
            StagedPublishFailure::Manifest,
            "manifest",
        ),
        (
            StagedFaultPoint::GenerationSync,
            StagedPublishFailure::GenerationPublish,
            "generation_publish",
        ),
        (
            StagedFaultPoint::GenerationPublish,
            StagedPublishFailure::GenerationPublish,
            "generation_publish",
        ),
        (
            StagedFaultPoint::PointerCommit,
            StagedPublishFailure::PointerCommit,
            "pointer_commit",
        ),
        (
            StagedFaultPoint::PointerSync,
            StagedPublishFailure::PointerCommit,
            "pointer_commit",
        ),
    ];

    for (point, expected_reason, failure_key) in cases {
        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let first = dir.path().join("first.rlib");
        let second = dir.path().join("second.rmeta");
        fs::write(&first, b"first complete output").unwrap();
        fs::write(&second, b"second complete output").unwrap();
        let key = "7".repeat(64);
        let _fault = StagedFaultGuard::arm(&artifact_dir, [point]);

        let error =
            persist_staged_artifact_paths(&artifact_dir, &key, &[first.into(), second.into()])
                .unwrap_err();
        assert_eq!(
            staged_publish_failure(&error),
            Some(expected_reason),
            "wrong failure reason for {point:?}: {error}"
        );
        let profiler = crate::daemon::staged_stats::StagedProfiler::new();
        profiler.failure(expected_reason.failure());
        assert_eq!(profiler.snapshot().failures[failure_key], 1);
        let loaded = load_staged_artifact_paths(&artifact_dir, &key, &[21, 22]).unwrap();
        if point == StagedFaultPoint::PointerSync {
            let loaded = loaded.expect("post-commit sync failure keeps a complete generation");
            assert_eq!(fs::read(&loaded[0]).unwrap(), b"first complete output");
            assert_eq!(fs::read(&loaded[1]).unwrap(), b"second complete output");
        } else {
            assert!(loaded.is_none(), "{point:?} exposed a partial generation");
        }
        let key_root = staged_root(&artifact_dir).join(&key);
        if key_root.exists() {
            assert!(fs::read_dir(key_root)
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().starts_with(".tmp-")));
        }
        _fault.assert_all_consumed();
    }
}

#[test]
fn staged_publication_can_replace_a_proven_corrupt_generation() {
    let dir = tempfile::tempdir().unwrap();
    let artifact_dir = dir.path().join("artifacts");
    fs::create_dir_all(&artifact_dir).unwrap();
    let sources = source_files(dir.path());
    let key = "9".repeat(64);
    persist_staged_artifact_paths(&artifact_dir, &key, &sources).unwrap();
    let old_generation = fs::read_to_string(pointer_path(&artifact_dir, &key)).unwrap();
    let payloads = load_staged_artifact_paths(&artifact_dir, &key, &[23, 24])
        .unwrap()
        .unwrap();
    make_writable(&payloads[0]).unwrap();
    fs::write(&payloads[0], b"corrupt").unwrap();

    fs::write(&sources[0], b"replacement immutable payload").unwrap();
    persist_staged_artifact_paths(&artifact_dir, &key, &sources).unwrap();
    let replacement = load_staged_artifact_paths(&artifact_dir, &key, &[29, 24])
        .unwrap()
        .unwrap();
    assert_eq!(
        fs::read(&replacement[0]).unwrap(),
        b"replacement immutable payload"
    );
    assert!(!generation_dir(&artifact_dir, &key, old_generation.trim()).exists());
}

#[test]
fn concurrent_same_key_publishers_never_overwrite_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let artifact_dir = dir.path().join("artifacts");
    fs::create_dir_all(&artifact_dir).unwrap();
    let source_a: NormalizedPath = dir.path().join("a.o").into();
    let source_b: NormalizedPath = dir.path().join("b.o").into();
    fs::write(&source_a, b"generation-a").unwrap();
    fs::write(&source_b, b"generation-b").unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let key = "8".repeat(64);

    let publishers: Vec<_> = [source_a, source_b]
        .into_iter()
        .map(|source| {
            let barrier = std::sync::Arc::clone(&barrier);
            let artifact_dir = artifact_dir.clone();
            let key = key.clone();
            std::thread::spawn(move || {
                barrier.wait();
                persist_staged_artifact_paths(&artifact_dir, &key, &[source])
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = publishers
        .into_iter()
        .map(|publisher| publisher.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| result
                .as_ref()
                .is_err_and(|error| error.kind() == io::ErrorKind::AlreadyExists))
            .count(),
        1
    );
    let payloads = load_staged_artifact_paths(&artifact_dir, &key, &[12])
        .unwrap()
        .unwrap();
    assert!(matches!(
        fs::read(&payloads[0]).unwrap().as_slice(),
        b"generation-a" | b"generation-b"
    ));
}

#[test]
fn clear_waits_for_in_flight_publication_then_removes_the_generation() {
    let dir = tempfile::tempdir().unwrap();
    let artifact_dir = dir.path().join("artifacts");
    fs::create_dir_all(&artifact_dir).unwrap();
    let sources = source_files(dir.path());
    let key = "1".repeat(64);
    let hook = StagedHookGuard::arm(&artifact_dir, StagedHookPoint::PublicationStoreLocked);
    let publisher = {
        let artifact_dir = artifact_dir.clone();
        let key = key.clone();
        std::thread::spawn(move || persist_staged_artifact_paths(&artifact_dir, &key, &sources))
    };
    hook.wait_until_reached();

    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let clearer = {
        let artifact_dir = artifact_dir.clone();
        std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx.send(clear_staged_artifacts(&artifact_dir)).unwrap();
        })
    };
    started_rx.recv().unwrap();
    assert!(matches!(
        done_rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));

    hook.resume();
    publisher.join().unwrap().unwrap();
    assert!(done_rx.recv().unwrap().unwrap() > 0);
    clearer.join().unwrap();
    assert!(load_staged_artifact_paths(&artifact_dir, &key, &[23, 24])
        .unwrap()
        .is_none());
}

#[test]
fn eviction_waits_for_in_flight_publication_then_removes_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let artifact_dir = dir.path().join("artifacts");
    fs::create_dir_all(&artifact_dir).unwrap();
    let sources = source_files(dir.path());
    let key = "2".repeat(64);
    let hook = StagedHookGuard::arm(&artifact_dir, StagedHookPoint::PublicationStoreLocked);
    let publisher = {
        let artifact_dir = artifact_dir.clone();
        let key = key.clone();
        std::thread::spawn(move || persist_staged_artifact_paths(&artifact_dir, &key, &sources))
    };
    hook.wait_until_reached();

    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let evictor = {
        let artifact_dir = artifact_dir.clone();
        let key = key.clone();
        std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx
                .send(evict_staged_artifact_keys(
                    &artifact_dir,
                    &std::collections::HashSet::from([key]),
                ))
                .unwrap();
        })
    };
    started_rx.recv().unwrap();
    assert!(matches!(
        done_rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));

    hook.resume();
    publisher.join().unwrap().unwrap();
    assert!(done_rx.recv().unwrap().unwrap() > 0);
    evictor.join().unwrap();
    assert!(load_staged_artifact_paths(&artifact_dir, &key, &[23, 24])
        .unwrap()
        .is_none());
}

#[test]
fn clear_during_salvage_cannot_remove_private_compiler_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let artifact_dir = dir.path().join("artifacts");
    fs::create_dir_all(&artifact_dir).unwrap();
    let published = source_files(dir.path());
    persist_staged_artifact_paths(&artifact_dir, &"3".repeat(64), &published).unwrap();

    let private_root = dir.path().join("private-staging");
    fs::create_dir_all(&private_root).unwrap();
    let staged: NormalizedPath = private_root.join("result.rlib").into();
    let requested: NormalizedPath = dir.path().join("result.rlib").into();
    fs::write(&staged, b"successful compiler output").unwrap();
    let plan = StagedCompilePlan::for_test(
        private_root,
        vec![StagedOutputPlan {
            requested: requested.clone(),
            staged,
        }],
    );
    let hook = StagedHookGuard::arm(&requested, StagedHookPoint::MaterializeOutput);
    let salvage = std::thread::spawn(move || plan.materialize());
    hook.wait_until_reached();

    assert!(clear_staged_artifacts(&artifact_dir).unwrap() > 0);
    assert!(!requested.exists());
    hook.resume();
    salvage.join().unwrap().unwrap();
    assert_eq!(fs::read(&requested).unwrap(), b"successful compiler output");
}

#[test]
fn staged_generation_rejects_same_size_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let artifact_dir = dir.path().join("artifacts");
    fs::create_dir_all(&artifact_dir).unwrap();
    let sources = source_files(dir.path());
    let key = "b".repeat(64);
    persist_staged_artifact_paths(&artifact_dir, &key, &sources).unwrap();
    let payloads = load_staged_artifact_paths(&artifact_dir, &key, &[23, 24])
        .unwrap()
        .unwrap();

    make_writable(&payloads[0]).unwrap();
    let mut corrupted = fs::read(&payloads[0]).unwrap();
    corrupted[0] ^= 0xff;
    fs::write(&payloads[0], corrupted).unwrap();
    assert_eq!(
        load_staged_artifact_paths(&artifact_dir, &key, &[23, 24])
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn staged_generation_pointer_never_selects_partial_set() {
    let dir = tempfile::tempdir().unwrap();
    let artifact_dir = dir.path().join("artifacts");
    fs::create_dir_all(&artifact_dir).unwrap();
    let sources = source_files(dir.path());
    let key = "c".repeat(64);
    persist_staged_artifact_paths(&artifact_dir, &key, &sources).unwrap();

    let pointer = artifact_dir
        .join(STAGED_ROOT)
        .join(format!("{key}.current"));
    let generation = fs::read_to_string(pointer).unwrap();
    let generation_dir = artifact_dir
        .join(STAGED_ROOT)
        .join(&key)
        .join(generation.trim());
    make_writable(&generation_dir.join("output-1")).unwrap();
    fs::remove_file(generation_dir.join("output-1")).unwrap();
    assert!(load_staged_artifact_paths(&artifact_dir, &key, &[23, 24]).is_err());
}

#[test]
fn staged_generation_cleans_abandoned_temporary_directories() {
    let dir = tempfile::tempdir().unwrap();
    let artifact_dir = dir.path().join("artifacts");
    let key_root = artifact_dir.join(STAGED_ROOT).join("d".repeat(64));
    fs::create_dir_all(&key_root).unwrap();
    fs::create_dir(key_root.join(".tmp-crashed")).unwrap();
    fs::create_dir(key_root.join("stable-generation")).unwrap();
    fs::create_dir(key_root.join("orphan-generation")).unwrap();
    fs::write(key_root.join(PUBLISH_LOCK), b"").unwrap();
    fs::write(
        artifact_dir
            .join(STAGED_ROOT)
            .join(format!("{}.current", "d".repeat(64))),
        "stable-generation",
    )
    .unwrap();

    assert_eq!(cleanup_staged_artifact_temps(&artifact_dir).unwrap(), 2);
    assert!(!key_root.join(".tmp-crashed").exists());
    assert!(key_root.join("stable-generation").exists());
    assert!(!key_root.join("orphan-generation").exists());
    assert!(key_root.join(PUBLISH_LOCK).exists());
}

#[test]
fn staged_clear_removes_every_visible_generation_but_keeps_coordination_lock() {
    let dir = tempfile::tempdir().unwrap();
    let artifact_dir = dir.path().join("artifacts");
    fs::create_dir_all(&artifact_dir).unwrap();
    let sources = source_files(dir.path());
    let key = "7".repeat(64);
    persist_staged_artifact_paths(&artifact_dir, &key, &sources).unwrap();
    #[cfg(unix)]
    let outside = {
        let outside = dir.path().join("outside-clear-boundary");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("must-survive"), b"outside").unwrap();
        std::os::unix::fs::symlink(
            &outside,
            artifact_dir.join(STAGED_ROOT).join("hostile-symlink"),
        )
        .unwrap();
        outside
    };

    assert!(clear_staged_artifacts(&artifact_dir).unwrap() > 0);
    #[cfg(unix)]
    assert_eq!(fs::read(outside.join("must-survive")).unwrap(), b"outside");
    assert!(load_staged_artifact_paths(&artifact_dir, &key, &[23, 24])
        .unwrap()
        .is_none());
    let remaining: Vec<_> = fs::read_dir(artifact_dir.join(STAGED_ROOT))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(remaining, vec![std::ffi::OsString::from(STORE_LOCK)]);
}

#[test]
fn mutable_page_writer_never_shares_backend_inode() {
    // This is intentionally a database-shaped page writer rather than
    // the sqlite-link compile fixture: it exercises truncate, same-size
    // page replacement, and a journal-like sibling file.
    let dir = tempfile::tempdir().unwrap();
    let artifact_dir = dir.path().join("artifacts");
    fs::create_dir_all(&artifact_dir).unwrap();
    let backend = dir.path().join("backend.db");
    let journal = dir.path().join("backend.db-wal");
    fs::write(&backend, vec![0x11_u8; 4096]).unwrap();
    fs::write(&journal, b"journal-before-checkpoint").unwrap();
    let sources = vec![backend.clone().into(), journal.clone().into()];
    persist_staged_artifact_paths(&artifact_dir, &"f".repeat(64), &sources).unwrap();
    let journal_size = fs::metadata(&journal).unwrap().len();
    let payloads =
        load_staged_artifact_paths(&artifact_dir, &"f".repeat(64), &[4096, journal_size])
            .unwrap()
            .unwrap();
    let destination = dir.path().join("work.db");
    materialize_independent_with_stats(&payloads[0], &destination).unwrap();
    let mut page = vec![0x22_u8; 4096];
    page[37] = 0x99;
    fs::write(&destination, page).unwrap();
    assert_eq!(fs::read(&backend).unwrap(), vec![0x11_u8; 4096]);
    assert_ne!(fs::read(&destination).unwrap(), fs::read(&backend).unwrap());
    assert!(!same_file(&payloads[0], &destination));
}
