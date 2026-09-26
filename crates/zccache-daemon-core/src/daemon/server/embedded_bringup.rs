//! Timed, logged embedded bring-up (#1652).
//!
//! fbuild saw 12-47 s between "system include cache restored" and "zccache
//! backend ready" with nothing logged in between, and its client gave up on a
//! daemon that was still starting. Every bring-up phase now logs its
//! `elapsed_ns`, the whole sequence lands in one durable `embedded_bringup`
//! lifecycle event, and the depgraph load (the unbounded phase: it scales
//! with every context the host ever compiled) runs after readiness. Compiles
//! wait for it through `wait_for_startup_depgraph_load`, exactly as the
//! standalone daemon's do (#784); status and ping answer at once.

use super::*;

/// Accumulates per-phase wall time for one bring-up.
pub(super) struct BringupTimer {
    started: std::time::Instant,
    phases_ns: Vec<(&'static str, u64)>,
}

impl BringupTimer {
    pub(super) fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
            phases_ns: Vec::new(),
        }
    }

    /// Run one phase, logging and recording how long it took.
    pub(super) async fn phase<R>(
        &mut self,
        name: &'static str,
        work: impl std::future::Future<Output = R>,
    ) -> R {
        let started = std::time::Instant::now();
        let result = work.await;
        let elapsed_ns = started.elapsed().as_nanos() as u64;
        tracing::info!(phase = name, elapsed_ns, "embedded bring-up phase");
        self.phases_ns.push((name, elapsed_ns));
        result
    }

    /// Log readiness and write the durable bring-up record.
    pub(super) fn ready(self, state: &SharedState) {
        let ready_ns = self.started.elapsed().as_nanos() as u64;
        let phases: serde_json::Map<String, serde_json::Value> = self
            .phases_ns
            .iter()
            .map(|(name, ns)| ((*name).to_owned(), serde_json::json!(ns)))
            .collect();
        tracing::info!(ready_ns, ?phases, "embedded bring-up ready");
        crate::core::lifecycle::write_event_in_cache_root(
            state.cache_dir.as_path(),
            crate::core::lifecycle::EVENT_EMBEDDED_BRINGUP,
            serde_json::json!({
                "ready_ns": ready_ns,
                "phases_ns": phases,
                "depgraph_load": "background",
            }),
        );
    }
}

/// Wait (bounded) for the startup depgraph load to install.
///
/// A flush that saved before the load installed would write the empty default
/// graph over the snapshot the load is about to read. Returns whether the load
/// completed; the caller skips the save otherwise, leaving the file intact.
pub(super) async fn await_depgraph_load(state: &SharedState) -> bool {
    const WAIT: std::time::Duration = std::time::Duration::from_secs(60);
    let deadline = kernal_api::async_engine::sleep(WAIT);
    let mut deadline = std::pin::pin!(deadline);
    loop {
        let notified = state.dep_graph_load_notify.notified();
        if state.dep_graph_load_complete.load(Ordering::Acquire) {
            return true;
        }
        let mut notified = std::pin::pin!(notified);
        if let kernal_api::async_engine::FairRace2::Second(()) =
            kernal_api::fair_race!((notified.as_mut()), (deadline.as_mut())).await
        {
            return state.dep_graph_load_complete.load(Ordering::Acquire);
        }
    }
}

/// Restore the metadata, compiler-hash and system-include snapshots, one timed
/// phase each. Each loader marks its cache loaded even when the file is
/// missing or unreadable, so the shutdown save knows the in-memory state is
/// canonical.
pub(super) async fn load_persisted_caches(
    timer: &mut BringupTimer,
    state: &Arc<SharedState>,
    runtime_handle: Option<&kernal_api::async_engine::RuntimeHandle>,
) {
    let metadata_state = Arc::clone(state);
    let metadata_path = state.metadata_path.clone();
    let _ = timer
        .phase(
            "metadata",
            super::embedded::launch_embedded_blocking(runtime_handle, move || {
                match crate::fscache::MetadataCache::load_from_disk(metadata_path.as_path()) {
                    Ok(loaded) => metadata_state.cache_system.metadata().merge_from(loaded),
                    Err(e) => tracing::warn!(
                        path = %metadata_path.display(),
                        "failed to load embedded metadata cache, starting empty: {e}"
                    ),
                }
                metadata_state
                    .metadata_cache_loaded
                    .store(true, Ordering::Release);
            }),
        )
        .await;

    let compiler_state = Arc::clone(state);
    let compiler_hash_cache_path = state.compiler_hash_cache_path.clone();
    let _ = timer
        .phase(
            "compiler_hash",
            super::embedded::launch_embedded_blocking(runtime_handle, move || {
                match CompilerHashCache::load_from_disk(compiler_hash_cache_path.as_path()) {
                    Ok(loaded) => compiler_state.compiler_hash_cache.merge_from(loaded),
                    Err(e) => tracing::warn!(
                        path = %compiler_hash_cache_path.display(),
                        "failed to load embedded compiler hash cache, starting empty: {e}"
                    ),
                }
                compiler_state
                    .compiler_hash_cache_loaded
                    .store(true, Ordering::Release);
            }),
        )
        .await;

    let includes_state = Arc::clone(state);
    let system_includes_cache_path = state.system_includes_cache_path.clone();
    let _ = timer
        .phase(
            "system_includes",
            super::embedded::launch_embedded_blocking(runtime_handle, move || {
                match crate::depgraph::SystemIncludeCache::load_from_disk(
                    system_includes_cache_path.as_path(),
                ) {
                    Ok(loaded) => {
                        let mut live = includes_state.system_includes.blocking_lock();
                        live.merge_from(loaded);
                    }
                    Err(e) => tracing::warn!(
                        path = %system_includes_cache_path.display(),
                        "failed to load embedded system include cache, starting empty: {e}"
                    ),
                }
                includes_state
                    .system_includes_loaded
                    .store(true, Ordering::Release);
            }),
        )
        .await;
}

/// Cache roots whose background depgraph load must not start yet (tests only).
///
/// A load whose snapshot path lies under a held root waits until the test
/// releases it, so a test can observe "ready, load still pending" without
/// racing a wall-clock delay (the earlier fixed 1.5 s sleep flaked on a loaded
/// arm runner whose bring-up alone took 2.3 s). Keyed by root so parallel
/// tests never hold each other's loads. The wait is capped so a test that
/// forgets to release cannot hang the suite.
#[cfg(test)]
pub(super) static TEST_DEPGRAPH_LOAD_HOLDS: std::sync::Mutex<Vec<std::path::PathBuf>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn wait_while_test_holds_load(depgraph_path: &Path) {
    let held = || {
        TEST_DEPGRAPH_LOAD_HOLDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|root| depgraph_path.starts_with(root))
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while held() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Load `depgraph.bin` off the readiness path.
///
/// `dep_graph_load_complete` is already `false` (armed in
/// `start_with_maintenance`), so compiles wait for this install and the
/// periodic save loop will not overwrite the on-disk graph with the empty
/// default meanwhile. The load runs under the depgraph persistence lock, like
/// every other reader and writer of the snapshot file.
pub(super) fn spawn_depgraph_load(
    state: Arc<SharedState>,
    depgraph_path: crate::core::NormalizedPath,
    runtime_handle: Option<&kernal_api::async_engine::RuntimeHandle>,
) -> kernal_api::async_engine::Task<()> {
    let load = move || {
        let started = std::time::Instant::now();
        #[cfg(test)]
        wait_while_test_holds_load(depgraph_path.as_path());
        let contexts = state.with_depgraph_snapshot(|_| {
            let outcome = crate::depgraph::classify_load(depgraph_path.as_path());
            let warning = outcome.warning(depgraph_path.as_path());
            let contexts = if let Some(graph) = outcome.into_graph() {
                let contexts = graph.stats().context_count;
                state.dep_graph.store(Arc::new(graph));
                state.dep_graph_persisted.store(true, Ordering::Release);
                contexts
            } else {
                0
            };
            if let Some(warning) = warning {
                let mut guard = state
                    .depgraph_load_warning
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *guard = Some(warning);
            }
            contexts
        });
        state.dep_graph_load_complete.store(true, Ordering::Release);
        state.dep_graph_load_notify.notify_waiters();
        let elapsed_ns = started.elapsed().as_nanos() as u64;
        tracing::info!(contexts, elapsed_ns, "embedded depgraph loaded");
        crate::core::lifecycle::write_event_in_cache_root(
            state.cache_dir.as_path(),
            crate::core::lifecycle::EVENT_EMBEDDED_DEPGRAPH_LOADED,
            serde_json::json!({ "contexts": contexts, "elapsed_ns": elapsed_ns }),
        );
    };
    match runtime_handle {
        Some(handle) => handle.launch_blocking(load),
        None => kernal_api::async_engine::launch_blocking(load),
    }
}
