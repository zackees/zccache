//! Transactional directory-output bundles for linker-like producers.

use super::*;
use bincode::Options;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

const DIRECTORY_OUTPUT_PREFIX: &str = "@zccache-directory-v1:";
const MAX_BUNDLE_ENTRIES: usize = 100_000;
const MAX_BUNDLE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
static DIRECTORY_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Serialize, Deserialize)]
struct DirectoryBundle {
    root_mode: u32,
    root_modified_secs: u64,
    root_modified_nanos: u32,
    entries: Vec<DirectoryEntry>,
}

#[derive(Serialize, Deserialize)]
struct DirectoryEntry {
    path: String,
    kind: EntryKind,
    bytes: Vec<u8>,
    mode: u32,
    modified_secs: u64,
    modified_nanos: u32,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
enum EntryKind {
    Directory,
    File,
}

pub(in crate::daemon::server) struct StagedDirectoryPlan {
    pub(in crate::daemon::server) rewritten_args: Vec<String>,
    requested: NormalizedPath,
    staged: NormalizedPath,
    archive: NormalizedPath,
    root: PathBuf,
}

impl StagedDirectoryPlan {
    #[cfg(test)]
    pub(in crate::daemon::server) fn for_test(
        root: PathBuf,
        requested: NormalizedPath,
        staged: NormalizedPath,
    ) -> Self {
        let archive = root.join("directory.bundle").into();
        Self {
            rewritten_args: Vec::new(),
            requested,
            staged,
            archive,
            root,
        }
    }

    pub(in crate::daemon::server) fn dsymutil(
        _staging_dir: &Path,
        args: &[String],
        requested: &Path,
        cwd: &Path,
    ) -> StagedPlanOutcome<Self> {
        if !super::staged_link_lane_enabled() {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::LaneDisabled);
        }
        let requested: NormalizedPath = if requested.is_absolute() {
            requested.into()
        } else {
            cwd.join(requested).into()
        };
        let Some(filename) = requested.file_name() else {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::OutputMissingFilename);
        };
        let parent = requested.parent().unwrap_or(cwd);
        if let Err(source) = std::fs::create_dir_all(parent) {
            return StagedPlanOutcome::Error(StagedPlanError {
                reason: StagedPlanReason::StagingDirectoryCreate,
                source,
            });
        }
        let root = match create_private_directory(parent) {
            Ok(root) => root,
            Err(source) => {
                return StagedPlanOutcome::Error(StagedPlanError {
                    reason: StagedPlanReason::StagingDirectoryCreate,
                    source,
                });
            }
        };
        let staged: NormalizedPath = root.join(filename).into();
        let archive: NormalizedPath = root.join("directory.bundle").into();
        let rewritten_args = rewrite_dsymutil_args(args, staged.as_path());
        StagedPlanOutcome::Enabled(Self {
            rewritten_args,
            requested,
            staged,
            archive,
            root,
        })
    }

    pub(in crate::daemon::server) fn pack(&self) -> std::io::Result<u64> {
        pack_directory(self.staged.as_path(), self.archive.as_path())
    }

    pub(in crate::daemon::server) fn archive_path(&self) -> &NormalizedPath {
        &self.archive
    }

    pub(in crate::daemon::server) fn output_name(&self) -> String {
        format!(
            "{DIRECTORY_OUTPUT_PREFIX}{}",
            self.requested
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        )
    }

    pub(in crate::daemon::server) fn materialize(&self) -> std::io::Result<()> {
        install_directory(self.staged.as_path(), self.requested.as_path())
    }

    pub(in crate::daemon::server) fn cleanup(&self) -> std::io::Result<()> {
        remove_directory_if_present(&self.root)
    }
}

fn create_private_directory(parent: &Path) -> std::io::Result<PathBuf> {
    for _ in 0..1_024 {
        let path = parent.join(format!(
            ".zccache-directory-{}-{}",
            std::process::id(),
            DIRECTORY_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique private directory",
    ))
}

fn rewrite_dsymutil_args(args: &[String], staged: &Path) -> Vec<String> {
    let mut rewritten_args = Vec::with_capacity(args.len() + 2);
    let mut index = 0;
    while index < args.len() {
        if matches!(args[index].as_str(), "-o" | "--out") {
            index += 2;
            continue;
        }
        if args[index].starts_with("--out=") {
            index += 1;
            continue;
        }
        rewritten_args.push(args[index].clone());
        index += 1;
    }
    rewritten_args.extend(["-o".to_string(), staged.to_string_lossy().into_owned()]);
    rewritten_args
}

impl Drop for StagedDirectoryPlan {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

pub(in crate::daemon::server) fn is_directory_output_name(name: &str) -> bool {
    name.starts_with(DIRECTORY_OUTPUT_PREFIX)
}

pub(in crate::daemon::server) fn materialize_directory_payload(
    payload: &CachedPayload,
    requested: &Path,
) -> std::io::Result<u64> {
    let bytes = match payload {
        CachedPayload::Bytes(bytes) => Arc::clone(bytes),
        CachedPayload::File(path) => Arc::new(std::fs::read(path)?),
    };
    let parent = requested.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".zccache-directory-{}-{}",
        std::process::id(),
        DIRECTORY_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        unpack_directory(&bytes, &temp)?;
        install_directory(&temp, requested)
    })();
    if result.is_err() {
        let _ = remove_directory_if_present(&temp);
    }
    result.map(|()| bytes.len() as u64)
}

fn pack_directory(source: &Path, archive: &Path) -> std::io::Result<u64> {
    if !source.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("directory output is missing: {}", source.display()),
        ));
    }
    let mut entries = Vec::new();
    let root_metadata = std::fs::metadata(source)?;
    let root_modified = modified_since_epoch(&root_metadata);
    collect_entries(source, source, &mut entries)?;
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    if entries.len() > MAX_BUNDLE_ENTRIES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "directory output exceeds bundle entry limit",
        ));
    }
    let options = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(MAX_BUNDLE_BYTES);
    let bytes = options
        .serialize(&DirectoryBundle {
            root_mode: permission_mode(&root_metadata),
            root_modified_secs: root_modified.as_secs(),
            root_modified_nanos: root_modified.subsec_nanos(),
            entries,
        })
        .map_err(invalid_bundle)?;
    std::fs::write(archive, &bytes)?;
    Ok(bytes.len() as u64)
}

fn collect_entries(
    root: &Path,
    directory: &Path,
    entries: &mut Vec<DirectoryEntry>,
) -> std::io::Result<()> {
    for child in std::fs::read_dir(directory)? {
        let child = child?;
        let path = child.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("directory output contains symlink: {}", path.display()),
            ));
        }
        let relative = path.strip_prefix(root).map_err(invalid_bundle)?;
        let relative = relative.to_str().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "directory output contains a non-UTF-8 path",
            )
        })?;
        let modified = modified_since_epoch(&metadata);
        let kind = if metadata.is_dir() {
            EntryKind::Directory
        } else if metadata.is_file() {
            EntryKind::File
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unsupported directory entry: {}", path.display()),
            ));
        };
        entries.push(DirectoryEntry {
            path: relative.replace('\\', "/"),
            kind,
            bytes: if metadata.is_file() {
                std::fs::read(&path)?
            } else {
                Vec::new()
            },
            mode: permission_mode(&metadata),
            modified_secs: modified.as_secs(),
            modified_nanos: modified.subsec_nanos(),
        });
        if metadata.is_dir() {
            collect_entries(root, &path, entries)?;
        }
    }
    Ok(())
}

fn unpack_directory(bytes: &[u8], target: &Path) -> std::io::Result<()> {
    let options = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(MAX_BUNDLE_BYTES);
    let bundle: DirectoryBundle = options.deserialize(bytes).map_err(invalid_bundle)?;
    if bundle.entries.len() > MAX_BUNDLE_ENTRIES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "directory bundle exceeds entry limit",
        ));
    }
    std::fs::create_dir(target)?;
    for entry in &bundle.entries {
        let relative = validated_relative_path(&entry.path)?;
        let path = target.join(relative);
        match entry.kind {
            EntryKind::Directory => std::fs::create_dir_all(&path)?,
            EntryKind::File => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, &entry.bytes)?;
                set_permissions(&path, entry.mode)?;
                set_mtime(&path, entry)?;
            }
        }
    }
    for entry in bundle
        .entries
        .iter()
        .rev()
        .filter(|entry| matches!(entry.kind, EntryKind::Directory))
    {
        let path = target.join(validated_relative_path(&entry.path)?);
        set_permissions(&path, entry.mode)?;
        set_mtime(&path, entry)?;
    }
    set_permissions(target, bundle.root_mode)?;
    set_timestamp(
        target,
        bundle.root_modified_secs,
        bundle.root_modified_nanos,
    )?;
    Ok(())
}

fn modified_since_epoch(metadata: &std::fs::Metadata) -> Duration {
    metadata
        .modified()
        .unwrap_or(UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

fn validated_relative_path(path: &str) -> std::io::Result<PathBuf> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "directory bundle contains an unsafe path",
        ));
    }
    Ok(path.to_path_buf())
}

fn install_directory(staged: &Path, requested: &Path) -> std::io::Result<()> {
    let parent = requested.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    if !requested.exists() {
        return std::fs::rename(staged, requested);
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        atomic_exchange_directories(staged, requested)?;
        return remove_directory_if_present(staged);
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let backup = parent.join(format!(
            ".zccache-directory-backup-{}-{}",
            std::process::id(),
            DIRECTORY_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::rename(requested, &backup)?;
        if let Err(error) = std::fs::rename(staged, requested) {
            let _ = std::fs::rename(&backup, requested);
            return Err(error);
        }
        remove_directory_if_present(&backup)
    }
}

#[cfg(target_os = "macos")]
fn atomic_exchange_directories(left: &Path, right: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let left = std::ffi::CString::new(left.as_os_str().as_bytes()).map_err(invalid_bundle)?;
    let right = std::ffi::CString::new(right.as_os_str().as_bytes()).map_err(invalid_bundle)?;
    // SAFETY: both pointers come from live CStrings, and renamex_np does not retain them.
    let result = unsafe { libc::renamex_np(left.as_ptr(), right.as_ptr(), libc::RENAME_SWAP) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn atomic_exchange_directories(left: &Path, right: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let left = std::ffi::CString::new(left.as_os_str().as_bytes()).map_err(invalid_bundle)?;
    let right = std::ffi::CString::new(right.as_os_str().as_bytes()).map_err(invalid_bundle)?;
    // SAFETY: both pointers come from live CStrings, and renameat2 does not retain them.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            left.as_ptr(),
            libc::AT_FDCWD,
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn remove_directory_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn set_mtime(path: &Path, entry: &DirectoryEntry) -> std::io::Result<()> {
    set_timestamp(path, entry.modified_secs, entry.modified_nanos)
}

fn set_timestamp(path: &Path, seconds: u64, nanos: u32) -> std::io::Result<()> {
    let modified = UNIX_EPOCH
        .checked_add(Duration::new(seconds, nanos))
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid mtime"))?;
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(modified))
}

#[cfg(unix)]
fn permission_mode(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode()
}

#[cfg(not(unix))]
fn permission_mode(metadata: &std::fs::Metadata) -> u32 {
    u32::from(metadata.permissions().readonly())
}

#[cfg(unix)]
fn set_permissions(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_permissions(path: &Path, mode: u32) -> std::io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_readonly(mode != 0);
    std::fs::set_permissions(path, permissions)
}

fn invalid_bundle(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_bundle_round_trip_preserves_tree_and_mtime() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.dSYM");
        let nested = source.join("Contents/Resources/DWARF");
        std::fs::create_dir_all(&nested).unwrap();
        let binary = nested.join("app");
        std::fs::write(&binary, b"debug-bytes").unwrap();
        let mtime = filetime::FileTime::from_unix_time(1_700_000_000, 123_456_700);
        filetime::set_file_mtime(&binary, mtime).unwrap();
        let root_mtime = filetime::FileTime::from_unix_time(1_600_000_000, 765_432_100);
        filetime::set_file_mtime(&source, root_mtime).unwrap();
        let archive = temp.path().join("bundle.bin");
        pack_directory(&source, &archive).unwrap();
        let target = temp.path().join("target.dSYM");

        unpack_directory(&std::fs::read(archive).unwrap(), &target).unwrap();

        let restored = target.join("Contents/Resources/DWARF/app");
        assert_eq!(std::fs::read(&restored).unwrap(), b"debug-bytes");
        assert_eq!(
            filetime::FileTime::from_last_modification_time(&std::fs::metadata(restored).unwrap()),
            mtime
        );
        assert_eq!(
            filetime::FileTime::from_last_modification_time(&std::fs::metadata(target).unwrap()),
            root_mtime
        );
    }

    #[test]
    fn directory_bundle_rejects_traversal() {
        let options = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_limit(MAX_BUNDLE_BYTES);
        let bytes = options
            .serialize(&DirectoryBundle {
                root_mode: 0,
                root_modified_secs: 0,
                root_modified_nanos: 0,
                entries: vec![DirectoryEntry {
                    path: "../escape".to_string(),
                    kind: EntryKind::File,
                    bytes: vec![1],
                    mode: 0,
                    modified_secs: 0,
                    modified_nanos: 0,
                }],
            })
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        assert!(unpack_directory(&bytes, &temp.path().join("target")).is_err());
        assert!(!temp.path().join("escape").exists());
    }

    #[test]
    fn dsymutil_rewrites_explicit_output_to_private_directory() {
        let temp = tempfile::tempdir().unwrap();
        let requested = temp.path().join("app.dSYM");
        let staged = temp.path().join("private/app.dSYM");
        let args = vec![
            "app".to_string(),
            "--out=old.dSYM".to_string(),
            "--verbose".to_string(),
        ];
        let rewritten_args = rewrite_dsymutil_args(&args, &staged);

        assert_eq!(rewritten_args[0], "app");
        assert_eq!(rewritten_args[1], "--verbose");
        assert_eq!(rewritten_args[2], "-o");
        assert_ne!(Path::new(&rewritten_args[3]), requested);
        assert_eq!(Path::new(&rewritten_args[3]), staged);
    }

    #[test]
    fn invalid_bundle_does_not_replace_existing_directory() {
        let temp = tempfile::tempdir().unwrap();
        let requested = temp.path().join("app.dSYM");
        std::fs::create_dir(&requested).unwrap();
        std::fs::write(requested.join("existing"), b"keep").unwrap();

        let error = materialize_directory_payload(
            &CachedPayload::Bytes(Arc::new(b"not a bundle".to_vec())),
            &requested,
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(requested.join("existing")).unwrap(), b"keep");
    }

    #[test]
    fn valid_bundle_replaces_existing_directory_as_a_complete_tree() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.dSYM");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("new"), b"new tree").unwrap();
        let archive = temp.path().join("bundle.bin");
        pack_directory(&source, &archive).unwrap();

        let requested = temp.path().join("app.dSYM");
        std::fs::create_dir(&requested).unwrap();
        std::fs::write(requested.join("old"), b"old tree").unwrap();
        materialize_directory_payload(&CachedPayload::File(archive.into()), requested.as_path())
            .unwrap();

        assert_eq!(std::fs::read(requested.join("new")).unwrap(), b"new tree");
        assert!(!requested.join("old").exists());
    }

    #[cfg(unix)]
    #[test]
    fn directory_bundle_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.dSYM");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(temp.path().join("outside"), b"outside").unwrap();
        symlink(temp.path().join("outside"), source.join("link")).unwrap();

        let error = pack_directory(&source, &temp.path().join("bundle.bin")).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
