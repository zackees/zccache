//! Unit tests for the logical worktree-root marker in user depfiles.
//!
//! Every root fixture comes from `zccache_test_support::from_root` so the
//! path is absolute on the host platform: POSIX literals like `/work/tree`
//! are rooted but *not* absolute on Windows, where `root_spelling` (correctly)
//! refuses them and the marker assertions would exercise the disabled path.

use super::*;
use zccache_test_support::from_root;

/// The same root with a `.` segment before its final component: a spelling
/// `canonicalize_depfile_root` must leave raw so the depfile binds.
fn with_dot_segment(root: &str) -> String {
    let (head, tail) = root.rsplit_once('/').expect("fixture root has a separator");
    format!("{head}/./{tail}")
}

#[test]
fn salt_replaces_only_whole_root_prefixes_at_argument_tokens() {
    let root = from_root("work/tree");
    let key_root = Some(Path::new(&root));
    let marked = |rest: &str| format!("{DEPFILE_WORKTREE_ROOT_MARKER}{rest}");
    assert_eq!(
        salt_depfile_arg(&format!("{root}/src/a.c"), key_root),
        marked("/src/a.c")
    );
    assert_eq!(
        salt_depfile_arg(&format!("-I{root}/include"), key_root),
        format!("-I{}", marked("/include"))
    );
    assert_eq!(
        salt_depfile_arg(&format!("-isystem{root}"), key_root),
        format!("-isystem{}", marked(""))
    );
    assert_eq!(
        salt_depfile_arg(&format!("-DDIR={root}/x"), key_root),
        format!("-DDIR={}", marked("/x"))
    );
    assert_eq!(
        salt_depfile_arg(&format!("-ffile-prefix-map={root}=."), key_root),
        format!("-ffile-prefix-map={}=.", marked(""))
    );
    let unchanged = [
        format!("{root}2/a.c"),
        format!("/other{root}/a.c"),
        "a.o".to_string(),
        "./a.o".to_string(),
        format!("-I.{root}"),
        format!("-I{root}-extra"),
    ];
    for arg in &unchanged {
        assert_eq!(salt_depfile_arg(arg, key_root), *arg);
    }
    assert_eq!(
        salt_depfile_arg(&format!("{root}/a.c"), None),
        format!("{root}/a.c")
    );
    assert_eq!(salt_depfile_arg("/a.c", Some(Path::new("/"))), "/a.c");
}

#[test]
fn depfile_root_round_trips_to_the_requesting_root() {
    let root = from_root("work/tree");
    let other = from_root("other/wt");
    let fixture = format!(
        "obj/a.o: {root}/src/a.c {root}/include/a.h \\\n {root}2/b.h /usr/include/stdio.h /x{root}/c.h\n"
    );
    let stored = canonicalize_depfile_root(fixture.as_bytes(), Path::new(&root));
    let text = String::from_utf8(stored.clone()).unwrap();
    assert!(!text.contains(&format!(" {root}/")), "{text}");
    assert!(
        text.contains(&format!("{root}2/b.h")) && text.contains(&format!("/x{root}/c.h")),
        "{text}"
    );

    let delivered = rehydrate_depfile_root(stored.clone(), Some(Path::new(&other))).unwrap();
    assert_eq!(
        String::from_utf8(delivered).unwrap(),
        format!(
            "obj/a.o: {other}/src/a.c {other}/include/a.h \\\n {root}2/b.h /usr/include/stdio.h /x{root}/c.h\n"
        )
    );
    assert!(rehydrate_depfile_root(stored, None).is_none());
    assert_eq!(
        rehydrate_depfile_root(b"a.o: a.c\n".to_vec(), None).unwrap(),
        b"a.o: a.c\n"
    );
}

#[test]
fn rewritten_and_foreign_paths_do_not_bind_the_depfile() {
    let root = from_root("work/tree");
    let fixture = format!("obj/a.o: {root}/src/a.c src/b.h {root}2/b.h /usr/include/stdio.h\n");
    let stored = canonicalize_depfile_root(fixture.as_bytes(), Path::new(&root));
    assert!(!names_unrewritten_root_path(&stored, Path::new(&root)));
}

#[test]
fn a_dot_segment_spelling_of_the_root_binds_the_depfile() {
    let root = from_root("work/tree");
    let dot_root = with_dot_segment(&root);
    let fixture = format!("obj/a.o: {root}/src/a.c {dot_root}/gen/config.h\n");
    let stored = canonicalize_depfile_root(fixture.as_bytes(), Path::new(&root));
    assert!(names_unrewritten_root_path(&stored, Path::new(&root)));
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

#[cfg(windows)]
#[test]
fn a_backslash_root_round_trips_on_windows() {
    let root = Path::new(r"C:\work\tree");
    assert_eq!(
        salt_depfile_arg(r"-IC:\work\tree\include", Some(root)),
        format!(r"-I{DEPFILE_WORKTREE_ROOT_MARKER}\include")
    );
    let fixture = "obj\\a.o: C:\\work\\tree\\src\\a.c \\\n C:\\work\\tree2\\b.h\n";
    let stored = canonicalize_depfile_root(fixture.as_bytes(), root);
    assert!(contains_depfile_root_marker(&stored));
    assert!(!names_unrewritten_root_path(&stored, root));
    let delivered = rehydrate_depfile_root(stored, Some(Path::new(r"D:\other\wt"))).unwrap();
    assert_eq!(
        String::from_utf8(delivered).unwrap(),
        "obj\\a.o: D:\\other\\wt\\src\\a.c \\\n C:\\work\\tree2\\b.h\n"
    );
}

/// The rewrite is byte-exact, so a drive-letter case or separator spelling
/// of the root stays raw; the path comparison folds both, so it binds.
#[cfg(windows)]
#[test]
fn drive_letter_case_and_separator_spellings_bind_the_depfile_on_windows() {
    let root = Path::new(r"C:\work\tree");
    for token in [r"c:\work\tree\gen\config.h", "C:/work/tree/gen/config.h"] {
        let fixture = format!("obj\\a.o: C:\\work\\tree\\src\\a.c {token}\n");
        let stored = canonicalize_depfile_root(fixture.as_bytes(), root);
        assert!(names_unrewritten_root_path(&stored, root), "{token}");
    }
}

/// A directory-name case spelling of an existing root resolves to the
/// on-disk name, so it binds like a symlink into the root does on Unix.
#[cfg(windows)]
#[test]
fn a_directory_case_spelling_of_the_root_binds_the_depfile_on_windows() {
    let tmp = tempfile::tempdir().unwrap();
    let base =
        crate::core::path::strip_verbatim_prefix(&std::fs::canonicalize(tmp.path()).unwrap());
    let root = base.join("tree");
    std::fs::create_dir_all(root.join("gen")).unwrap();
    std::fs::write(root.join("gen").join("config.h"), "").unwrap();
    let alias = base.join("TREE").join("gen").join("config.h");
    let token = crate::daemon::server::quote_make_depfile_path(alias.to_str().unwrap().as_bytes());
    let mut depfile = b"obj\\a.o: ".to_vec();
    depfile.extend_from_slice(&token);
    depfile.push(b'\n');
    let stored = canonicalize_depfile_root(&depfile, &root);
    assert!(!contains_depfile_root_marker(&stored));
    assert!(names_unrewritten_root_path(&stored, &root));
}

#[test]
fn a_bound_depfile_replays_only_to_its_own_root() {
    let root = from_root("work/tree");
    let other = from_root("other/wt");
    let dot_root = with_dot_segment(&root);
    let fixture = format!("obj/a.o: {root}/src/a.c {dot_root}/gen/config.h\n");
    let stored = canonicalize_depfile_root(fixture.as_bytes(), Path::new(&root));
    let bound = bind_depfile_to_root(&stored, Path::new(&root));
    assert_eq!(
        rehydrate_depfile_root(bound.clone(), Some(Path::new(&root))).unwrap(),
        fixture.as_bytes()
    );
    assert!(rehydrate_depfile_root(bound.clone(), Some(Path::new(&other))).is_none());
    assert!(rehydrate_depfile_root(bound, None).is_none());
}

#[test]
fn depfile_root_respects_make_quoting() {
    let root = from_root("work/my tree");
    let quoted = root.replace(' ', "\\ ");
    let other = from_root("w/b c");
    let quoted_other = other.replace(' ', "\\ ");
    let fixture = format!("{quoted}/a.o: {quoted}/a.c /x/a\\ {quoted}/b.h\n");
    let stored = canonicalize_depfile_root(fixture.as_bytes(), Path::new(&root));
    let text = String::from_utf8(stored.clone()).unwrap();
    assert_eq!(
        text,
        format!(
            "{m}/a.o: {m}/a.c /x/a\\ {quoted}/b.h\n",
            m = DEPFILE_WORKTREE_ROOT_MARKER
        )
    );
    let delivered = rehydrate_depfile_root(stored, Some(Path::new(&other))).unwrap();
    assert_eq!(
        String::from_utf8(delivered).unwrap(),
        format!("{quoted_other}/a.o: {quoted_other}/a.c /x/a\\ {quoted}/b.h\n")
    );
}
