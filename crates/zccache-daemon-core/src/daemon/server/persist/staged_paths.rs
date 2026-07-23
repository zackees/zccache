//! Stable path identities for privately staged compiler outputs.

use crate::core::path::NormalizedPath;
use std::io;
use std::path::Path;

pub(in crate::daemon::server) const STAGED_OUTPUT_REMAP_ROOT: &str = "/__zccache_staged_output__";
const STAGED_OUTPUT_REMAP_ROOT_WINDOWS: &str = r"\__zccache_staged_output__";

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
    for requested in requested_outputs {
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
        std::fs::write(path, rewritten)?;
    }
    Ok(())
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
        assert_eq!(canonical, b"\xff/__zccache_staged_output__/libfixture.rlib");
        let logical = rehydrate_staged_output_bytes(&canonical, &[output]);
        assert_eq!(logical, b"\xff/current/target/libfixture.rlib");
    }

    #[test]
    fn staged_output_marker_detection_accepts_native_and_windows_spelling() {
        assert!(contains_staged_output_marker(
            b"note: /__zccache_staged_output__/libfixture.rlib"
        ));
        assert!(contains_staged_output_marker(
            br"note: \__zccache_staged_output__\libfixture.rlib"
        ));
        assert!(!contains_staged_output_marker(
            b"note: /current/target/libfixture.rlib"
        ));
    }
}
