//! Time-based trimming helpers for ephemeral daemon caches.
//!
//! Every cache entry carries a `cached_at: Instant`; these helpers retain
//! only entries within their configured `max_age` and additionally enforce
//! per-cache size ceilings on the request/rsp caches.

use super::*;

/// Remove fast-hit cache entries older than `max_age`. Returns entries removed.
pub(crate) fn trim_fast_hit_cache(
    cache: &DashMap<ContextKey, FastHitEntry>,
    max_age: Duration,
) -> usize {
    trim_fast_hit_cache_at(cache, max_age, Instant::now())
}

pub(super) fn cache_age_at(now: Instant, cached_at: Instant) -> Duration {
    now.saturating_duration_since(cached_at)
}

pub(super) fn cache_entry_expired_at(now: Instant, cached_at: Instant, max_age: Duration) -> bool {
    cache_age_at(now, cached_at) > max_age
}

pub(super) fn cache_entry_fresh_at(now: Instant, cached_at: Instant, max_age: Duration) -> bool {
    cache_age_at(now, cached_at) < max_age
}

pub(super) fn trim_fast_hit_cache_at(
    cache: &DashMap<ContextKey, FastHitEntry>,
    max_age: Duration,
    now: Instant,
) -> usize {
    let mut removed = 0;
    cache.retain(|_, entry| {
        if cache_entry_expired_at(now, entry.cached_at, max_age) {
            removed += 1;
            false
        } else {
            true
        }
    });
    removed
}

pub(super) fn trim_request_cache(
    cache: &DashMap<ContentHash, RequestCacheEntry>,
    max_age: Duration,
) -> usize {
    trim_request_cache_at(cache, max_age, Instant::now())
}

pub(super) fn trim_request_validation_cache(
    cache: &DashMap<RequestValidationKey, RequestValidationEntry>,
    max_age: Duration,
) -> usize {
    trim_request_validation_cache_at(cache, max_age, Instant::now())
}

pub(super) fn trim_request_cache_at(
    cache: &DashMap<ContentHash, RequestCacheEntry>,
    max_age: Duration,
    now: Instant,
) -> usize {
    let mut removed = 0;
    cache.retain(|_, entry| {
        if cache_entry_expired_at(now, entry.cached_at, max_age) {
            removed += 1;
            false
        } else {
            true
        }
    });
    if cache.len() > REQUEST_CACHE_MAX_ENTRIES {
        let remaining = cache.len();
        cache.clear();
        removed += remaining;
    }
    removed
}

pub(super) fn trim_request_validation_cache_at(
    cache: &DashMap<RequestValidationKey, RequestValidationEntry>,
    max_age: Duration,
    now: Instant,
) -> usize {
    let mut removed = 0;
    cache.retain(|_, entry| {
        if cache_entry_expired_at(now, entry.cached_at, max_age) {
            removed += 1;
            false
        } else {
            true
        }
    });
    if cache.len() > REQUEST_VALIDATION_CACHE_MAX_ENTRIES {
        let remaining = cache.len();
        cache.clear();
        removed += remaining;
    }
    removed
}

pub(super) fn trim_rsp_cache(
    cache: &DashMap<NormalizedPath, RspCacheEntry>,
    max_age: Duration,
) -> usize {
    trim_rsp_cache_at(cache, max_age, Instant::now())
}

pub(super) fn trim_rsp_cache_at(
    cache: &DashMap<NormalizedPath, RspCacheEntry>,
    max_age: Duration,
    now: Instant,
) -> usize {
    let mut removed = 0;
    cache.retain(|_, entry| {
        if cache_entry_expired_at(now, entry.cached_at, max_age) {
            removed += 1;
            false
        } else {
            true
        }
    });
    if cache.len() > RSP_CACHE_MAX_ENTRIES {
        let remaining = cache.len();
        cache.clear();
        removed += remaining;
    }
    removed
}

pub(super) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
