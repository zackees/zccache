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

/// Header line of a stored depfile that only its own worktree may replay.
const DEPFILE_ROOT_BOUND_PREFIX: &[u8] = b"# zccache-worktree-bound ";

/// Whether canonical depfile bytes still name a path under `key_root` in a
/// spelling the rewrite did not recognise: a `.` segment, a symlink into
/// the root, another case or separator. The artifact key resolves every
/// dependency with `canonicalize_path` and keys those under the root
/// relative to it, so such a path lets a sibling worktree share an entry
/// whose depfile fits only this one. Non-UTF-8 bytes count as bound.
pub(super) fn names_unrewritten_root_path(canonical: &[u8], key_root: &Path) -> bool {
    let Ok(text) = std::str::from_utf8(canonical) else {
        return true;
    };
    crate::depgraph::depfile::depfile_path_tokens(text)
        .iter()
        .any(|token| {
            let path = Path::new(token);
            path.is_absolute()
                && !token.starts_with(DEPFILE_WORKTREE_ROOT_MARKER)
                && crate::depgraph::depfile::canonicalize_path(path, key_root)
                    .as_path()
                    .starts_with(key_root)
        })
}

/// Prefix stored depfile bytes with the root they are bound to. A hit from
/// any other root cannot rehydrate them and recompiles instead.
pub(super) fn bind_depfile_to_root(canonical: &[u8], key_root: &Path) -> Vec<u8> {
    let root = key_root.to_string_lossy();
    let mut bound =
        Vec::with_capacity(DEPFILE_ROOT_BOUND_PREFIX.len() + root.len() + 1 + canonical.len());
    bound.extend_from_slice(DEPFILE_ROOT_BOUND_PREFIX);
    bound.extend_from_slice(root.as_bytes());
    bound.push(b'\n');
    bound.extend_from_slice(canonical);
    bound
}

/// Rewrite stored depfile bytes for the requesting key root: drop a bound
/// header naming this root and replace the logical marker. `None` when the
/// bytes belong to another root or need a root this request cannot supply.
pub(super) fn rehydrate_depfile_root(
    mut bytes: Vec<u8>,
    key_root: Option<&Path>,
) -> Option<Vec<u8>> {
    if bytes.starts_with(DEPFILE_ROOT_BOUND_PREFIX) {
        let rest = &bytes[DEPFILE_ROOT_BOUND_PREFIX.len()..];
        let end = rest.iter().position(|&byte| byte == b'\n')?;
        if rest[..end] != *key_root?.to_string_lossy().as_bytes() {
            return None;
        }
        bytes.drain(..DEPFILE_ROOT_BOUND_PREFIX.len() + end + 1);
    }
    if !contains_depfile_root_marker(&bytes) {
        return Some(bytes);
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

/// Rewrite a delivered depfile in place for the requesting compile: staged
/// output names, then the logical worktree root. One read and at most one
/// atomic replace, which never touches the shared blob.
pub(super) fn rehydrate_delivered_depfile(
    path: &NormalizedPath,
    requested_outputs: &[NormalizedPath],
    key_root: Option<&NormalizedPath>,
) -> std::io::Result<()> {
    let bytes = std::fs::read(path.as_path())?;
    let staged = crate::daemon::server::rehydrate_logical_depfile_bytes(&bytes, requested_outputs);
    let rewritten = rehydrate_depfile_root(staged, key_root.map(NormalizedPath::as_path))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "cached depfile is bound to another worktree root",
            )
        })?;
    if rewritten != bytes {
        crate::daemon::server::replace_depfile_bytes(path.as_path(), &rewritten)?;
    }
    Ok(())
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

        let delivered =
            rehydrate_depfile_root(stored.clone(), Some(Path::new("/other/wt"))).unwrap();
        assert_eq!(
            String::from_utf8(delivered).unwrap(),
            "obj/a.o: /other/wt/src/a.c /other/wt/include/a.h \\\n /work/tree2/b.h /usr/include/stdio.h /x/work/tree/c.h\n"
        );
        assert!(rehydrate_depfile_root(stored, None).is_none());
        assert_eq!(
            rehydrate_depfile_root(b"a.o: a.c\n".to_vec(), None).unwrap(),
            b"a.o: a.c\n"
        );
    }

    #[test]
    fn rewritten_and_foreign_paths_do_not_bind_the_depfile() {
        let stored = canonicalize_depfile_root(
            b"obj/a.o: /work/tree/src/a.c src/b.h /work/tree2/b.h /usr/include/stdio.h\n",
            Path::new(ROOT),
        );
        assert!(!names_unrewritten_root_path(&stored, Path::new(ROOT)));
    }

    #[test]
    fn a_dot_segment_spelling_of_the_root_binds_the_depfile() {
        let stored = canonicalize_depfile_root(
            b"obj/a.o: /work/tree/src/a.c /work/./tree/gen/config.h\n",
            Path::new(ROOT),
        );
        assert!(names_unrewritten_root_path(&stored, Path::new(ROOT)));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_into_the_root_binds_the_depfile() {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let root = base.join("tree");
        std::fs::create_dir_all(root.join("gen")).unwrap();
        std::fs::write(root.join("gen/config.h"), "").unwrap();
        std::os::unix::fs::symlink(&root, base.join("alias")).unwrap();
        let depfile = format!("obj/a.o: {}/gen/config.h\n", base.join("alias").display());
        let stored = canonicalize_depfile_root(depfile.as_bytes(), &root);
        assert!(names_unrewritten_root_path(&stored, &root));
    }

    #[test]
    fn a_bound_depfile_replays_only_to_its_own_root() {
        let stored = canonicalize_depfile_root(
            b"obj/a.o: /work/tree/src/a.c /work/./tree/gen/config.h\n",
            Path::new(ROOT),
        );
        let bound = bind_depfile_to_root(&stored, Path::new(ROOT));
        assert_eq!(
            rehydrate_depfile_root(bound.clone(), Some(Path::new(ROOT))).unwrap(),
            b"obj/a.o: /work/tree/src/a.c /work/./tree/gen/config.h\n"
        );
        assert!(rehydrate_depfile_root(bound.clone(), Some(Path::new("/other/wt"))).is_none());
        assert!(rehydrate_depfile_root(bound, None).is_none());
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
        let delivered = rehydrate_depfile_root(stored, Some(Path::new("/w/b c"))).unwrap();
        assert_eq!(
            String::from_utf8(delivered).unwrap(),
            "/w/b\\ c/a.o: /w/b\\ c/a.c /x/a\\ /work/my\\ tree/b.h\n"
        );
    }
}
