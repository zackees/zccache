//! 128-byte release footer appended to the end of distributed zccache
//! binaries. Read at runtime via `current_exe()` to discover the
//! version, target triple, and source revision the binary was built
//! from. The shipped GitHub release of that exact version+triple
//! carries matching debug symbols.
//!
//! ## Format
//!
//! ```text
//! [  0.. 40]  git SHA hex (NUL-padded if short)
//! [ 40.. 56]  semver version, NUL-padded
//! [ 56.. 88]  rustc target triple, NUL-padded (longest seen: 26 bytes,
//!             slot sized for forward-compat)
//! [ 88.. 96]  build timestamp (u64 LE, unix seconds)
//! [ 96..120]  reserved zeros (forward-compat)
//! [120..128]  magic = b"ZCCSYMv1"
//! ```
//!
//! Absence of the magic means "dev build" — callers fall back to local
//! `target/release/*.{pdb,dwp,dSYM}` rather than attempting any fetch.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

pub const MARKER_MAGIC: &[u8; 8] = b"ZCCSYMv1";
pub const MARKER_SIZE: usize = 128;

const OFFSET_SHA: usize = 0;
const OFFSET_VERSION: usize = 40;
const OFFSET_TRIPLE: usize = 56;
const OFFSET_TIMESTAMP: usize = 88;
const OFFSET_MAGIC: usize = 120;

const SHA_LEN: usize = 40;
const VERSION_LEN: usize = 16;
const TRIPLE_LEN: usize = 32;

/// Parsed release marker fields. All string fields are stripped of
/// trailing NULs the on-disk format uses for padding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseMarker {
    pub git_sha: String,
    pub version: String,
    pub triple: String,
    pub build_timestamp: u64,
}

/// Read the marker from the currently-running executable.
///
/// Returns `None` if the binary is too small, the magic doesn't match
/// (dev build), or the file can't be read.
#[must_use]
pub fn read_marker_from_current_exe() -> Option<ReleaseMarker> {
    let exe = std::env::current_exe().ok()?;
    read_marker_from_path(&exe)
}

/// Read the marker from a specific path. Used by tests and by `zccache
/// symbolicate` when the user passes `--binary <path>` explicitly.
#[must_use]
pub fn read_marker_from_path(path: &Path) -> Option<ReleaseMarker> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len < MARKER_SIZE as u64 {
        return None;
    }
    file.seek(SeekFrom::End(-(MARKER_SIZE as i64))).ok()?;
    let mut buf = [0u8; MARKER_SIZE];
    file.read_exact(&mut buf).ok()?;
    decode_footer(&buf)
}

/// Append a marker footer to `path`. The caller is responsible for
/// running this AFTER any stripping step — strip would clip the footer.
/// And BEFORE any signing step (Authenticode/codesign cover trailing
/// bytes), once we sign artifacts at all.
///
/// Errors if the file can't be opened for append, or if a string field
/// exceeds its on-disk slot.
pub fn write_marker_to_binary(path: &Path, marker: &ReleaseMarker) -> std::io::Result<()> {
    let footer = encode_footer(marker)?;
    let mut file = OpenOptions::new().append(true).open(path)?;
    file.write_all(&footer)?;
    file.flush()?;
    Ok(())
}

fn decode_footer(buf: &[u8; MARKER_SIZE]) -> Option<ReleaseMarker> {
    if &buf[OFFSET_MAGIC..OFFSET_MAGIC + 8] != MARKER_MAGIC {
        return None;
    }
    let git_sha = decode_nul_padded(&buf[OFFSET_SHA..OFFSET_SHA + SHA_LEN])?;
    let version = decode_nul_padded(&buf[OFFSET_VERSION..OFFSET_VERSION + VERSION_LEN])?;
    let triple = decode_nul_padded(&buf[OFFSET_TRIPLE..OFFSET_TRIPLE + TRIPLE_LEN])?;
    let mut ts_bytes = [0u8; 8];
    ts_bytes.copy_from_slice(&buf[OFFSET_TIMESTAMP..OFFSET_TIMESTAMP + 8]);
    let build_timestamp = u64::from_le_bytes(ts_bytes);
    Some(ReleaseMarker {
        git_sha,
        version,
        triple,
        build_timestamp,
    })
}

fn encode_footer(marker: &ReleaseMarker) -> std::io::Result<[u8; MARKER_SIZE]> {
    let mut out = [0u8; MARKER_SIZE];
    write_nul_padded(
        &mut out[OFFSET_SHA..OFFSET_SHA + SHA_LEN],
        &marker.git_sha,
        "git_sha",
    )?;
    write_nul_padded(
        &mut out[OFFSET_VERSION..OFFSET_VERSION + VERSION_LEN],
        &marker.version,
        "version",
    )?;
    write_nul_padded(
        &mut out[OFFSET_TRIPLE..OFFSET_TRIPLE + TRIPLE_LEN],
        &marker.triple,
        "triple",
    )?;
    out[OFFSET_TIMESTAMP..OFFSET_TIMESTAMP + 8]
        .copy_from_slice(&marker.build_timestamp.to_le_bytes());
    out[OFFSET_MAGIC..OFFSET_MAGIC + 8].copy_from_slice(MARKER_MAGIC);
    Ok(out)
}

fn decode_nul_padded(slice: &[u8]) -> Option<String> {
    let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    std::str::from_utf8(&slice[..end]).ok().map(str::to_owned)
}

fn write_nul_padded(dest: &mut [u8], value: &str, field: &str) -> std::io::Result<()> {
    let bytes = value.as_bytes();
    if bytes.len() > dest.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{field}: {} bytes exceeds slot size {}",
                bytes.len(),
                dest.len()
            ),
        ));
    }
    dest[..bytes.len()].copy_from_slice(bytes);
    // Remaining bytes left as zero from `[0u8; MARKER_SIZE]` init.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_marker() -> ReleaseMarker {
        ReleaseMarker {
            git_sha: "032432c000000000000000000000000000000000".to_string(),
            version: "1.7.2".to_string(),
            triple: "x86_64-pc-windows-msvc".to_string(),
            build_timestamp: 1_700_000_000,
        }
    }

    #[test]
    fn roundtrip_encode_decode() {
        let m = sample_marker();
        let footer = encode_footer(&m).unwrap();
        assert_eq!(footer.len(), MARKER_SIZE);
        assert_eq!(&footer[OFFSET_MAGIC..OFFSET_MAGIC + 8], MARKER_MAGIC);
        let parsed = decode_footer(&footer).unwrap();
        assert_eq!(parsed, m);
    }

    #[test]
    fn triple_too_long_errors() {
        let mut m = sample_marker();
        m.triple = "x86_64-some-extremely-long-triple-that-cannot-fit".to_string();
        assert!(encode_footer(&m).is_err());
    }

    #[test]
    fn missing_magic_returns_none() {
        let mut buf = [0u8; MARKER_SIZE];
        buf[OFFSET_VERSION..OFFSET_VERSION + 5].copy_from_slice(b"1.7.2");
        // magic slot left as zeros — should not parse
        assert!(decode_footer(&buf).is_none());
    }

    #[test]
    fn write_marker_appends_to_file() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"existing binary contents goes here")
            .unwrap();
        let path = tmp.into_temp_path();
        let m = sample_marker();
        write_marker_to_binary(&path, &m).unwrap();
        let parsed = read_marker_from_path(&path).unwrap();
        assert_eq!(parsed, m);
    }

    #[test]
    fn short_file_returns_none() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"tiny").unwrap();
        let path = tmp.into_temp_path();
        assert!(read_marker_from_path(&path).is_none());
    }
}
