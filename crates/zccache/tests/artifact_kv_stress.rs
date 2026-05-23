//! Stress / concurrency / platform-compliance tests for the K/V store.
//!
//! Heavy tests (thousands of fsync commits, 64 MiB allocations, busy-loop
//! reader/writer races) are marked `#[ignore]` so they only run under
//! `./test --full`. Default `./test` and the Stop hook stay fast.

use std::sync::Arc;

use zccache_artifact::kv::{is_valid_namespace, INLINE_THRESHOLD, MAX_VALUE_BYTES};
use zccache_artifact::{Key, KvError, KvStore};

fn store_in_dir(dir: &std::path::Path) -> KvStore {
    KvStore::open(dir).unwrap()
}

fn key_from(seed: &[u8]) -> Key {
    Key::from_hash(blake3::hash(seed))
}

// =====================================================================
// Concurrency (C1..C5)
// =====================================================================

#[test]
#[ignore = "stress: 1600 fsync commits"]
fn c1_thundering_herd_same_key() {
    let dir = tempfile::tempdir().unwrap();
    let s = store_in_dir(dir.path());
    let k = key_from(b"herd");
    let valid: std::collections::HashSet<Vec<u8>> = (0u8..16).map(|tid| vec![tid; 1024]).collect();

    std::thread::scope(|scope| {
        for tid in 0u8..16 {
            let s = s.clone();
            scope.spawn(move || {
                let buf = vec![tid; 1024];
                for _ in 0..100 {
                    s.put("ns", &k, &buf).unwrap();
                }
            });
        }
    });

    // Final read: must be exactly one of the 16 written variants. Never None,
    // never error.
    let got = s.get("ns", &k).unwrap().expect("post-herd read must hit");
    assert!(
        valid.contains(&got),
        "post-herd read returned a value no thread wrote"
    );
}

#[test]
#[ignore = "stress: 640 fsync commits"]
fn c2_distinct_key_parallel_writers() {
    let dir = tempfile::tempdir().unwrap();
    let s = store_in_dir(dir.path());
    const THREADS: u32 = 16;
    const KEYS_PER_THREAD: u32 = 40;

    std::thread::scope(|scope| {
        for tid in 0..THREADS {
            let s = s.clone();
            scope.spawn(move || {
                for i in 0..KEYS_PER_THREAD {
                    let seed = format!("t{tid}-k{i}");
                    let k = key_from(seed.as_bytes());
                    s.put("ns", &k, seed.as_bytes()).unwrap();
                }
            });
        }
    });

    // Verify every key is readable.
    for tid in 0..THREADS {
        for i in 0..KEYS_PER_THREAD {
            let seed = format!("t{tid}-k{i}");
            let k = key_from(seed.as_bytes());
            let got = s.get("ns", &k).unwrap().unwrap();
            assert_eq!(got, seed.as_bytes());
        }
    }

    let listed = s.list_namespace("ns").unwrap();
    assert_eq!(listed.len(), (THREADS * KEYS_PER_THREAD) as usize);
}

#[test]
#[ignore = "stress: spinning reader/writer race"]
fn c3_reader_writer_race() {
    let dir = tempfile::tempdir().unwrap();
    let s = store_in_dir(dir.path());
    let k = key_from(b"rw-race");
    let valid: std::collections::HashSet<Vec<u8>> = (0u8..8).map(|tid| vec![tid; 64]).collect();

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    std::thread::scope(|scope| {
        // Writers
        for tid in 0u8..8 {
            let s = s.clone();
            scope.spawn(move || {
                let buf = vec![tid; 64];
                for _ in 0..200 {
                    s.put("ns", &k, &buf).unwrap();
                }
            });
        }
        // Readers — spin until writers are done.
        for _ in 0..8 {
            let s = s.clone();
            let stop = stop.clone();
            let valid = valid.clone();
            scope.spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    match s.get("ns", &k).unwrap() {
                        None => {} // pre-first-write read is OK
                        Some(v) => assert!(valid.contains(&v), "reader saw torn value"),
                    }
                }
            });
        }
        // Once writers join (the for-loop above doesn't actually join until
        // scope exit, so we mark stop right before scope drains).
        // Wait briefly using a sentinel write so the readers definitely run.
        // (No sleeps — we just let scope drain naturally for writers, then
        // signal stop in the closure below.)
        let s_stopper = s.clone();
        scope.spawn(move || {
            // Writers will all complete in bounded time; once the prior
            // closures' iterations finish, signal readers to stop. We can't
            // directly join here because we're inside the same scope; but
            // the readers will exit when stop=true. We achieve that by
            // spinning until the post-condition is verifiable.
            for _ in 0..50_000 {
                if s_stopper.get("ns", &k).unwrap().is_some() {
                    break;
                }
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        });
    });

    // Post-join: the value must be a valid one.
    let got = s.get("ns", &k).unwrap().unwrap();
    assert!(valid.contains(&got));
}

#[test]
fn c4_open_while_write_visibility() {
    let dir = tempfile::tempdir().unwrap();
    let a = store_in_dir(dir.path());
    let b = a.clone(); // shared Arc<Database> — visible commits

    let k = key_from(b"shared-vis");
    b.put("ns", &k, b"hello").unwrap();
    // Reader A must see B's commit.
    let got = a.get("ns", &k).unwrap().unwrap();
    assert_eq!(got, b"hello");
}

#[test]
#[ignore = "stress: 1000 fsync commits"]
fn c5_clear_while_writes_keeps_consistency() {
    let dir = tempfile::tempdir().unwrap();
    let s = store_in_dir(dir.path());

    std::thread::scope(|scope| {
        let s_writer = s.clone();
        scope.spawn(move || {
            for i in 0..1000 {
                let k = key_from(format!("c5-{i}").as_bytes());
                s_writer.put("ns", &k, b"v").unwrap();
            }
        });
        let s_clearer = s.clone();
        scope.spawn(move || {
            // One clear interleaved with the writes.
            s_clearer.clear_namespace("ns").unwrap();
        });
    });

    // After both join, listing must succeed without panic. Either empty or a
    // subset of the writes — both are consistent outcomes.
    let listed = s.list_namespace("ns").unwrap();
    for (k, _len) in &listed {
        // Each surviving entry must round-trip via get without error.
        let _ = s.get("ns", k).unwrap();
    }
}

// =====================================================================
// Durability (D3) — repeated open/close
// =====================================================================

#[test]
#[ignore = "stress: 50 redb open/close cycles"]
fn d3_repeated_open_close() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..50 {
        let s = KvStore::open(dir.path()).unwrap();
        let k = key_from(format!("rt-{i}").as_bytes());
        s.put("ns", &k, b"v").unwrap();
        drop(s);
    }
    let s = KvStore::open(dir.path()).unwrap();
    let listed = s.list_namespace("ns").unwrap();
    assert_eq!(listed.len(), 50);
}

// =====================================================================
// Platform compliance (P1, P2, P4, P5, P6)
// =====================================================================

#[test]
fn p1_path_separator_uses_path_join() {
    let dir = tempfile::tempdir().unwrap();
    let s = store_in_dir(dir.path());
    let k = key_from(b"p1");
    s.put("ns", &k, &vec![0u8; INLINE_THRESHOLD + 1]).unwrap();
    let expected_parent = dir.path().join("kv").join("ns");
    let entry_path = expected_parent.join(format!("{}.bin", k.to_hex()));
    assert!(entry_path.exists());
    assert_eq!(entry_path.parent().unwrap(), expected_parent);
}

#[cfg(windows)]
#[test]
fn p2_windows_long_path_spill() {
    // Build a deep nested path so the spill file path exceeds 260 chars.
    let base = tempfile::tempdir().unwrap();
    let mut deep = base.path().to_path_buf();
    while deep.to_string_lossy().len() < 200 {
        deep = deep.join("nested-segment-x");
    }
    std::fs::create_dir_all(&deep).unwrap();
    let s = KvStore::open(&deep).unwrap();
    let k = key_from(b"p2");
    let payload = vec![9u8; INLINE_THRESHOLD + 1];
    s.put("ns", &k, &payload).unwrap();
    let got = s.get("ns", &k).unwrap().unwrap();
    assert_eq!(got, payload);
}

#[cfg(windows)]
#[test]
fn p2b_windows_spill_remove_and_clear_at_long_path() {
    // Lock in the long-path safety beyond plain put/get: remove + clear_namespace
    // both reach into spill_path() too. Build a path whose final spill file
    // strictly exceeds the 260-char `MAX_PATH` ceiling, then exercise every
    // code path that joins off the store root.
    let base = tempfile::tempdir().unwrap();
    let mut deep = base.path().to_path_buf();
    while deep.to_string_lossy().len() < 220 {
        deep = deep.join("very-long-segment");
    }
    std::fs::create_dir_all(&deep).unwrap();
    let s = KvStore::open(&deep).unwrap();

    // Sanity: we want the spill path to definitely exceed legacy MAX_PATH so
    // the test would actually catch a regression of the `\\?\` fix.
    let k = key_from(b"p2b");
    let probe = deep
        .join("kv")
        .join("ns")
        .join(format!("{}.bin", k.to_hex()));
    assert!(
        probe.to_string_lossy().len() > 260,
        "test setup must produce a path >260 chars, got {} chars",
        probe.to_string_lossy().len()
    );

    let payload = vec![7u8; INLINE_THRESHOLD + 8];
    s.put("ns", &k, &payload).unwrap();
    let got = s.get("ns", &k).unwrap().unwrap();
    assert_eq!(got, payload);

    // remove must succeed on the long path (touches spill_path()).
    s.remove("ns", &k).unwrap();
    assert!(s.get("ns", &k).unwrap().is_none());

    // clear_namespace removes the entire <ns> dir under kv/ — also a long path.
    let k2 = key_from(b"p2b-2");
    s.put("ns", &k2, &vec![3u8; INLINE_THRESHOLD + 3]).unwrap();
    s.clear_namespace("ns").unwrap();
    assert!(s.get("ns", &k2).unwrap().is_none());
}

#[cfg(unix)]
#[test]
fn p4_symlinked_store_dir() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("real");
    std::fs::create_dir(&target).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let s = KvStore::open(&link).unwrap();
    let k = key_from(b"p4");
    s.put("ns", &k, &vec![1u8; INLINE_THRESHOLD + 1]).unwrap();
    // Target should now contain the spill file.
    assert!(target
        .join("kv")
        .join("ns")
        .join(format!("{}.bin", k.to_hex()))
        .exists());
    let got = s.get("ns", &k).unwrap().unwrap();
    assert_eq!(got, vec![1u8; INLINE_THRESHOLD + 1]);
}

#[cfg(unix)]
#[test]
fn p5_readonly_dir_put_fails_io() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let s = KvStore::open(dir.path()).unwrap();
    // Make `kv/` un-writable so the spill write path fails.
    let kv_dir = dir.path().join("kv");
    std::fs::create_dir_all(&kv_dir).unwrap();
    let mut perms = std::fs::metadata(&kv_dir).unwrap().permissions();
    perms.set_mode(0o555);
    std::fs::set_permissions(&kv_dir, perms.clone()).unwrap();

    let k = key_from(b"p5");
    let res = s.put("ns", &k, &vec![0u8; INLINE_THRESHOLD + 1]);
    // Reset permissions so TempDir cleanup can run regardless of result.
    let mut undo = perms.clone();
    undo.set_mode(0o755);
    std::fs::set_permissions(&kv_dir, undo).unwrap();

    match res {
        Err(KvError::Io(_)) => {}
        other => panic!("expected Io error, got {other:?}"),
    }
}

#[test]
fn p6_utf8_namespace_rejected_on_every_os() {
    let dir = tempfile::tempdir().unwrap();
    let s = store_in_dir(dir.path());
    let k = key_from(b"p6");
    assert!(matches!(
        s.put("中文", &k, b"v"),
        Err(KvError::BadNamespace)
    ));
    assert!(!is_valid_namespace("中文"));
}

// =====================================================================
// Input edge cases (I5)
// =====================================================================

// I5: value at MAX_VALUE_BYTES is accepted. Allocates 64 MiB and writes it,
// so we hide it behind #[ignore] / `./test --full`.
#[test]
#[ignore = "stress: 64 MiB allocation + spill"]
fn i5_value_at_max_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let s = store_in_dir(dir.path());
    let k = key_from(b"i5");
    let v = vec![1u8; MAX_VALUE_BYTES];
    let n = s.put("ns", &k, &v).unwrap();
    assert_eq!(n, MAX_VALUE_BYTES);
    let got = s.get("ns", &k).unwrap().unwrap();
    assert_eq!(got.len(), MAX_VALUE_BYTES);
    assert_eq!(got[0], 1);
    assert_eq!(got[MAX_VALUE_BYTES - 1], 1);
}
