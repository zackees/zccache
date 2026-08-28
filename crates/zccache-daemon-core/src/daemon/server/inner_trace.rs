//! Task-scoped compile-id plumbing for the `ZCCACHE_INNER_TRACE` sub-phase
//! trace (issue #940).
//!
//! `EmbeddedDaemon::compile` (the `ZccacheService::compile` entry) generates
//! a per-compile id and calls the pipeline through [`scope`]. Deep inside the
//! pipeline — at the hash/verify seam, the miss-path store, and the cached-hit
//! materialize — the timing seams call [`record_ns`] with a named sub-phase.
//! `record_ns` reads the current task's compile-id via the task-local and
//! forwards to [`crate::compile_trace::record`], which is itself a no-op unless
//! `ZCCACHE_INNER_TRACE` points at a writable file.
//!
//! ## Why a task-local instead of threading a `compile_id` argument
//!
//! The sub-phase seams live 3–4 calls deep (`handle_compile_ephemeral` →
//! `handle_compile` → `handle_compile_request` → `store_outcome`/`cached_hit`)
//! and are shared by both the embedded path and the IPC wrapper path. Threading
//! a `compile_id: &str` through every signature would churn ~12 test call sites
//! and both production callers for a diagnostic-only feature. A task-local
//! scopes the id to exactly the embedded compile future without touching any
//! signature: the seams read it when present and emit nothing otherwise, so the
//! IPC path (which does not open a scope) stays silent by construction.
//!
//! The foreground seams all run in the same task as the [`scope`] wrapper — the
//! only `tokio::spawn` in the compile path is the *background* artifact persist,
//! which is enqueued after `cache_store` timing is already recorded — so the
//! task-local is always in scope where [`record_ns`] is called.

tokio::task_local! {
    /// The current embedded compile's trace id. Present only inside a
    /// [`scope`] future; absent (and every [`record_ns`] a no-op) otherwise.
    pub(crate) static INNER_COMPILE_ID: String;
}

/// Record a sub-phase duration (in **nanoseconds**, converted to the trace's
/// microsecond unit) against the current embedded compile's trace id.
///
/// No-op when called outside a [`scope`] future (e.g. the IPC wrapper path) or
/// when `ZCCACHE_INNER_TRACE` is unset. Never blocks, allocates on the disabled
/// path, or perturbs the compile it measures.
pub(crate) fn record_ns(phase: &str, elapsed_ns: u64) {
    let _ = INNER_COMPILE_ID.try_with(|id| {
        crate::compile_trace::record(phase, elapsed_ns / 1_000, id);
    });
}

/// Record the dependency-graph state immediately after context registration.
///
/// The durable journal's `context_not_found` reason intentionally stays
/// compact. These categorical, opt-in trace markers distinguish a genuinely
/// new context from an equivalent worktree context and show the selected
/// entry's state without logging paths or compiler arguments.
pub(crate) fn record_context_registration(
    equivalent_root: bool,
    state: crate::depgraph::ContextState,
) {
    record_ns(context_registration_phase(equivalent_root, state), 0);
}

fn context_registration_phase(
    equivalent_root: bool,
    state: crate::depgraph::ContextState,
) -> &'static str {
    use crate::depgraph::ContextState::{Cold, Stale, Warm};
    match (equivalent_root, state) {
        (true, Cold) => "context_register_equivalent_cold",
        (true, Warm) => "context_register_equivalent_warm",
        (true, Stale) => "context_register_equivalent_stale",
        (false, Cold) => "context_register_no_cross_root_cold",
        (false, Warm) => "context_register_no_cross_root_warm",
        (false, Stale) => "context_register_no_cross_root_stale",
    }
}

/// Run `fut` with `id` installed as the current embedded compile's trace id so
/// sub-phase [`record_ns`] calls emitted within it attribute to `id`.
pub(crate) async fn scope<F>(id: String, fut: F) -> F::Output
where
    F: std::future::Future,
{
    INNER_COMPILE_ID.scope(id, fut).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn try_with_is_err_outside_scope() {
        // The gating contract: with no scope open, the task-local read fails
        // and `record_ns` therefore emits nothing (and must not panic).
        assert!(INNER_COMPILE_ID.try_with(|_| ()).is_err());
        record_ns("input_hash", 5_000);
    }

    #[tokio::test]
    async fn scope_installs_current_id() {
        let seen = scope("zbeef".to_owned(), async {
            INNER_COMPILE_ID.with(|id| id.clone())
        })
        .await;
        assert_eq!(seen, "zbeef");
    }

    #[tokio::test]
    async fn record_ns_is_reachable_inside_scope() {
        // Inside a scope the read succeeds, so `record_ns` reaches
        // `compile_trace::record` (which is itself a no-op unless the env var
        // is set). We assert the task-local is visible; the file-format wire
        // shape is covered by tests/perf/inner_trace_file_test.rs.
        scope("z00000001".to_owned(), async {
            assert_eq!(INNER_COMPILE_ID.with(|id| id.clone()), "z00000001");
            record_ns("cache_store", 12_000);
        })
        .await;
    }

    #[test]
    fn context_registration_markers_cover_every_state() {
        use crate::depgraph::ContextState::{Cold, Stale, Warm};
        assert_eq!(
            context_registration_phase(true, Warm),
            "context_register_equivalent_warm"
        );
        assert_eq!(
            context_registration_phase(true, Cold),
            "context_register_equivalent_cold"
        );
        assert_eq!(
            context_registration_phase(true, Stale),
            "context_register_equivalent_stale"
        );
        assert_eq!(
            context_registration_phase(false, Warm),
            "context_register_no_cross_root_warm"
        );
        assert_eq!(
            context_registration_phase(false, Cold),
            "context_register_no_cross_root_cold"
        );
        assert_eq!(
            context_registration_phase(false, Stale),
            "context_register_no_cross_root_stale"
        );
    }
}
