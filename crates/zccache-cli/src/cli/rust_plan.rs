//! `zccache rust-plan` subcommands and helpers.

use std::path::Path;
use std::process::ExitCode;
use zccache_artifact::{
    restore_rust_plan_local, rust_plan_bundle_dir, rust_plan_cache_key, save_rust_plan_local,
    RustArtifactPlanV1, RustPlanError, RustPlanOperation, RustPlanSummary,
};
use zccache_monocrate::core::NormalizedPath;
use zccache_monocrate::gha::{GhaCache, GhaError};

use super::args::{RustPlanBackendArg, RustPlanCommands};
use super::session::query_session_stats_json;
use super::targz::{tar_gz_decode, tar_gz_encode};
use super::util::{absolute_path, format_bytes, resolve_endpoint};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RustPlanRuntimeErrorKind {
    Unavailable,
    Failure,
}

#[derive(Debug)]
pub(crate) enum RustPlanRuntimeError {
    Backend {
        backend: RustPlanBackendArg,
        kind: RustPlanRuntimeErrorKind,
        message: String,
    },
}

impl RustPlanRuntimeError {
    pub(crate) fn backend(&self) -> RustPlanBackendArg {
        match self {
            Self::Backend { backend, .. } => *backend,
        }
    }

    pub(crate) fn kind(&self) -> RustPlanRuntimeErrorKind {
        match self {
            Self::Backend { kind, .. } => *kind,
        }
    }

    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Backend { message, .. } => message,
        }
    }

    pub(crate) fn with_kind(self, kind: RustPlanRuntimeErrorKind) -> Self {
        match self {
            Self::Backend {
                backend, message, ..
            } => Self::Backend {
                backend,
                kind,
                message,
            },
        }
    }
}

pub(crate) async fn cmd_rust_plan(action: RustPlanCommands) -> ExitCode {
    match action {
        RustPlanCommands::Validate {
            plan,
            json,
            session_id,
            endpoint,
            journal,
            cache_dir,
        } => {
            let cache_dir = resolve_rust_plan_cache_dir(cache_dir.as_deref());
            match load_rust_plan_for_cli(&plan, RustPlanOperation::Validate, json) {
                Ok(plan) => {
                    let mut summary =
                        RustPlanSummary::validation_success(&plan, cache_dir.as_path());
                    enrich_rust_plan_summary(
                        &mut summary,
                        session_id.as_deref(),
                        endpoint.as_deref(),
                        journal.as_deref(),
                    )
                    .await;
                    print_rust_plan_summary(&summary, json);
                    ExitCode::SUCCESS
                }
                Err(code) => code,
            }
        }
        RustPlanCommands::Restore {
            plan,
            json,
            session_id,
            endpoint,
            journal,
            backend,
            cache_dir,
        } => {
            let cache_dir = resolve_rust_plan_cache_dir(cache_dir.as_deref());
            let plan = match load_rust_plan_for_cli(&plan, RustPlanOperation::Restore, json) {
                Ok(plan) => plan,
                Err(code) => return code,
            };
            let backend = resolve_rust_plan_backend(backend);
            match run_rust_plan_restore(&plan, cache_dir.as_path(), backend).await {
                Ok(mut summary) => {
                    enrich_rust_plan_summary(
                        &mut summary,
                        session_id.as_deref(),
                        endpoint.as_deref(),
                        journal.as_deref(),
                    )
                    .await;
                    print_rust_plan_summary(&summary, json);
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    print_rust_plan_runtime_error(
                        RustPlanOperation::Restore,
                        &plan,
                        cache_dir.as_path(),
                        backend,
                        &err,
                        json,
                    );
                    ExitCode::FAILURE
                }
            }
        }
        RustPlanCommands::Save {
            plan,
            json,
            session_id,
            endpoint,
            journal,
            backend,
            cache_dir,
        } => {
            let cache_dir = resolve_rust_plan_cache_dir(cache_dir.as_deref());
            let plan = match load_rust_plan_for_cli(&plan, RustPlanOperation::Save, json) {
                Ok(plan) => plan,
                Err(code) => return code,
            };
            let backend = resolve_rust_plan_backend(backend);
            match run_rust_plan_save(&plan, cache_dir.as_path(), backend).await {
                Ok(mut summary) => {
                    enrich_rust_plan_summary(
                        &mut summary,
                        session_id.as_deref(),
                        endpoint.as_deref(),
                        journal.as_deref(),
                    )
                    .await;
                    print_rust_plan_summary(&summary, json);
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    print_rust_plan_runtime_error(
                        RustPlanOperation::Save,
                        &plan,
                        cache_dir.as_path(),
                        backend,
                        &err,
                        json,
                    );
                    ExitCode::FAILURE
                }
            }
        }
    }
}

fn resolve_rust_plan_cache_dir(explicit: Option<&str>) -> NormalizedPath {
    explicit
        .map(NormalizedPath::from)
        .unwrap_or_else(|| zccache_monocrate::core::config::default_cache_dir().join("rust-artifacts"))
}

fn load_rust_plan_for_cli(
    path: &str,
    operation: RustPlanOperation,
    json: bool,
) -> Result<RustArtifactPlanV1, ExitCode> {
    match RustArtifactPlanV1::load(Path::new(path)) {
        Ok(plan) => Ok(plan),
        Err(err) => {
            print_rust_plan_error(operation, &err, json);
            Err(ExitCode::FAILURE)
        }
    }
}

async fn run_rust_plan_restore(
    plan: &RustArtifactPlanV1,
    cache_dir: &Path,
    backend: RustPlanBackendArg,
) -> Result<RustPlanSummary, RustPlanRuntimeError> {
    match backend {
        RustPlanBackendArg::Local => restore_rust_plan_local(plan, cache_dir)
            .map_err(|err| rust_plan_backend_failure(backend, err.to_string())),
        RustPlanBackendArg::Gha => restore_rust_plan_gha(plan, cache_dir).await,
        RustPlanBackendArg::Auto => unreachable!("auto backend is resolved before execution"),
    }
}

async fn run_rust_plan_save(
    plan: &RustArtifactPlanV1,
    cache_dir: &Path,
    backend: RustPlanBackendArg,
) -> Result<RustPlanSummary, RustPlanRuntimeError> {
    match backend {
        RustPlanBackendArg::Local => save_rust_plan_local(plan, cache_dir)
            .map_err(|err| rust_plan_backend_failure(backend, err.to_string())),
        RustPlanBackendArg::Gha => save_rust_plan_gha(plan, cache_dir).await,
        RustPlanBackendArg::Auto => unreachable!("auto backend is resolved before execution"),
    }
}

fn resolve_rust_plan_backend(backend: RustPlanBackendArg) -> RustPlanBackendArg {
    match backend {
        RustPlanBackendArg::Auto if GhaCache::is_available() => RustPlanBackendArg::Gha,
        RustPlanBackendArg::Auto => RustPlanBackendArg::Local,
        other => other,
    }
}

pub(crate) fn rust_plan_gha_version(cache_key: &str) -> String {
    GhaCache::version_hash(&["zccache-rust-plan-v1", cache_key])
}

async fn restore_rust_plan_gha(
    plan: &RustArtifactPlanV1,
    cache_dir: &Path,
) -> Result<RustPlanSummary, RustPlanRuntimeError> {
    let cache_key = rust_plan_cache_key(plan);
    let version = rust_plan_gha_version(&cache_key);
    let cache = GhaCache::from_env().map_err(rust_plan_gha_error)?;
    let Some(data) = cache
        .restore(&cache_key, &version)
        .await
        .map_err(rust_plan_gha_error)?
    else {
        let mut summary = restore_rust_plan_local(plan, cache_dir)
            .map_err(|err| rust_plan_backend_failure(RustPlanBackendArg::Gha, err.to_string()))?;
        summary.set_backend("gha", Some(cache_key), Some(version));
        summary.record_skip("<gha-cache>", "backend_cache_miss");
        return Ok(summary);
    };

    let bundle_dir = rust_plan_bundle_dir(cache_dir, &cache_key);
    if bundle_dir.exists() {
        std::fs::remove_dir_all(&bundle_dir)
            .map_err(|err| rust_plan_backend_failure(RustPlanBackendArg::Gha, err.to_string()))?;
    }
    let bundle_parent = bundle_dir.parent().ok_or_else(|| {
        rust_plan_backend_failure(
            RustPlanBackendArg::Gha,
            "invalid rust-plan bundle path".to_string(),
        )
    })?;
    std::fs::create_dir_all(bundle_parent)
        .map_err(|err| rust_plan_backend_failure(RustPlanBackendArg::Gha, err.to_string()))?;
    tar_gz_decode(&data, bundle_parent)
        .map_err(|err| rust_plan_backend_failure(RustPlanBackendArg::Gha, err.to_string()))?;
    let mut summary = restore_rust_plan_local(plan, cache_dir)
        .map_err(|err| rust_plan_backend_failure(RustPlanBackendArg::Gha, err.to_string()))?;
    summary.set_backend("gha", Some(cache_key), Some(version));
    Ok(summary)
}

async fn save_rust_plan_gha(
    plan: &RustArtifactPlanV1,
    cache_dir: &Path,
) -> Result<RustPlanSummary, RustPlanRuntimeError> {
    let summary = save_rust_plan_local(plan, cache_dir)
        .map_err(|err| rust_plan_backend_failure(RustPlanBackendArg::Gha, err.to_string()))?;
    let cache_key = summary.cache_key.clone();
    let bundle_dir = rust_plan_bundle_dir(cache_dir, &cache_key);
    let data = tar_gz_encode(&bundle_dir)
        .map_err(|err| rust_plan_backend_failure(RustPlanBackendArg::Gha, err.to_string()))?;
    let version = rust_plan_gha_version(&cache_key);
    let cache = GhaCache::from_env().map_err(rust_plan_gha_error)?;
    cache
        .save(&cache_key, &version, &data)
        .await
        .map_err(rust_plan_gha_error)?;
    let mut summary = summary;
    summary.set_backend("gha", Some(cache_key), Some(version));
    Ok(summary)
}

fn rust_plan_gha_error(err: GhaError) -> RustPlanRuntimeError {
    let kind = if matches!(&err, GhaError::NotAvailable) {
        RustPlanRuntimeErrorKind::Unavailable
    } else {
        RustPlanRuntimeErrorKind::Failure
    };
    rust_plan_backend_failure(RustPlanBackendArg::Gha, err.to_string()).with_kind(kind)
}

fn rust_plan_backend_failure(backend: RustPlanBackendArg, message: String) -> RustPlanRuntimeError {
    RustPlanRuntimeError::Backend {
        backend,
        kind: RustPlanRuntimeErrorKind::Failure,
        message,
    }
}

fn rust_plan_runtime_error_message(err: &RustPlanRuntimeError) -> String {
    let backend = match err.backend() {
        RustPlanBackendArg::Local => "local",
        RustPlanBackendArg::Gha => "GHA",
        RustPlanBackendArg::Auto => unreachable!("auto backend is resolved before execution"),
    };
    let kind = match err.kind() {
        RustPlanRuntimeErrorKind::Unavailable => "unavailable",
        RustPlanRuntimeErrorKind::Failure => "failure",
    };
    format!("{backend} cache backend {kind}: {}", err.message())
}

fn rust_plan_runtime_failure_summary(
    operation: RustPlanOperation,
    plan: &RustArtifactPlanV1,
    cache_dir: &Path,
    backend: RustPlanBackendArg,
    err: &RustPlanRuntimeError,
) -> RustPlanSummary {
    let mut summary = RustPlanSummary::validation_success(plan, cache_dir);
    summary.operation = operation;
    summary.backend = match backend {
        RustPlanBackendArg::Local => "local".to_string(),
        RustPlanBackendArg::Gha => "gha".to_string(),
        RustPlanBackendArg::Auto => unreachable!("auto backend is resolved before execution"),
    };
    if matches!(backend, RustPlanBackendArg::Gha) {
        let cache_key = summary.cache_key.clone();
        summary.backend_cache_key = Some(cache_key.clone());
        summary.backend_cache_version = Some(rust_plan_gha_version(&cache_key));
    }
    summary.compatibility.status = "error".to_string();
    summary.compatibility.errors = vec![rust_plan_runtime_error_message(err)];
    summary
}

fn print_rust_plan_runtime_error(
    operation: RustPlanOperation,
    plan: &RustArtifactPlanV1,
    cache_dir: &Path,
    backend: RustPlanBackendArg,
    err: &RustPlanRuntimeError,
    json: bool,
) {
    if json {
        let summary = rust_plan_runtime_failure_summary(operation, plan, cache_dir, backend, err);
        print_rust_plan_summary(&summary, true);
    } else {
        eprintln!(
            "zccache rust-plan: {}",
            rust_plan_runtime_error_message(err)
        );
    }
}

async fn enrich_rust_plan_summary(
    summary: &mut RustPlanSummary,
    session_id: Option<&str>,
    endpoint: Option<&str>,
    journal: Option<&str>,
) {
    if let Some(journal) = journal {
        summary.journal_log_path = Some(absolute_path(journal));
    }

    if let Some(session_id) = session_id {
        let endpoint = resolve_endpoint(endpoint);
        summary.compile_cache_stats = Some(query_session_stats_json(&endpoint, session_id).await);
    }
}

fn print_rust_plan_summary(summary: &RustPlanSummary, json: bool) {
    if json {
        match serde_json::to_string_pretty(summary) {
            Ok(s) => println!("{s}"),
            Err(err) => eprintln!("zccache rust-plan: failed to encode JSON summary: {err}"),
        }
        return;
    }

    println!(
        "zccache rust-plan {}: {}",
        match summary.operation {
            RustPlanOperation::Validate => "validate",
            RustPlanOperation::Restore => "restore",
            RustPlanOperation::Save => "save",
        },
        summary.compatibility.status
    );
    println!("  mode: {}", summary.mode);
    println!("  backend: {}", summary.backend);
    println!("  cache key: {}", summary.cache_key);
    if let Some(key) = &summary.backend_cache_key {
        println!("  backend cache key: {key}");
    }
    if let Some(version) = &summary.backend_cache_version {
        println!("  backend cache version: {version}");
    }
    if let Some(path) = &summary.archive_path {
        println!("  bundle: {}", path.display());
    }
    if summary.saved_file_count > 0 || summary.saved_bytes > 0 {
        println!(
            "  saved: {} files ({})",
            summary.saved_file_count,
            format_bytes(summary.saved_bytes)
        );
    }
    if summary.restored_file_count > 0 || summary.restored_bytes > 0 {
        println!(
            "  restored: {} files ({})",
            summary.restored_file_count,
            format_bytes(summary.restored_bytes)
        );
    }
    if summary.skipped_count > 0 {
        println!("  skipped: {}", summary.skipped_count);
        for (reason, count) in &summary.skipped_reasons {
            println!("    {reason}: {count}");
        }
    }
    for mismatch in &summary.key_input_mismatches {
        println!("  mismatch: {mismatch}");
    }
    if let Some(stats) = &summary.compile_cache_stats {
        println!("  compile cache stats: {stats}");
    }
}

fn print_rust_plan_error(operation: RustPlanOperation, err: &RustPlanError, json: bool) {
    if json {
        let summary = RustPlanSummary::compatibility_failure(operation, err);
        print_rust_plan_summary(&summary, true);
    } else {
        eprintln!("zccache rust-plan: {err}");
    }
}
