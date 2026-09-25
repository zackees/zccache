//! zccache#1661 / #1660: bincode format, persisted wall-clock ages, TTL,
//! size budget, and no-panic failed saves.

use std::io::Cursor;
use std::time::Duration;

use bincode::Options;
use tempfile::TempDir;

use super::super::super::context::ContextKey;
use super::super::super::graph::DepGraph;
use super::super::super::snapshot::now_unix_ms;
use super::super::super::snapshot::{
    classify_load, load_from_file, load_from_file_with, save_to_file, save_to_file_with,
    ContextEntrySnapshot, DepGraphLoadOutcome, DepGraphSnapshot, FileEntrySnapshot,
    IncludeDirectiveSnapshot, LoadOptions, RustcEnvDepSnapshot, RustcExternSnapshot, SaveOptions,
    SnapshotError, SnapshotStats, DEPGRAPH_MAGIC, DEPGRAPH_VERSION, GC_TTL, HEADER_SIZE,
};
use super::{make_ctx, test_path};

const HOUR_MS: u64 = 3_600_000;
const DAY_MS: u64 = 24 * HOUR_MS;
/// Fixed wall-clock origin so tests never depend on the host clock.
const T0: u64 = 1_800_000_000_000;

fn codec() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .allow_trailing_bytes()
}

fn ctx_snap(i: u8, last_accessed_unix_ms: u64) -> ContextEntrySnapshot {
    ContextEntrySnapshot {
        context_key: [i; 32],
        logical_context_key: [i.wrapping_add(100); 32],
        key_root: Some(format!("/root{i}")),
        source_file: format!("/src/file{i}.cpp"),
        iquote: vec![format!("/iq{i}")],
        user: vec![format!("/user{i}")],
        system: vec![format!("/sys{i}")],
        after: vec![format!("/after{i}")],
        defines: (0..20).map(|d| format!("DEFINE_{d}_{i}=value")).collect(),
        flags: vec!["-O2".into(), format!("-flag{i}")],
        force_includes: vec![format!("/force{i}.h")],
        unknown_flags: vec![format!("-unknown{i}")],
        resolved_includes: vec![format!("/inc/{i}.h")],
        unresolved_includes: vec![format!("missing{i}.h")],
        has_computed_includes: true,
        artifact_key: Some([i.wrapping_add(7); 32]),
        last_file_hashes: vec![(format!("/inc/{i}.h"), [i.wrapping_add(9); 32])],
        rustc_externs: vec![RustcExternSnapshot {
            name: format!("dep{i}"),
            path: format!("/target/libdep{i}.rlib"),
        }],
        rustc_env_deps: vec![
            RustcEnvDepSnapshot {
                name: "CARGO_PKG_NAME".into(),
                value_hash: Some([3; 32]),
            },
            RustcEnvDepSnapshot {
                name: "UNSET".into(),
                value_hash: None,
            },
        ],
        state: 1,
        last_accessed_unix_ms,
    }
}

fn full_snapshot() -> DepGraphSnapshot {
    DepGraphSnapshot {
        files: vec![FileEntrySnapshot {
            path: "/inc/1.h".into(),
            includes: (0u8..5)
                .map(|kind| IncludeDirectiveSnapshot {
                    kind,
                    path: format!("h{kind}.h"),
                    line: u32::from(kind) + 1,
                })
                .collect(),
        }],
        contexts: vec![ctx_snap(1, T0), ctx_snap(2, T0 - HOUR_MS)],
        stats: SnapshotStats {
            saved_at_epoch_secs: T0 / 1000,
            file_count: 1,
            context_count: 2,
        },
    }
}

fn save_opts(now_unix_ms: u64) -> SaveOptions {
    SaveOptions {
        now_unix_ms,
        ..SaveOptions::default()
    }
}

fn load_opts(now_unix_ms: u64) -> LoadOptions {
    LoadOptions {
        now_unix_ms,
        ..LoadOptions::default()
    }
}

/// Every field survives a streaming `serialize_into` / `deserialize_from`.
#[test]
fn every_field_round_trips_through_streaming_bincode() {
    let snap = full_snapshot();
    let mut buf = Vec::new();
    codec().serialize_into(&mut buf, &snap).unwrap();
    let back: DepGraphSnapshot = codec().deserialize_from(Cursor::new(&buf)).unwrap();
    assert_eq!(format!("{snap:?}"), format!("{back:?}"));
}

/// Every field survives a save/load through the real file path.
#[test]
fn every_field_round_trips_through_the_file() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::from_snapshot(full_snapshot());
    save_to_file_with(&graph, &path, &save_opts(T0)).unwrap();

    let data = std::fs::read(&path).unwrap();
    assert_eq!(data[0..4], DEPGRAPH_MAGIC);
    let payload_len = u64::from_le_bytes(data[8..16].try_into().unwrap());
    assert_eq!(payload_len as usize, data.len() - HEADER_SIZE);
    let back: DepGraphSnapshot = codec()
        .deserialize_from(Cursor::new(&data[HEADER_SIZE..]))
        .unwrap();

    let mut contexts = back.contexts;
    contexts.sort_by_key(|c| c.context_key);
    let expected = full_snapshot().contexts;
    // Access times are wall-clock and persisted verbatim: exact equality.
    for (got, want) in contexts.iter().zip(expected.iter()) {
        assert_eq!(got.last_accessed_unix_ms, want.last_accessed_unix_ms);
        // Paths are normalized to the host separator on load (backslashes
        // on Windows), so compare with separators folded to '/'.
        let got_dbg = format!("{got:?}").replace("\\\\", "/");
        assert_eq!(got_dbg, format!("{want:?}"));
    }
    assert_eq!(contexts.len(), 2);
}

#[test]
fn truncated_file_is_err_not_panic() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::from_snapshot(full_snapshot());
    save_to_file_with(&graph, &path, &save_opts(T0)).unwrap();

    let data = std::fs::read(&path).unwrap();
    for cut in [
        HEADER_SIZE - 1,
        HEADER_SIZE + 1,
        data.len() / 2,
        data.len() - 1,
    ] {
        std::fs::write(&path, &data[..cut]).unwrap();
        assert!(load_from_file(&path).is_err(), "cut at {cut} must be Err");
        assert!(matches!(
            classify_load(&path),
            DepGraphLoadOutcome::Corrupt { .. }
        ));
    }
}

#[test]
fn garbage_payload_is_err_not_panic() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let garbage = vec![0xABu8; 4096];
    let mut data = Vec::new();
    data.extend_from_slice(&DEPGRAPH_MAGIC);
    data.extend_from_slice(&DEPGRAPH_VERSION.to_le_bytes());
    data.extend_from_slice(&(garbage.len() as u64).to_le_bytes());
    data.extend_from_slice(&garbage);
    std::fs::write(&path, &data).unwrap();

    match load_from_file(&path) {
        Err(SnapshotError::Corrupt(_)) => {}
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

/// A v7 (rkyv) snapshot is classified stale: one-time cold start.
#[test]
fn v7_rkyv_header_is_version_mismatch() {
    assert_eq!(DEPGRAPH_VERSION, 8);
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let mut data = Vec::new();
    data.extend_from_slice(&DEPGRAPH_MAGIC);
    data.extend_from_slice(&7u32.to_le_bytes());
    data.extend_from_slice(&64u64.to_le_bytes());
    data.extend_from_slice(&[0u8; 64]);
    std::fs::write(&path, &data).unwrap();

    match classify_load(&path) {
        DepGraphLoadOutcome::VersionMismatch {
            file_version: 7,
            expected_version: 8,
        } => {}
        other => panic!("expected VersionMismatch v7, got {other:?}"),
    }
}

/// RED before #1661: load re-stamped `Instant::now()`, so a context idle
/// past the TTL came back fresh on every restart and was never trimmed.
#[test]
fn restart_survives_trim_idle_context_is_dropped() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();
    graph.register(make_ctx("/src/idle.cpp"));
    let t = now_unix_ms();
    save_to_file_with(&graph, &path, &save_opts(t)).unwrap();

    let loaded = load_from_file_with(&path, &load_opts(t + 8 * DAY_MS)).unwrap();
    loaded.trim_at(GC_TTL, t + 8 * DAY_MS);
    assert_eq!(loaded.stats().context_count, 0);
    assert_eq!(loaded.stats().file_count, 0);
}

/// The restored access time is the persisted wall-clock time, so a `trim`
/// after load sees the full age.
#[test]
fn restored_age_is_visible_to_trim() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();
    graph.register(make_ctx("/src/a.cpp"));
    let t = now_unix_ms();
    save_to_file_with(&graph, &path, &save_opts(t)).unwrap();

    let later = t + 3 * DAY_MS;
    let loaded = load_from_file_with(&path, &load_opts(later)).unwrap();
    assert_eq!(loaded.stats().context_count, 1);
    assert_eq!(loaded.trim_at(Duration::from_secs(2 * 86_400), later), 1);
}

#[test]
fn recent_context_survives_load_and_trim() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/recent.cpp"));
    let t = now_unix_ms();
    save_to_file_with(&graph, &path, &save_opts(t)).unwrap();

    let loaded = load_from_file_with(&path, &load_opts(t + HOUR_MS)).unwrap();
    loaded.trim_at(GC_TTL, t + HOUR_MS);
    assert_eq!(loaded.stats().context_count, 1);
    assert!(loaded.get_state(&key).is_some());
}

/// Clock skew (stored time in the future) clamps to age zero, no panic.
#[test]
fn future_timestamp_is_clamped() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();
    graph.register(make_ctx("/src/skew.cpp"));
    let t = now_unix_ms();
    save_to_file_with(&graph, &path, &save_opts(t)).unwrap();

    // "now" a day before the stored access time.
    let loaded = load_from_file_with(&path, &load_opts(t - DAY_MS)).unwrap();
    assert_eq!(loaded.stats().context_count, 1);
    assert_eq!(loaded.trim_at(Duration::from_secs(60), t - DAY_MS), 0);
}

/// RED before the wall-clock fix: load rebuilt each age as
/// `Instant::now() - age`, and a monotonic clock cannot reach back before
/// boot, so any context older than machine uptime came back with age zero
/// and survived the TTL after every reboot.
#[test]
fn context_older_than_uptime_keeps_age_and_is_trimmed() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let t = now_unix_ms();
    let old = t - 30 * DAY_MS;
    let recent = t - HOUR_MS;
    let snap = DepGraphSnapshot {
        files: Vec::new(),
        contexts: vec![ctx_snap(1, old), ctx_snap(2, recent)],
        stats: SnapshotStats {
            saved_at_epoch_secs: t / 1000,
            file_count: 0,
            context_count: 2,
        },
    };
    // Save and load without the TTL filter so the age itself is observable.
    let keep_all = SaveOptions {
        now_unix_ms: t,
        ttl: Duration::from_secs(365 * 86_400),
        ..SaveOptions::default()
    };
    save_to_file_with(&DepGraph::from_snapshot(snap), &path, &keep_all).unwrap();

    // Load without the TTL filter so the age itself is observable.
    let opts = LoadOptions {
        now_unix_ms: t,
        ttl: Duration::from_secs(365 * 86_400),
    };
    let loaded = load_from_file_with(&path, &opts).unwrap();
    let mut ages: Vec<(u8, u64)> = loaded
        .to_snapshot_at(t)
        .contexts
        .iter()
        .map(|c| (c.context_key[0], t - c.last_accessed_unix_ms))
        .collect();
    ages.sort_unstable();
    assert_eq!(ages, vec![(1, 30 * DAY_MS), (2, HOUR_MS)]);

    assert_eq!(loaded.trim(GC_TTL), 1);
    assert!(loaded.get_state(&ContextKey::from_raw([1; 32])).is_none());
    assert!(loaded.get_state(&ContextKey::from_raw([2; 32])).is_some());
}

/// Over budget: least-recently-used contexts go, newest stay, file fits.
#[test]
fn size_budget_evicts_lru_and_keeps_newest() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let n: u8 = 40;
    let make_snapshot = || DepGraphSnapshot {
        files: Vec::new(),
        contexts: (0..n)
            .map(|i| ctx_snap(i, T0 - u64::from(n - i) * HOUR_MS))
            .collect(),
        stats: SnapshotStats {
            saved_at_epoch_secs: T0 / 1000,
            file_count: 0,
            context_count: u64::from(n),
        },
    };

    let unbounded = DepGraph::from_snapshot(make_snapshot());
    save_to_file_with(&unbounded, &path, &save_opts(T0)).unwrap();
    let full_len = std::fs::metadata(&path).unwrap().len();

    let budget = full_len / 2;
    let graph = DepGraph::from_snapshot(make_snapshot());
    let opts = SaveOptions {
        now_unix_ms: T0,
        budget_bytes: budget,
        ..SaveOptions::default()
    };
    save_to_file_with(&graph, &path, &opts).unwrap();

    let len = std::fs::metadata(&path).unwrap().len();
    assert!(len <= budget, "file {len} bytes exceeds budget {budget}");

    let loaded = load_from_file_with(&path, &load_opts(T0)).unwrap();
    let count = loaded.stats().context_count;
    assert!(count > 0 && count < usize::from(n));
    let newest = ContextKey::from_raw([n - 1; 32]);
    let oldest = ContextKey::from_raw([0; 32]);
    assert!(loaded.get_state(&newest).is_some(), "newest must be kept");
    assert!(
        loaded.get_state(&oldest).is_none(),
        "oldest must be evicted"
    );
    // The live graph was bounded too, so it does not regrow the snapshot.
    assert_eq!(graph.stats().context_count, count);
}

/// A failed save leaves the previous snapshot byte-identical, removes its
/// tmp file, and does not prevent the next save.
#[test]
fn failed_save_leaves_previous_snapshot_intact() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();
    graph.register(make_ctx("/src/a.cpp"));
    save_to_file(&graph, &path).unwrap();
    let before = std::fs::read(&path).unwrap();

    let later = graph.register(make_ctx("/src/b.cpp"));
    let failing = SaveOptions {
        fail_injection: true,
        ..SaveOptions::default()
    };
    assert!(save_to_file_with(&graph, &path, &failing).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "tmp files left: {leftovers:?}");

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();
    assert!(loaded.get_state(&later).is_some());
}
