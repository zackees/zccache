//! Blocking dependency-scan collection for successful compile misses.

use crate::core::NormalizedPath;
use crate::daemon::server::dependency_policy::DependencyDiscoveryMode;
use crate::daemon::server::rustc::scan_rustc_deps;
use crate::depgraph::DepfileStrategy;

pub(super) struct CompileScanRequest {
    pub(super) is_rustc: bool,
    pub(super) rustc_args: Option<crate::depgraph::RustcParsedArgs>,
    pub(super) source_path: NormalizedPath,
    pub(super) cwd_path: NormalizedPath,
    pub(super) depfile_strategy: DepfileStrategy,
    pub(super) show_includes_scan: Option<crate::depgraph::ScanResult>,
    pub(super) include_search: crate::depgraph::IncludeSearchPaths,
    pub(super) dependency_mode: DependencyDiscoveryMode,
}

pub(super) struct CompileScanCollection {
    pub(super) scan_result: crate::depgraph::ScanResult,
    /// Env-dep names scanned from rustc dep-info (zccache#1021).
    pub(super) rustc_env_dep_names: Vec<String>,
    pub(super) user_depfile_capture: Option<(NormalizedPath, Vec<u8>)>,
    pub(super) depfile_parse_warning: Option<String>,
}

pub(super) async fn collect_compile_scan_blocking(
    req: CompileScanRequest,
) -> CompileScanCollection {
    tokio::task::spawn_blocking(move || collect_compile_scan(req))
        .await
        .unwrap_or_else(|e| CompileScanCollection {
            rustc_env_dep_names: Vec::new(),
            scan_result: crate::depgraph::ScanResult {
                resolved: Vec::new(),
                unresolved: vec![format!("compile dependency scan worker failed: {e}")],
                has_computed: false,
            },
            user_depfile_capture: None,
            depfile_parse_warning: None,
        })
}

fn collect_compile_scan(req: CompileScanRequest) -> CompileScanCollection {
    let CompileScanRequest {
        is_rustc,
        rustc_args,
        source_path,
        cwd_path,
        depfile_strategy,
        show_includes_scan,
        include_search,
        dependency_mode,
    } = req;

    if is_rustc {
        let (scan_result, rustc_env_dep_names) = rustc_args.as_ref().map_or_else(
            || {
                (
                    crate::depgraph::ScanResult {
                        resolved: Vec::new(),
                        unresolved: vec![
                            "missing parsed rustc args for rustc dependency scan".into()
                        ],
                        has_computed: false,
                    },
                    Vec::new(),
                )
            },
            |args| {
                let dep_scan = scan_rustc_deps(args, &source_path, &cwd_path);
                (dep_scan.scan, dep_scan.env_dep_names)
            },
        );
        return CompileScanCollection {
            scan_result,
            rustc_env_dep_names,
            user_depfile_capture: None,
            depfile_parse_warning: None,
        };
    }

    // Static scans canonicalize resolved headers. Resolve include roots once
    // so fallback filtering uses the same path spelling on every platform.
    let include_search = include_search.canonicalized();

    let mut user_depfile_capture = None;
    let mut depfile_parse_warning = None;
    let mut used_static_fallback = false;
    let mut scan_result = match &depfile_strategy {
        DepfileStrategy::Injected { path }
        | DepfileStrategy::UserSpecified { path, .. }
        | DepfileStrategy::UserDefault { path, .. } => {
            let want_capture = matches!(
                depfile_strategy,
                DepfileStrategy::UserSpecified { .. } | DepfileStrategy::UserDefault { .. }
            );
            let augment_system_headers = matches!(
                depfile_strategy,
                DepfileStrategy::UserSpecified {
                    augment_system_headers: true,
                    ..
                } | DepfileStrategy::UserDefault {
                    augment_system_headers: true,
                    ..
                }
            );
            match crate::depgraph::depfile::parse_depfile_path(path, &source_path, &cwd_path) {
                Ok(mut result) => {
                    if want_capture {
                        if let Ok(bytes) = std::fs::read(path) {
                            user_depfile_capture = Some((path.clone(), bytes));
                        }
                    }
                    if matches!(depfile_strategy, DepfileStrategy::Injected { .. }) {
                        let _ = std::fs::remove_file(path);
                    }
                    if augment_system_headers {
                        result = crate::depgraph::depfile::merge_scan_results_conservative(
                            result,
                            crate::depgraph::scanner::scan_recursive(&source_path, &include_search),
                        );
                    }
                    result
                }
                Err(e) => {
                    used_static_fallback = true;
                    depfile_parse_warning = Some(format!("path={} error={e}", path.display()));
                    if matches!(depfile_strategy, DepfileStrategy::Injected { .. }) {
                        let _ = std::fs::remove_file(path);
                    }
                    crate::depgraph::scanner::scan_recursive(&source_path, &include_search)
                }
            }
        }
        DepfileStrategy::InjectedMmd { path } => {
            match crate::depgraph::depfile::parse_depfile_path(path, &source_path, &cwd_path) {
                Ok(result) => {
                    let _ = std::fs::remove_file(path);
                    result
                }
                Err(e) => {
                    used_static_fallback = true;
                    depfile_parse_warning = Some(format!("path={} error={e}", path.display()));
                    let _ = std::fs::remove_file(path);
                    crate::depgraph::scanner::scan_recursive(&source_path, &include_search)
                }
            }
        }
        DepfileStrategy::ShowIncludes => show_includes_scan.unwrap_or_else(|| {
            used_static_fallback = true;
            crate::depgraph::scanner::scan_recursive(&source_path, &include_search)
        }),
        DepfileStrategy::Unsupported => {
            used_static_fallback = true;
            crate::depgraph::scanner::scan_recursive(&source_path, &include_search)
        }
    };
    apply_static_fallback_policy(
        dependency_mode,
        used_static_fallback,
        &mut scan_result,
        &include_search,
    );

    CompileScanCollection {
        scan_result,
        rustc_env_dep_names: Vec::new(),
        user_depfile_capture,
        depfile_parse_warning,
    }
}

pub(super) fn apply_static_fallback_policy(
    dependency_mode: DependencyDiscoveryMode,
    used_static_fallback: bool,
    result: &mut crate::depgraph::ScanResult,
    include_search: &crate::depgraph::IncludeSearchPaths,
) {
    if used_static_fallback {
        dependency_mode.apply_static_fallback(result, include_search);
    }
}
