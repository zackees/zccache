use super::*;

fn store() -> (tempfile::TempDir, KvStore) {
    let dir = tempfile::tempdir().unwrap();
    let s = KvStore::open(dir.path()).unwrap();
    (dir, s)
}

fn key_from(seed: &[u8]) -> Key {
    Key::from_hash(::kernal_api::hash::blake3_bytes(seed))
}

fn value_file(dir: &tempfile::TempDir, ns: &str, k: &Key) -> std::path::PathBuf {
    dir.path()
        .join("kv")
        .join(ns)
        .join(format!("{}.bin", k.to_hex()))
}

// ---- F1: round-trip across size boundaries ----
#[test]
fn f1_round_trip_sizes() {
    let (_d, s) = store();
    let sizes = [0, 1, 100, 4095, 4096, 4097, 64 * 1024];
    for (i, n) in sizes.iter().enumerate() {
        let k = key_from(&i.to_le_bytes());
        let val: Vec<u8> = (0..*n).map(|j| (j % 251) as u8).collect();
        assert_eq!(s.put("ns", &k, &val).unwrap(), val.len());
        let got = s.get("ns", &k).unwrap().unwrap();
        assert_eq!(got, val, "size {n} round-trip mismatch");
    }
}

// ---- F2: miss returns Ok(None) ----
#[test]
fn f2_miss_returns_none() {
    let (_d, s) = store();
    let k = key_from(b"nope");
    assert!(s.get("ns", &k).unwrap().is_none());
}

// ---- F3: overwrite ----
#[test]
fn f3_overwrite() {
    let (_d, s) = store();
    let k = key_from(b"ow");
    s.put("ns", &k, b"v1").unwrap();
    s.put("ns", &k, b"v2").unwrap();
    assert_eq!(s.get("ns", &k).unwrap().unwrap(), b"v2");
}

// ---- F4: remove + idempotent ----
#[test]
fn f4_remove() {
    let (_d, s) = store();
    let k = key_from(b"r");
    s.put("ns", &k, b"x").unwrap();
    s.remove("ns", &k).unwrap();
    assert!(s.get("ns", &k).unwrap().is_none());
    s.remove("ns", &k).unwrap();
}

// ---- F5: clear_namespace isolation ----
#[test]
fn f5_clear_namespace_isolation() {
    let (_d, s) = store();
    let k = key_from(b"k");
    s.put("a", &k, b"in-a").unwrap();
    s.put("b", &k, b"in-b").unwrap();
    s.clear_namespace("a").unwrap();
    assert!(s.get("a", &k).unwrap().is_none());
    assert_eq!(s.get("b", &k).unwrap().unwrap(), b"in-b");
}

// ---- F5b: clearing a namespace that was never written is a no-op ----
#[test]
fn f5b_clear_absent_namespace_is_ok() {
    let (_d, s) = store();
    s.clear_namespace("never-written").unwrap();
}

// ---- F6: list_namespace sorted, lengths correct ----
#[test]
fn f6_list_sorted_and_lengths() {
    let (_d, s) = store();
    let mut keys: Vec<Key> = (0u32..5).map(|i| key_from(&i.to_le_bytes())).collect();
    let mut expected: std::collections::HashMap<String, u64> = Default::default();
    for (i, k) in keys.iter().enumerate() {
        let n = if i % 2 == 0 { 10 } else { 4196 };
        s.put("ns", k, &vec![i as u8; n]).unwrap();
        expected.insert(k.to_hex(), n as u64);
    }
    let listed = s.list_namespace("ns").unwrap();
    assert_eq!(listed.len(), 5);
    keys.sort_by_key(|k| k.to_hex());
    for (i, (k, len)) in listed.iter().enumerate() {
        assert_eq!(k.to_hex(), keys[i].to_hex(), "list not sorted at {i}");
        assert_eq!(*len, expected[&k.to_hex()], "payload length for entry {i}");
    }
}

// ---- F6b: listing an absent namespace is empty, not an error ----
#[test]
fn f6b_list_absent_namespace_is_empty() {
    let (_d, s) = store();
    assert!(s.list_namespace("absent").unwrap().is_empty());
}

// ---- F7: total_bytes == sum of namespace_bytes ----
#[test]
fn f7_total_eq_sum() {
    let (_d, s) = store();
    for ns in &["a", "b", "c"] {
        for i in 0..3 {
            let k = key_from(format!("{ns}-{i}").as_bytes());
            s.put(ns, &k, &vec![0u8; 50 + i]).unwrap();
        }
    }
    let total = s.total_bytes().unwrap();
    let sum: u64 = ["a", "b", "c"]
        .iter()
        .map(|ns| s.namespace_bytes(ns).unwrap())
        .sum();
    assert_eq!(total, sum);
    // Header bytes must not be counted: 3 namespaces x (50 + 51 + 52).
    assert_eq!(total, 3 * (50 + 51 + 52));
}

// ---- F8: every value is a file of header + payload ----
#[test]
fn f8_values_are_files() {
    let (d, s) = store();
    let small = key_from(b"small");
    let large = key_from(b"large");
    s.put("ns", &small, &[1u8; 16]).unwrap();
    s.put("ns", &large, &vec![2u8; 40_000]).unwrap();

    for (k, payload) in [(&small, 16usize), (&large, 40_000usize)] {
        let path = value_file(&d, "ns", k);
        assert!(path.exists(), "value must be on disk at {}", path.display());
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            (payload + HEADER_LEN) as u64,
            "file is header + payload"
        );
    }
}

// ---- F9: tampered payload → Corrupt ----
#[test]
fn f9_tampered_payload_detected() {
    let (d, s) = store();
    let k = key_from(b"corrupt");
    s.put("ns", &k, &vec![7u8; 4196]).unwrap();
    let path = value_file(&d, "ns", &k);
    let mut bytes = std::fs::read(&path).unwrap();
    // Flip a payload byte, leaving the header's recorded blake3 intact.
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&path, &bytes).unwrap();
    let err = s.get("ns", &k).unwrap_err();
    match err {
        KvError::Corrupt(_, msg) => assert!(msg.contains("blake3 mismatch"), "msg={msg}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

// ---- F9b: truncated payload → Corrupt, not a short read ----
#[test]
fn f9b_truncated_file_detected() {
    let (d, s) = store();
    let k = key_from(b"trunc");
    s.put("ns", &k, &vec![3u8; 1000]).unwrap();
    let path = value_file(&d, "ns", &k);
    let bytes = std::fs::read(&path).unwrap();
    std::fs::write(&path, &bytes[..bytes.len() - 10]).unwrap();
    let err = s.get("ns", &k).unwrap_err();
    match err {
        KvError::Corrupt(_, msg) => assert!(msg.contains("length mismatch"), "msg={msg}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

// ---- F9c: a header-only truncation is still Corrupt ----
#[test]
fn f9c_header_truncation_detected() {
    let (d, s) = store();
    let k = key_from(b"hdr");
    s.put("ns", &k, b"payload").unwrap();
    std::fs::write(value_file(&d, "ns", &k), b"ZCKV").unwrap();
    let err = s.get("ns", &k).unwrap_err();
    match err {
        KvError::Corrupt(_, msg) => assert!(msg.contains("truncated"), "msg={msg}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

// ---- F9d: a file that is not ours at all is Corrupt, not silently data ----
#[test]
fn f9d_foreign_magic_detected() {
    let (d, s) = store();
    let k = key_from(b"foreign");
    s.put("ns", &k, b"payload").unwrap();
    std::fs::write(value_file(&d, "ns", &k), vec![0u8; HEADER_LEN + 8]).unwrap();
    let err = s.get("ns", &k).unwrap_err();
    match err {
        KvError::Corrupt(_, msg) => assert!(msg.contains("bad magic"), "msg={msg}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

// ---- F10: hex round-trip + bad inputs ----
#[test]
fn f10_key_hex_round_trip() {
    let h = ::kernal_api::hash::blake3_bytes(b"hello");
    let k = Key::from_hash(h);
    let hex = k.to_hex();
    assert_eq!(hex.len(), 64);
    assert!(hex
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    assert_eq!(Key::from_hex(&hex).unwrap(), k);
    assert_eq!(Key::from_hex(&hex.to_ascii_uppercase()).unwrap(), k);

    assert!(matches!(Key::from_hex(""), Err(KvError::BadKey)));
    assert!(matches!(Key::from_hex("zz"), Err(KvError::BadKey)));
    assert!(matches!(
        Key::from_hex(&"a".repeat(63)),
        Err(KvError::BadKey)
    ));
    assert!(matches!(
        Key::from_hex(&"a".repeat(65)),
        Err(KvError::BadKey)
    ));
    let mut bad = "a".repeat(64);
    bad.replace_range(0..1, "g");
    assert!(matches!(Key::from_hex(&bad), Err(KvError::BadKey)));
}

// ---- F11: namespace validator ----
#[test]
fn f11_namespace_validator() {
    assert!(is_valid_namespace("a"));
    assert!(is_valid_namespace("0"));
    assert!(is_valid_namespace("library-selection"));
    assert!(is_valid_namespace(&"x".repeat(64)));

    assert!(!is_valid_namespace(""));
    assert!(!is_valid_namespace("A"));
    assert!(!is_valid_namespace("name with space"));
    assert!(!is_valid_namespace("a/b"));
    assert!(!is_valid_namespace("日本語"));
    assert!(!is_valid_namespace(&"x".repeat(65)));
    assert!(!is_valid_namespace("a::b"));
}

// ---- F12: schema_version mismatch surfaces as Corrupt ----
#[test]
fn f12_schema_version_mismatch() {
    let (d, s) = store();
    let k = key_from(b"sv");
    s.put("ns", &k, b"hi").unwrap();
    let path = value_file(&d, "ns", &k);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[4..8].copy_from_slice(&(SCHEMA_VERSION + 1).to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();

    let err = s.get("ns", &k).unwrap_err();
    match err {
        KvError::Corrupt(_, msg) => assert!(msg.contains("schema_version="), "msg={msg}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

// ---- F13: a foreign file in the namespace dir is ignored by listing ----
#[test]
fn f13_foreign_files_are_skipped() {
    let (d, s) = store();
    let k = key_from(b"real");
    s.put("ns", &k, b"value").unwrap();
    let ns_dir = d.path().join("kv").join("ns");
    std::fs::write(ns_dir.join("README.txt"), b"not a value").unwrap();
    std::fs::write(ns_dir.join("deadbeef.bin"), b"bad hex name").unwrap();

    let listed = s.list_namespace("ns").unwrap();
    assert_eq!(listed.len(), 1, "only the real value is listed");
    assert_eq!(listed[0].0.to_hex(), k.to_hex());
}

// ---- I1..I4: namespace edge cases via put ----
#[test]
fn i1_empty_namespace_rejected() {
    let (_d, s) = store();
    let k = key_from(b"x");
    assert!(matches!(s.put("", &k, b"v"), Err(KvError::BadNamespace)));
}

#[test]
fn i2_namespace_at_limit_ok() {
    let (_d, s) = store();
    let k = key_from(b"x");
    let ns = "a".repeat(64);
    s.put(&ns, &k, b"v").unwrap();
}

#[test]
fn i3_namespace_too_long_rejected() {
    let (_d, s) = store();
    let k = key_from(b"x");
    let ns = "a".repeat(65);
    assert!(matches!(s.put(&ns, &k, b"v"), Err(KvError::BadNamespace)));
}

#[test]
fn i4_namespace_with_double_colon_rejected() {
    let (_d, s) = store();
    let k = key_from(b"x");
    assert!(matches!(
        s.put("a::b", &k, b"v"),
        Err(KvError::BadNamespace)
    ));
}

// ---- I6: max value bytes (allocates 64 MiB, runs only under --full) ----
#[test]
#[ignore = "allocates 64 MiB; see tests/stress/artifact_kv_stress.rs for max-cap coverage"]
fn i6_too_large_rejected() {
    let (_d, s) = store();
    let k = key_from(b"big");
    let oversized = MAX_VALUE_BYTES + 1;
    let v = vec![0u8; oversized];
    let err = s.put("ns", &k, &v).unwrap_err();
    assert!(matches!(err, KvError::TooLarge(n, m) if n == oversized && m == MAX_VALUE_BYTES));
}

// ---- I7: same key, different namespaces are independent ----
#[test]
fn i7_namespaces_are_independent() {
    let (_d, s) = store();
    let k = key_from(b"shared");
    s.put("a", &k, b"a-val").unwrap();
    s.put("b", &k, b"b-val").unwrap();
    assert_eq!(s.get("a", &k).unwrap().unwrap(), b"a-val");
    assert_eq!(s.get("b", &k).unwrap().unwrap(), b"b-val");
}

// ---- P8 / I8: reopen sees prior writes ----
#[test]
fn p8_reopen_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let k = key_from(b"persist");
    {
        let s = KvStore::open(dir.path()).unwrap();
        s.put("ns", &k, &vec![3u8; 4106]).unwrap();
    }
    let s = KvStore::open(dir.path()).unwrap();
    assert_eq!(s.get("ns", &k).unwrap().unwrap(), vec![3u8; 4106]);
}

// ---- P3: case-insensitive key parsing means UPPER and lower collide ----
#[test]
fn p3_case_insensitive_key_parses_to_same_key() {
    let k = Key::from_hash(::kernal_api::hash::blake3_bytes(b"x"));
    let lower = k.to_hex();
    let upper = lower.to_ascii_uppercase();
    assert_eq!(
        Key::from_hex(&lower).unwrap(),
        Key::from_hex(&upper).unwrap()
    );
}

// ---- #1352: two stores over one directory coexist ----
//
// This is the regression the redb-backed implementation could not pass: a
// second `Database::create` on the same file returned
// `DatabaseAlreadyOpen`, which is the whole reason this store dropped its
// database. Two `KvStore`s are now independent handles onto a shared
// directory, so this must simply work.
#[test]
fn concurrent_stores_over_one_dir_do_not_lock_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let a = KvStore::open(dir.path()).unwrap();
    let b = KvStore::open(dir.path()).unwrap();

    let ka = key_from(b"from-a");
    let kb = key_from(b"from-b");
    a.put("ns", &ka, b"a-wrote").unwrap();
    b.put("ns", &kb, b"b-wrote").unwrap();

    // Each store sees the other's committed write.
    assert_eq!(b.get("ns", &ka).unwrap().unwrap(), b"a-wrote");
    assert_eq!(a.get("ns", &kb).unwrap().unwrap(), b"b-wrote");
    assert_eq!(a.list_namespace("ns").unwrap().len(), 2);
}

// A `put` racing `clear_namespace` must linearize: the write lands either
// before the clear (and is dropped) or after it (and survives). It used to
// fail with `NotFound` whenever the clear removed the namespace directory
// between the writer's `create_dir_all` and its tempfile create or rename,
// and the clear could itself fail with `DirectoryNotEmpty` when a writer
// dropped a file into the directory mid-walk. The ignored stress test
// `c5_clear_while_writes_keeps_consistency` hit this on CI (#1648); this
// repeats the interleaving enough times to reproduce it in the unit suite.
#[test]
fn put_racing_clear_namespace_never_errors() {
    let (_d, s) = store();
    let stop = std::sync::atomic::AtomicBool::new(false);
    // Errors are collected, not unwrapped in the loops: a panicking clearer
    // would never raise `stop` and the scope would wait on the writers forever.
    let (put_errors, clear_errors) = std::thread::scope(|scope| {
        let writers: Vec<_> = (0..4)
            .map(|w| {
                let s = s.clone();
                let stop = &stop;
                scope.spawn(move || {
                    let mut errors = Vec::new();
                    let mut i = 0u32;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let k = key_from(format!("race-{w}-{i}").as_bytes());
                        if let Err(e) = s.put("ns", &k, b"v") {
                            errors.push(e.to_string());
                        }
                        i += 1;
                    }
                    errors
                })
            })
            .collect();
        let clear_errors: Vec<String> = (0..100)
            .filter_map(|_| s.clear_namespace("ns").err().map(|e| e.to_string()))
            .collect();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let put_errors: Vec<String> = writers
            .into_iter()
            .flat_map(|writer| writer.join().unwrap())
            .collect();
        (put_errors, clear_errors)
    });
    assert!(put_errors.is_empty(), "puts failed: {put_errors:?}");
    assert!(clear_errors.is_empty(), "clears failed: {clear_errors:?}");
    // Every surviving entry must still be a complete value.
    for (k, _len) in s.list_namespace("ns").unwrap() {
        assert_eq!(s.get("ns", &k).unwrap().unwrap(), b"v");
    }
    // A clear leaves nothing behind that a listing can see.
    s.clear_namespace("ns").unwrap();
    assert!(s.list_namespace("ns").unwrap().is_empty());
    assert_eq!(s.total_bytes().unwrap(), 0);
}
