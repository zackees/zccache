//! Development-build daemon namespace initialization (issue #1362).

use std::ffi::OsString;
use std::io;
use std::path::Path;

const HASH_PREFIX_BYTES: usize = 8;

/// Directory under the cache root that holds memoized executable hashes.
const MEMO_DIR_NAME: &str = "dev-identity";
const MEMO_HEADER: &str = "zccache-dev-identity v1";

/// Hash `exe`, reusing an earlier result while the file is unchanged (#1649).
///
/// Cargo starts `RUSTC_WRAPPER` with its own environment, so a development
/// build never inherits a namespace and derived it on every invocation:
/// blake3 over the whole executable, 41 ms for the 76 MB release binary and
/// 76 ms for the unoptimized one, paid by each `rustc -vV` probe and each
/// compile. The digest is now recorded against the file's identity (size,
/// mtime and, on Unix, device, inode and ctime), so later invocations cost a
/// stat and one small read. Any change to the file changes that identity.
///
/// The memo is best-effort: an unreadable, stale or corrupt memo means hashing
/// again, and a failed write only means the next invocation hashes too. It is
/// recorded only when the identity is the same before and after hashing, so a
/// binary replaced mid-hash cannot pin a digest of the wrong content.
fn memoized_exe_hash<F>(exe: &Path, memo_dir: &Path, hash: F) -> io::Result<[u8; 32]>
where
    F: FnOnce() -> io::Result<[u8; 32]>,
{
    let Some(before) = file_identity(exe) else {
        return hash();
    };
    let memo = memo_dir.join(memo_file_name(exe));
    if let Some(digest) = read_memo(&memo, &before) {
        return Ok(digest);
    }
    let digest = hash()?;
    if file_identity(exe).as_deref() == Some(before.as_str()) {
        write_memo(memo_dir, &memo, &before, &digest);
    }
    Ok(digest)
}

/// One memo file per executable path.
fn memo_file_name(exe: &Path) -> String {
    let path_hash = kernal_api::hash::blake3_bytes(exe.as_os_str().as_encoded_bytes());
    format!("{}.identity", &path_hash.to_hex().as_str()[..32])
}

/// Everything about `path` that changes when its content can have changed.
fn file_identity(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    let mtime_ns = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    #[cfg(unix)]
    let platform = {
        use std::os::unix::fs::MetadataExt;
        format!(
            "{} {} {}.{}",
            metadata.dev(),
            metadata.ino(),
            metadata.ctime(),
            metadata.ctime_nsec()
        )
    };
    #[cfg(not(unix))]
    let platform = metadata
        .created()
        .ok()
        .and_then(|created| created.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |created| created.as_nanos())
        .to_string();
    Some(format!("{} {mtime_ns} {platform}", metadata.len()))
}

fn read_memo(memo: &Path, identity: &str) -> Option<[u8; 32]> {
    let text = std::fs::read_to_string(memo).ok()?;
    let mut lines = text.lines();
    if lines.next()? != MEMO_HEADER || lines.next()? != identity {
        return None;
    }
    let hex = lines.next()?;
    if hex.len() != 64 || lines.next().is_some() {
        return None;
    }
    let mut digest = [0u8; 32];
    for (byte, pair) in digest.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(digest)
}

fn write_memo(memo_dir: &Path, memo: &Path, identity: &str, digest: &[u8; 32]) {
    use std::io::Write;

    let hex = kernal_api::hash::Blake3Digest::from_bytes(*digest).to_hex();
    let body = format!("{MEMO_HEADER}\n{identity}\n{}\n", hex.as_str());
    let written = std::fs::create_dir_all(memo_dir)
        .and_then(|()| tempfile::NamedTempFile::new_in(memo_dir))
        .and_then(|mut tmp| {
            tmp.write_all(body.as_bytes())?;
            tmp.persist(memo).map(drop).map_err(|error| error.error)
        });
    // Best-effort by design: the next invocation simply hashes again.
    drop(written);
}

fn namespace_for_process<F>(
    inherited: Option<OsString>,
    release_build: bool,
    hash_current_exe: F,
) -> io::Result<Option<String>>
where
    F: FnOnce() -> io::Result<[u8; 32]>,
{
    if inherited
        .as_deref()
        .and_then(|value| {
            crate::core::config::namespace::sanitize_daemon_namespace(&value.to_string_lossy())
        })
        .is_some()
    {
        return Ok(None);
    }
    if release_build {
        return Ok(None);
    }

    let hash = kernal_api::hash::Blake3Digest::from_bytes(hash_current_exe()?);
    let hex = hash.to_hex();
    let hash_prefix = &hex.as_str()[..HASH_PREFIX_BYTES * 2];
    Ok(Some(format!("{}-{hash_prefix}", crate::core::VERSION)))
}

/// Establish the daemon namespace before CLI or daemon configuration is read.
pub fn initialize() -> io::Result<()> {
    let inherited = std::env::var_os(crate::core::config::DAEMON_NAMESPACE_ENV);
    if inherited
        .as_deref()
        .and_then(|value| {
            crate::core::config::namespace::sanitize_daemon_namespace(&value.to_string_lossy())
        })
        .is_some()
    {
        return Ok(());
    }

    let current_exe = kernal_api::platform::executable::current_image().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot locate the development zccache executable: {error}"),
        )
    })?;
    let release_build = crate::symbols::read_marker_from_path(&current_exe).is_some();
    let memo_dir = crate::core::config::default_cache_dir().join(MEMO_DIR_NAME);
    let namespace = namespace_for_process(inherited, release_build, || {
        memoized_exe_hash(&current_exe, memo_dir.as_path(), || {
            kernal_api::hash::blake3_file(&current_exe, kernal_api::hash::Blake3ReadOptions::new())
                .map(|hash| *hash.as_bytes())
                .map_err(|error| {
                    io::Error::new(
                        error.io_error_kind().unwrap_or(io::ErrorKind::Other),
                        format!(
                            "cannot hash development zccache executable {}: {error}",
                            current_exe.display()
                        ),
                    )
                })
        })
    })?;
    if let Some(namespace) = namespace {
        // The binary entrypoints call this before CLI/daemon initialization.
        // The value then flows to compiler children and the spawned daemon.
        std::env::set_var(crate::core::config::DAEMON_NAMESPACE_ENV, namespace);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn hash(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    #[test]
    fn inherited_namespace_wins_without_hashing() {
        let hashed = Cell::new(false);
        let namespace = namespace_for_process(Some("soldr-owned".into()), false, || {
            hashed.set(true);
            Ok(hash(0xaa))
        })
        .unwrap();

        assert_eq!(namespace, None);
        assert!(
            !hashed.get(),
            "an inherited value must avoid per-wrapper hashing"
        );
    }

    #[test]
    fn official_release_keeps_the_bare_namespace_without_hashing() {
        let hashed = Cell::new(false);
        let namespace = namespace_for_process(None, true, || {
            hashed.set(true);
            Ok(hash(0xaa))
        })
        .unwrap();

        assert_eq!(namespace, None);
        assert!(!hashed.get(), "official releases retain upgrade semantics");
    }

    #[test]
    fn development_build_uses_version_and_first_sixteen_hash_digits() {
        let namespace = namespace_for_process(None, false, || Ok(hash(0xab))).unwrap();

        assert_eq!(
            namespace.as_deref(),
            Some(concat!(env!("CARGO_PKG_VERSION"), "-abababababababab"))
        );
    }

    #[test]
    fn empty_inherited_namespace_is_not_treated_as_an_identity() {
        let namespace = namespace_for_process(Some("  ".into()), false, || Ok(hash(0x12))).unwrap();

        assert_eq!(
            namespace.as_deref(),
            Some(concat!(env!("CARGO_PKG_VERSION"), "-1212121212121212"))
        );
    }

    /// Calls `memoized_exe_hash` and reports whether the hasher ran.
    fn memoized(exe: &Path, memo_dir: &Path, digest: [u8; 32]) -> ([u8; 32], bool) {
        let hashed = Cell::new(false);
        let result = memoized_exe_hash(exe, memo_dir, || {
            hashed.set(true);
            Ok(digest)
        })
        .unwrap();
        (result, hashed.get())
    }

    #[test]
    fn memo_reuses_the_digest_while_the_executable_is_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("zccache");
        std::fs::write(&exe, b"binary").unwrap();
        let memo_dir = tmp.path().join("memo");

        assert_eq!(memoized(&exe, &memo_dir, hash(0x11)), (hash(0x11), true));
        // The hasher would now answer differently; the memo must win.
        assert_eq!(memoized(&exe, &memo_dir, hash(0x22)), (hash(0x11), false));
    }

    #[test]
    fn memo_is_invalidated_when_the_executable_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("zccache");
        std::fs::write(&exe, b"binary").unwrap();
        let memo_dir = tmp.path().join("memo");
        memoized(&exe, &memo_dir, hash(0x11));

        std::fs::write(&exe, b"rebuilt binary").unwrap();
        assert_eq!(memoized(&exe, &memo_dir, hash(0x22)), (hash(0x22), true));
        assert_eq!(memoized(&exe, &memo_dir, hash(0x33)), (hash(0x22), false));
    }

    #[test]
    fn corrupt_memo_is_rehashed_and_repaired() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("zccache");
        std::fs::write(&exe, b"binary").unwrap();
        let memo_dir = tmp.path().join("memo");
        memoized(&exe, &memo_dir, hash(0x11));
        let memo = memo_dir.join(memo_file_name(&exe));
        let text = std::fs::read_to_string(&memo).unwrap();
        std::fs::write(&memo, text.replace(&"1".repeat(64), &"z".repeat(64))).unwrap();

        assert_eq!(memoized(&exe, &memo_dir, hash(0x44)), (hash(0x44), true));
        assert_eq!(memoized(&exe, &memo_dir, hash(0x55)), (hash(0x44), false));
    }

    #[test]
    fn unwritable_memo_still_yields_the_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("zccache");
        std::fs::write(&exe, b"binary").unwrap();
        // A file where the memo directory should be: nothing can be written.
        let memo_dir = tmp.path().join("memo");
        std::fs::write(&memo_dir, b"not a directory").unwrap();

        assert_eq!(memoized(&exe, &memo_dir, hash(0x11)), (hash(0x11), true));
        assert_eq!(memoized(&exe, &memo_dir, hash(0x22)), (hash(0x22), true));
    }

    #[test]
    fn development_hash_failure_is_not_silently_downgraded() {
        let error = namespace_for_process(None, false, || {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "locked"))
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}
