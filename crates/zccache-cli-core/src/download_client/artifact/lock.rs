use kernal_api::platform::fs::OwnedFileLock;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use crate::download::stable_download_id;

use super::resolve::ResolvedFetchRequest;
use super::WaitMode;

pub(super) struct FetchLock {
    // The lock owns the handle: under `fs2` the open `File` *was* the token,
    // released when it closed. `OwnedFileLock` says that rather than implying
    // it, and still releases on drop.
    _lock: OwnedFileLock,
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
    let lock = match request.wait_mode {
        WaitMode::Block => {
            kernal_api::platform::fs::lock_exclusive_owned(file).map_err(|e| e.to_string())?
        }
        WaitMode::NoWait => kernal_api::platform::fs::try_lock_exclusive_owned(file)
            .map_err(|_| "locked".to_string())?,
    };
    Ok(FetchLock { _lock: lock })
}

fn fetch_lock_path(request: &ResolvedFetchRequest) -> PathBuf {
    let mut key = crate::core::normalize_for_key(&request.cache_path);
    if let Some(expanded_path) = &request.expanded_path {
        key.push('\n');
        key.push_str(&crate::core::normalize_for_key(expanded_path));
    }
    let hash = stable_download_id(Path::new(&key));
    crate::core::config::daemon_state_dir()
        .join("downloads")
        .join("locks")
        .join(format!("{hash}.lock"))
        .into_path_buf()
}
