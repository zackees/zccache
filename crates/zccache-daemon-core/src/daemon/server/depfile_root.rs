//! Worktree-independent user depfiles for shared C/C++ cache entries.
//!
//! A user depfile names the compile's inputs by the spelling the compiler
//! saw, so a CMake/Ninja compile with absolute `-I` and source paths writes
//! the worktree root into it. The cache key salts the depfile-shaping argv
//! with that root replaced by a logical marker, the stored depfile carries
//! the same marker, and every hit rewrites the marker to the requesting
//! request's key root. Only whole-root path prefixes at token boundaries are
//! rewritten, byte for byte, so spellings the compiler would render
//! differently (`a.o` versus `/root/a.o`, `./x` versus `x`) keep distinct keys.

use crate::core::path::NormalizedPath;
use std::path::Path;

/// Collision-resistant logical root for depfile paths beneath the key root.
pub(super) const DEPFILE_WORKTREE_ROOT_MARKER: &str =
    "/__zccache_worktree_root_3f0c1e5a9b7d4c26a8e1f4b2d6c9a071__";

/// The byte spelling of a key root that can be replaced safely, or `None`
/// for a root that is relative, the filesystem root, or not UTF-8.
fn root_spelling(root: &Path) -> Option<&str> {
    let text = root.to_str()?;
    let text = text.trim_end_matches(['/', '\\']);
    (root.is_absolute() && !text.is_empty() && !text.ends_with(':')).then_some(text)
}

fn is_separator(byte: u8) -> bool {
    byte == b'/' || (crate::platform::host::is_windows() && byte == b'\\')
}

/// Salt spelling for one depfile-shaping compiler argument.
pub(super) fn salt_depfile_arg(arg: &str, key_root: Option<&Path>) -> String {
    let Some(root) = key_root.and_then(root_spelling) else {
        return arg.to_string();
    };
    let bytes = arg.as_bytes();
    let mut rewritten = String::with_capacity(arg.len());
    let mut cursor = 0;
    while let Some(offset) = arg[cursor..].find(root) {
        let start = cursor + offset;
        let end = start + root.len();
        let prefix = &arg[..start];
        let starts_token = prefix.is_empty()
            || prefix.ends_with(['=', ','])
            || (prefix.len() > 1
                && prefix.starts_with('-')
                && prefix[1..].bytes().all(|byte| byte.is_ascii_alphabetic()));
        let ends_root =
            end == bytes.len() || matches!(bytes[end], b'=' | b',') || is_separator(bytes[end]);
        rewritten.push_str(&arg[cursor..start]);
        if starts_token && ends_root {
            rewritten.push_str(DEPFILE_WORKTREE_ROOT_MARKER);
        } else {
            rewritten.push_str(root);
        }
        cursor = end;
    }
    rewritten.push_str(&arg[cursor..]);
    rewritten
}

fn unescaped_whitespace_before(bytes: &[u8], index: usize) -> bool {
    let Some(&byte) = index.checked_sub(1).and_then(|at| bytes.get(at)) else {
        return true;
    };
    match byte {
        b'\n' | b'\r' => true,
        b' ' | b'\t' => {
            let backslashes = bytes[..index - 1]
                .iter()
                .rev()
                .take_while(|&&byte| byte == b'\\')
                .count();
            backslashes % 2 == 0
        }
        _ => false,
    }
}

fn root_ends_at(bytes: &[u8], end: usize) -> bool {
    match bytes.get(end) {
        None => true,
        Some(b' ' | b'\t' | b'\n' | b'\r' | b':') => true,
        Some(b'\\') => {
            matches!(bytes.get(end + 1), Some(b'\n' | b'\r'))
                || (crate::platform::host::is_windows()
                    && !matches!(bytes.get(end + 1), Some(b' ' | b'\t' | b'#')))
        }
        Some(&byte) => is_separator(byte),
    }
}

/// Replace every whole-root path prefix in Make depfile bytes with the
/// logical marker. Unchanged when the root cannot be rewritten safely.
pub(super) fn canonicalize_depfile_root(bytes: &[u8], key_root: &Path) -> Vec<u8> {
    let Some(root) = root_spelling(key_root) else {
        return bytes.to_vec();
    };
    let needle = crate::daemon::server::quote_make_depfile_path(root.as_bytes());
    let mut rewritten = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    while let Some(offset) = bytes[cursor..]
        .windows(needle.len())
        .position(|window| window == needle.as_slice())
    {
        let start = cursor + offset;
        let end = start + needle.len();
        rewritten.extend_from_slice(&bytes[cursor..start]);
        if unescaped_whitespace_before(bytes, start) && root_ends_at(bytes, end) {
            rewritten.extend_from_slice(DEPFILE_WORKTREE_ROOT_MARKER.as_bytes());
        } else {
            rewritten.extend_from_slice(&needle);
        }
        cursor = end;
    }
    rewritten.extend_from_slice(&bytes[cursor..]);
    rewritten
}

pub(super) fn contains_depfile_root_marker(bytes: &[u8]) -> bool {
    let marker = DEPFILE_WORKTREE_ROOT_MARKER.as_bytes();
    bytes.windows(marker.len()).any(|window| window == marker)
}

/// Rewrite the logical marker to the requesting key root. `None` when the
/// bytes need a root this request cannot supply.
pub(super) fn rehydrate_depfile_root(bytes: &[u8], key_root: Option<&Path>) -> Option<Vec<u8>> {
    if !contains_depfile_root_marker(bytes) {
        return Some(bytes.to_vec());
    }
    let root = key_root.and_then(root_spelling)?;
    let quoted = crate::daemon::server::quote_make_depfile_path(root.as_bytes());
    let marker = DEPFILE_WORKTREE_ROOT_MARKER.as_bytes();
    let mut rewritten = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    while let Some(offset) = bytes[cursor..]
        .windows(marker.len())
        .position(|window| window == marker)
    {
        let start = cursor + offset;
        rewritten.extend_from_slice(&bytes[cursor..start]);
        rewritten.extend_from_slice(&quoted);
        cursor = start + marker.len();
    }
    rewritten.extend_from_slice(&bytes[cursor..]);
    Some(rewritten)
}

/// Rewrite a delivered depfile in place for the requesting key root.
pub(super) fn rehydrate_depfile_root_file(
    path: &NormalizedPath,
    key_root: Option<&NormalizedPath>,
) -> std::io::Result<()> {
    let bytes = std::fs::read(path.as_path())?;
    if !contains_depfile_root_marker(&bytes) {
        return Ok(());
    }
    let rewritten = rehydrate_depfile_root(&bytes, key_root.map(NormalizedPath::as_path))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "cached depfile names the worktree root but the request has none",
            )
        })?;
    crate::daemon::server::replace_depfile_bytes(path.as_path(), &rewritten)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/work/tree";

    #[test]
    fn salt_replaces_only_whole_root_prefixes_at_argument_tokens() {
        let root = Some(Path::new(ROOT));
        let marked = |rest: &str| format!("{DEPFILE_WORKTREE_ROOT_MARKER}{rest}");
        assert_eq!(
            salt_depfile_arg("/work/tree/src/a.c", root),
            marked("/src/a.c")
        );
        assert_eq!(
            salt_depfile_arg("-I/work/tree/include", root),
            format!("-I{}", marked("/include"))
        );
        assert_eq!(
            salt_depfile_arg("-isystem/work/tree", root),
            format!("-isystem{}", marked(""))
        );
        assert_eq!(
            salt_depfile_arg("-DDIR=/work/tree/x", root),
            format!("-DDIR={}", marked("/x"))
        );
        assert_eq!(
            salt_depfile_arg("-ffile-prefix-map=/work/tree=.", root),
            format!("-ffile-prefix-map={}=.", marked(""))
        );
        for unchanged in [
            "/work/tree2/a.c",
            "/other/work/tree/a.c",
            "a.o",
            "./a.o",
            "-I./work/tree",
            "-I/work/tree-extra",
        ] {
            assert_eq!(salt_depfile_arg(unchanged, root), unchanged);
        }
        assert_eq!(salt_depfile_arg("/work/tree/a.c", None), "/work/tree/a.c");
        assert_eq!(salt_depfile_arg("/a.c", Some(Path::new("/"))), "/a.c");
    }

    #[test]
    fn depfile_root_round_trips_to_the_requesting_root() {
        let stored = canonicalize_depfile_root(
            b"obj/a.o: /work/tree/src/a.c /work/tree/include/a.h \\\n /work/tree2/b.h /usr/include/stdio.h /x/work/tree/c.h\n",
            Path::new(ROOT),
        );
        let text = String::from_utf8(stored.clone()).unwrap();
        assert!(!text.contains(" /work/tree/"), "{text}");
        assert!(
            text.contains("/work/tree2/b.h") && text.contains("/x/work/tree/c.h"),
            "{text}"
        );

        let delivered = rehydrate_depfile_root(&stored, Some(Path::new("/other/wt"))).unwrap();
        assert_eq!(
            String::from_utf8(delivered).unwrap(),
            "obj/a.o: /other/wt/src/a.c /other/wt/include/a.h \\\n /work/tree2/b.h /usr/include/stdio.h /x/work/tree/c.h\n"
        );
        assert!(rehydrate_depfile_root(&stored, None).is_none());
        assert_eq!(
            rehydrate_depfile_root(b"a.o: a.c\n", None).unwrap(),
            b"a.o: a.c\n"
        );
    }

    #[test]
    fn depfile_root_respects_make_quoting() {
        let spaced = Path::new("/work/my tree");
        let stored = canonicalize_depfile_root(
            b"/work/my\\ tree/a.o: /work/my\\ tree/a.c /x/a\\ /work/my\\ tree/b.h\n",
            spaced,
        );
        let text = String::from_utf8(stored.clone()).unwrap();
        assert_eq!(
            text,
            format!(
                "{m}/a.o: {m}/a.c /x/a\\ /work/my\\ tree/b.h\n",
                m = DEPFILE_WORKTREE_ROOT_MARKER
            )
        );
        let delivered = rehydrate_depfile_root(&stored, Some(Path::new("/w/b c"))).unwrap();
        assert_eq!(
            String::from_utf8(delivered).unwrap(),
            "/w/b\\ c/a.o: /w/b\\ c/a.c /x/a\\ /work/my\\ tree/b.h\n"
        );
    }
}
