//! Local, delta, and layered Rust artifact plan save/restore execution.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rayon::prelude::*;
use zccache_core::{normalize_for_key, NormalizedPath};

use super::manifest::{
    read_bundle_manifest, safe_join, validate_manifest, write_bundle_manifest,
    RustArtifactBundleLayerKind, RustArtifactBundleManifest, RustBundledArtifact, BUNDLE_FILES_DIR,
};
use super::schema::{
    ensure_supported_cache_schema_version, RustArtifactClass, RustArtifactPlanV1, RustPlanError,
    RUST_ARTIFACT_CACHE_SCHEMA_VERSION,
};
use super::selection::{resolve_cargo_artifacts, select_artifacts, SelectedArtifact};
use super::summary::{RustPlanOperation, RustPlanSummary};
use super::threads::resolve_rust_plan_tar_threads;

pub fn rust_plan_cache_key(plan: &RustArtifactPlanV1) -> String {
    let identity = rust_plan_identity_hash(plan);
    format!("rust-plan-v1-{}", &identity[..32])
}

/// Compute the stable identity hash used by manifests.
///
/// The hash folds in `cache_profile` and `dropped_artifact_classes` (added
/// in soldr#461) so a thin-v1 bundle and a thin-v2 bundle for the same
/// otherwise-identical inputs get different keys and never alias each
/// other â€” they ship different file sets and would corrupt each other's
/// restore expectations if the cache_key collided.
#[must_use]
pub fn rust_plan_identity_hash(plan: &RustArtifactPlanV1) -> String {
    let mut dropped: Vec<RustArtifactClass> = plan.dropped_artifact_classes.clone();
    dropped.sort();
    dropped.dedup();
    let payload = serde_json::json!({
        "schema_version": plan.schema_version,
        "mode": plan.mode,
        "workspace_root": normalize_for_key(plan.workspace_root.as_path()),
        "target_dir": normalize_for_key(plan.target_dir.as_path()),
        "toolchain": plan.toolchain,
        "target_triple": plan.target_triple,
        "profile": plan.profile,
        "inputs": plan.inputs,
        "packages": plan.packages,
        "allowed_artifact_classes": plan.effective_allowed_classes().into_iter().collect::<Vec<_>>(),
        "cache_schema_version": plan.cache_schema_version,
        "cache_profile": plan.cache_profile,
        "dropped_artifact_classes": dropped,
    });
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    zccache_hash::hash_bytes(&bytes).to_hex()
}

/// Bundle directory for a plan cache key.
#[must_use]
pub fn rust_plan_bundle_dir(cache_dir: &Path, cache_key: &str) -> NormalizedPath {
    NormalizedPath::new(cache_dir.join("rust-plan").join(cache_key))
}

/// Execute local bundle save for a validated plan.
pub fn save_rust_plan_local(
    plan: &RustArtifactPlanV1,
    cache_dir: &Path,
) -> Result<RustPlanSummary, RustPlanError> {
    plan.validate()?;
    ensure_supported_cache_schema_version(plan.cache_schema_version)?;
    let cache_key = rust_plan_cache_key(plan);
    let bundle_dir = rust_plan_bundle_dir(cache_dir, &cache_key);
    let files_dir = bundle_dir.join(BUNDLE_FILES_DIR);
    let mut summary = RustPlanSummary::new(
        RustPlanOperation::Save,
        plan.mode,
        plan.schema_version,
        plan.cache_schema_version,
        cache_key.clone(),
        Some(bundle_dir.clone()),
        plan.journal_log_path.clone(),
    );

    let mut candidates = resolve_cargo_artifacts(plan)?;
    candidates.sort();

    let selected = select_artifacts(plan, candidates, &mut summary);

    if bundle_dir.exists() {
        remove_bundle_dir(&bundle_dir)?;
    }
    std::fs::create_dir_all(&files_dir)?;

    let artifacts = bundle_selected_artifacts(&selected, &files_dir)?;
    summary.saved_file_count += artifacts.len() as u64;
    summary.saved_bytes += artifacts.iter().map(|a| a.size).sum::<u64>();

    let manifest = RustArtifactBundleManifest {
        manifest_schema_version: RUST_ARTIFACT_CACHE_SCHEMA_VERSION,
        plan_schema_version: plan.schema_version,
        cache_schema_version: plan.cache_schema_version,
        mode: plan.mode,
        cache_key,
        created_at_secs: now_secs(),
        plan_identity_hash: rust_plan_identity_hash(plan),
        artifacts,
        layer_kind: RustArtifactBundleLayerKind::Complete,
        base_cache_key: None,
        deleted_paths: Vec::new(),
    };
    write_bundle_manifest(&bundle_dir, &manifest)?;
    Ok(summary)
}

/// Copy + stat + hash every selected artifact into `files_dir`, returning the
/// manifest entries in input order (which `select_artifacts` has already sorted
/// by `relative_path` for determinism). Reads `resolve_rust_plan_tar_threads()`
/// for the parallelism setting â€” see issue #177 for the Windows-CI motivation.
fn bundle_selected_artifacts(
    selected: &[SelectedArtifact],
    files_dir: &Path,
) -> Result<Vec<RustBundledArtifact>, RustPlanError> {
    bundle_selected_artifacts_with_threads(selected, files_dir, resolve_rust_plan_tar_threads())
}

/// Same as `bundle_selected_artifacts`, but with `threads` injected so tests
/// can exercise the parallel path without racing on process-global env vars.
pub(super) fn bundle_selected_artifacts_with_threads(
    selected: &[SelectedArtifact],
    files_dir: &Path,
    threads: usize,
) -> Result<Vec<RustBundledArtifact>, RustPlanError> {
    if threads <= 1 || selected.len() < 2 {
        return selected
            .iter()
            .map(|sel| bundle_one_artifact(sel, files_dir))
            .collect();
    }

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|idx| format!("zccache-rust-plan-{idx}"))
        .build()
        .map_err(|err| {
            RustPlanError::Io(std::io::Error::other(format!(
                "failed to build rust-plan thread pool: {err}"
            )))
        })?;

    pool.install(|| {
        selected
            .par_iter()
            .map(|sel| bundle_one_artifact(sel, files_dir))
            .collect()
    })
}

fn bundle_one_artifact(
    sel: &SelectedArtifact,
    files_dir: &Path,
) -> Result<RustBundledArtifact, RustPlanError> {
    let dst = safe_join(files_dir, &sel.relative_path)?;
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    snapshot_to_bundle(&sel.source_path, &dst)?;
    snapshot_selected_artifact(sel)
}

fn snapshot_selected_artifact(
    sel: &SelectedArtifact,
) -> Result<RustBundledArtifact, RustPlanError> {
    let metadata = std::fs::metadata(&sel.source_path)?;
    let size = metadata.len();
    let content_hash = zccache_hash::hash_file(&sel.source_path)?.to_hex();
    Ok(RustBundledArtifact {
        relative_path: sel.relative_path.clone(),
        class: sel.class,
        size,
        content_hash,
        mtime_unix_nanos: metadata
            .modified()
            .ok()
            .map(system_time_to_unix_nanos)
            .unwrap_or(0),
    })
}

/// Decide how many worker threads to use for the rust-plan save copy+hash loop.
///
/// Grammar (mirrors `SOLDR_TARGET_CACHE_TAR_THREADS` validated upstream by
/// soldr#273):
/// - unset / `auto` / empty / unparseable â†’ vCPU-bounded, capped at 8
/// - `1` â†’ sequential (regression escape hatch)
/// - positive integer N â†’ `min(N, MAX_RUST_PLAN_TAR_THREADS)`
///
/// `ZCCACHE_RUST_PLAN_TAR_THREADS` takes precedence over the soldr-side var so
pub fn restore_rust_plan_local(
    plan: &RustArtifactPlanV1,
    cache_dir: &Path,
) -> Result<RustPlanSummary, RustPlanError> {
    plan.validate()?;
    ensure_supported_cache_schema_version(plan.cache_schema_version)?;
    let cache_key = rust_plan_cache_key(plan);
    let bundle_dir = rust_plan_bundle_dir(cache_dir, &cache_key);
    let mut summary = RustPlanSummary::new(
        RustPlanOperation::Restore,
        plan.mode,
        plan.schema_version,
        plan.cache_schema_version,
        cache_key.clone(),
        Some(bundle_dir.clone()),
        plan.journal_log_path.clone(),
    );

    if !bundle_dir.exists() {
        summary.skip("<bundle>", "artifact_absent_from_restored_plan");
        summary.refresh_effectiveness(0);
        return Ok(summary);
    }

    let manifest = read_bundle_manifest(&bundle_dir)?;
    if !validate_manifest(plan, &cache_key, &manifest, &mut summary)? {
        summary.refresh_effectiveness(0);
        return Ok(summary);
    }

    let eligible = manifest.artifacts.len() as u64;
    restore_manifest_artifacts(plan, &bundle_dir, &manifest, &mut summary, false)?;

    summary.refresh_effectiveness(eligible);
    Ok(summary)
}

fn restore_manifest_artifacts(
    plan: &RustArtifactPlanV1,
    bundle_dir: &Path,
    manifest: &RustArtifactBundleManifest,
    summary: &mut RustPlanSummary,
    allow_overwrite_existing: bool,
) -> Result<(), RustPlanError> {
    let files_dir = bundle_dir.join(BUNDLE_FILES_DIR);
    for deleted_path in &manifest.deleted_paths {
        let dst = match safe_join(plan.target_dir.as_path(), deleted_path) {
            Ok(path) => path,
            Err(err) => {
                summary.skip(deleted_path, "path_traversal");
                summary.compatibility.errors.push(err.to_string());
                continue;
            }
        };
        if dst.exists() {
            std::fs::remove_file(&dst)?;
        }
    }

    let verify_blake3 = restore_verify_blake3_enabled();
    let threads = resolve_rust_plan_tar_threads();
    let outcomes = if threads <= 1 || manifest.artifacts.len() < 2 {
        manifest
            .artifacts
            .iter()
            .map(|artifact| {
                restore_one_artifact(
                    plan,
                    &files_dir,
                    artifact,
                    verify_blake3,
                    allow_overwrite_existing,
                )
            })
            .collect::<Vec<_>>()
    } else {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|idx| format!("zccache-rust-plan-{idx}"))
            .build()
            .map_err(|err| {
                RustPlanError::Io(std::io::Error::other(format!(
                    "failed to build rust-plan thread pool: {err}"
                )))
            })?;
        pool.install(|| {
            manifest
                .artifacts
                .par_iter()
                .map(|artifact| {
                    restore_one_artifact(
                        plan,
                        &files_dir,
                        artifact,
                        verify_blake3,
                        allow_overwrite_existing,
                    )
                })
                .collect()
        })
    };

    // Rayon preserves the input order when collecting, so merging here keeps
    // counts, error ordering, and the bounded skip sample deterministic.
    for outcome in outcomes {
        match outcome {
            RestoreArtifactOutcome::Restored { bytes } => {
                summary.restored_file_count += 1;
                summary.restored_bytes += bytes;
            }
            RestoreArtifactOutcome::Skipped {
                path,
                reason,
                error,
            } => {
                summary.skip(path, reason);
                if let Some(error) = error {
                    summary.compatibility.errors.push(error);
                }
            }
            RestoreArtifactOutcome::Error { path, error } => {
                summary.compatibility.status = "warning".to_string();
                summary
                    .compatibility
                    .errors
                    .push(format!("{path}: {error}"));
            }
        }
    }

    Ok(())
}

const RESTORE_VERIFY_BLAKE3_ENV: &str = "ZCCACHE_RUST_PLAN_RESTORE_VERIFY_BLAKE3";

fn restore_verify_blake3_enabled() -> bool {
    std::env::var(RESTORE_VERIFY_BLAKE3_ENV)
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
}

enum RestoreArtifactOutcome {
    Restored {
        bytes: u64,
    },
    Skipped {
        path: String,
        reason: &'static str,
        error: Option<String>,
    },
    Error {
        path: String,
        error: String,
    },
}

fn restore_one_artifact(
    plan: &RustArtifactPlanV1,
    files_dir: &Path,
    artifact: &RustBundledArtifact,
    verify_blake3: bool,
    allow_overwrite_existing: bool,
) -> RestoreArtifactOutcome {
    let path = artifact.relative_path.clone();
    let src = match safe_join(files_dir, &path) {
        Ok(path) => path,
        Err(err) => {
            return RestoreArtifactOutcome::Skipped {
                path,
                reason: "path_traversal",
                error: Some(err.to_string()),
            };
        }
    };
    let dst = match safe_join(plan.target_dir.as_path(), &path) {
        Ok(path) => path,
        Err(err) => {
            return RestoreArtifactOutcome::Skipped {
                path,
                reason: "path_traversal",
                error: Some(err.to_string()),
            };
        }
    };
    let Ok(metadata) = std::fs::metadata(&src) else {
        return RestoreArtifactOutcome::Skipped {
            path,
            reason: "restored_payload_missing_or_corrupt",
            error: None,
        };
    };
    if metadata.len() != artifact.size {
        return RestoreArtifactOutcome::Skipped {
            path,
            reason: "restored_payload_missing_or_corrupt",
            error: None,
        };
    }
    if verify_blake3 {
        let Ok(content_hash) = zccache_hash::hash_file(&src).map(|hash| hash.to_hex()) else {
            return RestoreArtifactOutcome::Skipped {
                path,
                reason: "restored_payload_missing_or_corrupt",
                error: None,
            };
        };
        if content_hash != artifact.content_hash {
            return RestoreArtifactOutcome::Skipped {
                path,
                reason: "restored_payload_missing_or_corrupt",
                error: None,
            };
        }
    }
    if dst.exists() && !allow_overwrite_existing {
        let matches_manifest = std::fs::metadata(&dst).is_ok_and(|metadata| {
            metadata.len() == artifact.size
                && zccache_hash::hash_file(&dst)
                    .map(|hash| hash.to_hex() == artifact.content_hash)
                    .unwrap_or(false)
        });
        return RestoreArtifactOutcome::Skipped {
            path,
            reason: if matches_manifest {
                "destination_already_present"
            } else {
                "destination_conflict"
            },
            error: None,
        };
    }
    let result = (|| -> std::io::Result<()> {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        restore_bundle_file(&src, &dst)?;
        if let Ok(file) = std::fs::File::open(&dst) {
            let modified = if artifact.mtime_unix_nanos == 0 {
                SystemTime::now()
            } else {
                unix_nanos_to_system_time(artifact.mtime_unix_nanos)
            };
            let file_times = std::fs::FileTimes::new()
                .set_accessed(modified)
                .set_modified(modified);
            let _ = file.set_times(file_times);
        }
        Ok(())
    })();
    match result {
        Ok(()) => RestoreArtifactOutcome::Restored {
            bytes: artifact.size,
        },
        Err(error) => RestoreArtifactOutcome::Error {
            path,
            error: error.to_string(),
        },
    }
}
pub fn save_rust_plan_delta_local(
    plan: &RustArtifactPlanV1,
    base_cache_dir: &Path,
    delta_cache_dir: &Path,
) -> Result<RustPlanSummary, RustPlanError> {
    plan.validate()?;
    ensure_supported_cache_schema_version(plan.cache_schema_version)?;
    let cache_key = rust_plan_cache_key(plan);
    let base_bundle_dir = rust_plan_bundle_dir(base_cache_dir, &cache_key);
    let delta_bundle_dir = rust_plan_bundle_dir(delta_cache_dir, &cache_key);
    let delta_files_dir = delta_bundle_dir.join(BUNDLE_FILES_DIR);
    let mut summary = RustPlanSummary::new(
        RustPlanOperation::Save,
        plan.mode,
        plan.schema_version,
        plan.cache_schema_version,
        cache_key.clone(),
        Some(delta_bundle_dir.clone()),
        plan.journal_log_path.clone(),
    );

    let base_manifest = if base_bundle_dir.exists() {
        let manifest = read_bundle_manifest(&base_bundle_dir)?;
        if validate_manifest(plan, &cache_key, &manifest, &mut summary)? {
            Some(manifest)
        } else {
            None
        }
    } else {
        summary.record_skip("<base-bundle>", "base_bundle_missing_for_delta");
        None
    };
    let base_artifacts: BTreeMap<String, RustBundledArtifact> = base_manifest
        .as_ref()
        .map(|manifest| {
            manifest
                .artifacts
                .iter()
                .map(|artifact| (artifact.relative_path.clone(), artifact.clone()))
                .collect()
        })
        .unwrap_or_default();

    let mut candidates = resolve_cargo_artifacts(plan)?;
    candidates.sort();
    let selected = select_artifacts(plan, candidates, &mut summary);

    if delta_bundle_dir.exists() {
        remove_bundle_dir(&delta_bundle_dir)?;
    }
    std::fs::create_dir_all(&delta_files_dir)?;

    let mut current_paths = BTreeSet::new();
    let mut artifacts = Vec::new();
    for sel in &selected {
        current_paths.insert(sel.relative_path.clone());
        let snapshot = snapshot_selected_artifact(sel)?;
        let unchanged = base_artifacts
            .get(&sel.relative_path)
            .map(|base| {
                base.size == snapshot.size
                    && base.content_hash == snapshot.content_hash
                    && base.mtime_unix_nanos == snapshot.mtime_unix_nanos
            })
            .unwrap_or(false);
        if unchanged {
            continue;
        }
        let dst = safe_join(&delta_files_dir, &sel.relative_path)?;
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        snapshot_to_bundle(&sel.source_path, &dst)?;
        artifacts.push(snapshot);
    }

    let deleted_paths = base_artifacts
        .keys()
        .filter(|path| !current_paths.contains(*path))
        .cloned()
        .collect::<Vec<_>>();

    summary.saved_file_count += artifacts.len() as u64;
    summary.saved_bytes += artifacts.iter().map(|artifact| artifact.size).sum::<u64>();

    let manifest = RustArtifactBundleManifest {
        manifest_schema_version: RUST_ARTIFACT_CACHE_SCHEMA_VERSION,
        plan_schema_version: plan.schema_version,
        cache_schema_version: plan.cache_schema_version,
        mode: plan.mode,
        cache_key: cache_key.clone(),
        created_at_secs: now_secs(),
        plan_identity_hash: rust_plan_identity_hash(plan),
        artifacts,
        layer_kind: RustArtifactBundleLayerKind::Delta,
        base_cache_key: Some(cache_key),
        deleted_paths,
    };
    write_bundle_manifest(&delta_bundle_dir, &manifest)?;
    Ok(summary)
}

/// Restore a base bundle and then overlay a delta bundle.
pub fn restore_rust_plan_layered_local(
    plan: &RustArtifactPlanV1,
    base_cache_dir: &Path,
    delta_cache_dir: &Path,
) -> Result<RustPlanSummary, RustPlanError> {
    plan.validate()?;
    ensure_supported_cache_schema_version(plan.cache_schema_version)?;
    let cache_key = rust_plan_cache_key(plan);
    let base_bundle_dir = rust_plan_bundle_dir(base_cache_dir, &cache_key);
    let delta_bundle_dir = rust_plan_bundle_dir(delta_cache_dir, &cache_key);
    let mut summary = RustPlanSummary::new(
        RustPlanOperation::Restore,
        plan.mode,
        plan.schema_version,
        plan.cache_schema_version,
        cache_key.clone(),
        Some(delta_bundle_dir.clone()),
        plan.journal_log_path.clone(),
    );

    let mut eligible = 0_u64;
    if base_bundle_dir.exists() {
        let manifest = read_bundle_manifest(&base_bundle_dir)?;
        if validate_manifest(plan, &cache_key, &manifest, &mut summary)? {
            eligible += manifest.artifacts.len() as u64;
            restore_manifest_artifacts(plan, &base_bundle_dir, &manifest, &mut summary, true)?;
        }
    } else {
        summary.record_skip("<base-bundle>", "base_bundle_missing_for_layered_restore");
    }

    if delta_bundle_dir.exists() {
        let manifest = read_bundle_manifest(&delta_bundle_dir)?;
        if validate_manifest(plan, &cache_key, &manifest, &mut summary)? {
            eligible += manifest.artifacts.len() as u64;
            restore_manifest_artifacts(plan, &delta_bundle_dir, &manifest, &mut summary, true)?;
        }
    } else {
        summary.record_skip("<delta-bundle>", "delta_bundle_missing_for_layered_restore");
    }

    summary.refresh_effectiveness(eligible);
    Ok(summary)
}
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn snapshot_to_bundle(src: &Path, dst: &Path) -> std::io::Result<()> {
    if reflink_copy::reflink(src, dst).is_err() {
        std::fs::copy(src, dst)?;
    }
    let mut permissions = std::fs::metadata(dst)?.permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(dst, permissions)
}

fn restore_bundle_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    // Rust-plan bundles are immutable. Reflink is ideal; where unavailable we
    // copy instead of hardlinking because this crate does not own the daemon's
    // mediated-write registry and must not create an untracked shared writer.
    if reflink_copy::reflink(src, dst).is_err() {
        std::fs::copy(src, dst)?;
    }
    make_bundle_file_writable(dst)
}

#[cfg_attr(windows, allow(clippy::permissions_set_readonly_false))]
fn make_bundle_file_writable(path: &Path) -> std::io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(permissions.mode() | 0o200);
    }
    #[cfg(windows)]
    permissions.set_readonly(false);
    std::fs::set_permissions(path, permissions)
}

fn remove_bundle_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    fn clear_readonly(path: &Path) -> std::io::Result<()> {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let child = entry.path();
            if entry.file_type()?.is_dir() {
                clear_readonly(&child)?;
            } else {
                if entry.metadata()?.permissions().readonly() {
                    make_bundle_file_writable(&child)?;
                }
            }
        }
        Ok(())
    }
    #[cfg(windows)]
    clear_readonly(path)?;
    std::fs::remove_dir_all(path)
}

pub(super) fn system_time_to_unix_nanos(time: SystemTime) -> u64 {
    let Ok(duration) = time.duration_since(UNIX_EPOCH) else {
        return 0;
    };
    duration
        .as_secs()
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::from(duration.subsec_nanos()))
}

pub(super) fn unix_nanos_to_system_time(nanos: u64) -> SystemTime {
    UNIX_EPOCH + std::time::Duration::from_nanos(nanos)
}
