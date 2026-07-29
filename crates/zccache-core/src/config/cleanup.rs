//! Cleanup helpers for legacy and stale daemon state.
//!
//! * [`cleanup_legacy_temp_root_state`] — older builds wrote cache state directly
//!   under `%TEMP%`. We remove those exact-match legacy paths when they're safe
//!   (i.e. not the current cache dir and not its ancestor).
//! * [`cleanup_stale_depfile_dirs`] — sweep `{pid}-{instance}` subdirs in the
//!   current depfile dir; remove ones whose PID is no longer alive.

use super::paths::depfile_dir;
use std::path::Path;
use std::time::Duration;

/// How old a depfile directory must be before it is reclaimed even though its
/// PID still looks alive (#1165 finding 6).
///
/// `is_alive(pid)` is the primary signal, but PIDs are reused. Once the OS
/// hands a dead daemon's number to an unrelated long-running process, that
/// daemon's directory is pinned for as long as the new process lives — which
/// on a busy machine with high PID churn is indefinitely.
///
/// Age is the backstop. A depfile directory belongs to one daemon instance and
/// is removed on clean shutdown, so one this old is debris regardless of what
/// currently holds its number. The window is deliberately generous: reclaiming
/// a live daemon's directory breaks a build, while leaving debris a few days
/// longer costs only disk.
const DEPFILE_DIR_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Remove legacy zccache state left directly under the OS temp directory.
///
/// Older builds stored the full cache root at `%TEMP%/.zccache` and created
/// depfile directories as `%TEMP%/zccache-depfiles-*`. Those paths are safe to
/// remove only when they are exact legacy matches and do not point at the
/// current cache directory.
pub fn cleanup_legacy_temp_root_state<F>(
    temp_root: &Path,
    current_cache_dir: &Path,
    is_alive: F,
) -> usize
where
    F: Fn(u32) -> bool,
{
    let mut cleaned = cleanup_legacy_temp_cache_dir(temp_root, current_cache_dir);
    cleaned += cleanup_legacy_temp_depfile_dirs(temp_root, is_alive);
    cleaned
}

/// Remove stale depfile directories from previous (dead) daemon instances.
///
/// Scans [`depfile_dir()`] for subdirectories matching `{pid}-{instance}`.
/// If the PID is no longer alive (per `is_alive`), removes the directory.
/// Returns the number of directories cleaned up.
pub fn cleanup_stale_depfile_dirs<F>(is_alive: F) -> usize
where
    F: Fn(u32) -> bool,
{
    sweep_depfile_dirs(depfile_dir().as_path(), is_alive, DEPFILE_DIR_MAX_AGE)
}

/// [`cleanup_stale_depfile_dirs`] with an explicit base directory and age
/// floor, so tests can exercise both sides of the gate without touching the
/// process-global depfile root or sleeping for a week.
pub(super) fn sweep_depfile_dirs<F>(base: &Path, is_alive: F, max_age: Duration) -> usize
where
    F: Fn(u32) -> bool,
{
    let entries = match std::fs::read_dir(base) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };

    let mut cleaned = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let pid: u32 = match name.split('-').next().and_then(|s| s.parse().ok()) {
            Some(p) => p,
            None => continue,
        };
        // #1165: a reused PID would otherwise pin this directory forever, so
        // age reclaims it regardless of what currently holds that number.
        let expired = dir_is_older_than(&path, max_age);
        if !is_alive(pid) || expired {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {
                    cleaned += 1;
                    tracing::info!(
                        path = %path.display(),
                        reason = if expired { "age" } else { "dead_pid" },
                        "removed stale depfile dir"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        "failed to remove stale depfile dir: {e}"
                    );
                }
            }
        }
    }
    cleaned
}

/// Is this directory old enough that a live-looking PID can only mean reuse?
///
/// Errs toward "no": an unreadable mtime, or a clock that makes the directory
/// look like it is from the future, both return false. Skipping real debris
/// costs disk until the next sweep; reclaiming a live daemon's depfile
/// directory breaks its build. Same bias as the staging-root age gate.
fn dir_is_older_than(path: &Path, max_age: Duration) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    let Ok(age) = std::time::SystemTime::now().duration_since(modified) else {
        return false;
    };
    age >= max_age
}

fn cleanup_legacy_temp_cache_dir(temp_root: &Path, current_cache_dir: &Path) -> usize {
    let legacy_cache_dir = temp_root.join(".zccache");
    if path_is_or_contains(&legacy_cache_dir, current_cache_dir) {
        return 0;
    }

    if !is_real_dir(&legacy_cache_dir) {
        return 0;
    }

    match std::fs::remove_dir_all(&legacy_cache_dir) {
        Ok(()) => {
            tracing::info!(path = %legacy_cache_dir.display(), "removed legacy temp cache dir");
            1
        }
        Err(e) => {
            tracing::warn!(
                path = %legacy_cache_dir.display(),
                "failed to remove legacy temp cache dir: {e}"
            );
            0
        }
    }
}

fn cleanup_legacy_temp_depfile_dirs<F>(temp_root: &Path, is_alive: F) -> usize
where
    F: Fn(u32) -> bool,
{
    let entries = match std::fs::read_dir(temp_root) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };

    let mut cleaned = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = entry.file_name();
        let name = match file_name.to_str() {
            Some(name) if name.starts_with("zccache-depfiles-") => name,
            _ => continue,
        };

        if !is_real_dir(&path) {
            continue;
        }

        let pid = match legacy_temp_depfile_pid(name) {
            Some(pid) => pid,
            None => continue,
        };

        if is_alive(pid) {
            continue;
        }

        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                cleaned += 1;
                tracing::info!(path = %path.display(), "removed legacy temp depfile dir");
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    "failed to remove legacy temp depfile dir: {e}"
                );
            }
        }
    }
    cleaned
}

fn legacy_temp_depfile_pid(name: &str) -> Option<u32> {
    let suffix = name.strip_prefix("zccache-depfiles-")?;
    suffix.split('-').next()?.parse().ok()
}

fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_dir())
        .unwrap_or(false)
}

fn path_is_or_contains(parent: &Path, child: &Path) -> bool {
    if child.starts_with(parent) {
        return true;
    }

    let parent = match std::fs::canonicalize(parent) {
        Ok(parent) => parent,
        Err(_) => return false,
    };
    std::fs::canonicalize(child)
        .map(|child| child.starts_with(parent))
        .unwrap_or(false)
}
