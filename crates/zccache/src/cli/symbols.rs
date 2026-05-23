//! Download and install matching debug symbols for the running zccache build.
//!
//! Motivation (zccache#276): when zccache.exe / zccache-daemon.exe faults on
//! Windows, the embedded CodeView record points at compile-time paths in
//! `target/.../release/deps/` that don't exist on the user's machine. Without
//! the matching `.pdb` files, cdb/WinDbg cannot resolve function names and
//! the stack trace is only RVAs. The release build now ships a separate
//! `<root>-debug.zip` (or `.tar.gz`) archive with the per-binary sidecars;
//! this module gives users a one-shot way to download the archive that
//! matches their installed binary and drop the sidecars next to the exe so
//! `dbghelp` finds them via its same-directory fallback.
//!
//! The build's version and target triple are embedded at compile time
//! (`CARGO_PKG_VERSION` and `ZCCACHE_BUILD_TARGET` from `build.rs`) so the
//! defaults always match the running binary.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_TARGET: &str = env!("ZCCACHE_BUILD_TARGET");
const RELEASE_BASE_URL: &str = "https://github.com/zackees/zccache/releases/download";
const LOCK_FILENAME: &str = ".zccache-symbols.lock";

/// Env var that, when set to a non-empty value, makes `zccache.exe` install
/// missing debug symbols next to itself on startup. Idempotent — installs are
/// skipped when the sidecars are already present.
pub const AUTO_INSTALL_ENV: &str = "ZCCACHE_AUTO_INSTALL_SYMBOLS";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LockBehavior {
    /// Block until the install lock is acquired. Used for the explicit
    /// `zccache symbols install` subcommand.
    #[default]
    Wait,
    /// Try once; if another process holds the lock, return without doing
    /// any work. Used for the auto-install hot path so compile-loop wrappers
    /// don't all pile up on a single download.
    SkipIfBusy,
}

#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    /// Version to fetch. Defaults to the running binary's version.
    pub version: Option<String>,
    /// Target triple to fetch. Defaults to the running binary's build target.
    pub target: Option<String>,
    /// Directory to drop sidecars into. Defaults to the directory containing
    /// the running zccache executable.
    pub prefix: Option<PathBuf>,
    /// Re-download even if matching sidecars are already present.
    pub force: bool,
    /// What to do when another zccache process is already mid-install.
    pub lock_behavior: LockBehavior,
}

#[derive(Debug)]
pub struct InstallReport {
    pub prefix: PathBuf,
    pub installed: Vec<PathBuf>,
    pub skipped_already_present: bool,
    /// Set when `LockBehavior::SkipIfBusy` and another process is currently
    /// holding the install lock. Caller can choose to retry later.
    pub skipped_lock_busy: bool,
    pub url: String,
    /// Set when the archive came from the on-disk cache under
    /// `<cache_dir>/symbols/` rather than the network.
    pub cache_hit: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SymbolsError {
    #[error("unable to locate the running zccache binary: {0}")]
    LocateExe(#[source] io::Error),
    #[error("network error fetching {url}: {source}")]
    Fetch {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("release asset returned HTTP {status} for {url}")]
    HttpStatus { url: String, status: u16 },
    #[error("io error writing {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("archive contained no debug sidecars (expected .pdb/.dwp/.dSYM entries)")]
    EmptyArchive,
    #[error("tokio runtime error: {0}")]
    Runtime(#[source] io::Error),
}

#[derive(Debug, Clone, Copy)]
enum ArchiveKind {
    /// Windows `-debug.zip` containing `.pdb` files.
    WindowsPdb,
    /// macOS `-debug.tar.gz` containing `.dSYM` bundles.
    MacOsDsym,
    /// Linux `-debug.tar.gz` containing `.dwp` files.
    LinuxDwp,
}

impl ArchiveKind {
    fn for_target(target: &str) -> Self {
        if target.contains("pc-windows") {
            Self::WindowsPdb
        } else if target.contains("apple-darwin") || target.contains("apple-ios") {
            Self::MacOsDsym
        } else {
            Self::LinuxDwp
        }
    }

    fn file_extension(self) -> &'static str {
        match self {
            Self::WindowsPdb => "zip",
            Self::MacOsDsym | Self::LinuxDwp => "tar.gz",
        }
    }

    /// Sidecar filenames expected next to the binaries after extraction.
    /// Matches `.github/actions/build-target/action.yml` — the producer
    /// side. Keep in sync.
    fn expected_sidecars(self) -> &'static [&'static str] {
        match self {
            // rustc's MSVC linker writes PDBs using the underscored crate
            // name (zccache#276): `zccache-daemon.exe` -> `zccache_daemon.pdb`.
            Self::WindowsPdb => &["zccache.pdb", "zccache_daemon.pdb", "zccache_fp.pdb"],
            Self::MacOsDsym => &["zccache.dSYM", "zccache-daemon.dSYM", "zccache-fp.dSYM"],
            Self::LinuxDwp => &["zccache.dwp", "zccache-daemon.dwp", "zccache-fp.dwp"],
        }
    }
}

fn resolved_prefix(opts_prefix: Option<&Path>) -> Result<PathBuf, SymbolsError> {
    if let Some(p) = opts_prefix {
        return Ok(p.to_path_buf());
    }
    let exe = env::current_exe().map_err(SymbolsError::LocateExe)?;
    Ok(exe
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(".")))
}

fn build_url(version: &str, target: &str, kind: ArchiveKind) -> String {
    // GitHub release tags omit the `v` (e.g. `1.6.0`) but asset filenames
    // include it (e.g. `zccache-v1.6.0-...`). Build both sides correctly.
    let tag = version;
    let ext = kind.file_extension();
    format!(
        "{base}/{tag}/zccache-v{version}-{target}-debug.{ext}",
        base = RELEASE_BASE_URL,
    )
}

fn all_sidecars_present(prefix: &Path, kind: ArchiveKind) -> bool {
    kind.expected_sidecars()
        .iter()
        .all(|name| prefix.join(name).exists())
}

/// Synchronous entry point. Wraps the async download in a private tokio
/// runtime so callers don't need an executor handy.
pub fn install(opts: InstallOptions) -> Result<InstallReport, SymbolsError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(SymbolsError::Runtime)?;
    runtime.block_on(install_async(opts))
}

pub async fn install_async(opts: InstallOptions) -> Result<InstallReport, SymbolsError> {
    let version = opts
        .version
        .clone()
        .unwrap_or_else(|| PKG_VERSION.to_string());
    let target = opts
        .target
        .clone()
        .unwrap_or_else(|| BUILD_TARGET.to_string());
    let prefix = resolved_prefix(opts.prefix.as_deref())?;
    let kind = ArchiveKind::for_target(&target);
    let url = build_url(&version, &target, kind);

    // Lock-free fast path: avoids creating a lockfile when symbols are
    // already installed (the common steady-state).
    if !opts.force && all_sidecars_present(&prefix, kind) {
        return Ok(InstallReport {
            prefix,
            installed: Vec::new(),
            skipped_already_present: true,
            skipped_lock_busy: false,
            url,
            cache_hit: false,
        });
    }

    fs::create_dir_all(&prefix).map_err(|e| SymbolsError::Io {
        path: prefix.clone(),
        source: e,
    })?;

    // Cross-process lock so two concurrent zccache invocations don't both
    // download the same archive and race on extraction. The lock is an OS
    // advisory lock on the lockfile handle (fs2 -> LockFileEx on Windows,
    // fcntl on Unix); the kernel releases it when the File handle drops,
    // when the process exits cleanly, when it panics, or when it's killed
    // with SIGKILL / TerminateProcess. There is no stale-lockfile cleanup
    // needed.
    let lockfile_path = prefix.join(LOCK_FILENAME);
    let lockfile = open_lockfile(&lockfile_path)?;
    if !acquire_exclusive(&lockfile, opts.lock_behavior)? {
        return Ok(InstallReport {
            prefix,
            installed: Vec::new(),
            skipped_already_present: false,
            skipped_lock_busy: true,
            url,
            cache_hit: false,
        });
    }

    // Re-check under the lock: another process may have completed the
    // install between our fast-path check and acquiring the lock.
    if !opts.force && all_sidecars_present(&prefix, kind) {
        return Ok(InstallReport {
            prefix,
            installed: Vec::new(),
            skipped_already_present: true,
            skipped_lock_busy: false,
            url,
            cache_hit: false,
        });
    }

    let (bytes, cache_hit) = fetch_archive(&url, &version, &target, kind, opts.force).await?;

    let installed = match kind {
        ArchiveKind::WindowsPdb => extract_zip(&bytes, &prefix)?,
        ArchiveKind::MacOsDsym | ArchiveKind::LinuxDwp => extract_targz(&bytes, &prefix)?,
    };

    if installed.is_empty() {
        return Err(SymbolsError::EmptyArchive);
    }

    // Lock released on `lockfile` drop here.
    drop(lockfile);

    Ok(InstallReport {
        prefix,
        installed,
        skipped_already_present: false,
        skipped_lock_busy: false,
        url,
        cache_hit,
    })
}

/// Returns the bytes of the matching debug archive plus a flag indicating
/// whether they came from the on-disk archive cache rather than the network.
///
/// Cache layout: `<zccache cache dir>/symbols/<asset-filename>`. The asset
/// filename already encodes version + target, so two callers asking for the
/// same archive land on the same path. Writes use a same-directory tempfile
/// plus atomic rename, so a kill during download can't leave a partial file
/// that looks like a cache hit to the next caller.
async fn fetch_archive(
    url: &str,
    version: &str,
    target: &str,
    kind: ArchiveKind,
    force: bool,
) -> Result<(Vec<u8>, bool), SymbolsError> {
    let cache_path = archive_cache_path(version, target, kind);

    if !force {
        if let Ok(bytes) = fs::read(&cache_path) {
            if !bytes.is_empty() {
                return Ok((bytes, true));
            }
        }
    }

    let client = reqwest::Client::builder()
        .user_agent(concat!("zccache/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| SymbolsError::Fetch {
            url: url.to_string(),
            source: e,
        })?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| SymbolsError::Fetch {
            url: url.to_string(),
            source: e,
        })?;
    if !response.status().is_success() {
        return Err(SymbolsError::HttpStatus {
            url: url.to_string(),
            status: response.status().as_u16(),
        });
    }
    let bytes = response.bytes().await.map_err(|e| SymbolsError::Fetch {
        url: url.to_string(),
        source: e,
    })?;

    // Best-effort cache write: a permission or disk-full error here should
    // not fail the install — the bytes are already in memory and the caller
    // can extract them.
    if let Some(parent) = cache_path.parent() {
        if fs::create_dir_all(parent).is_ok() {
            let _ = write_atomically(&cache_path, &mut io::Cursor::new(bytes.as_ref()));
        }
    }

    Ok((bytes.to_vec(), false))
}

fn archive_cache_path(version: &str, target: &str, kind: ArchiveKind) -> PathBuf {
    let filename = format!(
        "zccache-v{version}-{target}-debug.{ext}",
        ext = kind.file_extension(),
    );
    zccache::core::config::symbols_cache_dir()
        .into_path_buf()
        .join(filename)
}

fn open_lockfile(path: &Path) -> Result<File, SymbolsError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| SymbolsError::Io {
            path: path.to_path_buf(),
            source: e,
        })
}

/// Acquire the install lock. Returns `Ok(true)` on success, `Ok(false)` only
/// when `SkipIfBusy` and another holder is present. Other errors propagate
/// as `SymbolsError::Io` so a permission problem surfaces clearly.
fn acquire_exclusive(file: &File, behavior: LockBehavior) -> Result<bool, SymbolsError> {
    // fs2 trait methods are called via UFCS to avoid the ambiguity with
    // `std::fs::File::try_lock_exclusive` that landed in Rust 1.89.
    match behavior {
        LockBehavior::SkipIfBusy => match fs2::FileExt::try_lock_exclusive(file) {
            Ok(()) => Ok(true),
            Err(err) if is_would_block(&err) => Ok(false),
            Err(err) => Err(SymbolsError::Io {
                path: PathBuf::from(LOCK_FILENAME),
                source: err,
            }),
        },
        LockBehavior::Wait => fs2::FileExt::lock_exclusive(file)
            .map(|()| true)
            .map_err(|err| SymbolsError::Io {
                path: PathBuf::from(LOCK_FILENAME),
                source: err,
            }),
    }
}

fn is_would_block(err: &io::Error) -> bool {
    if matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::ResourceBusy
    ) {
        return true;
    }
    // Windows: `LockFileEx` with `LOCKFILE_FAIL_IMMEDIATELY` returns
    // `ERROR_LOCK_VIOLATION (33)`, which std currently surfaces as
    // `ErrorKind::Uncategorized`. Treat it as "would block".
    #[cfg(windows)]
    {
        if matches!(err.raw_os_error(), Some(33)) {
            return true;
        }
    }
    false
}

/// Extract `.pdb` files from a zip into `prefix`. Strips the archive's
/// top-level directory (`zccache-vX.Y.Z-<target>-debug/`) so files land
/// directly next to the binaries. Each file is written via tempfile + rename
/// so an interrupted install can't leave a partial PDB that subsequent
/// `all_sidecars_present` checks would treat as complete.
fn extract_zip(bytes: &[u8], prefix: &Path) -> Result<Vec<PathBuf>, SymbolsError> {
    let cursor = io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor)?;
    let mut installed = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        let raw_name = match entry.enclosed_name() {
            Some(p) => p.to_path_buf(),
            None => continue,
        };
        // Keep only the trailing path component for PDB sidecars; debuggers
        // search by basename in the binary's directory.
        let leaf = match raw_name.file_name() {
            Some(n) => Path::new(n).to_path_buf(),
            None => continue,
        };
        if !is_debug_sidecar(&leaf) {
            continue;
        }
        let dest = prefix.join(&leaf);
        write_atomically(&dest, &mut entry)?;
        installed.push(dest);
    }
    Ok(installed)
}

/// Write a single file via a same-directory tempfile + rename. On Windows
/// `NamedTempFile::persist` uses `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`
/// which is atomic for any concurrent reader.
fn write_atomically(dest: &Path, src: &mut dyn io::Read) -> Result<(), SymbolsError> {
    let parent = dest.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| SymbolsError::Io {
        path: parent.to_path_buf(),
        source: e,
    })?;
    io::copy(src, tmp.as_file_mut()).map_err(|e| SymbolsError::Io {
        path: tmp.path().to_path_buf(),
        source: e,
    })?;
    tmp.persist(dest).map_err(|e| SymbolsError::Io {
        path: dest.to_path_buf(),
        source: e.error,
    })?;
    Ok(())
}

/// Extract `.dwp` files or `.dSYM` bundles from a gzip-compressed tarball.
fn extract_targz(bytes: &[u8], prefix: &Path) -> Result<Vec<PathBuf>, SymbolsError> {
    let cursor = io::Cursor::new(bytes);
    let decoder = flate2::read::GzDecoder::new(cursor);
    let mut archive = tar::Archive::new(decoder);
    let mut installed = Vec::new();
    for entry in archive.entries().map_err(|e| SymbolsError::Io {
        path: prefix.to_path_buf(),
        source: e,
    })? {
        let mut entry = entry.map_err(|e| SymbolsError::Io {
            path: prefix.to_path_buf(),
            source: e,
        })?;
        let raw_path = match entry.path() {
            Ok(p) => p.into_owned(),
            Err(_) => continue,
        };
        let components: Vec<_> = raw_path.components().collect();
        // The archive layout is `<root>/<sidecar...>`. Strip the top-level
        // wrapper directory so contents land directly under `prefix`.
        if components.len() < 2 {
            continue;
        }
        let inner: PathBuf = components[1..]
            .iter()
            .map(|c| c.as_os_str())
            .collect::<PathBuf>();
        // Filter: top-level sidecar entry must look like a debug file or
        // dSYM bundle root. Children of dSYM bundles are copied verbatim
        // once the bundle root is allowed through.
        let first_inner = match inner.components().next() {
            Some(c) => Path::new(c.as_os_str()).to_path_buf(),
            None => continue,
        };
        if !is_debug_sidecar(&first_inner) {
            continue;
        }
        let dest = prefix.join(&inner);
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&dest).map_err(|e| SymbolsError::Io {
                path: dest.clone(),
                source: e,
            })?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| SymbolsError::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
        entry.unpack(&dest).map_err(|e| SymbolsError::Io {
            path: dest.clone(),
            source: e,
        })?;
        // Record the bundle root once, not every file inside it.
        if inner.components().count() == 1 {
            installed.push(dest);
        }
    }
    Ok(installed)
}

fn is_debug_sidecar(leaf: &Path) -> bool {
    matches!(
        leaf.extension().and_then(|s| s.to_str()),
        Some("pdb" | "dwp" | "dSYM")
    )
}

/// Called from `main()` when the auto-install env var is set. Best-effort:
/// any failure is reported to stderr so a transient network blip on
/// startup doesn't break the user's actual command. The fast-path (already
/// installed) is silent so the env var can stay set permanently without
/// adding noise to every invocation.
///
/// Concurrency model for compile-loop wrappers: this uses
/// `LockBehavior::SkipIfBusy` so when many zccache invocations start in
/// parallel only the first one downloads; the rest see the lock and return
/// immediately. Once the winner finishes, the next invocation takes the
/// silent fast path.
pub fn maybe_auto_install() {
    if env::var_os(AUTO_INSTALL_ENV).is_none_or(|v| v.is_empty()) {
        return;
    }
    // Fast-path sidecar-presence check before reporting any activity, so a
    // permanently-set env var stays quiet once symbols are installed.
    let kind = ArchiveKind::for_target(BUILD_TARGET);
    if let Ok(prefix) = resolved_prefix(None) {
        if all_sidecars_present(&prefix, kind) {
            return;
        }
    }
    let opts = InstallOptions {
        lock_behavior: LockBehavior::SkipIfBusy,
        ..InstallOptions::default()
    };
    match install(opts) {
        Ok(report) if report.skipped_lock_busy => {
            // Another zccache is already installing — don't block this one's
            // actual command. The next invocation will see the result.
            eprintln!(
                "zccache: another process is installing debug sidecars in {}, skipping",
                report.prefix.display()
            );
        }
        Ok(report) if report.skipped_already_present => {
            // Race: another process completed between fast-path check and
            // post-lock re-check. Nothing more to do, stay quiet.
        }
        Ok(report) => {
            eprintln!(
                "zccache: installed {} debug sidecar(s) into {}",
                report.installed.len(),
                report.prefix.display()
            );
        }
        Err(err) => {
            eprintln!("zccache: debug symbol auto-install failed: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_kind_for_target() {
        assert!(matches!(
            ArchiveKind::for_target("x86_64-pc-windows-msvc"),
            ArchiveKind::WindowsPdb
        ));
        assert!(matches!(
            ArchiveKind::for_target("aarch64-pc-windows-msvc"),
            ArchiveKind::WindowsPdb
        ));
        assert!(matches!(
            ArchiveKind::for_target("x86_64-apple-darwin"),
            ArchiveKind::MacOsDsym
        ));
        assert!(matches!(
            ArchiveKind::for_target("aarch64-unknown-linux-musl"),
            ArchiveKind::LinuxDwp
        ));
        assert!(matches!(
            ArchiveKind::for_target("x86_64-unknown-linux-gnu"),
            ArchiveKind::LinuxDwp
        ));
    }

    #[test]
    fn build_url_windows_uses_zip_and_v_prefix() {
        let url = build_url("1.6.0", "x86_64-pc-windows-msvc", ArchiveKind::WindowsPdb);
        assert_eq!(
            url,
            "https://github.com/zackees/zccache/releases/download/1.6.0/zccache-v1.6.0-x86_64-pc-windows-msvc-debug.zip"
        );
    }

    #[test]
    fn build_url_linux_uses_tar_gz() {
        let url = build_url("1.6.0", "x86_64-unknown-linux-musl", ArchiveKind::LinuxDwp);
        assert!(url.ends_with(".tar.gz"));
        assert!(url.contains("zccache-v1.6.0-x86_64-unknown-linux-musl-debug"));
    }

    #[test]
    fn expected_sidecars_use_underscored_pdb_names_on_windows() {
        // Regression guard for zccache#276: rustc's MSVC PDB filename uses
        // the underscored crate name, not the [[bin]] name.
        let names = ArchiveKind::WindowsPdb.expected_sidecars();
        assert!(names.contains(&"zccache.pdb"));
        assert!(names.contains(&"zccache_daemon.pdb"));
        assert!(names.contains(&"zccache_fp.pdb"));
    }

    #[test]
    fn is_debug_sidecar_recognizes_extensions() {
        assert!(is_debug_sidecar(Path::new("zccache.pdb")));
        assert!(is_debug_sidecar(Path::new("zccache-daemon.dwp")));
        assert!(is_debug_sidecar(Path::new("zccache-fp.dSYM")));
        assert!(!is_debug_sidecar(Path::new("zccache.exe")));
        assert!(!is_debug_sidecar(Path::new("README.md")));
    }

    #[test]
    fn skips_install_when_sidecars_already_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ArchiveKind::WindowsPdb.expected_sidecars() {
            fs::write(dir.path().join(name), b"stub").unwrap();
        }
        assert!(all_sidecars_present(dir.path(), ArchiveKind::WindowsPdb));
    }

    #[test]
    fn detects_missing_sidecar() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("zccache.pdb"), b"stub").unwrap();
        // missing daemon + fp
        assert!(!all_sidecars_present(dir.path(), ArchiveKind::WindowsPdb));
    }

    /// A second `try_lock_exclusive` while the first holder is alive must
    /// fail — the regression we'd be guarding against is a stale-flag
    /// implementation that lets two installers run concurrently.
    #[test]
    fn lockfile_blocks_second_try_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = dir.path().join(LOCK_FILENAME);
        let first = open_lockfile(&lock).expect("open lock 1");
        assert!(acquire_exclusive(&first, LockBehavior::SkipIfBusy).unwrap());

        let second = open_lockfile(&lock).expect("open lock 2");
        assert!(
            !acquire_exclusive(&second, LockBehavior::SkipIfBusy).unwrap(),
            "second process should have been told the lock is busy"
        );
    }

    /// When the holder drops the file handle (or the process dies — same
    /// kernel-level behavior), the lock must become available again
    /// without any cleanup step.
    #[test]
    fn lockfile_released_on_handle_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = dir.path().join(LOCK_FILENAME);

        {
            let first = open_lockfile(&lock).expect("open lock 1");
            assert!(acquire_exclusive(&first, LockBehavior::SkipIfBusy).unwrap());
            // handle drops here — kernel releases the advisory lock.
        }

        let second = open_lockfile(&lock).expect("open lock 2");
        assert!(
            acquire_exclusive(&second, LockBehavior::SkipIfBusy).unwrap(),
            "lock should be free after first holder drops the handle"
        );
    }

    /// Atomic-write helper must materialize the destination file only after
    /// the source is fully copied. We assert via post-state, not timing —
    /// just confirm the destination has the right contents.
    #[test]
    fn write_atomically_persists_full_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("zccache.pdb");
        let mut src: &[u8] = b"PDB-payload";
        write_atomically(&dest, &mut src).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"PDB-payload");
    }

    /// The non-blocking path used by auto-install must not return
    /// `Ok(true)` when the lock is contended. Couples to
    /// `is_would_block` correctly mapping the platform error code.
    #[test]
    fn skip_if_busy_classifies_contended_lock_as_skip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = dir.path().join(LOCK_FILENAME);
        let holder = open_lockfile(&lock).unwrap();
        assert!(acquire_exclusive(&holder, LockBehavior::SkipIfBusy).unwrap());

        let challenger = open_lockfile(&lock).unwrap();
        let got = acquire_exclusive(&challenger, LockBehavior::SkipIfBusy).unwrap();
        assert!(!got, "challenger must see SkipIfBusy -> Ok(false)");
    }

    /// The archive cache lives under the configured `default_cache_dir`
    /// (overridable via `ZCCACHE_CACHE_DIR`), never `$TMPDIR`. This is the
    /// invariant that the `ban_unrooted_tempdir` dylint enforces from the
    /// other direction.
    #[test]
    fn archive_cache_path_is_under_zccache_cache_dir() {
        let path = archive_cache_path("1.6.0", "x86_64-pc-windows-msvc", ArchiveKind::WindowsPdb);
        let expected_leaf = "zccache-v1.6.0-x86_64-pc-windows-msvc-debug.zip";
        assert_eq!(
            path.file_name().and_then(|s| s.to_str()),
            Some(expected_leaf)
        );

        let parent = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str());
        assert_eq!(parent, Some("symbols"));

        let expected_root = zccache::core::config::default_cache_dir();
        assert!(
            path.starts_with(expected_root.as_path()),
            "cache path {} should be under default_cache_dir {}",
            path.display(),
            expected_root.as_path().display(),
        );
    }
}
