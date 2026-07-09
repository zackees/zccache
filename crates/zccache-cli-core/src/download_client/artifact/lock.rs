use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use crate::download::stable_download_id;

use super::resolve::ResolvedFetchRequest;
use super::WaitMode;

pub(super) struct FetchLock {
    _file: File,
}

pub(super) fn acquire_fetch_lock(request: &ResolvedFetchRequest) -> Result<FetchLock, String> {
    let lock_path = fetch_lock_path(request);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| e.to_string())?;
    match request.wait_mode {
        WaitMode::Block => fs2::FileExt::lock_exclusive(&file).map_err(|e| e.to_string())?,
        WaitMode::NoWait => {
            if fs2::FileExt::try_lock_exclusive(&file).is_err() {
                return Err("locked".to_string());
            }
        }
    }
    Ok(FetchLock { _file: file })
}

fn fetch_lock_path(request: &ResolvedFetchRequest) -> PathBuf {
    let mut key = crate::core::normalize_for_key(&request.cache_path);
    if let Some(expanded_path) = &request.expanded_path {
        key.push('\n');
        key.push_str(&crate::core::normalize_for_key(expanded_path));
    }
    let hash = stable_download_id(Path::new(&key));
    crate::core::config::default_cache_dir()
        .join("downloads")
        .join("locks")
        .join(format!("{hash}.lock"))
        .into_path_buf()
}
