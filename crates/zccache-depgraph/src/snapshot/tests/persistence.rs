//! File I/O and on-disk format edge cases: header validation, error
//! variants (bad magic, version mismatch, truncated payload, missing
//! file), atomic tmp cleanup, overwrite semantics, trailing garbage,
//! payload-length overflow, plus the full `classify_load` matrix.

use tempfile::TempDir;

use super::{make_ctx, test_path};
use crate::graph::DepGraph;
use crate::scanner::ScanResult;
use crate::snapshot::{
    classify_load, load_from_file, save_to_file, DepGraphLoadOutcome, DEPGRAPH_MAGIC,
    DEPGRAPH_VERSION,
};
use crate::snapshot::{SnapshotError, HEADER_SIZE};

#[test]
fn version_mismatch() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);

    let mut data = Vec::new();
    data.extend_from_slice(&DEPGRAPH_MAGIC);
    data.extend_from_slice(&99u32.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes());
    std::fs::write(&path, &data).unwrap();

    match load_from_file(&path) {
        Err(SnapshotError::VersionMismatch {
            file: 99,
            expected: DEPGRAPH_VERSION,
        }) => {}
        other => panic!("expected VersionMismatch, got {other:?}"),
    }
}

#[test]
fn bad_magic() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);

    let mut data = Vec::new();
    data.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
    data.extend_from_slice(&DEPGRAPH_VERSION.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes());
    std::fs::write(&path, &data).unwrap();

    match load_from_file(&path) {
        Err(SnapshotError::BadMagic) => {}
        other => panic!("expected BadMagic, got {other:?}"),
    }
}

#[test]
fn truncated_payload() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);

    let mut data = Vec::new();
    data.extend_from_slice(&DEPGRAPH_MAGIC);
    data.extend_from_slice(&DEPGRAPH_VERSION.to_le_bytes());
    data.extend_from_slice(&1000u64.to_le_bytes()); // claims 1000 bytes
    data.extend_from_slice(&[0u8; 10]); // only 10 bytes
    std::fs::write(&path, &data).unwrap();

    match load_from_file(&path) {
        Err(SnapshotError::Corrupt(_)) => {}
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[test]
fn file_not_found() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("nonexistent.bin");

    match load_from_file(&path) {
        Err(SnapshotError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
        other => panic!("expected Io(NotFound), got {other:?}"),
    }
}

#[test]
fn atomic_write_cleans_tmp() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let tmp_path = path.with_extension("bin.tmp");

    let graph = DepGraph::new();
    save_to_file(&graph, &path).unwrap();

    assert!(path.exists());
    assert!(!tmp_path.exists(), ".tmp file should be cleaned up");
}

/// Overwriting an existing snapshot file must work (tests the
/// Windows remove-before-rename path).
#[test]
fn overwrite_existing_file() {
    use crate::graph::ContextState;

    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);

    // First save.
    let graph1 = DepGraph::new();
    graph1.register(make_ctx("/src/old.cpp"));
    save_to_file(&graph1, &path).unwrap();

    // Second save with different content.
    let graph2 = DepGraph::new();
    let key = graph2.register(make_ctx("/src/new.cpp"));
    graph2.update(
        &key,
        ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        },
        super::dummy_hash,
    );
    save_to_file(&graph2, &path).unwrap();

    // Load should see the second graph.
    let loaded = load_from_file(&path).unwrap();
    assert_eq!(loaded.stats().context_count, 1);
    assert_eq!(loaded.get_state(&key), Some(ContextState::Warm));
}

/// A file with correct header but zero-length payload.
#[test]
fn zero_length_payload_rejected() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);

    let mut data = Vec::new();
    data.extend_from_slice(&DEPGRAPH_MAGIC);
    data.extend_from_slice(&DEPGRAPH_VERSION.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes()); // zero-length payload
    std::fs::write(&path, &data).unwrap();

    // rkyv should reject an empty payload.
    match load_from_file(&path) {
        Err(SnapshotError::Corrupt(_)) => {}
        other => panic!("expected Corrupt for empty payload, got {other:?}"),
    }
}

/// Just the magic bytes and nothing else — shorter than header.
#[test]
fn header_too_short() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);

    std::fs::write(&path, DEPGRAPH_MAGIC).unwrap();

    match load_from_file(&path) {
        Err(SnapshotError::Corrupt(msg)) => {
            assert!(msg.contains("too small"), "unexpected message: {msg}");
        }
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

/// Payload with trailing garbage bytes after the declared length.
/// The loader should ignore trailing data (only read payload_len bytes).
#[test]
fn trailing_garbage_after_payload_ignored() {
    use crate::graph::ContextState;

    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    let key = graph.register(make_ctx("/src/a.cpp"));
    graph.update(
        &key,
        ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        },
        super::dummy_hash,
    );
    save_to_file(&graph, &path).unwrap();

    // Append garbage to the file.
    let mut data = std::fs::read(&path).unwrap();
    data.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0xFF]);
    std::fs::write(&path, &data).unwrap();

    // Should still load fine — trailing data is beyond payload_len.
    let loaded = load_from_file(&path).unwrap();
    assert_eq!(loaded.stats().context_count, 1);
    assert_eq!(loaded.get_state(&key), Some(ContextState::Warm));
}

/// A crafted file with payload_len = u64::MAX must not panic or cause
/// undefined behavior. The addition HEADER_SIZE + payload_len overflows
/// usize, which panics in debug mode and wraps in release.
#[test]
fn payload_length_overflow_u64_max() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);

    let mut data = Vec::new();
    data.extend_from_slice(&DEPGRAPH_MAGIC);
    data.extend_from_slice(&DEPGRAPH_VERSION.to_le_bytes());
    data.extend_from_slice(&u64::MAX.to_le_bytes());
    data.extend_from_slice(&[0u8; 64]); // some payload bytes
    std::fs::write(&path, &data).unwrap();

    // Must return an error, not panic.
    assert!(
        load_from_file(&path).is_err(),
        "u64::MAX payload_len must be rejected"
    );
}

/// payload_len = usize::MAX - HEADER_SIZE + 1 causes overflow of
/// HEADER_SIZE + payload_len.
#[test]
fn payload_length_overflow_boundary() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);

    // This value causes HEADER_SIZE + payload_len to wrap to exactly 0.
    let evil_len = (usize::MAX - HEADER_SIZE).wrapping_add(1) as u64;

    let mut data = Vec::new();
    data.extend_from_slice(&DEPGRAPH_MAGIC);
    data.extend_from_slice(&DEPGRAPH_VERSION.to_le_bytes());
    data.extend_from_slice(&evil_len.to_le_bytes());
    data.extend_from_slice(&[0u8; 64]);
    std::fs::write(&path, &data).unwrap();

    // Must return an error, not panic.
    assert!(
        load_from_file(&path).is_err(),
        "overflow-inducing payload_len must be rejected"
    );
}

// ── classify_load tests (issue #320) ─────────────────────────────────────

#[test]
fn classify_load_missing_returns_missing() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("absent.bin");

    let outcome = classify_load(&path);
    assert!(matches!(outcome, DepGraphLoadOutcome::Missing));
    assert!(outcome.warning(&path).is_none(), "Missing must not warn");
    assert!(outcome.into_graph().is_none());
}

#[test]
fn classify_load_valid_returns_loaded() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);

    let graph = DepGraph::new();
    let _ = graph.register(make_ctx("/src/main.cpp"));
    save_to_file(&graph, &path).unwrap();

    let outcome = classify_load(&path);
    assert!(matches!(outcome, DepGraphLoadOutcome::Loaded { .. }));
    assert!(outcome.warning(&path).is_none(), "Loaded must not warn");
    let loaded = outcome.into_graph().expect("Loaded must yield graph");
    assert_eq!(loaded.stats().context_count, 1);
}

#[test]
fn classify_load_version_mismatch_warns() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);

    let mut data = Vec::new();
    data.extend_from_slice(&DEPGRAPH_MAGIC);
    data.extend_from_slice(&99u32.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes());
    std::fs::write(&path, &data).unwrap();

    let outcome = classify_load(&path);
    match &outcome {
        DepGraphLoadOutcome::VersionMismatch {
            file_version: 99,
            expected_version,
        } => {
            assert_eq!(*expected_version, DEPGRAPH_VERSION);
        }
        other => panic!("expected VersionMismatch, got {other:?}"),
    }
    let warning = outcome.warning(&path).expect("must warn");
    assert!(warning.contains("version 99"));
    assert!(warning.contains("treating session as cold"));
}

#[test]
fn classify_load_bad_magic_is_corrupt() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);

    let mut data = Vec::new();
    data.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
    data.extend_from_slice(&DEPGRAPH_VERSION.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes());
    std::fs::write(&path, &data).unwrap();

    let outcome = classify_load(&path);
    assert!(matches!(outcome, DepGraphLoadOutcome::Corrupt { .. }));
    let warning = outcome.warning(&path).expect("must warn");
    assert!(warning.contains("corrupt"));
    assert!(warning.contains("treating session as cold"));
}

#[test]
fn classify_load_truncated_is_corrupt() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);

    // Too small to even hold the header.
    std::fs::write(&path, [0x5Au8, 0x43, 0x44]).unwrap();

    let outcome = classify_load(&path);
    assert!(matches!(outcome, DepGraphLoadOutcome::Corrupt { .. }));
    assert!(outcome.warning(&path).is_some());
}
