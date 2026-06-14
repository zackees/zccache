//! Tests for the artifact pack format and the unpacked-layout persist
//! path (`build_pack`, `parse_pack_header`, `try_load_packed_payload`,
//! `persist_artifact_payloads`, `persist_artifact_paths`).

use super::super::*;

#[test]
fn pack_round_trip_extracts_each_payload() {
    let p0: Arc<Vec<u8>> = Arc::new(b"first payload".to_vec());
    let p1: Arc<Vec<u8>> = Arc::new((0u8..200).cycle().take(4096).collect());
    let p2: Arc<Vec<u8>> = Arc::new(Vec::new()); // 0-length payload edge case
    let payloads = vec![Arc::clone(&p0), Arc::clone(&p1), Arc::clone(&p2)];
    let pack = build_pack(&payloads);

    let entries = parse_pack_header(&pack).unwrap();
    assert_eq!(entries.len(), 3);
    for (i, (offset, size)) in entries.iter().enumerate() {
        let s = *offset as usize;
        let e = s + *size as usize;
        assert_eq!(&pack[s..e], payloads[i].as_slice());
    }
}

#[test]
fn parse_pack_header_rejects_garbage() {
    assert!(parse_pack_header(b"").is_err());
    assert!(parse_pack_header(b"NOTAZCPK").is_err());
    // Magic OK but truncated header
    assert!(parse_pack_header(b"ZCPK\x05\x00\x00\x00").is_err());
}

#[test]
fn try_load_packed_payload_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let key = "deadbeef";
    let payloads: Vec<Arc<Vec<u8>>> = vec![
        Arc::new(b"alpha".to_vec()),
        Arc::new(b"bravo bravo bravo".to_vec()),
    ];
    let pack = build_pack(&payloads);
    std::fs::write(pack_path_for(dir.path(), key), &pack).unwrap();

    assert_eq!(
        try_load_packed_payload(dir.path(), key, 0).unwrap(),
        b"alpha".to_vec()
    );
    assert_eq!(
        try_load_packed_payload(dir.path(), key, 1).unwrap(),
        b"bravo bravo bravo".to_vec()
    );
    assert!(try_load_packed_payload(dir.path(), key, 2).is_none());
    assert!(try_load_packed_payload(dir.path(), "missing", 0).is_none());
}

#[test]
fn persist_artifact_payloads_unpacked_layout() {
    // Default: not packed.
    std::env::remove_var("ZCCACHE_PACK_ARTIFACTS");
    let dir = tempfile::tempdir().unwrap();
    let key = "abc123";
    let payloads = vec![Arc::new(b"one".to_vec()), Arc::new(b"two".to_vec())];
    persist_artifact_payloads(dir.path(), key, &payloads).unwrap();
    assert_eq!(std::fs::read(dir.path().join("abc123_0")).unwrap(), b"one");
    assert_eq!(std::fs::read(dir.path().join("abc123_1")).unwrap(), b"two");
    assert!(!dir.path().join("abc123.pack").exists());
}

#[test]
fn persist_artifact_paths_hardlinks_in_unpacked_layout() {
    std::env::remove_var("ZCCACHE_PACK_ARTIFACTS");
    let dir = tempfile::tempdir().unwrap();
    let key = "deadc0de";
    // Source files that simulate "compiler just wrote these".
    let src_a = dir.path().join("foo.rlib");
    let src_b = dir.path().join("foo.rmeta");
    std::fs::write(&src_a, b"rlib-bytes").unwrap();
    std::fs::write(&src_b, b"rmeta-bytes").unwrap();
    let sources = vec![
        NormalizedPath::from(src_a.clone()),
        NormalizedPath::from(src_b.clone()),
    ];
    persist_artifact_paths(dir.path(), key, &sources).unwrap();

    let cache_a = dir.path().join("deadc0de_0");
    let cache_b = dir.path().join("deadc0de_1");
    assert_eq!(std::fs::read(&cache_a).unwrap(), b"rlib-bytes");
    assert_eq!(std::fs::read(&cache_b).unwrap(), b"rmeta-bytes");

    // On the same-volume happy path we expect a real hardlink — both
    // names should resolve to the same inode. Inode-equality test via
    // platform metadata. Skip on platforms that don't easily expose
    // it (Windows tests still verify the bytes match above).
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let src_ino = std::fs::metadata(&src_a).unwrap().ino();
        let cache_ino = std::fs::metadata(&cache_a).unwrap().ino();
        assert_eq!(src_ino, cache_ino, "expected hardlink (shared inode)");
    }
}

#[test]
fn persist_artifact_paths_falls_back_to_copy_when_source_missing() {
    std::env::remove_var("ZCCACHE_PACK_ARTIFACTS");
    let dir = tempfile::tempdir().unwrap();
    let key = "nopath";
    let missing = dir.path().join("does-not-exist.rlib");
    let sources = vec![NormalizedPath::from(missing)];
    // Hardlink fails (source missing), copy also fails → err propagates.
    // Caller's contract is "best effort; on err skip caching."
    assert!(persist_artifact_paths(dir.path(), key, &sources).is_err());
}

#[test]
fn persist_artifact_paths_err_includes_diagnostics_for_missing_source() {
    // Issue #728: WARN at the persist call site has historically been a bare
    // "failed to persist artifact output: os error 2" with no path context —
    // we could not tell whether ninja deleted the output mid-flight, whether
    // the destination dir was wrong, or whether Defender quarantined the
    // file. The error returned from `persist_artifact_paths` now embeds the
    // full diagnostic so callers (the daemon WARN sites) surface it without
    // any extra plumbing.
    std::env::remove_var("ZCCACHE_PACK_ARTIFACTS");
    let dir = tempfile::tempdir().unwrap();
    let key = "diagkey";
    let missing = dir.path().join("does-not-exist.rlib");
    let sources = vec![NormalizedPath::from(missing.clone())];
    let err = persist_artifact_paths(dir.path(), key, &sources).expect_err("expected err");
    let msg = format!("{err}");
    assert!(msg.contains("src="), "missing src= field in {msg}");
    assert!(msg.contains("dst="), "missing dst= field in {msg}");
    assert!(msg.contains("errno="), "missing errno= field in {msg}");
    assert!(
        msg.contains("src_exists_now=false"),
        "expected src_exists_now=false in {msg}"
    );
    assert!(
        msg.contains("src_size_now=?"),
        "expected src_size_now=? for missing source in {msg}"
    );
}

#[test]
fn persist_artifact_output_err_includes_dst_diagnostics() {
    // Counterpart for `persist_artifact_output` (payload writes): payloads
    // come from RAM so there's no source-path question, but `dst=` and
    // `errno=` must still be embedded so a write-failure WARN is debuggable.
    std::env::remove_var("ZCCACHE_PACK_ARTIFACTS");
    // Cache path under a *file* (not a dir) so create_dir_all fails:
    // mkdir-ing a path whose parent is a regular file yields NotADirectory
    // on Linux and InvalidInput on Windows — either way an error path we
    // can exercise without root.
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"not a directory").unwrap();
    let cache_path = blocker.join("nested").join("artifact_0");
    let err = persist_artifact_output(&cache_path, b"bytes").expect_err("expected err");
    let msg = format!("{err}");
    assert!(msg.contains("dst="), "missing dst= field in {msg}");
    assert!(msg.contains("errno="), "missing errno= field in {msg}");
    // No src= for payload writes — that field is gated on Some(src).
    assert!(
        !msg.contains("src="),
        "payload writes have no source path; src= must not appear in {msg}"
    );
}
