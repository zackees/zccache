//! System include discovery + initial watch for the compile pipeline.
//!
//! Discovery is per-compiler-path memoized in `state.system_includes`. This
//! module is the only caller of the discovery helper plus the post-discovery
//! `watch_directories` for the include roots.
//!
//! ## Two-level cache (L1 in-RAM, L2 on-disk — ISSUE-201)
//!
//! The in-memory `Mutex<SystemIncludeCache>` on `SharedState` is the L1
//! fast path. The on-disk snapshot at `state.system_includes_cache_path`
//! is the L2 — loaded once at startup via `SystemIncludesLoader`
//! (issue #784 phase 2c) and previously persisted only at graceful
//! shutdown. ISSUE-201 closes the SIGKILL gap: on every actual L1
//! insert (not on hits), we clone the cache under the lock, drop the
//! lock, and spawn a `tokio::task::spawn_blocking` to call
//! `SystemIncludeCache::save_to_disk` so the L2 stays in lock-step with
//! the L1 without blocking the request thread on disk I/O. The
//! `state.system_includes_loaded` gate prevents write-through from
//! racing the background loader and clobbering the loaded-from-disk
//! superset with a fresh-daemon subset. Stat-verify on every L1 / L2
//! lookup keeps the cache invalidating itself if the compiler binary
//! changes mtime or size in-place (apt upgrade, brew upgrade, etc.).

use super::super::super::*;

const SYSTEM_INCLUDE_DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub(super) struct SystemIncludesOutcome {
    pub(super) includes: Vec<NormalizedPath>,
    pub(super) system_includes_ns: u64,
    pub(super) system_watch_ns: u64,
}

/// Discover system include directories for `compiler` and register them with
/// the watcher. `want_rust_miss_profile` gates per-phase clock reads so warm
/// hits don't pay the timing tax. Returns the discovered include paths plus
/// the phase ns counters (zero when the gate is off).
pub(super) async fn discover_system_includes(
    state: &SharedState,
    compiler: &NormalizedPath,
    lineage: &crate::daemon::lineage::Lineage,
    compiler_priority: CompilePriority,
    want_rust_miss_profile: bool,
) -> SystemIncludesOutcome {
    // Discover system includes for this compiler (cached per compiler path).
    //
    // Issue #517: skip discovery entirely for the rust toolchain. The
    // discovery args (`-v -E -x c++ NUL`) are C/C++-preprocessor flags;
    // rustc / clippy-driver / rustfmt do not understand them and do not have
    // a notion of system includes anyway. Spawning rustc just to capture an
    // error contributes ~30-50 ms (Linux) on every first-after-clear rust
    // compile, which is the dominant share of the 91 ms `rust-workspace-link
    // Cold` overhead measured in `benchmark-stats/latest.json`. Short-circuit
    // to an empty include list — `watch_directories(&[])` is a fast no-op.
    let t_system_includes = want_rust_miss_profile.then(std::time::Instant::now);
    let compiler_family = crate::compiler::detect_family(&compiler.to_string_lossy());
    let needs_discovery = compiler_family.needs_system_include_discovery();
    let system_includes = if !needs_discovery {
        Vec::new()
    } else {
        // Issue #541 option B: for the clang family the daemon prefers
        // `clang -###` discovery (~3-5 ms) over the slower `-v -E`
        // (~30-50 ms). Clang's `-###` prints the cc1 command line with
        // every `-internal-isystem` / `-internal-externc-isystem`
        // argument WITHOUT spawning the real preprocessor, so the
        // parser can pull include paths straight out of the printed
        // argv. Gcc / Msvc don't emit this format; they keep using
        // the slow path.
        let use_fast = matches!(compiler_family, crate::compiler::CompilerFamily::Clang);
        let cached = {
            let cache = state.system_includes.lock().await;
            cache.get(compiler).map(|paths| paths.to_vec())
        };
        if let Some(paths) = cached {
            paths
        } else {
            let discovered =
                discover_system_include_paths(compiler, lineage, compiler_priority, use_fast).await;
            // Inserted-this-call flag drives a single async write-through
            // snapshot after we drop the cache lock. We never block the
            // request thread on disk I/O — the snapshot runs in a
            // `tokio::task::spawn_blocking` and any failure is logged but
            // does not surface to the compile request (the in-memory L1
            // entry is still authoritative for this daemon's lifetime).
            let (resolved, inserted_snapshot) = {
                let mut cache = state.system_includes.lock().await;
                if let Some(paths) = cache.get(compiler) {
                    (paths.to_vec(), None)
                } else if let Some(discovered) = discovered {
                    cache.insert(compiler.clone(), discovered);
                    let paths = cache
                        .get(compiler)
                        .map(|paths| paths.to_vec())
                        .unwrap_or_default();
                    // Snapshot the full cache under the lock. We only do
                    // this on an actual insert (not a hit), so the cost
                    // is paid once per (compiler binary, mtime) — the
                    // same denominator as the spawn cost we're trying to
                    // amortize. Cloning the `SystemIncludeCache` is a
                    // shallow `HashMap` clone (≤ a few dozen entries in
                    // practice) — orders of magnitude cheaper than the
                    // `<compiler> -###` / `-v -E` spawn we just paid for.
                    let snapshot = cache.clone();
                    (paths, Some(snapshot))
                } else {
                    (Vec::new(), None)
                }
            };
            // Issue #784 phase 2c invariant: don't write-through until
            // the on-disk snapshot has been merged into the live cache.
            // Saving a subset over the loaded-from-disk superset would
            // silently lose entries on the next restart. Once the
            // background loader sets `system_includes_loaded`, the
            // in-memory cache is canonical and write-through is safe.
            if let Some(snapshot) = inserted_snapshot {
                if state.system_includes_loaded.load(Ordering::Acquire) {
                    let path = state.system_includes_cache_path.clone();
                    tokio::task::spawn_blocking(move || {
                        if let Err(e) = snapshot.save_to_disk(path.as_path()) {
                            tracing::warn!(
                                path = %path.display(),
                                "system include cache write-through failed: {e}"
                            );
                        }
                    });
                }
            }
            resolved
        }
    };
    let system_includes_ns = t_system_includes
        .map(|t| t.elapsed().as_nanos() as u64)
        .unwrap_or(0);

    // Watch system include directories
    let t_system_watch = want_rust_miss_profile.then(std::time::Instant::now);
    watch_directories(state, &system_includes).await;
    let system_watch_ns = t_system_watch
        .map(|t| t.elapsed().as_nanos() as u64)
        .unwrap_or(0);

    SystemIncludesOutcome {
        includes: system_includes,
        system_includes_ns,
        system_watch_ns,
    }
}

async fn discover_system_include_paths(
    compiler: &NormalizedPath,
    lineage: &crate::daemon::lineage::Lineage,
    compiler_priority: CompilePriority,
    use_fast: bool,
) -> Option<Vec<NormalizedPath>> {
    let disc_args = if use_fast {
        crate::depgraph::discovery_args_fast()
    } else {
        crate::depgraph::discovery_args()
    };
    let output = run_discovery_command(compiler, &disc_args, lineage, compiler_priority).await;
    match output {
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let mut paths = if use_fast {
                crate::depgraph::parse_cc1_system_include_output(&stderr)
            } else {
                crate::depgraph::parse_system_include_output(&stderr)
            };
            // Defensive fall-through: if the fast probe returned no paths
            // (e.g. an older clang that doesn't emit `-internal-isystem`
            // flags, or the binary detected as Clang turned out to be gcc
            // behind a clang symlink), retry with the slow `-v -E`
            // discovery. The cache memoizes the result either way.
            if use_fast && paths.is_empty() {
                let slow_args = crate::depgraph::discovery_args();
                match run_discovery_command(compiler, &slow_args, lineage, compiler_priority).await
                {
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        paths = crate::depgraph::parse_system_include_output(&stderr);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "failed to run fallback compiler for include discovery: {e}"
                        );
                    }
                }
            }
            Some(paths)
        }
        Err(e) => {
            tracing::warn!("failed to run compiler for include discovery: {e}");
            None
        }
    }
}

async fn run_discovery_command(
    compiler: &NormalizedPath,
    args: &[&str],
    lineage: &crate::daemon::lineage::Lineage,
    compiler_priority: CompilePriority,
) -> std::io::Result<std::process::Output> {
    let mut cmd = tokio::process::Command::new(compiler);
    cmd.args(args);
    lineage.apply_to_tokio(&mut cmd, None);
    crate::daemon::process::tokio_command_output_with_priority_timeout(
        &mut cmd,
        compiler_priority,
        SYSTEM_INCLUDE_DISCOVERY_TIMEOUT,
    )
    .await
}
