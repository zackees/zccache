//! Compiler-exec phase: depfile prep, response file, pre-hash overlap, spawn.
//!
//! Runs on the miss path after the depgraph verdict has been logged and the
//! cached-hit branches have been exhausted. Returns the exec output plus the
//! per-phase timings consumed by the miss profiles.

use super::super::super::*;

pub(super) struct CompileExecRequest<'a> {
    pub(super) state_arc: &'a Arc<SharedState>,
    pub(super) compiler: &'a NormalizedPath,
    pub(super) effective_args: &'a [String],
    pub(super) cwd: &'a Path,
    pub(super) cwd_path: &'a NormalizedPath,
    pub(super) source_path: &'a NormalizedPath,
    pub(super) output_path: &'a NormalizedPath,
    pub(super) compilation: &'a crate::compiler::CacheableCompilation,
    pub(super) dep_flags: &'a UserDepFlags,
    pub(super) rustc_args_opt: Option<&'a crate::depgraph::RustcParsedArgs>,
    pub(super) rustc_extern_paths: &'a [NormalizedPath],
    pub(super) is_rustc: bool,
    pub(super) client_env: &'a Option<Vec<(String, String)>>,
    pub(super) lineage: &'a crate::daemon::lineage::Lineage,
    pub(super) compile_start: std::time::Instant,
    pub(super) snap_clock: Clock,
    pub(super) dependency_mode: DependencyDiscoveryMode,
}

pub(super) struct CompileExecOutcome {
    pub(super) exit_code: i32,
    pub(super) stdout: Arc<Vec<u8>>,
    pub(super) stderr: Arc<Vec<u8>>,
    pub(super) depfile_strategy: DepfileStrategy,
    pub(super) dependency_scan: Option<crate::depgraph::ScanResult>,
    pub(super) pre_hash_task: Option<tokio::task::JoinHandle<HashMap<NormalizedPath, ContentHash>>>,
    pub(super) compiler_priority_decision: crate::daemon::process::CompilePriorityDecision,
    pub(super) pre_exec_ns: u64,
    pub(super) break_outputs_ns: u64,
    pub(super) compiler_process_ns: u64,
    pub(super) compiler_exec_ns: u64,
    pub(super) compiler_prep_ns: u64,
    pub(super) post_exec_ns: u64,
    pub(super) staged_plan: Option<StagedCompilePlan>,
}

pub(super) enum CompileExecResult {
    Ok(CompileExecOutcome),
    Error(Response),
}

struct PrivateHeaderTrace {
    path: NormalizedPath,
}

impl Drop for PrivateHeaderTrace {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.path.as_path());
    }
}

/// Prepare depfile/response-file/output-paths, spawn the compiler, and gather
/// timings. The `pre_hash_task` returned is the rustc-only background hash of
/// source + externs (issue #532) — `await`ed later in the store phase so its
/// work overlaps with the compiler process itself.
pub(super) async fn run_compile_exec(req: CompileExecRequest<'_>) -> CompileExecResult {
    let CompileExecRequest {
        state_arc,
        compiler,
        effective_args,
        cwd,
        cwd_path,
        source_path,
        output_path,
        compilation,
        dep_flags,
        rustc_args_opt,
        rustc_extern_paths,
        is_rustc,
        client_env,
        lineage,
        compile_start,
        snap_clock,
        dependency_mode,
    } = req;

    let state = state_arc.as_ref();

    // ── Phase: compiler exec (with depfile injection) ────────────────
    let pre_exec_ns = compile_start.elapsed().as_nanos() as u64;
    let t_exec = std::time::Instant::now();
    let supports_depfile = compilation.family.supports_depfile();
    let inject_header_trace = should_inject_header_trace(
        dependency_mode,
        compilation.family,
        effective_args,
        dep_flags,
    );
    let use_mmd = matches!(
        compilation.family,
        crate::compiler::CompilerFamily::Gcc | crate::compiler::CompilerFamily::Clang
    ) && (dependency_mode.use_mmd() || inject_header_trace);
    let (mut extra_args, mut depfile_strategy) = crate::depgraph::depfile::prepare_depfile_with_mmd(
        use_mmd,
        supports_depfile,
        dep_flags,
        output_path,
        &state.depfile_tmpdir,
    );
    let header_trace_enabled =
        inject_header_trace && matches!(depfile_strategy, DepfileStrategy::InjectedMmd { .. });
    let private_header_trace = if header_trace_enabled
        && use_private_clang_header_trace(compilation.family, source_path.as_path(), effective_args)
    {
        Some(prepare_private_clang_header_trace(
            &mut extra_args,
            &mut depfile_strategy,
        ))
    } else {
        None
    };
    let stderr_header_trace = header_trace_enabled && private_header_trace.is_none();
    if stderr_header_trace {
        extra_args.push("-H".to_string());
    }

    // For MSVC, use /showIncludes to get complete dependency info
    // (equivalent to depfiles for gcc/clang). This enables cache hits
    // for files with computed includes like `#include MACRO`.
    if compilation.family == crate::compiler::CompilerFamily::Msvc
        && depfile_strategy == DepfileStrategy::Unsupported
    {
        if !dep_flags.has_md {
            extra_args.push("/showIncludes".to_string());
        }
        depfile_strategy = DepfileStrategy::ShowIncludes;
    }

    let expected_outputs = if let Some(rustc_args) = rustc_args_opt {
        rustc_expected_output_paths(rustc_args, output_path, cwd_path, client_env.as_deref())
    } else {
        vec![output_path.clone()]
    };
    use crate::daemon::staged_stats::{StagedCounter, StagedTiming};
    let planning_started = std::time::Instant::now();
    state.profiler.staged.count(StagedCounter::PlanAttempted);
    let staged_plan_result = if is_rustc {
        StagedCompilePlan::rustc(
            state.staging.path(),
            effective_args,
            output_path,
            &expected_outputs,
            cwd,
        )
    } else {
        StagedCompilePlan::cc(
            state.staging.path(),
            compilation.family,
            effective_args,
            output_path,
            cwd,
            dep_flags,
        )
    };
    state.profiler.staged.timing(
        StagedTiming::Planning,
        planning_started.elapsed().as_nanos() as u64,
    );
    let staged_plan = match staged_plan_result {
        StagedPlanOutcome::Enabled(plan) => {
            state.profiler.staged.count(StagedCounter::PlanEnabled);
            Some(plan)
        }
        StagedPlanOutcome::Unsupported(reason) => {
            state.profiler.staged.count(StagedCounter::PlanUnsupported);
            state.profiler.staged.failure(reason.failure());
            None
        }
        StagedPlanOutcome::Error(error) => {
            state.profiler.staged.count(StagedCounter::PlanError);
            state.profiler.staged.failure(error.reason.failure());
            return CompileExecResult::Error(Response::Error {
                message: format!(
                    "failed to prepare private compiler staging: {}",
                    error.source
                ),
            });
        }
    };
    let compiler_args = staged_plan.as_ref().map_or_else(
        || effective_args.to_vec(),
        |plan| plan.rewritten_args.clone(),
    );

    // Combine expanded_args + extra_args for response-file length check.
    // Only allocates when extra_args is non-empty.
    let combined_args;
    let rsp_args: &[String] = if extra_args.is_empty() {
        &compiler_args
    } else {
        combined_args = [compiler_args.as_slice(), extra_args.as_slice()].concat();
        &combined_args
    };

    let _rsp_guard = match crate::compiler::response_file::write_response_file_if_needed(
        rsp_args,
        &state.depfile_tmpdir,
        compilation.family,
    ) {
        Ok(guard) => guard,
        Err(e) => {
            return CompileExecResult::Error(Response::Error {
                message: format!("failed to write response file: {e}"),
            });
        }
    };

    let output_paths = staged_plan
        .as_ref()
        .map_or(expected_outputs, StagedCompilePlan::output_paths);
    let t_break_outputs = std::time::Instant::now();
    for path in &output_paths {
        if let Err(e) = break_output_hardlink_before_compile(path) {
            return CompileExecResult::Error(Response::Error {
                message: format!(
                    "failed to detach hardlinked output before compile {}: {e}",
                    path.display()
                ),
            });
        }
    }
    let break_outputs_ns = t_break_outputs.elapsed().as_nanos() as u64;

    let mut cmd = tokio::process::Command::new(compiler);
    if let Some(ref rsp) = _rsp_guard {
        cmd.arg(rsp.at_arg()).current_dir(cwd);
    } else {
        cmd.args(&compiler_args).current_dir(cwd);
        if !extra_args.is_empty() {
            cmd.args(&extra_args);
        }
    }
    apply_client_env(&mut cmd, client_env, lineage);
    let t_compiler_process = std::time::Instant::now();
    let is_link_like = rustc_args_opt
        .is_some_and(|rustc_args| rustc_args.emit_types.iter().any(|emit| emit == "link"));
    let compiler_priority =
        CompilePriority::from_client_env_for_link_like(client_env.as_deref(), is_link_like);
    let compiler_priority_decision = compiler_priority.resolve_for_current_load();

    // soldr#2781: this point is reached only after every cache-hit branch has
    // missed. Ordinary compiler children share the gate; an amalgamated C
    // translation unit or known amalgamated Rust release crate drains those
    // readers and runs alone. The shared helper acquires the bounded FIFO
    // compile slot first and the resource gate second, then both remain held
    // across overlapping input hashing and the compiler child.
    let exclusive = crate::daemon::server::compile_resource_gate::requires_exclusive_access(
        compilation.family,
        effective_args,
        source_path.as_path(),
    );
    let (_compiler_admission, available_before) =
        crate::daemon::server::compile_resource_gate::acquire_compiler_admission(state, exclusive)
            .await;
    if exclusive {
        tracing::info!(
            event = "compile_exclusive",
            source = %source_path.display(),
            "amalgamated compiler unit acquired exclusive build access"
        );
    }

    // Issue #532: kick off hashing of pre-known inputs (source +
    // rustc_extern_paths) on a blocking thread, in parallel with the
    // rustc spawn. The 50-rlib externs of a workspace link dominate
    // hash_all_ns (~64 ms on a 4-core CI runner); overlapping them with
    // the ~38 ms rustc exec hides most of that cost. Late-arriving
    // include paths (from rustc's dep-info) are hashed post-compile and
    // merged with the pre-hash result. Skip for non-rustc compilers —
    // they don't have a known-ahead extern list, and their cold hash_all
    // is small anyway.
    let pre_hash_task: Option<tokio::task::JoinHandle<HashMap<NormalizedPath, ContentHash>>> =
        if is_rustc && !rustc_extern_paths.is_empty() {
            let pre_state = Arc::clone(state_arc);
            let pre_source = source_path.clone();
            let pre_externs: Vec<NormalizedPath> = rustc_extern_paths.to_vec();
            let pre_clock = snap_clock;
            Some(tokio::task::spawn_blocking(move || {
                use rayon::prelude::*;
                let all_paths: Vec<&NormalizedPath> = std::iter::once(&pre_source)
                    .chain(pre_externs.iter())
                    .collect();
                all_paths
                    .par_iter()
                    .filter_map(|path| {
                        let hash_path = resolve_pch_source(path, &pre_state.pch_source_map)
                            .unwrap_or_else(|| (*path).clone());
                        hash_file(&pre_state.cache_system, &hash_path, pre_clock)
                            .ok()
                            .map(|h| ((*path).clone(), h))
                    })
                    .collect()
            }))
        } else {
            None
        };

    // Issue #813 / #816: acquire a compile-concurrency permit before
    // spawning the compiler. The semaphore (when present — None means
    // ZCCACHE_MAX_PARALLEL_COMPILES=0 opt-out) gates total in-flight
    // compiler children across ALL clients sharing this daemon.
    // Permit is held for the duration of the spawn + wait; drops on
    // scope exit, freeing the slot for the next queued request.
    //
    // The `compile_start` / `compile_end` log events are deliberately
    // structured so an integration test (sub-task #817) can parse the
    // log and assert no two compile intervals overlap when the cap is
    // 1 (sub-task #5 of the meta).
    //
    // Issue #1216: the wait is registered in `state.compile_queue` so the
    // connection layer can push `CompileProgress` heartbeats naming this
    // request's queue position while it sits here. `_compile_gate` restores
    // the permit and counters on every exit path, including cancellation.
    let client_pid = lineage.client_pid.unwrap_or(0);
    if let Some(available_before) = available_before {
        tracing::info!(
            event = "compile_start",
            client_pid,
            available_before,
            "compile_start client_pid={client_pid} available_before={available_before}",
        );
    }
    let compile_span_start = std::time::Instant::now();

    let (result, streamed_output) = if let Some(context) = crate::daemon::compile_output::current()
    {
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let filter = if depfile_strategy == DepfileStrategy::ShowIncludes {
            crate::daemon::compile_output::StderrFilter::ShowIncludes {
                source: source_path.as_path(),
                cwd: cwd_path.as_path(),
            }
        } else if stderr_header_trace {
            crate::daemon::compile_output::StderrFilter::HeaderTrace {
                source: source_path.as_path(),
                cwd: cwd_path.as_path(),
            }
        } else {
            crate::daemon::compile_output::StderrFilter::None
        };
        let process = crate::daemon::process::tokio_command_output_streaming_with_priority_stdin(
            &mut cmd,
            compiler_priority_decision.effective,
            None,
            sender,
        );
        let consume = crate::daemon::compile_output::consume(receiver, context, filter);
        let (process_result, capture_result) = tokio::join!(process, consume);
        (process_result, Some(capture_result))
    } else {
        (
            crate::daemon::process::tokio_command_output_with_priority(
                &mut cmd,
                compiler_priority_decision.effective,
            )
            .await,
            None,
        )
    };
    let compiler_process_ns = t_compiler_process.elapsed().as_nanos() as u64;
    if staged_plan.is_some() {
        state.profiler.staged.count(StagedCounter::CompilerStaged);
        state
            .profiler
            .staged
            .timing(StagedTiming::Compiler, compiler_process_ns);
    }

    if state.compile_concurrency.is_some() {
        let duration_ns = compile_span_start.elapsed().as_nanos() as u64;
        let exit_code = result
            .as_ref()
            .ok()
            .and_then(|o| o.status.code())
            .unwrap_or(-1);
        tracing::info!(
            event = "compile_end",
            client_pid,
            duration_ns,
            exit_code,
            "compile_end client_pid={client_pid} duration_ns={duration_ns} exit_code={exit_code}",
        );
    }

    let output = match result {
        Ok(o) => o,
        Err(e) => {
            return CompileExecResult::Error(Response::Error {
                message: format!("failed to run compiler: {e}"),
            });
        }
    };
    let streamed_output = match streamed_output {
        Some(Ok(output)) => Some(output),
        Some(Err(e)) => {
            return CompileExecResult::Error(Response::Error {
                message: format!("failed to stream compiler output: {e}"),
            });
        }
        None => None,
    };
    let compiler_exec_ns = t_exec.elapsed().as_nanos() as u64;
    let compiler_prep_ns = compiler_exec_ns.saturating_sub(compiler_process_ns);

    let t_post_exec = std::time::Instant::now();
    let exit_code = output.status.code().unwrap_or(-1);
    let (stdout_bytes, dependency_scan, stderr_bytes) = if let Some(streamed) = streamed_output {
        (streamed.stdout, streamed.dependency_scan, streamed.stderr)
    } else if depfile_strategy == DepfileStrategy::ShowIncludes {
        let (scan, filtered) = crate::depgraph::show_includes::parse_show_includes(
            &output.stderr,
            source_path,
            cwd_path,
        );
        (output.stdout, Some(scan), filtered)
    } else if stderr_header_trace {
        let (scan, filtered) = crate::depgraph::header_trace::parse_header_trace(
            &output.stderr,
            source_path,
            cwd_path,
        );
        (output.stdout, Some(scan), filtered)
    } else {
        (output.stdout, None, output.stderr)
    };
    let dependency_scan = if let Some(trace) = private_header_trace.as_ref() {
        let scan = crate::depgraph::header_trace::parse_dependency_graph_file(
            trace.path.as_path(),
            source_path,
            cwd_path,
        );
        Some(scan)
    } else {
        dependency_scan
    };
    let stdout = Arc::new(stdout_bytes);
    let stderr = Arc::new(stderr_bytes);
    let post_exec_ns = t_post_exec.elapsed().as_nanos() as u64;

    // Drop the response-file guard now that the compiler has exited. The
    // pre-split function held the guard until end-of-function via `let
    // _rsp_guard = ...`; keeping it bound to a local in this helper does
    // the same — the guard drops when `run_compile_exec` returns, which is
    // before any subsequent post-exec work touches the response file.
    drop(_rsp_guard);

    CompileExecResult::Ok(CompileExecOutcome {
        exit_code,
        stdout,
        stderr,
        depfile_strategy,
        dependency_scan,
        pre_hash_task,
        compiler_priority_decision,
        pre_exec_ns,
        break_outputs_ns,
        compiler_process_ns,
        compiler_exec_ns,
        compiler_prep_ns,
        post_exec_ns,
        staged_plan,
    })
}

fn header_trace_path(depfile: &NormalizedPath) -> NormalizedPath {
    depfile.as_path().with_extension("headers").into()
}

fn prepare_private_clang_header_trace(
    extra_args: &mut Vec<String>,
    depfile_strategy: &mut DepfileStrategy,
) -> PrivateHeaderTrace {
    let DepfileStrategy::InjectedMmd { path } =
        std::mem::replace(depfile_strategy, DepfileStrategy::CompilerTrace)
    else {
        unreachable!("private header trace requires an injected MMD depfile");
    };
    let trace = PrivateHeaderTrace {
        path: header_trace_path(&path),
    };
    // Clang's dependency graph includes system headers without the
    // `-sys-header-deps` switch that makes the compact MMD path expensive.
    // It is complete on its own, so no second manifest is needed.
    extra_args.clear();
    extra_args.extend([
        "-Xclang".to_string(),
        "-dependency-dot".to_string(),
        "-Xclang".to_string(),
        trace.path.to_string_lossy().into_owned(),
    ]);
    trace
}

/// Clang's file-backed frontend trace avoids `-H` stderr traffic on the hot C
/// miss path. Keep it deliberately narrower than the rejected all-language
/// candidate: C++ continues to use the public trace because its much larger
/// header graph previously made the private path exceed the hosted watchdog.
fn use_private_clang_header_trace(
    family: crate::compiler::CompilerFamily,
    source: &Path,
    args: &[String],
) -> bool {
    family == crate::compiler::CompilerFamily::Clang
        && source.extension().is_some_and(|extension| extension == "c")
        && !args.iter().any(|arg| {
            arg == "-x"
                || arg.starts_with("-x")
                || contains_private_header_trace_flag(arg)
                || contains_sysroot_flag(arg)
        })
}

fn contains_private_header_trace_flag(arg: &str) -> bool {
    const PRIVATE_FLAGS: [&str; 3] = [
        "-dependency-dot",
        "-header-include-file",
        "-sys-header-deps",
    ];
    PRIVATE_FLAGS.contains(&arg)
        || arg
            .strip_prefix("-Xclang=")
            .is_some_and(|arg| PRIVATE_FLAGS.contains(&arg))
        || arg
            .strip_prefix("-Wp,")
            .is_some_and(|args| args.split(',').any(|arg| PRIVATE_FLAGS.contains(&arg)))
}

fn contains_sysroot_flag(arg: &str) -> bool {
    fn is_sysroot_flag(arg: &str) -> bool {
        arg.starts_with("-isysroot") || arg == "--sysroot" || arg.starts_with("--sysroot=")
    }

    is_sysroot_flag(arg)
        || arg.strip_prefix("-Xclang=").is_some_and(is_sysroot_flag)
        || arg
            .strip_prefix("-Wp,")
            .is_some_and(|args| args.split(',').any(is_sysroot_flag))
}

fn header_trace_is_supported(family: crate::compiler::CompilerFamily, args: &[String]) -> bool {
    if !matches!(
        family,
        crate::compiler::CompilerFamily::Gcc | crate::compiler::CompilerFamily::Clang
    ) {
        return false;
    }
    !args.iter().any(|arg| {
        arg == "-H"
            || (arg.starts_with("-Wp,") && arg.split(',').any(|part| part == "-H"))
            || arg == "-Xpreprocessor=-H"
            || arg == "-Xclang=-H"
            || arg == "-fshow-skipped-includes"
            || contains_private_header_trace_flag(arg)
            || arg == "-fdiagnostics-format"
            || arg.starts_with("-fdiagnostics-format=")
    })
}

fn should_inject_header_trace(
    dependency_mode: DependencyDiscoveryMode,
    family: crate::compiler::CompilerFamily,
    args: &[String],
    dep_flags: &UserDepFlags,
) -> bool {
    dependency_mode == DependencyDiscoveryMode::AllHeaders
        && !dep_flags.has_md
        && dep_flags.mf_path.is_none()
        && !dep_flags.depfile_to_stdout
        && header_trace_is_supported(family, args)
}

#[cfg(test)]
mod tests {
    use super::{
        header_trace_is_supported, prepare_private_clang_header_trace, should_inject_header_trace,
        use_private_clang_header_trace,
    };
    use crate::compiler::CompilerFamily;
    use crate::daemon::server::dependency_policy::DependencyDiscoveryMode;
    use crate::depgraph::UserDepFlags;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn header_trace_is_limited_to_unmodified_gnu_diagnostic_streams() {
        assert!(header_trace_is_supported(
            CompilerFamily::Clang,
            &args(&["-c", "main.c", "-Iinclude"]),
        ));
        assert!(header_trace_is_supported(
            CompilerFamily::Gcc,
            &args(&["-c", "main.cc"]),
        ));
        assert!(!header_trace_is_supported(
            CompilerFamily::Msvc,
            &args(&["/c", "main.c"]),
        ));
        for incompatible in [
            "-H",
            "-Wp,-H",
            "-Wp,-DTRACE,-H,-UOLD",
            "-Xpreprocessor=-H",
            "-Xclang=-H",
            "-fshow-skipped-includes",
            "-header-include-file",
            "-sys-header-deps",
            "-Xclang=-header-include-file",
            "-Xclang=-sys-header-deps",
            "-dependency-dot",
            "-Xclang=-dependency-dot",
            "-Wp,-dependency-dot,custom.dot",
            "-Wp,-header-include-file,custom.headers,-sys-header-deps",
            "-fdiagnostics-format=json",
        ] {
            assert!(!header_trace_is_supported(
                CompilerFamily::Clang,
                &args(&["-c", "main.c", incompatible]),
            ));
        }
        assert!(!header_trace_is_supported(
            CompilerFamily::Gcc,
            &args(&["-c", "main.c", "-Xpreprocessor", "-H"]),
        ));
    }

    #[test]
    fn user_depfiles_never_enable_private_header_trace() {
        let compile_args = args(&["-c", "main.c"]);
        assert!(should_inject_header_trace(
            DependencyDiscoveryMode::AllHeaders,
            CompilerFamily::Clang,
            &compile_args,
            &UserDepFlags::default(),
        ));
        for dep_flags in [
            UserDepFlags {
                has_md: true,
                has_mmd: true,
                ..Default::default()
            },
            UserDepFlags {
                has_md: true,
                has_mmd: true,
                mf_path: Some("custom.d".into()),
                ..Default::default()
            },
            UserDepFlags {
                depfile_to_stdout: true,
                ..Default::default()
            },
        ] {
            assert!(!should_inject_header_trace(
                DependencyDiscoveryMode::AllHeaders,
                CompilerFamily::Clang,
                &compile_args,
                &dep_flags,
            ));
        }
    }

    #[test]
    fn private_clang_trace_is_bounded_to_plain_c_translation_units() {
        assert!(use_private_clang_header_trace(
            CompilerFamily::Clang,
            std::path::Path::new("main.c"),
            &args(&["-c", "main.c"]),
        ));
        assert!(!use_private_clang_header_trace(
            CompilerFamily::Clang,
            std::path::Path::new("main.cpp"),
            &args(&["-c", "main.cpp"]),
        ));
        assert!(!use_private_clang_header_trace(
            CompilerFamily::Clang,
            std::path::Path::new("main.C"),
            &args(&["-c", "main.C"]),
        ));
        assert!(!use_private_clang_header_trace(
            CompilerFamily::Clang,
            std::path::Path::new("main.c"),
            &args(&["-x", "c++", "-c", "main.c"]),
        ));
        assert!(!use_private_clang_header_trace(
            CompilerFamily::Gcc,
            std::path::Path::new("main.c"),
            &args(&["-c", "main.c"]),
        ));
        for incompatible in [
            "-Xclang=-header-include-file",
            "-Xclang=-sys-header-deps",
            "-Xclang=-dependency-dot",
            "-Wp,-dependency-dot,custom.dot",
            "-Wp,-header-include-file,custom.headers,-sys-header-deps",
            "--sysroot=/sdk",
            "-isysroot/sdk",
            "-Xclang=-isysroot",
            "-Xclang=-isysroot=/sdk",
            "-Xclang=--sysroot",
            "-Xclang=--sysroot=/sdk",
            "-Wp,-isysroot,/sdk",
            "-Wp,--sysroot,/sdk",
        ] {
            assert!(!use_private_clang_header_trace(
                CompilerFamily::Clang,
                std::path::Path::new("main.c"),
                &args(&["-c", "main.c", incompatible]),
            ));
        }
    }

    #[test]
    fn private_clang_trace_does_not_duplicate_the_header_graph_in_mmd() {
        let temp = tempfile::TempDir::new().unwrap();
        let depfile = temp.path().join("main.d");
        let mut extra_args = args(&["-MMD", "-MF", depfile.to_str().unwrap()]);
        let mut strategy = crate::depgraph::DepfileStrategy::InjectedMmd {
            path: depfile.into(),
        };

        let trace = prepare_private_clang_header_trace(&mut extra_args, &mut strategy);

        assert_eq!(strategy, crate::depgraph::DepfileStrategy::CompilerTrace);
        assert!(!extra_args.iter().any(|arg| arg == "-MMD" || arg == "-MF"));
        assert!(extra_args.iter().any(|arg| arg == "-dependency-dot"));
        assert!(!extra_args.iter().any(|arg| arg == "-sys-header-deps"));
        assert_eq!(
            trace.path.extension(),
            Some(std::ffi::OsStr::new("headers"))
        );
    }
}
