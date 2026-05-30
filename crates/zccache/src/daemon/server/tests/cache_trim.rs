//! Tests for the in-memory request / response / fast-hit cache trim
//! routines: age-based eviction, hard-cap clears, freshness checks, and
//! cross-root resolution of cached input paths.

use std::path::Path;

use super::super::*;

fn test_context_key(source: &str) -> ContextKey {
    CompileContext {
        source_file: source.into(),
        include_search: crate::depgraph::IncludeSearchPaths::default(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    }
    .context_key()
}

fn test_request_entry(cached_at: std::time::Instant) -> RequestCacheEntry {
    let context_key = test_context_key("/tmp/source.c");
    let source_path: NormalizedPath = "/tmp/source.c".into();
    let output_path: NormalizedPath = "/tmp/source.o".into();
    RequestCacheEntry {
        context_key,
        root: None,
        source_path: CachedRequestPath::capture(&source_path, None),
        output_path: CachedRequestPath::capture(&output_path, None),
        input_paths: vec![CachedRequestPath::capture(&source_path, None)],
        cross_root_shareable: false,
        cached_at,
    }
}

fn test_rsp_entry(cached_at: std::time::Instant) -> RspCacheEntry {
    RspCacheEntry {
        expanded: Vec::new(),
        dependencies: Vec::new(),
        cached_at,
    }
}

fn test_fast_hit_entry(cached_at: std::time::Instant) -> FastHitEntry {
    FastHitEntry {
        clock: Clock::ZERO,
        artifact_key_hex: "artifact".to_string(),
        cached_at,
    }
}

fn test_content_hash(index: usize) -> ContentHash {
    let mut bytes = [0; 32];
    bytes[..8].copy_from_slice(&(index as u64).to_le_bytes());
    ContentHash::from_bytes(bytes)
}

fn test_request_validation_key(index: usize, root: &Path) -> RequestValidationKey {
    RequestValidationKey {
        request_fp: test_content_hash(index),
        root: NormalizedPath::new(root),
    }
}

fn test_request_validation_entry(cached_at: std::time::Instant) -> RequestValidationEntry {
    RequestValidationEntry {
        artifact_key_hex: "artifact".to_string(),
        clock: Clock::ZERO,
        cached_at,
    }
}

#[test]
fn trim_request_cache_removes_old_entries() {
    let cache = DashMap::new();
    let max_age = std::time::Duration::from_millis(10);
    let old_at = std::time::Instant::now();
    let now = old_at.checked_add(max_age * 2).unwrap();
    cache.insert(ContentHash::from_bytes([2; 32]), test_request_entry(old_at));
    cache.insert(ContentHash::from_bytes([1; 32]), test_request_entry(now));

    let removed = trim_request_cache_at(&cache, max_age, now);

    assert_eq!(removed, 1);
    assert_eq!(cache.len(), 1);
    assert!(cache.contains_key(&ContentHash::from_bytes([1; 32])));
}

#[test]
fn cache_entry_freshness_uses_supplied_timestamp() {
    let max_age = std::time::Duration::from_millis(10);
    let cached_at = std::time::Instant::now();
    let compile_start = cached_at.checked_add(max_age / 2).unwrap();
    let later_check = cached_at.checked_add(max_age * 2).unwrap();

    assert!(cache_entry_fresh_at(compile_start, cached_at, max_age));
    assert!(!cache_entry_fresh_at(later_check, cached_at, max_age));
}

#[test]
fn trim_request_cache_keeps_future_entries() {
    let cache = DashMap::new();
    let max_age = std::time::Duration::from_millis(10);
    let now = std::time::Instant::now();
    let future = now.checked_add(max_age * 2).unwrap();
    cache.insert(ContentHash::from_bytes([1; 32]), test_request_entry(future));

    let removed = trim_request_cache_at(&cache, max_age, now);

    assert_eq!(removed, 0);
    assert_eq!(cache.len(), 1);
}

#[test]
fn trim_request_cache_clears_when_over_hard_cap() {
    let cache = DashMap::new();
    let now = std::time::Instant::now();
    for i in 0..=REQUEST_CACHE_MAX_ENTRIES {
        cache.insert(test_content_hash(i), test_request_entry(now));
    }

    let removed = trim_request_cache_at(&cache, EPHEMERAL_CACHE_MAX_AGE, now);

    assert_eq!(removed, REQUEST_CACHE_MAX_ENTRIES + 1);
    assert!(cache.is_empty());
}

#[test]
fn trim_request_validation_cache_removes_old_entries() {
    let cache = DashMap::new();
    let tmp = tempfile::tempdir().unwrap();
    let max_age = std::time::Duration::from_millis(10);
    let old_at = std::time::Instant::now();
    let now = old_at.checked_add(max_age * 2).unwrap();
    cache.insert(
        test_request_validation_key(1, &tmp.path().join("old-root")),
        test_request_validation_entry(old_at),
    );
    cache.insert(
        test_request_validation_key(2, &tmp.path().join("fresh-root")),
        test_request_validation_entry(now),
    );

    let removed = trim_request_validation_cache_at(&cache, max_age, now);

    assert_eq!(removed, 1);
    assert_eq!(cache.len(), 1);
    assert!(cache.contains_key(&test_request_validation_key(
        2,
        &tmp.path().join("fresh-root")
    )));
}

#[test]
fn trim_request_validation_cache_uses_its_own_larger_hard_cap_not_request_cache_max() {
    // #453: validation cache should have its own bound separate from the
    // request cache, sized larger (lighter per-entry → can hold more).
    // Filling it with REQUEST_CACHE_MAX_ENTRIES + 100 entries (4196) must
    // NOT trigger the hard-cap clear, because the validation cap is 8192.
    let cache = DashMap::new();
    let tmp = tempfile::tempdir().unwrap();
    let now = std::time::Instant::now();
    for i in 0..(REQUEST_CACHE_MAX_ENTRIES + 100) {
        cache.insert(
            test_request_validation_key(i, &tmp.path().join(format!("root-{i}"))),
            test_request_validation_entry(now),
        );
    }

    let removed = trim_request_validation_cache_at(&cache, EPHEMERAL_CACHE_MAX_AGE, now);

    assert_eq!(
        removed, 0,
        "validation cap is 8192, holding 4196 must not evict"
    );
    assert_eq!(cache.len(), REQUEST_CACHE_MAX_ENTRIES + 100);
    // Sanity: the new cap really is larger than the old shared one.
    const _: () = assert!(REQUEST_VALIDATION_CACHE_MAX_ENTRIES > REQUEST_CACHE_MAX_ENTRIES);
}

#[test]
fn trim_request_validation_cache_clears_when_over_validation_hard_cap() {
    // #453: when filled past the validation-specific cap (8192), the cache
    // is wiped just like request_cache is past its own cap.
    let cache = DashMap::new();
    let tmp = tempfile::tempdir().unwrap();
    let now = std::time::Instant::now();
    for i in 0..=REQUEST_VALIDATION_CACHE_MAX_ENTRIES {
        cache.insert(
            test_request_validation_key(i, &tmp.path().join(format!("root-{i}"))),
            test_request_validation_entry(now),
        );
    }

    let removed = trim_request_validation_cache_at(&cache, EPHEMERAL_CACHE_MAX_AGE, now);

    assert_eq!(removed, REQUEST_VALIDATION_CACHE_MAX_ENTRIES + 1);
    assert!(cache.is_empty());
}

#[test]
fn request_cache_resolved_inputs_requires_cross_root_shareable_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let root_a = tmp.path().join("workspace-a");
    let root_b = tmp.path().join("workspace-b");
    let source_a: NormalizedPath = root_a.join("src/main.cc").into();
    let header_a: NormalizedPath = root_a.join("include/common.h").into();
    let output_a: NormalizedPath = root_a.join("build/main.o").into();
    let entry = request_cache_entry(
        test_context_key("src/main.cc"),
        &source_a,
        &output_a,
        vec![source_a.clone(), header_a],
        Some(&NormalizedPath::new(&root_a)),
    );

    let resolved = request_cache_resolved_inputs(&entry, &NormalizedPath::new(&root_b)).unwrap();

    assert_eq!(
        resolved,
        vec![
            NormalizedPath::new(root_b.join("src/main.cc")),
            NormalizedPath::new(root_b.join("include/common.h")),
        ]
    );
}

#[test]
fn request_cache_inputs_fresh_since_uses_journal_tracking() {
    let journal = crate::fscache::ChangeJournal::new();
    let path: NormalizedPath = "/tmp/request-cache-input.cc".into();
    let clock = journal.current_clock();

    assert!(!request_cache_inputs_fresh_since(
        &journal,
        std::slice::from_ref(&path),
        clock
    ));

    journal.register(path.clone());
    let validation_clock = journal.current_clock();
    assert!(request_cache_inputs_fresh_since(
        &journal,
        std::slice::from_ref(&path),
        validation_clock
    ));

    journal.advance(vec![path.clone()]);
    assert!(!request_cache_inputs_fresh_since(
        &journal,
        std::slice::from_ref(&path),
        validation_clock
    ));
}

#[test]
fn trim_rsp_cache_removes_old_entries() {
    let cache = DashMap::new();
    let max_age = std::time::Duration::from_millis(10);
    let old_at = std::time::Instant::now();
    let now = old_at.checked_add(max_age * 2).unwrap();
    cache.insert(NormalizedPath::from("/tmp/old.rsp"), test_rsp_entry(old_at));
    cache.insert(NormalizedPath::from("/tmp/fresh.rsp"), test_rsp_entry(now));

    let removed = trim_rsp_cache_at(&cache, max_age, now);

    assert_eq!(removed, 1);
    assert_eq!(cache.len(), 1);
    assert!(cache.contains_key(&NormalizedPath::from("/tmp/fresh.rsp")));
}

#[test]
fn trim_rsp_cache_keeps_future_entries() {
    let cache = DashMap::new();
    let max_age = std::time::Duration::from_millis(10);
    let now = std::time::Instant::now();
    let future = now.checked_add(max_age * 2).unwrap();
    cache.insert(
        NormalizedPath::from("/tmp/future.rsp"),
        test_rsp_entry(future),
    );

    let removed = trim_rsp_cache_at(&cache, max_age, now);

    assert_eq!(removed, 0);
    assert_eq!(cache.len(), 1);
}

#[test]
fn trim_rsp_cache_clears_when_over_hard_cap() {
    let cache = DashMap::new();
    let now = std::time::Instant::now();
    for i in 0..=RSP_CACHE_MAX_ENTRIES {
        cache.insert(
            NormalizedPath::from(format!("/tmp/args{i}.rsp")),
            test_rsp_entry(now),
        );
    }

    let removed = trim_rsp_cache_at(&cache, EPHEMERAL_CACHE_MAX_AGE, now);

    assert_eq!(removed, RSP_CACHE_MAX_ENTRIES + 1);
    assert!(cache.is_empty());
}

#[test]
fn trim_fast_hit_cache_removes_old_entries() {
    let cache = DashMap::new();
    let max_age = std::time::Duration::from_millis(10);
    let old_at = std::time::Instant::now();
    let now = old_at.checked_add(max_age * 2).unwrap();
    let old_key = test_context_key("/tmp/old.c");
    let fresh_key = test_context_key("/tmp/fresh.c");
    cache.insert(old_key, test_fast_hit_entry(old_at));
    cache.insert(fresh_key, test_fast_hit_entry(now));

    let removed = trim_fast_hit_cache_at(&cache, max_age, now);

    assert_eq!(removed, 1);
    assert_eq!(cache.len(), 1);
    assert!(cache.contains_key(&fresh_key));
}

#[test]
fn trim_fast_hit_cache_keeps_future_entries() {
    let cache = DashMap::new();
    let max_age = std::time::Duration::from_millis(10);
    let now = std::time::Instant::now();
    let future = now.checked_add(max_age * 2).unwrap();
    let key = test_context_key("/tmp/future.c");
    cache.insert(key, test_fast_hit_entry(future));

    let removed = trim_fast_hit_cache_at(&cache, max_age, now);

    assert_eq!(removed, 0);
    assert_eq!(cache.len(), 1);
}
