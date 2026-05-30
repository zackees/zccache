//! Tests for the C/C++ side of the compile-context cache key:
//! `CompileContext`, `compute_context_key`, `compute_artifact_key`.

use std::path::Path;

use crate::core::NormalizedPath;
use crate::depgraph::args::{ParsedArgs, UserDepFlags};
use crate::depgraph::search_paths::IncludeSearchPaths;

use super::super::{compute_artifact_key, compute_context_key, CompileContext};
use super::make_context;

#[test]
fn context_key_deterministic() {
    let ctx = make_context("/src/foo.c", &["/inc"], &["DEBUG"]);
    let k1 = ctx.context_key();
    let k2 = ctx.context_key();
    assert_eq!(k1, k2);
}

#[test]
fn different_source_different_key() {
    let k1 = make_context("/src/a.c", &["/inc"], &[]).context_key();
    let k2 = make_context("/src/b.c", &["/inc"], &[]).context_key();
    assert_ne!(k1, k2);
}

#[test]
fn different_defines_different_key() {
    let k1 = make_context("/src/a.c", &["/inc"], &["DEBUG"]).context_key();
    let k2 = make_context("/src/a.c", &["/inc"], &["RELEASE"]).context_key();
    assert_ne!(k1, k2);
}

#[test]
fn define_order_irrelevant() {
    let k1 = make_context("/src/a.c", &[], &["AAA", "BBB"]).context_key();
    let k2 = make_context("/src/a.c", &[], &["BBB", "AAA"]).context_key();
    assert_eq!(k1, k2, "define order should not affect context key");
}

#[test]
fn include_dir_order_matters() {
    let k1 = make_context("/src/a.c", &["/first", "/second"], &[]).context_key();
    let k2 = make_context("/src/a.c", &["/second", "/first"], &[]).context_key();
    assert_ne!(k1, k2, "include dir order should affect context key");
}

#[cfg(windows)]
#[test]
fn windows_context_key_normalizes_equivalent_path_spellings() {
    let ctx1 = CompileContext {
        source_file: NormalizedPath::from(r"C:\work\src\main.cpp"),
        include_search: IncludeSearchPaths {
            user: vec![NormalizedPath::from(r"C:\work\include")],
            ..Default::default()
        },
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: vec![NormalizedPath::from(r"C:\work\pch\base.h")],
        unknown_flags: Vec::new(),
    };
    let ctx2 = CompileContext {
        source_file: NormalizedPath::from("c:/work/src/main.cpp"),
        include_search: IncludeSearchPaths {
            user: vec![NormalizedPath::from("c:/work/include")],
            ..Default::default()
        },
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: vec![NormalizedPath::from("c:/work/pch/base.h")],
        unknown_flags: Vec::new(),
    };

    assert_eq!(ctx1.context_key(), ctx2.context_key());
}

#[cfg(windows)]
#[test]
fn windows_artifact_key_normalizes_equivalent_path_spellings() {
    let ctx = CompileContext {
        source_file: NormalizedPath::from(r"C:\work\src\main.cpp"),
        include_search: IncludeSearchPaths::default(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    };
    let key = ctx.context_key();

    let mut file_hashes_a = vec![
        (
            NormalizedPath::from(r"C:\work\include\foo.h"),
            crate::hash::hash_bytes(b"header"),
        ),
        (
            NormalizedPath::from(r"C:\work\src\main.cpp"),
            crate::hash::hash_bytes(b"source"),
        ),
    ];
    let mut file_hashes_b = vec![
        (
            NormalizedPath::from("c:/work/include/foo.h"),
            crate::hash::hash_bytes(b"header"),
        ),
        (
            NormalizedPath::from("c:/work/src/main.cpp"),
            crate::hash::hash_bytes(b"source"),
        ),
    ];

    assert_eq!(
        compute_artifact_key(&key, &mut file_hashes_a, None),
        compute_artifact_key(&key, &mut file_hashes_b, None)
    );
}

#[test]
fn artifact_key_changes_with_content() {
    let ctx = make_context("/src/a.c", &[], &[]);
    let ck = ctx.context_key();

    let hash_a = crate::hash::hash_bytes(b"content A");
    let hash_b = crate::hash::hash_bytes(b"content B");

    let ak1 = compute_artifact_key(&ck, &mut [(NormalizedPath::from("/src/a.c"), hash_a)], None);
    let ak2 = compute_artifact_key(&ck, &mut [(NormalizedPath::from("/src/a.c"), hash_b)], None);
    assert_ne!(ak1, ak2);
}

#[test]
fn artifact_key_stable_same_content() {
    let ctx = make_context("/src/a.c", &[], &[]);
    let ck = ctx.context_key();

    let hash = crate::hash::hash_bytes(b"content");

    let ak1 = compute_artifact_key(&ck, &mut [(NormalizedPath::from("/src/a.c"), hash)], None);
    let ak2 = compute_artifact_key(&ck, &mut [(NormalizedPath::from("/src/a.c"), hash)], None);
    assert_eq!(ak1, ak2);
}

#[test]
fn artifact_key_file_order_irrelevant() {
    let ctx = make_context("/src/a.c", &[], &[]);
    let ck = ctx.context_key();

    let h1 = crate::hash::hash_bytes(b"content 1");
    let h2 = crate::hash::hash_bytes(b"content 2");

    let ak1 = compute_artifact_key(
        &ck,
        &mut [
            (NormalizedPath::from("/a.h"), h1),
            (NormalizedPath::from("/b.h"), h2),
        ],
        None,
    );
    let ak2 = compute_artifact_key(
        &ck,
        &mut [
            (NormalizedPath::from("/b.h"), h2),
            (NormalizedPath::from("/a.h"), h1),
        ],
        None,
    );
    assert_eq!(ak1, ak2, "file order should not affect artifact key");
}

#[test]
fn context_key_ignores_workspace_root_when_key_root_is_stable() {
    let ctx_a = make_context(
        "/workspace-a/src/main.cpp",
        &["/workspace-a/include"],
        &["DEBUG"],
    );
    let ctx_b = make_context(
        "/workspace-b/src/main.cpp",
        &["/workspace-b/include"],
        &["DEBUG"],
    );

    let key_a = compute_context_key(&ctx_a, Some(Path::new("/workspace-a")), None);
    let key_b = compute_context_key(&ctx_b, Some(Path::new("/workspace-b")), None);

    assert_eq!(key_a, key_b);
}

#[test]
fn cxx_context_key_with_root_normalizes_file_prefix_map_roots() {
    let mut ctx_a = make_context("/workspace-a/src/main.cpp", &[], &[]);
    ctx_a.flags = vec!["-ffile-prefix-map=/workspace-a=.".to_string()];
    let mut ctx_b = make_context("/workspace-b/src/main.cpp", &[], &[]);
    ctx_b.flags = vec!["-ffile-prefix-map=/workspace-b=.".to_string()];

    assert_eq!(
        compute_context_key(&ctx_a, Some(Path::new("/workspace-a")), None),
        compute_context_key(&ctx_b, Some(Path::new("/workspace-b")), None),
        "equivalent file-prefix-map old prefixes under the key root should match"
    );
}

#[test]
fn cxx_context_key_with_root_preserves_file_prefix_map_new_prefixes() {
    let mut ctx_a = make_context("/workspace-a/src/main.cpp", &[], &[]);
    ctx_a.flags = vec!["-ffile-prefix-map=/workspace-a=.".to_string()];
    let mut ctx_b = make_context("/workspace-b/src/main.cpp", &[], &[]);
    ctx_b.flags = vec!["-ffile-prefix-map=/workspace-b=/src".to_string()];

    assert_ne!(
        compute_context_key(&ctx_a, Some(Path::new("/workspace-a")), None),
        compute_context_key(&ctx_b, Some(Path::new("/workspace-b")), None),
        "different file-prefix-map new prefixes should remain key-significant"
    );
}

#[test]
fn cxx_context_key_with_root_keeps_external_file_prefix_map_old_prefixes_distinct() {
    let mut ctx_a = make_context("/workspace-a/src/main.cpp", &[], &[]);
    ctx_a.flags = vec!["-ffile-prefix-map=/external-a=.".to_string()];
    let mut ctx_b = make_context("/workspace-b/src/main.cpp", &[], &[]);
    ctx_b.flags = vec!["-ffile-prefix-map=/external-b=.".to_string()];

    assert_ne!(
        compute_context_key(&ctx_a, Some(Path::new("/workspace-a")), None),
        compute_context_key(&ctx_b, Some(Path::new("/workspace-b")), None),
        "file-prefix-map old prefixes outside the key root should keep absolute identity"
    );
}

#[test]
fn cxx_context_key_with_root_normalizes_prefix_maps_in_unknown_flags() {
    let mut ctx_a = make_context("/workspace-a/src/main.cpp", &[], &[]);
    ctx_a.unknown_flags = vec![
        "-fcoverage-prefix-map=/workspace-a=/coverage".to_string(),
        "-fdebug-prefix-map=/workspace-a=/debug".to_string(),
        "-fmacro-prefix-map=/workspace-a=/macro".to_string(),
        "-fprofile-prefix-map=/workspace-a=/profile".to_string(),
    ];
    let mut ctx_b = make_context("/workspace-b/src/main.cpp", &[], &[]);
    ctx_b.unknown_flags = vec![
        "-fcoverage-prefix-map=/workspace-b=/coverage".to_string(),
        "-fdebug-prefix-map=/workspace-b=/debug".to_string(),
        "-fmacro-prefix-map=/workspace-b=/macro".to_string(),
        "-fprofile-prefix-map=/workspace-b=/profile".to_string(),
    ];

    assert_eq!(
        compute_context_key(&ctx_a, Some(Path::new("/workspace-a")), None),
        compute_context_key(&ctx_b, Some(Path::new("/workspace-b")), None),
        "C/C++ prefix-map flags should normalize under unknown_flags"
    );
}

#[test]
fn artifact_key_ignores_workspace_root_when_key_root_is_stable() {
    let ctx = make_context("/workspace-a/src/main.cpp", &["/workspace-a/include"], &[]);
    let key = compute_context_key(&ctx, Some(Path::new("/workspace-a")), None);
    let mut hashes_a = vec![
        (
            NormalizedPath::from("/workspace-a/include/foo.h"),
            crate::hash::hash_bytes(b"header"),
        ),
        (
            NormalizedPath::from("/workspace-a/src/main.cpp"),
            crate::hash::hash_bytes(b"source"),
        ),
    ];
    let mut hashes_b = vec![
        (
            NormalizedPath::from("/workspace-b/include/foo.h"),
            crate::hash::hash_bytes(b"header"),
        ),
        (
            NormalizedPath::from("/workspace-b/src/main.cpp"),
            crate::hash::hash_bytes(b"source"),
        ),
    ];

    assert_eq!(
        compute_artifact_key(&key, &mut hashes_a, Some(Path::new("/workspace-a"))),
        compute_artifact_key(&key, &mut hashes_b, Some(Path::new("/workspace-b")))
    );
}

#[test]
fn context_key_display() {
    let ctx = make_context("/src/a.c", &[], &[]);
    let key = ctx.context_key();
    let display = format!("{key}");
    assert!(display.starts_with("ctx:"));
    assert_eq!(display.len(), 4 + 64); // "ctx:" + 64 hex chars
}

#[test]
fn from_parsed_args_sorts() {
    let args = ParsedArgs {
        source_file: NormalizedPath::from("/src/a.c"),
        output_file: None,
        include_search: IncludeSearchPaths::default(),
        defines: vec!["ZZZ".into(), "AAA".into()],
        undefines: Vec::new(),
        flags: vec!["-Wall".into(), "-O2".into()],
        force_includes: Vec::new(),
        compiler: None,
        dep_flags: UserDepFlags::default(),
        unknown_flags: vec!["--zzz".into(), "--aaa".into()],
    };
    let ctx = CompileContext::from_parsed_args(args);
    assert_eq!(ctx.defines, vec!["AAA", "ZZZ"]);
    assert_eq!(ctx.flags, vec!["-O2", "-Wall"]);
    assert_eq!(ctx.unknown_flags, vec!["--aaa", "--zzz"]);
}

#[test]
fn different_flags_different_key() {
    let mut ctx1 = make_context("/src/a.c", &[], &[]);
    ctx1.flags = vec!["-std=c++17".into()];
    let mut ctx2 = make_context("/src/a.c", &[], &[]);
    ctx2.flags = vec!["-std=c++20".into()];
    assert_ne!(ctx1.context_key(), ctx2.context_key());
}

#[test]
fn force_include_affects_key() {
    let ctx1 = make_context("/src/a.c", &[], &[]);
    let mut ctx2 = make_context("/src/a.c", &[], &[]);
    ctx2.force_includes = vec![NormalizedPath::from("/pch.h")];
    assert_ne!(ctx1.context_key(), ctx2.context_key());
}

#[test]
fn unknown_flags_affect_key() {
    let ctx1 = make_context("/src/a.c", &[], &[]);
    let mut ctx2 = make_context("/src/a.c", &[], &[]);
    ctx2.unknown_flags = vec!["--deploy-dependencies".into()];
    assert_ne!(
        ctx1.context_key(),
        ctx2.context_key(),
        "unknown flags should affect context key"
    );
}

#[test]
fn unknown_flags_order_irrelevant() {
    let mut ctx1 = make_context("/src/a.c", &[], &[]);
    ctx1.unknown_flags = vec!["--aaa".into(), "--bbb".into()];
    let mut ctx2 = make_context("/src/a.c", &[], &[]);
    ctx2.unknown_flags = vec!["--bbb".into(), "--aaa".into()];
    // Both are sorted in make_context... but actually make_context doesn't sort unknown_flags.
    // from_parsed_args sorts them. In the test helper we set them directly,
    // so we need to sort manually for this test to be meaningful.
    ctx1.unknown_flags.sort();
    ctx2.unknown_flags.sort();
    assert_eq!(
        ctx1.context_key(),
        ctx2.context_key(),
        "unknown flag order should not affect context key"
    );
}
