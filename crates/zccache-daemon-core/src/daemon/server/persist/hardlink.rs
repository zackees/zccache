//! Cross-platform hardlink helpers: hardlink-detach (write-without-mutating-cache)
//! and the failure-injection seams its tests use. File identity, link
//! counts, and permission mechanics live behind `crate::platform`.

use super::*;

#[cfg(test)]
static FAIL_DETACH_REMOVE_PATHS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<PathBuf>>,
> = std::sync::OnceLock::new();
#[cfg(test)]
static FAIL_DETACH_RENAME_PATHS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<PathBuf>>,
> = std::sync::OnceLock::new();

pub(in crate::daemon::server) fn remove_output_file(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if let Ok(mut injected) = FAIL_DETACH_REMOVE_PATHS
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
        .lock()
    {
        if injected.remove(path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected detach remove failure",
            ));
        }
    }
    std::fs::remove_file(path)
}

fn rename_detached_output(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if let Ok(mut injected) = FAIL_DETACH_RENAME_PATHS
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
        .lock()
    {
        if injected.remove(to) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected detach rename failure",
            ));
        }
    }
    std::fs::rename(from, to)
}

#[cfg(test)]
pub(in crate::daemon::server) fn fail_detach_remove_for_test(path: &Path) {
    FAIL_DETACH_REMOVE_PATHS
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
        .lock()
        .expect("detach failure injection lock")
        .insert(path.to_path_buf());
}

#[cfg(test)]
pub(in crate::daemon::server) fn fail_detach_rename_for_test(path: &Path) {
    FAIL_DETACH_RENAME_PATHS
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
        .lock()
        .expect("detach rename failure injection lock")
        .insert(path.to_path_buf());
}

pub(in crate::daemon::server) fn break_output_hardlink_before_compile(
    path: &Path,
) -> std::io::Result<()> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }

    if crate::platform::fs::links::hard_link_count(path)? <= 1 {
        crate::platform::fs::permissions::make_writable(path)?;
        return Ok(());
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("output"))
        .to_string_lossy();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();

    let mut last_err = None;
    for attempt in 0..32 {
        let tmp_path = parent.join(format!(
            ".zccache-detach-{pid}-{nonce}-{attempt}-{file_name}"
        ));
        let copy_result = (|| {
            let mut src = std::fs::File::open(path)?;
            let mut dst = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)?;
            std::io::copy(&mut src, &mut dst)?;
            dst.sync_all()?;
            let permissions = src.metadata()?.permissions();
            std::fs::set_permissions(&tmp_path, permissions)?;
            Ok::<(), std::io::Error>(())
        })();

        match copy_result {
            Ok(()) => {
                let registration = prepare_registered_detach(path);
                if let Err(error) = crate::platform::fs::permissions::make_writable(path) {
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(error);
                }
                if let Err(e) = remove_output_file(path) {
                    if let Some((_, blob_path)) = &registration {
                        let _ = crate::platform::fs::permissions::set_readonly(
                            blob_path,
                            readonly_enabled(),
                        );
                    }
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(e);
                }
                if let Err(e) = rename_detached_output(&tmp_path, path) {
                    if let Some((id, blob_path)) = &registration {
                        let _ = crate::platform::fs::permissions::set_readonly(
                            blob_path,
                            readonly_enabled(),
                        );
                        commit_registered_detach(*id, path);
                    }
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(e);
                }
                if let Some((id, _)) = &registration {
                    commit_registered_detach(*id, path);
                }
                crate::platform::fs::permissions::make_writable(path)?;
                if let Some((_, blob_path)) = registration {
                    let _ = crate::platform::fs::permissions::set_readonly(
                        &blob_path,
                        readonly_enabled(),
                    );
                }
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_err = Some(e);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(e);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "failed to create hardlink detach temp file",
        )
    }))
}
