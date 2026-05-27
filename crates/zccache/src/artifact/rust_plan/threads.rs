//! Tar/thread count resolution for Rust artifact bundle save operations.

pub(super) const DEFAULT_RUST_PLAN_TAR_THREADS_CAP: usize = 8;
/// Hard upper bound regardless of caller request â€” protects small runners from
/// per-thread buffer blowup if someone passes a huge value.
pub(super) const MAX_RUST_PLAN_TAR_THREADS: usize = 64;
pub fn resolve_rust_plan_tar_threads() -> usize {
    let raw = std::env::var("ZCCACHE_RUST_PLAN_TAR_THREADS")
        .ok()
        .or_else(|| std::env::var("SOLDR_TARGET_CACHE_TAR_THREADS").ok());
    parse_rust_plan_tar_threads(raw.as_deref())
}

pub(super) fn parse_rust_plan_tar_threads(raw: Option<&str>) -> usize {
    let trimmed = raw.map(str::trim).filter(|s| !s.is_empty());
    match trimmed {
        None => default_rust_plan_tar_threads(),
        Some(s) if s.eq_ignore_ascii_case("auto") => default_rust_plan_tar_threads(),
        Some(s) => match s.parse::<usize>() {
            Ok(0) => default_rust_plan_tar_threads(),
            Ok(n) => n.min(MAX_RUST_PLAN_TAR_THREADS),
            Err(_) => default_rust_plan_tar_threads(),
        },
    }
}

pub(super) fn default_rust_plan_tar_threads() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .min(DEFAULT_RUST_PLAN_TAR_THREADS_CAP)
}
