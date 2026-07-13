//! Whole-invocation planning before any cache lookup or requested-path write.

use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
struct InputStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    file_id: Option<FileId>,
    change_marker: Option<i128>,
}

fn input_stamp(path: &Path) -> Option<InputStamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(InputStamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
        file_id: get_file_id(path),
        change_marker: get_file_change_marker(path),
    })
}

fn stamps_have_native_markers(stamps: &HashMap<NormalizedPath, InputStamp>) -> bool {
    stamps.values().all(|stamp| stamp.change_marker.is_some())
}

fn direct_response_family(
    family: crate::compiler::CompilerFamily,
    args: &[String],
) -> crate::compiler::CompilerFamily {
    if family == crate::compiler::CompilerFamily::Msvc
        || crate::compiler::parse_msvc::looks_like_msvc_args(args)
    {
        crate::compiler::CompilerFamily::Msvc
    } else {
        family
    }
}

fn derived_staged_side_outputs(
    compilations: &[crate::compiler::CacheableCompilation],
    args: &[String],
    cwd: &NormalizedPath,
) -> HashSet<NormalizedPath> {
    let lower: Vec<String> = args.iter().map(|arg| arg.to_ascii_lowercase()).collect();
    let split_dwarf = lower.iter().any(|arg| arg.starts_with("-gsplit-dwarf"));
    let stack_usage = lower.iter().any(|arg| arg == "-fstack-usage");
    let save_temps = lower.iter().any(|arg| arg.starts_with("-save-temps"));
    let default_depfiles = lower
        .iter()
        .any(|arg| matches!(arg.as_str(), "-md" | "-mmd"))
        && !args
            .iter()
            .any(|arg| arg == "-MF" || (arg.starts_with("-MF") && arg.len() > 3));
    let coverage = lower.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--coverage" | "-coverage" | "-fprofile-arcs" | "-ftest-coverage"
        )
    });
    let time_trace = lower.iter().any(|arg| arg == "-ftime-trace");
    let callgraph = lower.iter().any(|arg| arg.starts_with("-fcallgraph-info"));
    let optimization_record = lower.iter().any(|arg| arg == "-fsave-optimization-record");
    let sarif = lower
        .iter()
        .any(|arg| arg.starts_with("-fdiagnostics-format=sarif-file"));
    let default_doc = lower
        .iter()
        .any(|arg| matches!(arg.as_str(), "/doc" | "-doc"));
    let creates_pch = args.iter().any(|arg| {
        arg.strip_prefix('/')
            .or_else(|| arg.strip_prefix('-'))
            .is_some_and(|body| body.starts_with("Yc"))
    });
    let explicit_pch = args.iter().any(|arg| {
        arg.strip_prefix('/')
            .or_else(|| arg.strip_prefix('-'))
            .is_some_and(|body| body.starts_with("Fp"))
    });
    let default_assembly_listing = lower.iter().any(|arg| {
        arg.strip_prefix('/')
            .or_else(|| arg.strip_prefix('-'))
            .is_some_and(|body| matches!(body, "fa" | "fac" | "fas" | "facs"))
    });
    let mut outputs = HashSet::new();
    for compilation in compilations {
        let source = if compilation.source_file.is_absolute() {
            compilation.source_file.clone()
        } else {
            cwd.join(&compilation.source_file)
        };
        let object = if compilation.output_file.is_absolute() {
            compilation.output_file.clone()
        } else {
            cwd.join(&compilation.output_file)
        };
        if split_dwarf {
            outputs.insert(object.as_path().with_extension("dwo").into());
        }
        if default_depfiles {
            outputs.insert(object.as_path().with_extension("d").into());
        }
        if stack_usage {
            outputs.insert(object.as_path().with_extension("su").into());
        }
        if coverage {
            outputs.insert(object.as_path().with_extension("gcno").into());
        }
        if time_trace {
            outputs.insert(object.as_path().with_extension("json").into());
        }
        if callgraph {
            outputs.insert(object.as_path().with_extension("ci").into());
        }
        if optimization_record {
            outputs.insert(object.as_path().with_extension("opt.yaml").into());
        }
        if sarif {
            outputs.insert(source.as_path().with_extension("sarif").into());
        }
        if default_doc {
            outputs.insert(source.as_path().with_extension("xdc").into());
        }
        if default_assembly_listing {
            outputs.insert(object.as_path().with_extension("asm").into());
        }
        if creates_pch && !explicit_pch {
            outputs.insert(source.as_path().with_extension("pch").into());
            if let Some(name) = source.file_stem() {
                outputs.insert(cwd.join(Path::new(name).with_extension("pch")));
            }
        }
        if save_temps {
            let preprocessed = if source
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("c"))
            {
                "i"
            } else {
                "ii"
            };
            for extension in [preprocessed, "s"] {
                outputs.insert(object.as_path().with_extension(extension).into());
                if let Some(name) = source.file_stem() {
                    outputs.insert(cwd.join(Path::new(name).with_extension(extension)));
                }
            }
        }
    }
    outputs
}

#[derive(Clone)]
pub(in crate::daemon::server) struct InputSnapshot {
    pub(super) hashes: HashMap<NormalizedPath, ContentHash>,
    stamps: HashMap<NormalizedPath, InputStamp>,
    complete: bool,
    clock: Clock,
}

impl InputSnapshot {
    pub(super) fn incomplete(clock: Clock) -> Self {
        Self {
            hashes: HashMap::new(),
            stamps: HashMap::new(),
            complete: false,
            clock,
        }
    }

    pub(super) fn capture(
        state: &SharedState,
        source: &NormalizedPath,
        ctx: &CompileContext,
        mut hashes: HashMap<NormalizedPath, ContentHash>,
        clock: Clock,
    ) -> Self {
        let scan = crate::depgraph::scanner::scan_recursive(source, &ctx.include_search);
        let mut complete = true;
        for path in scan.resolved.iter().chain(ctx.force_includes.iter()) {
            if hashes.contains_key(path) {
                continue;
            }
            match hash_file(&state.cache_system, path, clock) {
                Ok(hash) => {
                    hashes.insert(path.clone(), hash);
                }
                Err(_) => complete = false,
            }
        }
        let stamps: HashMap<NormalizedPath, InputStamp> = hashes
            .keys()
            .filter_map(|path| input_stamp(path).map(|stamp| (path.clone(), stamp)))
            .collect();
        complete &= stamps.len() == hashes.len();
        complete &= stamps_have_native_markers(&stamps);
        Self {
            hashes,
            stamps,
            complete,
            clock,
        }
    }

    pub(super) fn stable(
        &self,
        state: &SharedState,
        paths: &[NormalizedPath],
        current_hashes: &HashMap<NormalizedPath, ContentHash>,
    ) -> bool {
        self.complete
            && self.hashes.len() == current_hashes.len()
            && self.stamps.len() == paths.len()
            && self
                .hashes
                .iter()
                .all(|(path, before)| current_hashes.get(path) == Some(before))
            && paths.iter().all(|path| {
                input_stamp(path).as_ref() == self.stamps.get(path)
                    && !state.cache_system.journal().changed_since(path, self.clock)
            })
    }
}

pub(super) async fn run_unsupported_batch(
    state: &Arc<SharedState>,
    sid: &SessionId,
    compiler: &NormalizedPath,
    compilations: &[crate::compiler::CacheableCompilation],
    original_args: &[String],
    cwd: &NormalizedPath,
    client_env: &Option<Vec<(String, String)>>,
) -> Option<Response> {
    use crate::daemon::staged_stats::{StagedCounter, StagedTiming};
    if !staged_lane_enabled(compilations[0].family) {
        return None;
    }
    let started = std::time::Instant::now();
    state.profiler.staged.count(StagedCounter::PlanAttempted);
    let outputs: Vec<NormalizedPath> = compilations
        .iter()
        .map(|compilation| compilation.output_file.clone())
        .collect();
    let outcome =
        classify_staged_multi_invocation(compilations[0].family, original_args, &outputs, cwd);
    state
        .profiler
        .staged
        .timing(StagedTiming::Planning, started.elapsed().as_nanos() as u64);
    match outcome {
        StagedPlanOutcome::Enabled(()) => {
            state.profiler.staged.count(StagedCounter::PlanEnabled);
            None
        }
        StagedPlanOutcome::Error(error) => {
            state.profiler.staged.count(StagedCounter::PlanError);
            state.profiler.staged.failure(error.reason.failure());
            Some(Response::Error {
                message: format!(
                    "failed to plan private multi-source outputs: {}",
                    error.source
                ),
            })
        }
        StagedPlanOutcome::Unsupported(reason) => {
            state.profiler.staged.count(StagedCounter::PlanUnsupported);
            state.profiler.staged.failure(reason.failure());
            let response_family = direct_response_family(compilations[0].family, original_args);
            let msvc_syntax = response_family == crate::compiler::CompilerFamily::Msvc;
            let mut outputs: HashSet<NormalizedPath> = compilations
                .iter()
                .map(|compilation| {
                    if compilation.output_file.is_absolute() {
                        compilation.output_file.clone()
                    } else {
                        cwd.join(&compilation.output_file)
                    }
                })
                .collect();
            outputs.extend(explicit_staged_side_outputs(
                original_args,
                msvc_syntax,
                cwd,
            ));
            outputs.extend(derived_staged_side_outputs(
                compilations,
                original_args,
                cwd,
            ));
            // Primary outputs are the only paths this cache can have
            // hardlinked on an earlier supported invocation. Unsupported
            // side outputs are never persisted or materialized by zccache;
            // explicit and safely derivable side paths above are detached as
            // defense in depth, without mutating unrelated files under cwd.
            for compilation in compilations {
                let source = if compilation.source_file.is_absolute() {
                    compilation.source_file.clone()
                } else {
                    cwd.join(&compilation.source_file)
                };
                outputs.remove(&source);
            }
            for output in outputs {
                if let Err(error) = break_output_hardlink_before_compile(&output) {
                    return Some(Response::Error {
                        message: format!(
                            "failed to detach hardlinked multi-source output {}: {error}",
                            output.display()
                        ),
                    });
                }
            }
            for _ in compilations {
                state.stats.record_compilation();
                state.stats.record_non_cacheable();
                record_session_stat(&state.sessions, sid, |stats| {
                    stats.record_non_cacheable();
                });
            }
            let response = run_compiler_direct_with_family(
                compiler,
                original_args,
                cwd,
                &state.sessions,
                sid,
                client_env,
                &[],
                &state.depfile_tmpdir,
                response_family,
            )
            .await;
            if matches!(
                &response,
                Response::CompileResult { exit_code, .. } if *exit_code != 0
            ) {
                state.stats.record_error();
                record_session_stat(&state.sessions, sid, |stats| stats.record_error());
            }
            Some(response)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clang_executable_with_msvc_syntax_selects_msvc_response_dialect() {
        let args = vec!["/c".into(), "/Foshared\\".into(), "first.c".into()];
        assert_eq!(
            direct_response_family(crate::compiler::CompilerFamily::Clang, &args),
            crate::compiler::CompilerFamily::Msvc
        );
    }

    #[test]
    fn missing_native_marker_fails_snapshot_completeness_closed() {
        let stamp = InputStamp {
            len: 1,
            modified: None,
            created: None,
            file_id: None,
            change_marker: None,
        };
        assert!(!stamps_have_native_markers(&HashMap::from([(
            NormalizedPath::from("input.c"),
            stamp,
        )])));
    }

    #[test]
    fn input_stamp_detects_aba_even_when_mtime_is_restored() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input.c");
        std::fs::write(&input, b"AAAA").unwrap();
        let before = input_stamp(&input).unwrap();
        let original_mtime =
            filetime::FileTime::from_last_modification_time(&std::fs::metadata(&input).unwrap());

        std::fs::write(&input, b"BBBB").unwrap();
        std::fs::write(&input, b"AAAA").unwrap();
        filetime::set_file_mtime(&input, original_mtime).unwrap();

        let after = input_stamp(&input).unwrap();
        assert_eq!(std::fs::read(&input).unwrap(), b"AAAA");
        assert_eq!(after.modified, before.modified);
        assert_eq!(after.file_id, before.file_id);
        assert_ne!(
            after, before,
            "native change marker must detect A-to-B-to-A mutation"
        );
    }

    #[tokio::test]
    async fn input_snapshot_rejects_aba_with_identical_content_hash_and_mtime() {
        let temp = tempfile::tempdir().unwrap();
        let cache_dir: NormalizedPath = temp.path().join("cache").into();
        let endpoint = crate::ipc::unique_test_endpoint();
        let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();
        let state = server.test_state_arc();
        let input: NormalizedPath = temp.path().join("input.c").into();
        std::fs::write(&input, b"AAAA").unwrap();
        let original_mtime =
            filetime::FileTime::from_last_modification_time(&std::fs::metadata(&input).unwrap());
        let clock = state.cache_system.current_clock();
        let before_hash = hash_file(&state.cache_system, &input, clock).unwrap();
        let snapshot = InputSnapshot {
            hashes: HashMap::from([(input.clone(), before_hash)]),
            stamps: HashMap::from([(input.clone(), input_stamp(&input).unwrap())]),
            complete: true,
            clock,
        };

        std::fs::write(&input, b"BBBB").unwrap();
        std::fs::write(&input, b"AAAA").unwrap();
        filetime::set_file_mtime(&input, original_mtime).unwrap();
        let current_hash = hash_file(
            &state.cache_system,
            &input,
            state.cache_system.current_clock(),
        )
        .unwrap();
        assert_eq!(current_hash, before_hash);
        assert!(!snapshot.stable(
            &state,
            std::slice::from_ref(&input),
            &HashMap::from([(input.clone(), current_hash)]),
        ));
    }
}
