//! Tests for path-remap behaviour (issue #474).
//!
//! Covers the two halves of the fix:
//!
//! 1. **Flag injection** — `effective_compile_args` should inject
//!    `-fmacro-prefix-map=<root>=.` and `-fdebug-prefix-map=<root>=.` for
//!    clang / gcc (alongside the existing `-ffile-prefix-map`). Modern clang
//!    treats `-ffile-prefix-map` as an umbrella, but older clang (< 10) and
//!    some gcc versions need the explicit pair; we inject both for
//!    portability. MSVC has no equivalent flag and must skip.
//!
//! 2. **Worktree-keyed cache** — `requires_worktree_in_key` is true for the
//!    cases where the compiler embeds absolute paths the remap flags can't
//!    scrub (PCH `.pch`/`.gch` binaries; all of MSVC because it has no
//!    flag). When true, `compute_artifact_key` hashes the `worktree_salt`
//!    into the key so two sibling worktrees of the same commit get
//!    distinct cache entries for these artifacts — preventing the
//!    cross-clone path-leak from #474.
//!
//! All tests are fixture-based; none invoke a real compiler. They are fast
//! enough (< 10 ms each) to keep in the unit-test tier.

use super::super::*;
use crate::compiler::{CompilerFamily, SourceMode};

// ── Piece A: flag injection ────────────────────────────────────────────────

#[test]
fn injects_macro_prefix_map_for_clang() {
    // Older clang doesn't honour `-ffile-prefix-map` as an umbrella over
    // `__FILE__` expansions; the explicit `-fmacro-prefix-map` is required
    // to stop fastled9 paths from leaking into the .rodata of artifacts
    // shipped to fastled10.
    let tmp = tempfile::tempdir().unwrap();
    let root_path = tmp.path().join("workspace");
    let root = NormalizedPath::new(&root_path);
    let env = vec![(PATH_REMAP_ENV.to_string(), "auto".to_string())];
    let args = vec!["-c".to_string(), "src/main.cc".to_string()];

    let effective = effective_compile_args(
        &args,
        Path::new("/usr/bin/clang++"),
        &root_path,
        Some(&root),
        Some(&env),
    );

    assert!(
        effective.contains(&format!("-fmacro-prefix-map={}=.", root_path.display())),
        "expected -fmacro-prefix-map=<root>=. in {:?}",
        effective
    );
}

#[test]
fn injects_debug_prefix_map_for_clang() {
    // DWARF debug info embeds compilation-directory + source paths. Without
    // an explicit `-fdebug-prefix-map`, gdb/lldb on a sibling worktree show
    // the original clone's paths.
    let tmp = tempfile::tempdir().unwrap();
    let root_path = tmp.path().join("workspace");
    let root = NormalizedPath::new(&root_path);
    let env = vec![(PATH_REMAP_ENV.to_string(), "auto".to_string())];
    let args = vec!["-c".to_string(), "src/main.cc".to_string()];

    let effective = effective_compile_args(
        &args,
        Path::new("/usr/bin/clang++"),
        &root_path,
        Some(&root),
        Some(&env),
    );

    assert!(
        effective.contains(&format!("-fdebug-prefix-map={}=.", root_path.display())),
        "expected -fdebug-prefix-map=<root>=. in {:?}",
        effective
    );
}

#[test]
fn injects_macro_prefix_map_for_gcc() {
    let tmp = tempfile::tempdir().unwrap();
    let root_path = tmp.path().join("workspace");
    let root = NormalizedPath::new(&root_path);
    let env = vec![(PATH_REMAP_ENV.to_string(), "auto".to_string())];
    let args = vec!["-c".to_string(), "src/main.cc".to_string()];

    let effective = effective_compile_args(
        &args,
        Path::new("/usr/bin/g++"),
        &root_path,
        Some(&root),
        Some(&env),
    );

    assert!(
        effective.contains(&format!("-fmacro-prefix-map={}=.", root_path.display())),
        "expected -fmacro-prefix-map=<root>=. for gcc in {:?}",
        effective
    );
}

#[test]
fn does_not_inject_redundant_macro_prefix_map_for_clang() {
    // User-supplied narrower `-fmacro-prefix-map` for the same root must
    // suppress the auto-injection — otherwise we'd ship two conflicting
    // remaps for the same FROM and the second-wins behaviour silently
    // changes which one is active.
    let tmp = tempfile::tempdir().unwrap();
    let root_path = tmp.path().join("workspace");
    let root = NormalizedPath::new(&root_path);
    let env = vec![(PATH_REMAP_ENV.to_string(), "auto".to_string())];
    let user_map = format!("-fmacro-prefix-map={}=/source", root_path.display());
    let args = vec![
        user_map.clone(),
        "-c".to_string(),
        "src/main.cc".to_string(),
    ];

    let effective = effective_compile_args(
        &args,
        Path::new("/usr/bin/clang++"),
        &root_path,
        Some(&root),
        Some(&env),
    );

    let macro_maps: Vec<&String> = effective
        .iter()
        .filter(|a| a.starts_with("-fmacro-prefix-map="))
        .collect();
    assert_eq!(
        macro_maps.len(),
        1,
        "expected one -fmacro-prefix-map (the user's), got {:?}",
        macro_maps
    );
    assert_eq!(macro_maps[0], &user_map);
}

#[test]
fn does_not_inject_for_msvc() {
    // cl.exe doesn't support `-fmacro-prefix-map` / `-ffile-prefix-map` /
    // `-fdebug-prefix-map`. Auto-injecting any of them would make MSVC
    // reject the command line.
    let tmp = tempfile::tempdir().unwrap();
    let root_path = tmp.path().join("workspace");
    let root = NormalizedPath::new(&root_path);
    let env = vec![(PATH_REMAP_ENV.to_string(), "auto".to_string())];
    let args = vec!["/c".to_string(), "src\\main.cpp".to_string()];

    let effective = effective_compile_args(
        &args,
        Path::new("cl.exe"),
        &root_path,
        Some(&root),
        Some(&env),
    );

    for arg in &effective {
        assert!(
            !arg.starts_with("-fmacro-prefix-map=")
                && !arg.starts_with("-fdebug-prefix-map=")
                && !arg.starts_with("-ffile-prefix-map="),
            "MSVC argv must not contain prefix-map flags, found {:?} in {:?}",
            arg,
            effective
        );
    }
}

#[test]
fn injects_remap_path_prefix_for_rustc() {
    // Pre-existing behaviour pinned: rustc gets `--remap-path-prefix=<root>=.`
    // when `ZCCACHE_PATH_REMAP=auto`. The new C++ flag injection must not
    // break this code path.
    let tmp = tempfile::tempdir().unwrap();
    let root_path = tmp.path().join("workspace");
    let root = NormalizedPath::new(&root_path);
    let env = vec![(PATH_REMAP_ENV.to_string(), "auto".to_string())];
    let args = vec![
        "--crate-type".to_string(),
        "lib".to_string(),
        "src/lib.rs".to_string(),
    ];

    let effective = effective_compile_args(
        &args,
        Path::new("rustc"),
        &root_path,
        Some(&root),
        Some(&env),
    );

    assert_eq!(
        &effective[..2],
        &[
            "--remap-path-prefix".to_string(),
            format!("{}=.", root_path.display())
        ],
        "rustc should still get --remap-path-prefix=<root>=. as its first arg pair"
    );
}

// ── Piece B: `requires_worktree_in_key` truth table ────────────────────────

#[test]
fn requires_worktree_in_key_for_pch_clang() {
    // PCH binary stores absolute header paths in its serialised AST table.
    // Flag-based remap can't scrub them; the cache entry must be per-
    // worktree to prevent fastled9's PCH from being served to fastled10.
    assert!(requires_worktree_in_key(
        CompilerFamily::Clang,
        SourceMode::Header
    ));
}

#[test]
fn requires_worktree_in_key_for_pch_gcc() {
    assert!(requires_worktree_in_key(
        CompilerFamily::Gcc,
        SourceMode::Header
    ));
}

#[test]
fn requires_worktree_in_key_for_msvc_regardless_of_mode() {
    // cl.exe has no `-fmacro-prefix-map` equivalent — every MSVC compile
    // potentially embeds absolute paths in debug info + `$$PDB` refs.
    // Worktree-key is the only robust answer.
    assert!(requires_worktree_in_key(
        CompilerFamily::Msvc,
        SourceMode::Normal
    ));
    assert!(requires_worktree_in_key(
        CompilerFamily::Msvc,
        SourceMode::Header
    ));
}

#[test]
fn requires_worktree_in_key_false_for_normal_clang() {
    // Cross-worktree sharing must stay intact for the common case
    // (clang non-PCH .cpp compile with auto-remap). Returning true here
    // would tank the cache hit rate for FastLED-class workflows.
    assert!(!requires_worktree_in_key(
        CompilerFamily::Clang,
        SourceMode::Normal
    ));
}

#[test]
fn requires_worktree_in_key_false_for_normal_gcc() {
    assert!(!requires_worktree_in_key(
        CompilerFamily::Gcc,
        SourceMode::Normal
    ));
}

#[test]
fn requires_worktree_in_key_false_for_rustc() {
    // rustc's `--remap-path-prefix` is enough to scrub paths from .rlib /
    // .rmeta artifacts; rustc artifacts must share across worktrees.
    assert!(!requires_worktree_in_key(
        CompilerFamily::Rustc,
        SourceMode::Normal
    ));
}

// ── Piece B: `compute_context_key` worktree-salt behaviour ─────────────────
//
// The salt lives on the context key (not the artifact key) so all artifact
// keys derived from a salted context are automatically per-worktree without
// the eight downstream `compute_artifact_key` callers needing to know about
// the salt. Tests pin that compute_context_key's optional salt parameter
// (a) yields distinct keys when present and different, (b) yields identical
// keys when absent (the cross-worktree-share-preserving default), and
// (c) is deterministic.

mod context_key_salt {
    use crate::core::NormalizedPath;
    use crate::depgraph::context::{compute_context_key, CompileContext};
    use crate::depgraph::search_paths::IncludeSearchPaths;

    fn minimal_context() -> CompileContext {
        CompileContext {
            source_file: NormalizedPath::from("src/main.cpp"),
            include_search: IncludeSearchPaths::default(),
            defines: Vec::new(),
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
        }
    }

    #[test]
    fn context_key_differs_across_worktrees_when_salt_supplied() {
        // The whole point of the salt: same context in two different
        // worktrees → distinct context keys → distinct downstream
        // artifact keys, even though every other input is identical.
        let tmp = tempfile::tempdir().unwrap();
        let root_a = tmp.path().join("worktree-a");
        let root_b = tmp.path().join("worktree-b");
        let ctx = minimal_context();

        let key_a = compute_context_key(&ctx, None, Some(&root_a));
        let key_b = compute_context_key(&ctx, None, Some(&root_b));

        assert_ne!(
            key_a, key_b,
            "PCH / MSVC: per-worktree salt must yield distinct context keys"
        );
    }

    #[test]
    fn context_key_matches_across_worktrees_when_no_salt() {
        // Cross-worktree sharing for the common case: clang non-PCH .cpp,
        // rustc, etc. Salt is None → same content → same key, regardless
        // of which worktree the build happens in.
        let ctx = minimal_context();

        let key_a = compute_context_key(&ctx, None, None);
        let key_b = compute_context_key(&ctx, None, None);

        assert_eq!(
            key_a, key_b,
            "no salt → identical keys; cross-worktree sharing must work"
        );
    }

    #[test]
    fn context_key_stable_for_same_worktree_salt() {
        // Sanity: the salt is deterministic. Same context + same salt →
        // same key on repeat invocation. Otherwise the first build in a
        // worktree would store a key the second build couldn't match.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("worktree-a");
        let ctx = minimal_context();

        let key_1 = compute_context_key(&ctx, None, Some(&root));
        let key_2 = compute_context_key(&ctx, None, Some(&root));

        assert_eq!(key_1, key_2, "salt must be deterministic across calls");
    }

    #[test]
    fn context_key_with_salt_differs_from_no_salt() {
        // Sanity: Some(x) and None must be distinguishable, otherwise the
        // salt branch is dead code and #474 stays broken.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("worktree");
        let ctx = minimal_context();

        let key_with_salt = compute_context_key(&ctx, None, Some(&root));
        let key_no_salt = compute_context_key(&ctx, None, None);

        assert_ne!(
            key_with_salt, key_no_salt,
            "Some(salt) must produce a different key from None — \
             otherwise the salt branch is a no-op"
        );
    }
}
