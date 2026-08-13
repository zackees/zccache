//! Stable path identities for privately staged compiler outputs.

#[cfg(windows)]
use super::break_output_hardlink_before_compile;
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

fn rehydrate_logical_depfile_bytes(bytes: &[u8], requested_outputs: &[NormalizedPath]) -> Vec<u8> {
    let mut rewritten = bytes.to_vec();
    let mut requested_outputs = requested_outputs.iter().collect::<Vec<_>>();
    requested_outputs
        .sort_by_cached_key(|requested| std::cmp::Reverse(canonical_output_path(requested).len()));
    for requested in &requested_outputs {
        let canonical = canonical_output_path(requested);
        let requested_path = requested.to_string_lossy();
        rewritten =
            replace_make_depfile_path(&rewritten, canonical.as_bytes(), requested_path.as_bytes());

        let canonical_backslash = canonical.replace('/', "\\");
        if canonical_backslash != canonical {
            rewritten = replace_make_depfile_path(
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
    rewritten = replace_make_depfile_path(
        &rewritten,
        STAGED_OUTPUT_REMAP_ROOT.as_bytes(),
        requested_parent.as_bytes(),
    );
    replace_make_depfile_path(
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

pub(in crate::daemon::server) fn canonicalize_logical_depfile(
    path: &Path,
    private_root: &Path,
    requested: &NormalizedPath,
) -> io::Result<()> {
    let bytes = std::fs::read(path)?;
    let staged_path = path.to_string_lossy();
    let canonical_path = canonical_output_path(requested);
    let mut rewritten =
        replace_make_depfile_path(&bytes, staged_path.as_bytes(), canonical_path.as_bytes());
    let staged_path_slashes = staged_path.replace('\\', "/");
    if staged_path_slashes != staged_path {
        rewritten = replace_make_depfile_path(
            &rewritten,
            staged_path_slashes.as_bytes(),
            canonical_path.as_bytes(),
        );
    }

    let native_root = private_root.to_string_lossy();
    rewritten = replace_make_depfile_path(
        &rewritten,
        native_root.as_bytes(),
        STAGED_OUTPUT_REMAP_ROOT.as_bytes(),
    );
    let slash_root = native_root.replace('\\', "/");
    if slash_root != native_root {
        rewritten = replace_make_depfile_path(
            &rewritten,
            slash_root.as_bytes(),
            STAGED_OUTPUT_REMAP_ROOT.as_bytes(),
        );
    }
    if rewritten != bytes {
        atomic_replace_bytes(path, &rewritten)?;
    }
    Ok(())
}

pub(in crate::daemon::server) fn rehydrate_logical_depfile(
    path: &Path,
    requested_outputs: &[NormalizedPath],
) -> io::Result<()> {
    let bytes = std::fs::read(path)?;
    let rewritten = rehydrate_logical_depfile_bytes(&bytes, requested_outputs);
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
        // The destination depfile is often the COW-lite hardlink materialized
        // for the current build's output (persist/hardlink.rs marks
        // materialized destinations read-only to protect the shared cache
        // blob, and a hardlinked destination shares that read-only bit with
        // the blob's inode/MFT record). `MoveFileExW(...,
        // MOVEFILE_REPLACE_EXISTING)` fails with ERROR_ACCESS_DENIED on
        // Windows when the existing destination is read-only. Naively
        // clearing the attribute in place would also clear it on the shared
        // blob; use the existing detach helper instead, which — when the
        // destination is actually hardlinked to a blob — copies the content
        // out, breaks the link, and restores read-only on the blob
        // afterward. When not shared, it just clears the local bit.
        //
        // `std::fs::rename` (the non-Windows `replace_path`) needs no such
        // dance: POSIX rename() only requires write access to the
        // containing directory, never to the target file being replaced, so
        // it already swaps the directory entry without touching the shared
        // inode. Gate the detach to Windows to avoid the extra metadata
        // stat + hard-link-count check on the Linux/macOS hot path.
        #[cfg(windows)]
        {
            let _ = break_output_hardlink_before_compile(path);
        }
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

/// Quote one path token using the Make depfile spelling emitted by Clang and
/// GCC. Dollar signs are doubled, `#` is backslash-escaped, and whitespace is
/// backslash-escaped after duplicating any immediately preceding backslashes.
pub(crate) fn quote_make_depfile_path(path: &[u8]) -> Vec<u8> {
    let mut quoted = Vec::with_capacity(path.len().saturating_mul(2));
    let mut preceding_backslashes = 0usize;
    for &byte in path {
        match byte {
            b'\\' => {
                quoted.push(byte);
                preceding_backslashes += 1;
            }
            b' ' | b'\t' => {
                quoted.extend(std::iter::repeat_n(b'\\', preceding_backslashes + 1));
                quoted.push(byte);
                preceding_backslashes = 0;
            }
            b'#' => {
                quoted.extend_from_slice(br"\#");
                preceding_backslashes = 0;
            }
            b'$' => {
                quoted.extend_from_slice(b"$$");
                preceding_backslashes = 0;
            }
            _ => {
                quoted.push(byte);
                preceding_backslashes = 0;
            }
        }
    }
    quoted
}

fn replace_make_depfile_path(bytes: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    let quoted_needle = quote_make_depfile_path(needle);
    let quoted_replacement = quote_make_depfile_path(replacement);
    let mut rewritten = replace_all(bytes, &quoted_needle, &quoted_replacement);
    if quoted_needle != needle {
        rewritten = replace_all(&rewritten, needle, &quoted_replacement);
    }
    rewritten
}

fn replace_path(source: &Path, destination: &Path) -> io::Result<()> {
    crate::platform::fs::replace::atomic_replace(source, destination)
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
        let logical = rehydrate_staged_output_bytes(&canonical, std::slice::from_ref(&output));
        // Rehydration writes the platform-native spelling of the requested
        // output path (backslashes on Windows), not a slash-normalized one —
        // depfiles are consumed by native build tooling on that platform.
        let mut expected_logical = vec![0xff];
        expected_logical.extend_from_slice(output.to_string_lossy().as_bytes());
        assert_eq!(logical, expected_logical);
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

        let logical = rehydrate_staged_output_bytes(
            canonical.as_bytes(),
            &[primary.clone(), depfile.clone()],
        );

        // Native spelling, same rationale as
        // `output_references_round_trip_without_utf8_conversion`.
        let expected = format!(
            "{} {}",
            primary.to_string_lossy(),
            depfile.to_string_lossy()
        );
        assert_eq!(logical, expected.as_bytes());
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

    #[test]
    fn make_depfile_quoting_matches_clang_for_special_bytes() {
        assert_eq!(
            quote_make_depfile_path(br"/cache root/#hash/$dollar/back\slash/with\ space.o"),
            br"/cache\ root/\#hash/$$dollar/back\slash/with\\\ space.o"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn depfile_round_trip_preserves_make_escaped_special_paths() {
        let temp = tempfile::tempdir().unwrap();
        let private_root = temp.path().join("cache root # $");
        std::fs::create_dir_all(&private_root).unwrap();
        let staged_depfile = private_root.join("custom deps.mk");
        let escaped_staged_output = format!(
            r"{}/cache\ root\ \#\ $$/object\ name\ \#\ $$.o: source.h",
            temp.path().display()
        );
        std::fs::write(&staged_depfile, format!("{escaped_staged_output}\n")).unwrap();

        let requested_root = temp.path().join("workspace root # $");
        let requested_depfile: NormalizedPath = requested_root.join("custom deps.mk").into();
        let requested_output: NormalizedPath = requested_root.join("object name # $.o").into();

        canonicalize_logical_depfile(&staged_depfile, &private_root, &requested_depfile).unwrap();
        let canonical = std::fs::read_to_string(&staged_depfile).unwrap();
        assert!(
            canonical.contains(STAGED_OUTPUT_REMAP_ROOT),
            "escaped private root must be canonicalized: {canonical}"
        );
        assert!(
            !canonical.contains(r"cache\ root"),
            "canonical depfile must not retain the private root: {canonical}"
        );

        rehydrate_logical_depfile(&staged_depfile, &[requested_depfile, requested_output]).unwrap();
        assert_eq!(
            std::fs::read_to_string(&staged_depfile).unwrap(),
            format!(
                r"{}/workspace\ root\ \#\ $$/object\ name\ \#\ $$.o: source.h{}",
                temp.path().display(),
                '\n'
            )
        );
    }

    #[test]
    fn logical_depfile_canonicalization_preserves_non_utf8_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let private_root = temp.path().join("private");
        std::fs::create_dir_all(&private_root).unwrap();
        let staged_depfile = private_root.join("custom.mk");
        let staged_output = private_root.join("libfixture.rlib");
        let requested_depfile: NormalizedPath = temp.path().join("requested/custom.mk").into();
        let requested_output: NormalizedPath = temp.path().join("requested/libfixture.rlib").into();
        let mut bytes = vec![0xff];
        bytes.extend_from_slice(staged_depfile.to_string_lossy().as_bytes());
        bytes.extend_from_slice(b": ");
        bytes.extend_from_slice(staged_output.to_string_lossy().as_bytes());
        std::fs::write(&staged_depfile, bytes).unwrap();

        canonicalize_logical_depfile(&staged_depfile, &private_root, &requested_depfile).unwrap();
        let canonical = std::fs::read(&staged_depfile).unwrap();
        assert_eq!(canonical[0], 0xff);
        assert!(contains_staged_output_marker(&canonical));
        assert!(!canonical
            .windows(private_root.to_string_lossy().len())
            .any(|window| window == private_root.to_string_lossy().as_bytes()));

        rehydrate_logical_depfile(
            &staged_depfile,
            &[requested_depfile.clone(), requested_output.clone()],
        )
        .unwrap();
        let rehydrated = std::fs::read(&staged_depfile).unwrap();
        assert_eq!(rehydrated[0], 0xff);
        assert!(rehydrated
            .windows(requested_depfile.to_string_lossy().len())
            .any(|window| window == requested_depfile.to_string_lossy().as_bytes()));
        assert!(rehydrated
            .windows(requested_output.to_string_lossy().len())
            .any(|window| window == requested_output.to_string_lossy().as_bytes()));
    }
}
