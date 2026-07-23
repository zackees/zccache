//! Stable path identities for privately staged compiler outputs.

use crate::core::path::NormalizedPath;
use std::io;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Collision-resistant logical root used only for staged compiler outputs.
///
/// The old human-readable sentinel could also occur in a real source path or
/// diagnostic excerpt and was then rewritten as if it were an output. The
/// random 128-bit namespace makes accidental collision infeasible while
/// remaining stable across worktrees for cache identity.
pub(in crate::daemon::server) const STAGED_OUTPUT_REMAP_ROOT: &str =
    "/__zccache_staged_output_7b6d6f0c5a944e8ba1c7e9634b287d91__";
const STAGED_OUTPUT_REMAP_ROOT_WINDOWS: &str =
    r"\__zccache_staged_output_7b6d6f0c5a944e8ba1c7e9634b287d91__";
static DEPFILE_REWRITE_COUNTER: AtomicU64 = AtomicU64::new(1);

pub(super) fn canonical_output_path(requested: &NormalizedPath) -> String {
    requested.file_name().map_or_else(
        || STAGED_OUTPUT_REMAP_ROOT.to_string(),
        |name| {
            format!(
                "{}/{}",
                STAGED_OUTPUT_REMAP_ROOT.trim_end_matches('/'),
                name.to_string_lossy()
            )
        },
    )
}

pub(in crate::daemon::server) fn canonicalize_staged_output_bytes(
    bytes: &[u8],
    private_root: &Path,
) -> Vec<u8> {
    let native_root = private_root.to_string_lossy();
    let mut rewritten = replace_all(
        bytes,
        native_root.as_bytes(),
        STAGED_OUTPUT_REMAP_ROOT.as_bytes(),
    );
    let slash_root = native_root.replace('\\', "/");
    if slash_root != native_root {
        rewritten = replace_all(
            &rewritten,
            slash_root.as_bytes(),
            STAGED_OUTPUT_REMAP_ROOT.as_bytes(),
        );
    }
    rewritten
}

pub(in crate::daemon::server) fn rehydrate_staged_output_bytes(
    bytes: &[u8],
    requested_outputs: &[NormalizedPath],
) -> Vec<u8> {
    let mut rewritten = bytes.to_vec();
    let mut requested_outputs = requested_outputs.iter().collect::<Vec<_>>();
    // Exact canonical output names can overlap (`foo` and `foo.d`). Replace
    // the longest name first so a shorter primary-output mapping cannot
    // consume the prefix of a longer side-output mapping.
    requested_outputs
        .sort_by_cached_key(|requested| std::cmp::Reverse(canonical_output_path(requested).len()));
    for requested in &requested_outputs {
        let canonical = canonical_output_path(requested);
        let requested_path = requested.to_string_lossy();
        rewritten = replace_all(&rewritten, canonical.as_bytes(), requested_path.as_bytes());

        let canonical_backslash = canonical.replace('/', "\\");
        if canonical_backslash != canonical {
            rewritten = replace_all(
                &rewritten,
                canonical_backslash.as_bytes(),
                requested_path.as_bytes(),
            );
        }
    }

    let requested_parent = requested_outputs
        .first()
        .and_then(|output| output.parent())
        .map_or_else(String::new, |parent| parent.to_string_lossy().into_owned());
    rewritten = replace_all(
        &rewritten,
        STAGED_OUTPUT_REMAP_ROOT.as_bytes(),
        requested_parent.as_bytes(),
    );
    replace_all(
        &rewritten,
        STAGED_OUTPUT_REMAP_ROOT_WINDOWS.as_bytes(),
        requested_parent.as_bytes(),
    )
}

pub(in crate::daemon::server) fn contains_staged_output_marker(bytes: &[u8]) -> bool {
    let marker = STAGED_OUTPUT_REMAP_ROOT.as_bytes();
    bytes.windows(marker.len()).any(|window| window == marker) || {
        let marker = STAGED_OUTPUT_REMAP_ROOT_WINDOWS.as_bytes();
        bytes.windows(marker.len()).any(|window| window == marker)
    }
}

pub(in crate::daemon::server) fn rehydrate_logical_depfile(
    path: &Path,
    requested_outputs: &[NormalizedPath],
) -> io::Result<()> {
    let bytes = std::fs::read(path)?;
    let rewritten = rehydrate_staged_output_bytes(&bytes, requested_outputs);
    if rewritten != bytes {
        atomic_replace_bytes(path, &rewritten)?;
    }
    Ok(())
}

fn atomic_replace_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "depfile replacement requires a parent directory",
        )
    })?;
    let name = path
        .file_name()
        .map_or_else(|| "depfile".into(), |name| name.to_string_lossy());
    let sequence = DEPFILE_REWRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{name}.zccache-rehydrate-{}-{sequence}.tmp",
        std::process::id()
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        replace_path(&temporary, path)?;
        if let Ok(directory) = std::fs::File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(not(windows))]
fn replace_path(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_path(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = super::windows_verbatim_file_path(source)?
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination = super::windows_verbatim_file_path(destination)?
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: both vectors are NUL-terminated UTF-16 paths and remain alive.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn replace_all(bytes: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return bytes.to_vec();
    }
    let mut rewritten = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    while let Some(offset) = bytes[cursor..]
        .windows(needle.len())
        .position(|window| window == needle)
    {
        let start = cursor + offset;
        rewritten.extend_from_slice(&bytes[cursor..start]);
        rewritten.extend_from_slice(replacement);
        cursor = start + needle.len();
    }
    rewritten.extend_from_slice(&bytes[cursor..]);
    rewritten
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_references_round_trip_without_utf8_conversion() {
        let output: NormalizedPath = "/current/target/libfixture.rlib".into();
        let private_root = Path::new("/cache/private");
        let bytes = b"\xff/cache/private/libfixture.rlib";
        let canonical = canonicalize_staged_output_bytes(bytes, private_root);
        let mut expected = vec![0xff];
        expected
            .extend_from_slice(format!("{STAGED_OUTPUT_REMAP_ROOT}/libfixture.rlib").as_bytes());
        assert_eq!(canonical, expected);
        let logical = rehydrate_staged_output_bytes(&canonical, &[output]);
        assert_eq!(logical, b"\xff/current/target/libfixture.rlib");
    }

    #[test]
    fn staged_output_marker_detection_accepts_native_and_windows_spelling() {
        let native = format!("note: {STAGED_OUTPUT_REMAP_ROOT}/libfixture.rlib");
        let windows = format!(
            "note: {}\\libfixture.rlib",
            STAGED_OUTPUT_REMAP_ROOT.replace('/', "\\")
        );
        assert!(contains_staged_output_marker(native.as_bytes()));
        assert!(contains_staged_output_marker(windows.as_bytes()));
        assert!(!contains_staged_output_marker(
            b"note: /current/target/libfixture.rlib"
        ));
        assert!(!contains_staged_output_marker(
            b"note: /__zccache_staged_output__/libfixture.rlib"
        ));
    }

    #[test]
    fn rehydration_prefers_longest_overlapping_output_name() {
        let primary: NormalizedPath = "/target-a/foo".into();
        let depfile: NormalizedPath = "/target-b/foo.d".into();
        let canonical = format!("{STAGED_OUTPUT_REMAP_ROOT}/foo {STAGED_OUTPUT_REMAP_ROOT}/foo.d");

        let logical = rehydrate_staged_output_bytes(canonical.as_bytes(), &[primary, depfile]);

        assert_eq!(logical, b"/target-a/foo /target-b/foo.d");
    }

    #[test]
    fn legacy_literal_marker_is_not_rewritten() {
        let output: NormalizedPath = "/target/foo.o".into();
        let bytes = b"source excerpt: /__zccache_staged_output__/real-input.h";

        assert_eq!(
            rehydrate_staged_output_bytes(bytes, &[output]),
            bytes,
            "the former human-readable marker is ordinary user data"
        );
    }

    #[test]
    fn depfile_rehydration_atomically_replaces_complete_contents() {
        let temp = tempfile::tempdir().unwrap();
        let depfile = temp.path().join("foo.d");
        std::fs::write(
            &depfile,
            format!("{STAGED_OUTPUT_REMAP_ROOT}/foo.o: input.h\n"),
        )
        .unwrap();
        let output: NormalizedPath = temp.path().join("foo.o").into();

        rehydrate_logical_depfile(&depfile, &[output]).unwrap();

        assert_eq!(
            std::fs::read_to_string(&depfile).unwrap(),
            format!("{}: input.h\n", temp.path().join("foo.o").display())
        );
        assert_eq!(
            std::fs::read_dir(temp.path())
                .unwrap()
                .filter_map(Result::ok)
                .count(),
            1,
            "temporary replacement files must not remain visible"
        );
    }
}
