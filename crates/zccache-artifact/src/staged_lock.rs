//! The staged-v2 store lock, shared by every process that touches a generation.
//!
//! # Why this lives here and not in the daemon
//!
//! A staged generation can be deleted by daemon maintenance at any moment.
//! Anything that *resolves* a generation path and then *materializes* it
//! (hardlink/reflink/copy into a caller-owned destination) must hold the
//! shared read lock across the whole window, or the tree can vanish between
//! the two — yielding a failed restore at best and a truncated artifact that
//! cargo accepts as a valid rlib at worst.
//!
//! The lock is an `fs2` advisory lock on a real file, so it is **cross-process
//! by construction**. That matters because the readers are not all in the
//! daemon: `zccache warm` runs entirely in the CLI process (it opens the redb
//! index directly and never sends an IPC request) while a daemon may be
//! concurrently running GC.
//!
//! Both sides therefore have to agree on *which* file to lock. Keeping one
//! copy of that decision in `zccache-artifact` — the crate both the daemon and
//! `warm` already consume for layout resolution — is what makes the exclusion
//! real. Two independent definitions of `.staged-v2/.store.lock` would keep
//! compiling, keep passing their own tests, and silently stop excluding each
//! other the moment either side changed a name.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// The staged-v2 store directory name, relative to the artifact dir.
pub const STAGED_ROOT: &str = ".staged-v2";
/// The lock file inside [`STAGED_ROOT`]. Exposed because callers that enumerate
/// the staged root must exclude it from "is this tree empty" checks.
pub const STORE_LOCK: &str = ".store.lock";

/// `<artifact_dir>/.staged-v2`.
pub fn staged_root(artifact_dir: &Path) -> PathBuf {
    artifact_dir.join(STAGED_ROOT)
}

/// Whether `metadata` describes a symlink (unix) or any reparse point
/// (Windows). Staged access refuses to traverse either.
pub fn is_staged_link_or_reparse(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// `Ok(false)` when the root is simply absent; `Err` when it exists but is
/// something we refuse to treat as the staged root (a link/reparse point, or
/// not a directory).
pub fn validate_staged_root_path(root: &Path) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if is_staged_link_or_reparse(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing staged artifact access through linked/reparse root: {}",
                root.display()
            ),
        ));
    }
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "staged artifact root is not a directory: {}",
                root.display()
            ),
        ));
    }
    Ok(true)
}

/// Open (creating if needed) the store lock file for an already-validated root.
pub fn open_store_lock(root: &Path) -> io::Result<File> {
    if !validate_staged_root_path(root)? {
        fs::create_dir_all(root)?;
        if !validate_staged_root_path(root)? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("staged artifact root disappeared: {}", root.display()),
            ));
        }
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(STORE_LOCK))
}

/// Shared (reader) ownership of the staged store.
///
/// Maintenance and eviction take the same lock exclusively, so holding this
/// keeps every published generation alive for the guard's lifetime. Drop
/// releases it — including on unwind, which is why this is an RAII type rather
/// than a lock/unlock pair.
#[derive(Debug)]
pub struct StagedReadGuard {
    _store_lock: File,
}

impl StagedReadGuard {
    /// Take the shared lock, creating the staged root if it does not exist.
    ///
    /// Prefer [`StagedReadGuard::acquire_if_present`] for read-only callers:
    /// this variant will materialize an empty `.staged-v2` directory in a cache
    /// that has never staged anything.
    pub fn acquire(artifact_dir: &Path) -> io::Result<Self> {
        let store_lock = open_store_lock(&staged_root(artifact_dir))?;
        fs2::FileExt::lock_shared(&store_lock)?;
        Ok(Self {
            _store_lock: store_lock,
        })
    }

    /// Take the shared lock only if a staged root already exists.
    ///
    /// `Ok(None)` means there is no staged store, so no generation can be
    /// resolved and there is nothing for maintenance to delete out from under
    /// the caller — the unlocked path is safe rather than merely tolerated.
    pub fn acquire_if_present(artifact_dir: &Path) -> io::Result<Option<Self>> {
        let root = staged_root(artifact_dir);
        if !validate_staged_root_path(&root)? {
            return Ok(None);
        }
        Self::acquire(artifact_dir).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_staged_root_means_no_guard_and_no_directory_is_created() {
        let dir = tempfile::tempdir().unwrap();

        let guard = StagedReadGuard::acquire_if_present(dir.path()).unwrap();

        assert!(guard.is_none(), "absent staged root must not take a lock");
        assert!(
            !staged_root(dir.path()).exists(),
            "a read-only probe must not create the staged root"
        );
    }

    #[test]
    fn an_existing_staged_root_yields_a_guard() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(staged_root(dir.path())).unwrap();

        let guard = StagedReadGuard::acquire_if_present(dir.path()).unwrap();

        assert!(guard.is_some(), "present staged root must be locked");
    }

    /// The whole point of the type: readers share, so a batch restore never
    /// serializes against another reader.
    #[test]
    fn two_readers_hold_the_lock_at_once() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(staged_root(dir.path())).unwrap();

        let first = StagedReadGuard::acquire(dir.path()).unwrap();
        let second = StagedReadGuard::acquire(dir.path()).unwrap();

        drop((first, second));
    }

    /// Pins the exclusion this module exists to provide, across the same
    /// lock file the daemon's maintenance uses.
    #[test]
    fn an_exclusive_holder_blocks_a_reader_until_it_releases() {
        use std::sync::mpsc;

        let dir = tempfile::tempdir().unwrap();
        let root = staged_root(dir.path());
        fs::create_dir_all(&root).unwrap();

        let exclusive = open_store_lock(&root).unwrap();
        fs2::FileExt::lock_exclusive(&exclusive).unwrap();

        let (tx, rx) = mpsc::channel();
        let path = dir.path().to_path_buf();
        let reader = std::thread::spawn(move || {
            let guard = StagedReadGuard::acquire(&path).unwrap();
            tx.send(()).unwrap();
            drop(guard);
        });

        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(250))
                .is_err(),
            "reader must not acquire while an exclusive lock is held"
        );

        drop(exclusive);

        rx.recv_timeout(std::time::Duration::from_secs(10))
            .expect("reader must acquire once the exclusive lock is released");
        reader.join().unwrap();
    }
}
