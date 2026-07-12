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
fn persist_artifact_paths_preserves_compiler_output_writability() {
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

    if staged_artifacts_enabled() {
        let payloads = load_staged_artifact_paths(dir.path(), key, &[10, 11])
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read(&payloads[0]).unwrap(), b"rlib-bytes");
        assert_eq!(std::fs::read(&payloads[1]).unwrap(), b"rmeta-bytes");
        assert!(!same_file(&src_a, &payloads[0]));
        assert!(!same_file(&src_b, &payloads[1]));
        return;
    }

    let cache_a = dir.path().join("deadc0de_0");
    let cache_b = dir.path().join("deadc0de_1");
    assert_eq!(std::fs::read(&cache_a).unwrap(), b"rlib-bytes");
    assert_eq!(std::fs::read(&cache_b).unwrap(), b"rmeta-bytes");

    // A hardlink-tier store may intentionally share the source inode, but
    // persistence must never apply the cache blob's read-only bit through
    // that alias and make the still-live compiler output unwritable.
    assert!(!std::fs::metadata(&src_a).unwrap().permissions().readonly());
    assert!(!std::fs::metadata(&src_b).unwrap().permissions().readonly());
}

#[test]
fn mixed_v1_pack_v2_lookup_and_downgrade_policy_are_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let v1_key = "1".repeat(64);
    let pack_key = "2".repeat(64);
    let v2_key = "3".repeat(64);
    std::fs::write(dir.path().join(format!("{v1_key}_0")), b"legacy-flat").unwrap();
    std::fs::write(
        pack_path_for(dir.path(), &pack_key),
        build_pack(&[Arc::new(b"legacy-pack".to_vec())]),
    )
    .unwrap();
    let v2_source: NormalizedPath = dir.path().join("v2-source.o").into();
    std::fs::write(&v2_source, b"staged-v2").unwrap();
    persist_staged_artifact_paths(dir.path(), &v2_key, &[v2_source]).unwrap();

    let index = |name: &str, size: u64| {
        CachedArtifact::from_index(ArtifactIndex::new(
            vec![name.to_string()],
            vec![size],
            Arc::new(Vec::new()),
            Arc::new(Vec::new()),
            0,
        ))
    };
    let bytes = |payload: &CachedPayload| match payload {
        CachedPayload::File(path) => std::fs::read(path).unwrap(),
        CachedPayload::Bytes(bytes) => bytes.as_ref().clone(),
    };

    let mut v1 = index("legacy.o", 11);
    let mut pack = index("packed.o", 11);
    let mut v2 = index("staged.o", 9);
    assert_eq!(
        bytes(&ensure_payloads_with_staged_policy(&mut v1, dir.path(), &v1_key, true).unwrap()[0]),
        b"legacy-flat"
    );
    assert_eq!(
        bytes(
            &ensure_payloads_with_staged_policy(&mut pack, dir.path(), &pack_key, true).unwrap()[0]
        ),
        b"legacy-pack"
    );
    assert_eq!(
        bytes(&ensure_payloads_with_staged_policy(&mut v2, dir.path(), &v2_key, true).unwrap()[0]),
        b"staged-v2"
    );

    // Downgrade/kill-switch policy: legacy layouts remain readable, while a
    // v2-only entry is a safe miss rather than being reinterpreted.
    let mut downgraded_v1 = index("legacy.o", 11);
    let mut downgraded_pack = index("packed.o", 11);
    let mut downgraded_v2 = index("staged.o", 9);
    assert!(
        ensure_payloads_with_staged_policy(&mut downgraded_v1, dir.path(), &v1_key, false)
            .is_some()
    );
    assert!(
        ensure_payloads_with_staged_policy(&mut downgraded_pack, dir.path(), &pack_key, false)
            .is_some()
    );
    assert!(
        ensure_payloads_with_staged_policy(&mut downgraded_v2, dir.path(), &v2_key, false)
            .is_none()
    );
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
