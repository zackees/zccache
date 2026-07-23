//! Shared bounded disk-retention policy for standalone and embedded services.
//!
//! Every entry point receives one exact artifact root.  This module never
//! discovers a parent directory or a sibling product root (issue #1148).

use super::*;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

pub(crate) const CACHE_BYTES_ENV: &str = "ZCCACHE_CACHE_SIZE_BYTES";
pub(crate) const CACHE_PERCENT_ENV: &str = "ZCCACHE_CACHE_SIZE_PERCENT";
const GIB: u64 = 1024 * 1024 * 1024;
const SOFT_AGE: Duration = Duration::from_secs(4 * 24 * 60 * 60);
const EXPIRE_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const PRESSURE_INTERVAL: Duration = Duration::from_secs(5 * 60);
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_secs(1);
const FULL_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const FULL_MARKER: &str = ".disk-maintenance-last-full-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MaintenanceKind {
    Pressure,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BudgetSpec {
    Dynamic,
    Bytes(u64),
    Percent(u8),
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MaintenancePolicy {
    budget: BudgetSpec,
}

impl Default for MaintenancePolicy {
    fn default() -> Self {
        Self {
            budget: BudgetSpec::Dynamic,
        }
    }
}

impl MaintenancePolicy {
    pub(crate) fn from_env() -> Result<Self, String> {
        Self::from_values(
            std::env::var(CACHE_BYTES_ENV).ok().as_deref(),
            std::env::var(CACHE_PERCENT_ENV).ok().as_deref(),
        )
    }

    fn from_values(bytes: Option<&str>, percent: Option<&str>) -> Result<Self, String> {
        match (
            bytes.filter(|v| !v.trim().is_empty()),
            percent.filter(|v| !v.trim().is_empty()),
        ) {
            (Some(_), Some(_)) => Err(format!(
                "{CACHE_BYTES_ENV} and {CACHE_PERCENT_ENV} are mutually exclusive"
            )),
            (Some(value), None) => {
                let bytes = value.parse::<u64>().map_err(|_| {
                    format!("{CACHE_BYTES_ENV} must be a positive integer byte count")
                })?;
                if bytes == 0 {
                    return Err(format!("{CACHE_BYTES_ENV} must be greater than zero"));
                }
                Ok(Self {
                    budget: BudgetSpec::Bytes(bytes),
                })
            }
            (None, Some(value)) => {
                let percent = value.parse::<u8>().map_err(|_| {
                    format!("{CACHE_PERCENT_ENV} must be an integer from 1 through 100")
                })?;
                if !(1..=100).contains(&percent) {
                    return Err(format!(
                        "{CACHE_PERCENT_ENV} must be an integer from 1 through 100"
                    ));
                }
                Ok(Self {
                    budget: BudgetSpec::Percent(percent),
                })
            }
            (None, None) => Ok(Self::default()),
        }
    }

    pub(crate) fn from_limits(bytes: Option<u64>, percent: Option<u8>) -> Result<Self, String> {
        match (bytes, percent) {
            (Some(_), Some(_)) => {
                Err("max_cache_bytes and max_cache_percent are mutually exclusive".to_string())
            }
            (Some(0), None) => Err("max_cache_bytes must be greater than zero".to_string()),
            (Some(bytes), None) => Ok(Self {
                budget: BudgetSpec::Bytes(bytes),
            }),
            (None, Some(percent)) if !(1..=100).contains(&percent) => {
                Err("max_cache_percent must be an integer from 1 through 100".to_string())
            }
            (None, Some(percent)) => Ok(Self {
                budget: BudgetSpec::Percent(percent),
            }),
            (None, None) => Ok(Self::default()),
        }
    }

    fn budget_bytes(self, capacity_bytes: u64) -> u64 {
        let requested = match self.budget {
            BudgetSpec::Dynamic => capacity_bytes
                .saturating_mul(5)
                .checked_div(100)
                .unwrap_or(0)
                .clamp(40 * GIB, 200 * GIB),
            BudgetSpec::Bytes(bytes) => bytes,
            BudgetSpec::Percent(percent) => capacity_bytes
                .saturating_mul(u64::from(percent))
                .checked_div(100)
                .unwrap_or(0),
        };
        // Keep the requested recovery reserve attainable on ordinary disks.
        // On small disks, half the volume is the largest feasible reserve.
        let recovery_reserve = recovery_free_bytes(capacity_bytes);
        requested.min(capacity_bytes.saturating_sub(recovery_reserve))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FilesystemSpace {
    pub(crate) capacity_bytes: u64,
    pub(crate) free_bytes: u64,
}

pub(crate) trait MaintenanceEnvironment: Send + Sync {
    fn now(&self) -> SystemTime;
    fn filesystem_space(&self, root: &Path) -> io::Result<FilesystemSpace>;
}

pub(crate) struct RealMaintenanceEnvironment;

impl MaintenanceEnvironment for RealMaintenanceEnvironment {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn filesystem_space(&self, root: &Path) -> io::Result<FilesystemSpace> {
        Ok(FilesystemSpace {
            capacity_bytes: fs2::total_space(root)?,
            free_bytes: fs2::available_space(root)?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MaintenancePressure {
    None,
    Soft,
    Hard,
}

#[derive(Debug, Clone)]
pub(crate) struct DiskMaintenanceReport {
    pub(crate) kind: MaintenanceKind,
    pub(crate) pressure: MaintenancePressure,
    pub(crate) budget_bytes: u64,
    pub(crate) usage_before_bytes: u64,
    pub(crate) usage_after_bytes: u64,
    pub(crate) bytes_reclaimed: u64,
    pub(crate) artifacts_removed: usize,
    pub(crate) expired_artifacts_removed: usize,
    pub(crate) pending_write_bytes: u64,
}

#[derive(Debug)]
struct DiskArtifact {
    key: String,
    allocated_bytes: u64,
    last_access: SystemTime,
    legacy_files: Vec<NormalizedPath>,
    staged: bool,
    staged_generation: Option<String>,
}

#[derive(Debug)]
struct MaintenancePlan {
    pressure: MaintenancePressure,
    selected: Vec<String>,
    expired: HashSet<String>,
}

fn low_space_bytes(capacity: u64) -> u64 {
    capacity
        .saturating_mul(5)
        .checked_div(100)
        .unwrap_or(0)
        .max(20 * GIB)
        .min(capacity)
}

fn recovery_free_bytes(capacity: u64) -> u64 {
    let desired = capacity
        .saturating_mul(8)
        .checked_div(100)
        .unwrap_or(0)
        .max(30 * GIB);
    // On small volumes the absolute 30 GiB target is not simultaneously
    // attainable with a useful cache. Reserve at most half the volume so the
    // dynamic budget remains non-zero (and exceeds 10 GiB once capacity is
    // above 20 GiB).
    desired.min(capacity / 2)
}

fn age(now: SystemTime, then: SystemTime) -> Duration {
    now.duration_since(then).unwrap_or_default()
}

#[cfg(test)]
fn plan_maintenance(
    policy: MaintenancePolicy,
    kind: MaintenanceKind,
    now: SystemTime,
    space: FilesystemSpace,
    artifacts: &[DiskArtifact],
    pending_write_bytes: u64,
) -> MaintenancePlan {
    plan_maintenance_at_least(
        policy,
        kind,
        now,
        space,
        artifacts,
        pending_write_bytes,
        MaintenancePressure::None,
    )
}

fn plan_maintenance_at_least(
    policy: MaintenancePolicy,
    kind: MaintenanceKind,
    now: SystemTime,
    space: FilesystemSpace,
    artifacts: &[DiskArtifact],
    pending_write_bytes: u64,
    minimum_pressure: MaintenancePressure,
) -> MaintenancePlan {
    let budget = policy.budget_bytes(space.capacity_bytes);
    let artifact_bytes = artifacts.iter().fold(0_u64, |total, artifact| {
        total.saturating_add(artifact.allocated_bytes)
    });
    let usage = artifact_bytes.saturating_add(pending_write_bytes);
    let mut selected = HashSet::new();
    let mut reclaimed = 0_u64;
    let mut expired = HashSet::new();

    if kind == MaintenanceKind::Full {
        for artifact in artifacts
            .iter()
            .filter(|artifact| age(now, artifact.last_access) > EXPIRE_AGE)
        {
            selected.insert(artifact.key.clone());
            expired.insert(artifact.key.clone());
            reclaimed = reclaimed.saturating_add(artifact.allocated_bytes);
        }
    }

    let projected_usage = usage.saturating_sub(reclaimed);
    let projected_free = space.free_bytes.saturating_add(reclaimed);
    let hard = projected_usage >= budget || projected_free < low_space_bytes(space.capacity_bytes);
    let soft = !hard && projected_usage >= budget.saturating_mul(85) / 100;
    let detected_pressure = if hard {
        MaintenancePressure::Hard
    } else if soft {
        MaintenancePressure::Soft
    } else {
        MaintenancePressure::None
    };
    let pressure = match (minimum_pressure, detected_pressure) {
        (MaintenancePressure::Hard, _) | (_, MaintenancePressure::Hard) => {
            MaintenancePressure::Hard
        }
        (MaintenancePressure::Soft, _) | (_, MaintenancePressure::Soft) => {
            MaintenancePressure::Soft
        }
        _ => MaintenancePressure::None,
    };

    let desired_reclaim = match pressure {
        MaintenancePressure::Hard => {
            let usage_need = projected_usage.saturating_sub(budget.saturating_mul(80) / 100);
            let free_need =
                recovery_free_bytes(space.capacity_bytes).saturating_sub(projected_free);
            usage_need.max(free_need)
        }
        MaintenancePressure::Soft => {
            projected_usage.saturating_sub(budget.saturating_mul(70) / 100)
        }
        MaintenancePressure::None => 0,
    };

    if desired_reclaim > 0 {
        let mut candidates: Vec<&DiskArtifact> = artifacts
            .iter()
            .filter(|artifact| !selected.contains(&artifact.key))
            .filter(|artifact| {
                pressure == MaintenancePressure::Hard || age(now, artifact.last_access) > SOFT_AGE
            })
            .collect();
        candidates.sort_by_key(|artifact| artifact.last_access);
        let mut pressure_reclaimed = 0_u64;
        for artifact in candidates {
            if pressure_reclaimed >= desired_reclaim {
                break;
            }
            selected.insert(artifact.key.clone());
            pressure_reclaimed = pressure_reclaimed.saturating_add(artifact.allocated_bytes);
        }
    }

    let mut selected: Vec<String> = selected.into_iter().collect();
    selected.sort();
    MaintenancePlan {
        pressure,
        selected,
        expired,
    }
}

fn add_file(
    path: &Path,
    artifact: &mut DiskArtifact,
    seen: &mut HashSet<FileId>,
) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Ok(());
    }
    artifact.last_access = artifact
        .last_access
        .max(metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH));
    if get_file_id(path).is_none_or(|id| seen.insert(id)) {
        artifact.allocated_bytes = artifact
            .allocated_bytes
            .saturating_add(allocated_bytes(path, &metadata));
    }
    Ok(())
}

#[cfg(unix)]
fn allocated_bytes(_path: &Path, metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn allocated_bytes(path: &Path, metadata: &std::fs::Metadata) -> u64 {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
    use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;

    let path = windows_verbatim_file_path(path).unwrap_or_else(|_| path.into());
    let wide: Vec<u16> = path
        .as_path()
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut high = 0_u32;
    unsafe {
        SetLastError(0);
        let low = GetCompressedFileSizeW(wide.as_ptr(), &mut high);
        windows_allocated_size_result(low, high, GetLastError(), metadata.len())
    }
}

#[cfg(any(test, windows))]
fn windows_allocated_size_result(low: u32, high: u32, error: u32, fallback: u64) -> u64 {
    if low == u32::MAX && error != 0 {
        fallback
    } else {
        (u64::from(high) << 32) | u64::from(low)
    }
}

#[cfg(windows)]
fn is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn validate_owned_artifact_root(artifact_dir: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(artifact_dir)?;
    if is_link_or_reparse(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing disk maintenance through linked artifact root: {}",
                artifact_dir.display()
            ),
        ));
    }
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "artifact root is not a directory: {}",
                artifact_dir.display()
            ),
        ));
    }
    validate_staged_artifact_root(artifact_dir)?;
    Ok(())
}

fn add_tree(
    root: &Path,
    artifact: &mut DiskArtifact,
    seen: &mut HashSet<FileId>,
) -> io::Result<()> {
    if root.is_file() {
        return add_file(root, artifact, seen);
    }
    if !root.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)?.flatten() {
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if is_link_or_reparse(&metadata) {
            continue;
        }
        if metadata.is_dir() {
            add_tree(&path, artifact, seen)?;
        } else if metadata.is_file() {
            add_file(&path, artifact, seen)?;
        }
    }
    Ok(())
}

fn legacy_key(name: &str) -> Option<(&str, Option<usize>)> {
    if name.starts_with('.') {
        return None;
    }
    let (key, output_index) = if let Some(key) = name.strip_suffix(".meta") {
        (key, None)
    } else if let Some(key) = name.strip_suffix(".pack") {
        (key, None)
    } else {
        let (key, output) = name.rsplit_once('_')?;
        let output_index = output.parse::<usize>().ok()?;
        (key, Some(output_index))
    };
    (key.len() <= 128 && !key.is_empty()).then_some((key, output_index))
}

fn scan_artifacts(artifact_dir: &Path) -> io::Result<Vec<DiskArtifact>> {
    let mut groups: HashMap<String, DiskArtifact> = HashMap::new();
    let mut seen = HashSet::new();
    if !artifact_dir.is_dir() {
        return Ok(Vec::new());
    }
    for entry in std::fs::read_dir(artifact_dir)?.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((key, output_index)) = legacy_key(&name) else {
            continue;
        };
        if let Some(output_index) = output_index {
            crate::artifact::record_legacy_artifact_access(
                &path,
                key,
                output_index,
                crate::artifact::LegacyArtifactAccessPurpose::EvictionScan,
                "daemon::disk_maintenance::scan_artifacts",
            );
        }
        let artifact = groups
            .entry(key.to_string())
            .or_insert_with(|| DiskArtifact {
                key: key.to_string(),
                allocated_bytes: 0,
                last_access: SystemTime::UNIX_EPOCH,
                legacy_files: Vec::new(),
                staged: false,
                staged_generation: None,
            });
        add_file(&path, artifact, &mut seen)?;
        artifact.legacy_files.push(path.into());
    }

    let staged_root = artifact_dir.join(".staged-v2");
    for staged in scan_v2_disk_artifacts(artifact_dir)? {
        let key = staged.key;
        let artifact = groups.entry(key.clone()).or_insert_with(|| DiskArtifact {
            key: key.clone(),
            allocated_bytes: 0,
            last_access: SystemTime::UNIX_EPOCH,
            legacy_files: Vec::new(),
            staged: true,
            staged_generation: None,
        });
        artifact.staged = true;
        artifact.staged_generation = staged.generation;
        artifact.last_access = artifact.last_access.max(staged.mtime);
        add_tree(&staged_root.join(&key), artifact, &mut seen)?;
        add_file(
            &staged_root.join(format!("{key}.current")),
            artifact,
            &mut seen,
        )
        .ok();
    }
    Ok(groups.into_values().collect())
}

struct MaintenancePass<'a> {
    artifact_dir: &'a Path,
    artifacts: &'a DashMap<String, CachedArtifact>,
    artifact_store: &'a ArtifactStore,
    index_writer_tx: Option<&'a tokio::sync::mpsc::UnboundedSender<IndexWriterCommand>>,
    dep_graph: &'a DepGraph,
    pending_write_bytes: u64,
    policy: MaintenancePolicy,
    kind: MaintenanceKind,
    environment: &'a dyn MaintenanceEnvironment,
}

fn refresh_live_access(
    scanned: &mut [DiskArtifact],
    artifacts: &DashMap<String, CachedArtifact>,
    now: SystemTime,
) {
    for artifact in scanned {
        if let Some(cached) = artifacts.get(&artifact.key) {
            let access = cached.access_snapshot();
            let live_last_use = if access.used_in_process {
                now.checked_sub(access.last_used.elapsed()).unwrap_or(now)
            } else {
                access.last_used_wall.min(now)
            };
            artifact.last_access = artifact.last_access.max(live_last_use);
        }
    }
}

fn remove_planned_artifacts(
    artifact_dir: &Path,
    scanned: &[DiskArtifact],
    plan: &MaintenancePlan,
    artifacts: &DashMap<String, CachedArtifact>,
    artifact_store: &ArtifactStore,
    index_writer_tx: Option<&tokio::sync::mpsc::UnboundedSender<IndexWriterCommand>>,
    dep_graph: &DepGraph,
) -> io::Result<Vec<String>> {
    let planned: HashSet<&str> = plan.selected.iter().map(String::as_str).collect();
    let staged_expected: HashMap<String, Option<String>> = scanned
        .iter()
        .filter(|artifact| artifact.staged && planned.contains(artifact.key.as_str()))
        .map(|artifact| (artifact.key.clone(), artifact.staged_generation.clone()))
        .collect();
    let staged_removed = evict_v2_artifact_keys_if_unchanged(artifact_dir, &staged_expected)?;
    let selected: HashSet<&str> = scanned
        .iter()
        .filter(|artifact| {
            planned.contains(artifact.key.as_str())
                && (!artifact.staged || staged_removed.contains(&artifact.key))
        })
        .map(|artifact| artifact.key.as_str())
        .collect();
    for artifact in scanned
        .iter()
        .filter(|artifact| selected.contains(artifact.key.as_str()))
    {
        for file in &artifact.legacy_files {
            if let Err(error) = remove_cow_blob(file) {
                if error.kind() != io::ErrorKind::NotFound {
                    return Err(error);
                }
            }
        }
    }
    let removed_keys: Vec<String> = plan
        .selected
        .iter()
        .filter(|key| selected.contains(key.as_str()))
        .cloned()
        .collect();
    if removed_keys.is_empty() {
        return Ok(removed_keys);
    }

    let keys: Vec<&str> = removed_keys.iter().map(String::as_str).collect();
    // The caller owns the publication barrier's write side. Every cache hit
    // that acquired an owned read lease has therefore finished payload
    // discovery and enqueued its access checkpoint; the subsequent Remove is
    // ordered after every such Insert. New lookups cannot acquire a lease and
    // rehydrate the entry from ArtifactStore in the gap.
    artifact_store.remove_batch(&keys);
    dep_graph.invalidate_artifact_keys(&removed_keys.iter().cloned().collect());
    for key in &removed_keys {
        artifacts.remove(key);
    }
    if let Some(tx) = index_writer_tx {
        let _ = tx.send(IndexWriterCommand::Remove(removed_keys.clone()));
    }
    Ok(removed_keys)
}

#[cfg(test)]
fn maintain_disk_artifacts(pass: MaintenancePass<'_>) -> io::Result<DiskMaintenanceReport> {
    maintain_disk_artifacts_with_barrier(pass, None)
}

fn maintain_disk_artifacts_with_barrier(
    pass: MaintenancePass<'_>,
    publication_barrier: Option<&Arc<tokio::sync::RwLock<()>>>,
) -> io::Result<DiskMaintenanceReport> {
    let MaintenancePass {
        artifact_dir,
        artifacts,
        artifact_store,
        index_writer_tx,
        dep_graph,
        pending_write_bytes,
        policy,
        kind,
        environment,
    } = pass;

    validate_owned_artifact_root(artifact_dir)?;
    let now = environment.now();
    let initial_space = environment.filesystem_space(artifact_dir)?;
    let mut scanned = scan_artifacts(artifact_dir)?;
    refresh_live_access(&mut scanned, artifacts, now);
    let usage_before = scanned.iter().fold(pending_write_bytes, |total, artifact| {
        total.saturating_add(artifact.allocated_bytes)
    });
    let mut pressure = MaintenancePressure::None;
    let mut removed = HashSet::new();
    let mut expired_removed = HashSet::new();

    loop {
        let space = environment.filesystem_space(artifact_dir)?;
        let plan = plan_maintenance_at_least(
            policy,
            kind,
            now,
            space,
            &scanned,
            pending_write_bytes,
            pressure,
        );
        if plan.selected.is_empty() {
            pressure = plan.pressure;
            break;
        }
        let (plan, removed_this_round) = if let Some(publication_barrier) = publication_barrier {
            // Filesystem scanning and candidate planning are read-only and may
            // take seconds on a large cache. Only exclude cache hits for the
            // short destructive commit, then refresh live access and re-plan
            // under the writer so a hit/publication that raced the scan is
            // not evicted as stale.
            let _publication_guard = publication_barrier.blocking_write();
            refresh_live_access(&mut scanned, artifacts, now);
            let commit_space = environment.filesystem_space(artifact_dir)?;
            let commit_plan = plan_maintenance_at_least(
                policy,
                kind,
                now,
                commit_space,
                &scanned,
                pending_write_bytes,
                pressure,
            );
            let removed = remove_planned_artifacts(
                artifact_dir,
                &scanned,
                &commit_plan,
                artifacts,
                artifact_store,
                index_writer_tx,
                dep_graph,
            )?;
            (commit_plan, removed)
        } else {
            let removed = remove_planned_artifacts(
                artifact_dir,
                &scanned,
                &plan,
                artifacts,
                artifact_store,
                index_writer_tx,
                dep_graph,
            )?;
            (plan, removed)
        };
        pressure = plan.pressure;
        if removed_this_round.is_empty() {
            break;
        }
        for key in removed_this_round {
            if plan.expired.contains(&key) {
                expired_removed.insert(key.clone());
            }
            removed.insert(key);
        }
        scanned = scan_artifacts(artifact_dir)?;
        refresh_live_access(&mut scanned, artifacts, now);
    }

    let usage_after = scanned.iter().fold(pending_write_bytes, |total, artifact| {
        total.saturating_add(artifact.allocated_bytes)
    });
    Ok(DiskMaintenanceReport {
        kind,
        pressure,
        budget_bytes: policy.budget_bytes(initial_space.capacity_bytes),
        usage_before_bytes: usage_before,
        usage_after_bytes: usage_after,
        bytes_reclaimed: usage_before.saturating_sub(usage_after),
        artifacts_removed: removed.len(),
        expired_artifacts_removed: expired_removed.len(),
        pending_write_bytes,
    })
}

fn full_marker_path(cache_dir: &Path) -> NormalizedPath {
    cache_dir.join(FULL_MARKER).into()
}

fn full_maintenance_due(cache_dir: &Path, now: SystemTime) -> bool {
    let Ok(contents) = std::fs::read_to_string(full_marker_path(cache_dir)) else {
        return true;
    };
    let Ok(last_secs) = contents.trim().parse::<u64>() else {
        return true;
    };
    let now_secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    last_secs > now_secs || now_secs - last_secs >= FULL_INTERVAL.as_secs()
}

fn record_full_maintenance(cache_dir: &Path, now: SystemTime) -> io::Result<()> {
    std::fs::create_dir_all(cache_dir)?;
    let marker = full_marker_path(cache_dir);
    let temp = cache_dir.join(format!("{FULL_MARKER}.{}.tmp", std::process::id()));
    let now_secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    std::fs::write(&temp, format!("{now_secs}\n"))?;
    match std::fs::rename(&temp, &marker) {
        Ok(()) => Ok(()),
        Err(error) if marker.exists() => {
            std::fs::remove_file(&marker)?;
            std::fs::rename(&temp, &marker).map_err(|_| error)
        }
        Err(error) => Err(error),
    }
}

pub(super) async fn maintain_state_disk(
    state: Arc<SharedState>,
    policy: MaintenancePolicy,
    kind: MaintenanceKind,
) -> io::Result<DiskMaintenanceReport> {
    let _maintenance_guard = state.disk_maintenance.lock().await;
    if state.shutdown_requested.load(Ordering::Acquire) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "disk maintenance skipped because daemon shutdown is in progress",
        ));
    }
    let cache_dir = state.cache_dir.clone();
    let index_writer_tx = state.index_writer_tx.clone();
    let maintenance_index_writer_tx = index_writer_tx.clone();
    let pending_write_bytes = state.in_flight_bytes.load(Ordering::Relaxed) as u64;
    let maintenance_state = Arc::clone(&state);
    let report = tokio::task::spawn_blocking(move || {
        let dep_graph = maintenance_state.dep_graph.load_full();
        maintain_disk_artifacts_with_barrier(
            MaintenancePass {
                artifact_dir: maintenance_state.artifact_dir.as_path(),
                artifacts: &maintenance_state.artifacts,
                artifact_store: &maintenance_state.artifact_store,
                index_writer_tx: Some(&maintenance_index_writer_tx),
                dep_graph: &dep_graph,
                pending_write_bytes,
                policy,
                kind,
                environment: &RealMaintenanceEnvironment,
            },
            Some(&maintenance_state.artifact_publication),
        )
    })
    .await
    .map_err(io::Error::other)??;

    if report.artifacts_removed > 0
        && !flush_index_writer(&index_writer_tx, Duration::from_secs(30)).await
    {
        return Err(io::Error::other(
            "artifact-index removals did not flush before the maintenance deadline",
        ));
    }

    if kind == MaintenanceKind::Full {
        record_full_maintenance(cache_dir.as_path(), SystemTime::now())?;
    }
    tracing::info!(
        maintenance_kind = ?report.kind,
        pressure = ?report.pressure,
        budget_bytes = report.budget_bytes,
        usage_before_bytes = report.usage_before_bytes,
        usage_after_bytes = report.usage_after_bytes,
        bytes_reclaimed = report.bytes_reclaimed,
        artifacts_removed = report.artifacts_removed,
        expired_artifacts_removed = report.expired_artifacts_removed,
        pending_write_bytes = report.pending_write_bytes,
        cache_root = %cache_dir.display(),
        "disk maintenance pass complete"
    );
    Ok(report)
}

async fn wait_for_artifact_load(state: &SharedState) -> bool {
    loop {
        if state.shutdown_requested.load(Ordering::Acquire) {
            return false;
        }
        if state.artifacts_loaded.load(Ordering::Acquire)
            && state.dep_graph_load_complete.load(Ordering::Acquire)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn pressure_scan_needed(state: &SharedState, policy: MaintenancePolicy) -> io::Result<bool> {
    let space = RealMaintenanceEnvironment.filesystem_space(state.artifact_dir.as_path())?;
    if space.free_bytes < low_space_bytes(space.capacity_bytes) {
        return Ok(true);
    }
    let indexed = state.artifact_store.total_size_bytes();
    let pending = state.in_flight_bytes.load(Ordering::Relaxed) as u64;
    // A deliberately early logical gate keeps ordinary five-minute checks
    // memory-only. Near half-budget we pay for exact allocated-block/FileId
    // accounting; daily full passes always do the physical scan.
    Ok(indexed.saturating_add(pending)
        >= policy.budget_bytes(space.capacity_bytes).saturating_mul(50) / 100)
}

/// Wait for the next maintenance pass without consuming the daemon's shared
/// shutdown notification.
///
/// The accept loop also waits on `state.shutdown`. Waiting on that same
/// edge-triggered `Notify` here can both steal a legacy `notify_one()` signal
/// and miss `notify_waiters()` between checking `shutdown_requested` and
/// registering the waiter. Polling the durable atomic flag keeps shutdown
/// bounded without either race.
async fn wait_for_next_pass_or_shutdown(
    shutdown_requested: &AtomicBool,
    interval: Duration,
    poll_interval: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + interval;
    loop {
        if shutdown_requested.load(Ordering::Acquire) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        tokio::time::sleep(remaining.min(poll_interval)).await;
    }
}

pub(super) fn spawn_disk_maintenance(
    state: Arc<SharedState>,
    policy: MaintenancePolicy,
    runtime_handle: Option<&tokio::runtime::Handle>,
) -> tokio::task::JoinHandle<()> {
    let task = async move {
        if !wait_for_artifact_load(&state).await {
            return;
        }
        loop {
            if state.shutdown_requested.load(Ordering::Acquire) {
                break;
            }
            let kind = if full_maintenance_due(state.cache_dir.as_path(), SystemTime::now()) {
                MaintenanceKind::Full
            } else {
                MaintenanceKind::Pressure
            };
            if kind == MaintenanceKind::Pressure {
                match pressure_scan_needed(&state, policy) {
                    Ok(false) => {
                        tracing::debug!(
                            cache_root = %state.cache_dir.display(),
                            "disk maintenance skipped exact scan below logical preflight threshold"
                        );
                        if wait_for_next_pass_or_shutdown(
                            &state.shutdown_requested,
                            PRESSURE_INTERVAL,
                            SHUTDOWN_POLL_INTERVAL,
                        )
                        .await
                        {
                            break;
                        }
                        continue;
                    }
                    Ok(true) => {}
                    Err(error) => tracing::warn!(%error, "disk maintenance preflight failed"),
                }
            }
            if let Err(error) = maintain_state_disk(Arc::clone(&state), policy, kind).await {
                tracing::warn!(
                    %error,
                    maintenance_kind = ?kind,
                    cache_root = %state.cache_dir.display(),
                    "disk maintenance pass failed"
                );
            }
            if state.shutdown_requested.load(Ordering::Acquire) {
                break;
            }
            if wait_for_next_pass_or_shutdown(
                &state.shutdown_requested,
                PRESSURE_INTERVAL,
                SHUTDOWN_POLL_INTERVAL,
            )
            .await
            {
                break;
            }
        }
    };
    match runtime_handle {
        Some(handle) => handle.spawn(task),
        None => tokio::spawn(task),
    }
}

#[cfg(test)]
#[path = "tests/disk_maintenance_unit.rs"]
mod tests;
