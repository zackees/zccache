//! Issue #813 / #816 — global compile-concurrency cap.
//!
//! Wraps daemon-spawned compiler children in a single tokio semaphore
//! so the box can't be saturated by M cargo invocations each asking
//! for `num_cpus` rustcs (the M × N explosion described in #812 +
//! #816). Every compile request handler acquires a permit before
//! spawning the compiler and holds it for the spawn's lifetime.
//!
//! ## Cap resolution
//!
//! Per-issue #816 design:
//!
//! - `ZCCACHE_MAX_PARALLEL_COMPILES=<N>` (N > 0) → use N as the cap.
//! - `ZCCACHE_MAX_PARALLEL_COMPILES=0` or `=unlimited` → no cap;
//!   `resolve_pool` returns `None` and the daemon falls back to
//!   today's unbounded behavior.
//! - Unset + interactive host → `max(1, num_cpus - 1)`.
//! - Unset + CI host (per [`crate::daemon::process::is_ci_host`]) →
//!   `num_cpus`.
//!
//! ## Why in-process semaphore, not GNU make jobserver pipes
//!
//! The daemon serves every compile request from every client. An
//! in-process semaphore on the daemon side gives global cross-client
//! coordination "for free" — every request, regardless of which cargo
//! invocation sent it, queues at the same gate. No IPC, no pipe
//! plumbing, no client cooperation required.
//!
//! The [`crate::daemon::jobserver::JobserverPool`] primitive that
//! shipped in sub-task #815 is for the *cargo-side* coordination
//! (preventing cargo from flooding the daemon with N requests per
//! invocation). That requires soldr-side env injection and is its own
//! sub-task. The in-process semaphore here is enough to deliver the
//! immediate user-visible win: heavy children are bounded by box
//! capacity, not by `M × N`.
//!
//! ## Logging contract (sub-task #5 per the meta)
//!
//! Two structured events are emitted around every gated compile:
//!
//! - `compile_start`: `{ event="compile_start", lineage, capacity,
//!   available_before, granted_at_ns }`
//! - `compile_end`: `{ event="compile_end", lineage, duration_ns,
//!   exit_code }`
//!
//! Tests can verify the cap holds by parsing the log and asserting
//! no two `compile_start`/`compile_end` intervals overlap when
//! capacity is 1.

use std::sync::Arc;

use tokio::sync::Semaphore;

const MAX_PARALLEL_ENV: &str = "ZCCACHE_MAX_PARALLEL_COMPILES";

/// Resolve the compile-concurrency cap for this daemon instance.
///
/// `is_ci` should come from [`crate::daemon::process::is_ci_host`].
/// Returns `None` when the user explicitly opted out via
/// `ZCCACHE_MAX_PARALLEL_COMPILES=0` (or `=unlimited`).
pub(super) fn resolve_pool(is_ci: bool) -> Option<Arc<Semaphore>> {
    resolve_pool_with_env(is_ci, |name| std::env::var(name).ok(), num_cpus_default())
}

/// Testable variant of [`resolve_pool`] — caller supplies the env
/// lookup and the platform `num_cpus` value.
pub(super) fn resolve_pool_with_env<F>(
    is_ci: bool,
    env_lookup: F,
    num_cpus: usize,
) -> Option<Arc<Semaphore>>
where
    F: Fn(&str) -> Option<String>,
{
    let cap = match env_lookup(MAX_PARALLEL_ENV) {
        Some(raw) => match parse_override(&raw) {
            CapOverride::Unlimited => return None,
            CapOverride::Explicit(n) => n,
            CapOverride::Invalid => {
                tracing::warn!(
                    env = MAX_PARALLEL_ENV,
                    value = raw,
                    "invalid {MAX_PARALLEL_ENV}; falling back to default"
                );
                default_cap(is_ci, num_cpus)
            }
        },
        None => default_cap(is_ci, num_cpus),
    };
    Some(Arc::new(Semaphore::new(cap)))
}

#[derive(Debug, PartialEq, Eq)]
enum CapOverride {
    Unlimited,
    Explicit(usize),
    Invalid,
}

fn parse_override(raw: &str) -> CapOverride {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("unlimited") || trimmed == "0" {
        return CapOverride::Unlimited;
    }
    match trimmed.parse::<usize>() {
        Ok(n) => CapOverride::Explicit(n),
        Err(_) => CapOverride::Invalid,
    }
}

fn default_cap(is_ci: bool, num_cpus: usize) -> usize {
    if is_ci {
        num_cpus.max(1)
    } else {
        num_cpus.saturating_sub(1).max(1)
    }
}

fn num_cpus_default() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn fixed_env(val: &'static str) -> impl Fn(&str) -> Option<String> {
        move |name: &str| {
            if name == MAX_PARALLEL_ENV {
                Some(val.to_string())
            } else {
                None
            }
        }
    }

    #[test]
    fn unset_env_interactive_defaults_to_num_cpus_minus_one() {
        let pool = resolve_pool_with_env(false, no_env, 16);
        let sem = pool.expect("pool should exist when not explicitly disabled");
        assert_eq!(
            sem.available_permits(),
            15,
            "interactive cap = num_cpus - 1"
        );
    }

    #[test]
    fn unset_env_interactive_floors_at_one_for_single_core() {
        let pool = resolve_pool_with_env(false, no_env, 1);
        let sem = pool.expect("pool should exist");
        assert_eq!(sem.available_permits(), 1, "single-core box floors to 1");
    }

    #[test]
    fn unset_env_ci_defaults_to_num_cpus() {
        let pool = resolve_pool_with_env(true, no_env, 8);
        let sem = pool.expect("pool should exist");
        assert_eq!(sem.available_permits(), 8, "CI cap = num_cpus");
    }

    #[test]
    fn explicit_override_wins() {
        let pool = resolve_pool_with_env(false, fixed_env("4"), 64);
        let sem = pool.expect("pool should exist");
        assert_eq!(sem.available_permits(), 4);
    }

    #[test]
    fn explicit_override_wins_on_ci_too() {
        let pool = resolve_pool_with_env(true, fixed_env("2"), 64);
        let sem = pool.expect("pool should exist");
        assert_eq!(sem.available_permits(), 2);
    }

    #[test]
    fn zero_disables_cap() {
        let pool = resolve_pool_with_env(false, fixed_env("0"), 16);
        assert!(pool.is_none(), "0 must opt out of the cap");
    }

    #[test]
    fn unlimited_keyword_disables_cap() {
        let pool = resolve_pool_with_env(false, fixed_env("unlimited"), 16);
        assert!(pool.is_none());
        let pool = resolve_pool_with_env(true, fixed_env("UNLIMITED"), 16);
        assert!(pool.is_none(), "case-insensitive");
    }

    #[test]
    fn invalid_value_falls_back_to_default() {
        let pool = resolve_pool_with_env(false, fixed_env("abc"), 8);
        let sem = pool.expect("should still create a pool with the default");
        assert_eq!(sem.available_permits(), 7);
    }

    #[test]
    fn parse_override_classifies() {
        assert_eq!(parse_override("0"), CapOverride::Unlimited);
        assert_eq!(parse_override("unlimited"), CapOverride::Unlimited);
        assert_eq!(parse_override("  Unlimited "), CapOverride::Unlimited);
        assert_eq!(parse_override("12"), CapOverride::Explicit(12));
        assert_eq!(parse_override("not-a-number"), CapOverride::Invalid);
    }

    #[test]
    fn default_cap_arithmetic() {
        assert_eq!(default_cap(true, 16), 16);
        assert_eq!(default_cap(false, 16), 15);
        assert_eq!(default_cap(false, 1), 1, "floor");
        assert_eq!(default_cap(true, 1), 1);
        assert_eq!(default_cap(false, 0), 1, "saturate");
    }
}
