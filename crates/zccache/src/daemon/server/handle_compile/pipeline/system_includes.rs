//! System include discovery + initial watch for the compile pipeline.
//!
//! Discovery is per-compiler-path memoized in `state.system_includes`. This
//! module is the only caller of the discovery helper plus the post-discovery
//! `watch_directories` for the include roots.

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
            let discovered = discover_system_include_paths(
                compiler,
                lineage,
                compiler_priority,
                use_fast,
            );
            let mut cache = state.system_includes.lock().await;
            if let Some(paths) = cache.get(compiler) {
                paths.to_vec()
            } else if let Some(discovered) = discovered {
                cache.insert(compiler.clone(), discovered);
                cache
                    .get(compiler)
                    .map(|paths| paths.to_vec())
                    .unwrap_or_default()
            } else {
                Vec::new()
            }
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

fn discover_system_include_paths(
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
    let output = run_discovery_command(compiler, &disc_args, lineage, compiler_priority);
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
                match run_discovery_command(compiler, &slow_args, lineage, compiler_priority) {
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

fn run_discovery_command(
    compiler: &NormalizedPath,
    args: &[&str],
    lineage: &crate::daemon::lineage::Lineage,
    compiler_priority: CompilePriority,
) -> std::io::Result<std::process::Output> {
    let mut cmd = std::process::Command::new(compiler);
    cmd.args(args);
    lineage.apply_to_sync(&mut cmd, None);
    crate::daemon::process::command_output_with_priority_timeout(
        &mut cmd,
        compiler_priority,
        SYSTEM_INCLUDE_DISCOVERY_TIMEOUT,
    )
}
