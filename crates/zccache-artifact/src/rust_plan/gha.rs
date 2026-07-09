//! In-process Rust-plan save/restore against the GitHub Actions cache backend.
//!
//! This is the CLI-independent library API lifted out of
//! `cli/commands/rust_plan.rs` (issue #960). It is callable in-process so that
//! soldr (soldr#1368) can drive GHA rust-plan save/restore without shelling out
//! to the `zccache rust-plan --backend gha` subcommand.
//!
//! The public surface is `save_rust_plan_gha`, `restore_rust_plan_gha`, and
//! `rust_plan_gha_version`, all re-exported from the crate root.

use std::path::Path;

use zccache_gha::{GhaCache, GhaError};

use super::local::{
    restore_rust_plan_local, rust_plan_bundle_dir, rust_plan_cache_key, save_rust_plan_local,
};
use super::schema::RustArtifactPlanV1;
use super::summary::RustPlanSummary;
use super::targz::{tar_gz_decode, tar_gz_encode};

/// Library-native error for the Rust-plan GHA backend.
///
/// Distinguishes a not-configured backend (`GhaError::NotAvailable`) from a
/// genuine save/restore failure so callers can decide whether to fall back to
/// the local backend. Intentionally free of any CLI types.
#[derive(Debug)]
pub enum RustPlanGhaError {
    /// The GHA cache backend is not available (not running inside GitHub
    /// Actions, or the cache environment variables are unset).
    Unavailable(String),
    /// The GHA save/restore operation failed.
    Failure(String),
}

impl RustPlanGhaError {
    /// The human-readable message describing the failure.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Unavailable(message) | Self::Failure(message) => message,
        }
    }

    /// Whether this error means the backend is unavailable (vs. a hard failure).
    #[must_use]
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

impl std::fmt::Display for RustPlanGhaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(message) => write!(f, "GHA cache backend unavailable: {message}"),
            Self::Failure(message) => write!(f, "GHA cache backend failure: {message}"),
        }
    }
}

impl std::error::Error for RustPlanGhaError {}

fn gha_error(err: GhaError) -> RustPlanGhaError {
    let message = err.to_string();
    if matches!(err, GhaError::NotAvailable) {
        RustPlanGhaError::Unavailable(message)
    } else {
        RustPlanGhaError::Failure(message)
    }
}

/// Run blocking Rust-plan work on Tokio's blocking pool, surfacing failures as
/// `RustPlanGhaError::Failure`.
async fn run_rust_plan_backend_blocking<F, T>(work: F) -> Result<T, RustPlanGhaError>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| RustPlanGhaError::Failure(format!("blocking rust-plan task failed: {err}")))?
        .map_err(RustPlanGhaError::Failure)
}

/// GHA cache version string for a Rust-plan cache key.
#[must_use]
pub fn rust_plan_gha_version(cache_key: &str) -> String {
    GhaCache::version_hash(&["zccache-rust-plan-v2-protobuf", cache_key])
}

/// Restore a Rust-plan bundle from the GitHub Actions cache into `cache_dir`.
///
/// On a cache miss, falls back to a local restore and records the miss in the
/// returned summary. Returns `RustPlanGhaError::Unavailable` when the GHA
/// backend is not configured.
///
/// Consumed in-process by soldr (soldr#1368).
pub async fn restore_rust_plan_gha(
    plan: &RustArtifactPlanV1,
    cache_dir: &Path,
) -> Result<RustPlanSummary, RustPlanGhaError> {
    let cache_key = rust_plan_cache_key(plan);
    let version = rust_plan_gha_version(&cache_key);
    let cache = GhaCache::from_env().map_err(gha_error)?;
    let Some(data) = cache
        .restore(&cache_key, &version)
        .await
        .map_err(gha_error)?
    else {
        let plan = plan.clone();
        let cache_dir = cache_dir.to_path_buf();
        let mut summary = run_rust_plan_backend_blocking(move || {
            restore_rust_plan_local(&plan, &cache_dir).map_err(|err| err.to_string())
        })
        .await?;
        summary.set_backend("gha", Some(cache_key), Some(version));
        summary.record_skip("<gha-cache>", "backend_cache_miss");
        return Ok(summary);
    };

    let plan = plan.clone();
    let cache_dir = cache_dir.to_path_buf();
    run_rust_plan_backend_blocking(move || {
        let bundle_dir = rust_plan_bundle_dir(&cache_dir, &cache_key);
        if bundle_dir.exists() {
            std::fs::remove_dir_all(&bundle_dir).map_err(|err| err.to_string())?;
        }
        let bundle_parent = bundle_dir
            .parent()
            .ok_or_else(|| "invalid rust-plan bundle path".to_string())?;
        std::fs::create_dir_all(bundle_parent).map_err(|err| err.to_string())?;
        tar_gz_decode(&data, bundle_parent).map_err(|err| err.to_string())?;
        let mut summary =
            restore_rust_plan_local(&plan, &cache_dir).map_err(|err| err.to_string())?;
        summary.set_backend("gha", Some(cache_key), Some(version));
        Ok(summary)
    })
    .await
}

/// Save a Rust-plan bundle from `cache_dir` into the GitHub Actions cache.
///
/// Runs a local save first (to produce the bundle), then uploads the tar.gz to
/// the GHA cache. Returns `RustPlanGhaError::Unavailable` when the GHA backend
/// is not configured.
///
/// Consumed in-process by soldr (soldr#1368).
pub async fn save_rust_plan_gha(
    plan: &RustArtifactPlanV1,
    cache_dir: &Path,
) -> Result<RustPlanSummary, RustPlanGhaError> {
    let plan = plan.clone();
    let cache_dir = cache_dir.to_path_buf();
    let (summary, data) = run_rust_plan_backend_blocking(move || {
        let summary = save_rust_plan_local(&plan, &cache_dir).map_err(|err| err.to_string())?;
        let cache_key = summary.cache_key.clone();
        let bundle_dir = rust_plan_bundle_dir(&cache_dir, &cache_key);
        let data = tar_gz_encode(&bundle_dir).map_err(|err| err.to_string())?;
        Ok((summary, data))
    })
    .await?;
    let cache_key = summary.cache_key.clone();
    let version = rust_plan_gha_version(&cache_key);
    let cache = GhaCache::from_env().map_err(gha_error)?;
    cache
        .save(&cache_key, &version, &data)
        .await
        .map_err(gha_error)?;
    let mut summary = summary;
    summary.set_backend("gha", Some(cache_key), Some(version));
    Ok(summary)
}
