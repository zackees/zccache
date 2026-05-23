//! Symbol cache layout and per-crash `.symref` sidecars.
//!
//! Symbols live ONCE on disk under `<cache>/symbols/<v>-<triple>/`.
//! Each crash dump gets a `<dump>.symref` sidecar next to it containing
//! the absolute path to that directory — true dedup, portable across
//! all OSes without needing symlinks (Windows symlinks require dev mode
//! or admin).

use std::path::{Path, PathBuf};
use zccache::core::NormalizedPath;

/// Sentinel file written after a successful extract. Its presence
/// declares the directory "complete and ready to consume." Absence
/// means either we've never fetched, or the previous fetch was
/// interrupted mid-extract — either way, re-fetch.
pub const READY_SENTINEL: &str = ".ready";

/// Returns the canonical cache directory for the symbols matching a
/// given `version` + target `triple`. Does NOT create the directory.
#[must_use]
pub fn symbols_dir_for(cache_root: &Path, version: &str, triple: &str) -> NormalizedPath {
    NormalizedPath::from(
        cache_root
            .join("symbols")
            .join(format!("{version}-{triple}")),
    )
}

/// True if the directory exists AND has the `.ready` sentinel — i.e. a
/// previous fetch completed atomically.
#[must_use]
pub fn is_ready(symbols_dir: &Path) -> bool {
    symbols_dir.join(READY_SENTINEL).exists()
}

/// Drop the `.ready` sentinel into `symbols_dir`. Caller has already
/// extracted the archive into this directory.
pub fn mark_ready(symbols_dir: &Path) -> std::io::Result<()> {
    std::fs::write(symbols_dir.join(READY_SENTINEL), b"")
}

/// Write a `<dump>.symref` next to `dump_path` whose contents are the
/// absolute path to the symbol directory (plus a trailing newline so
/// `cat` output stays tidy). The "no duplicates" half of the
/// placement contract.
pub fn write_symref_sidecar(dump_path: &Path, symbols_dir: &Path) -> std::io::Result<PathBuf> {
    let mut sidecar = dump_path.as_os_str().to_owned();
    sidecar.push(".symref");
    let sidecar_path = PathBuf::from(sidecar);
    let body = format!("{}\n", symbols_dir.display());
    std::fs::write(&sidecar_path, body)?;
    Ok(sidecar_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbols_dir_layout() {
        let dir = symbols_dir_for(Path::new("/tmp/zc"), "1.7.2", "x86_64-pc-windows-msvc");
        assert!(dir.ends_with("symbols/1.7.2-x86_64-pc-windows-msvc"));
    }

    #[test]
    fn ready_sentinel_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("symbols");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!is_ready(&dir));
        mark_ready(&dir).unwrap();
        assert!(is_ready(&dir));
    }

    #[test]
    fn symref_sidecar_records_absolute_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dump = tmp.path().join("crash-1700000000.dmp");
        std::fs::write(&dump, b"fake dump").unwrap();
        let symbols = tmp.path().join("symbols").join("1.7.2-host");
        std::fs::create_dir_all(&symbols).unwrap();
        let sidecar = write_symref_sidecar(&dump, &symbols).unwrap();
        assert_eq!(sidecar.file_name().unwrap(), "crash-1700000000.dmp.symref");
        let content = std::fs::read_to_string(&sidecar).unwrap();
        assert!(content.contains(symbols.to_str().unwrap()));
        assert!(content.ends_with('\n'));
    }
}
