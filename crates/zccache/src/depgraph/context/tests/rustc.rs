//! Tests for the rustc side of the compile-context cache key:
//! `RustcCompileContext`, `compute_rustc_artifact_key`,
//! `compute_rustc_artifact_key_with_root`, and the `VOLATILE_CARGO_ENV_VARS`
//! filter regressions (issues #139 / #396).

use std::path::Path;

use crate::core::NormalizedPath;
use crate::depgraph::rustc_args::{ExternCrate, RustcParsedArgs};

use super::super::{
    compute_rustc_artifact_key, compute_rustc_artifact_key_with_root, RustcCompileContext,
};
use super::{make_context, make_rustc_context, make_rustc_context_with_env};

#[cfg(windows)]
#[test]
fn windows_rustc_context_key_normalizes_equivalent_source_path_spellings() {
    let ctx1 = RustcCompileContext {
        source_file: NormalizedPath::from(r"C:\work\src\lib.rs"),
        crate_name: Some("demo".to_string()),
        crate_types: vec!["rlib".to_string()],
        edition: Some("2021".to_string()),
        emit_types: vec!["link".to_string()],
        cfgs: Vec::new(),
        check_cfgs: Vec::new(),
        codegen_flags: Vec::new(),
        cargo_metadata: None,
        extra_filename: None,
        target: None,
        cap_lints: None,
        extern_crates: Vec::new(),
        lint_flags: Vec::new(),
        unknown_flags: Vec::new(),
        remap_path_prefixes: Vec::new(),
        env_vars: Vec::new(),
        compiler_hash: None,
    };
    let mut ctx2 = ctx1.clone();
    ctx2.source_file = NormalizedPath::from("c:/work/src/lib.rs");

    assert_eq!(ctx1.context_key(), ctx2.context_key());
}

#[test]
fn rustc_context_key_deterministic() {
    let ctx = make_rustc_context("/src/lib.rs", "2021");
    let k1 = ctx.context_key();
    let k2 = ctx.context_key();
    assert_eq!(k1, k2);
}

#[test]
fn rustc_context_key_delegates_to_rootless_helper() {
    let ctx = make_rustc_context("/src/lib.rs", "2021");
    assert_eq!(ctx.context_key(), ctx.context_key_with_root(None));
}

#[test]
fn rustc_context_key_with_root_matches_equivalent_roots() {
    let ctx_a = make_rustc_context("/workspace-a/crates/demo/src/lib.rs", "2021");
    let ctx_b = make_rustc_context("/workspace-b/crates/demo/src/lib.rs", "2021");

    assert_ne!(
        ctx_a.context_key(),
        ctx_b.context_key(),
        "rootless rustc context keys should keep the existing absolute-path behavior"
    );
    assert_eq!(
        ctx_a.context_key_with_root(Some(Path::new("/workspace-a"))),
        ctx_b.context_key_with_root(Some(Path::new("/workspace-b"))),
        "source paths under equivalent roots should hash relative to those roots"
    );
}

#[test]
fn rustc_context_key_with_root_keeps_external_sources_distinct() {
    let ctx_a = make_rustc_context("/external-a/generated/lib.rs", "2021");
    let ctx_b = make_rustc_context("/external-b/generated/lib.rs", "2021");

    assert_ne!(
        ctx_a.context_key_with_root(Some(Path::new("/workspace-a"))),
        ctx_b.context_key_with_root(Some(Path::new("/workspace-b"))),
        "sources outside the supplied roots must retain absolute path identity"
    );
}

#[test]
fn rustc_context_key_with_root_normalizes_remap_left_side_under_root() {
    let mut ctx_a = make_rustc_context("/workspace-a/crates/demo/src/lib.rs", "2021");
    ctx_a.remap_path_prefixes = vec!["/workspace-a=/src".to_string()];
    let mut ctx_b = make_rustc_context("/workspace-b/crates/demo/src/lib.rs", "2021");
    ctx_b.remap_path_prefixes = vec!["/workspace-b=/src".to_string()];

    assert_eq!(
        ctx_a.context_key_with_root(Some(Path::new("/workspace-a"))),
        ctx_b.context_key_with_root(Some(Path::new("/workspace-b"))),
        "remap left sides under the root should hash relative to the root"
    );
}

#[test]
fn rustc_context_key_with_root_normalizes_root_remap_left_side() {
    let mut ctx_a = make_rustc_context("/workspace-a/crates/demo/src/lib.rs", "2021");
    ctx_a.remap_path_prefixes = vec!["/workspace-a=.".to_string()];
    let mut ctx_b = make_rustc_context("/workspace-b/crates/demo/src/lib.rs", "2021");
    ctx_b.remap_path_prefixes = vec!["/workspace-b=.".to_string()];

    assert_eq!(
        ctx_a.context_key_with_root(Some(Path::new("/workspace-a"))),
        ctx_b.context_key_with_root(Some(Path::new("/workspace-b"))),
        "root-covering remaps should hash equivalently across roots"
    );
}

#[test]
fn rustc_context_key_with_root_keeps_external_remap_left_sides_distinct() {
    let mut ctx_a = make_rustc_context("/workspace-a/crates/demo/src/lib.rs", "2021");
    ctx_a.remap_path_prefixes = vec!["/external-a=/src".to_string()];
    let mut ctx_b = make_rustc_context("/workspace-b/crates/demo/src/lib.rs", "2021");
    ctx_b.remap_path_prefixes = vec!["/external-b=/src".to_string()];

    assert_ne!(
        ctx_a.context_key_with_root(Some(Path::new("/workspace-a"))),
        ctx_b.context_key_with_root(Some(Path::new("/workspace-b"))),
        "remap left sides outside the root should keep absolute path identity"
    );
}

#[test]
fn rustc_context_key_with_root_does_not_normalize_remap_right_side() {
    let mut ctx_a = make_rustc_context("/workspace-a/crates/demo/src/lib.rs", "2021");
    ctx_a.remap_path_prefixes = vec!["/workspace-a=/workspace-a".to_string()];
    let mut ctx_b = make_rustc_context("/workspace-b/crates/demo/src/lib.rs", "2021");
    ctx_b.remap_path_prefixes = vec!["/workspace-b=/workspace-b".to_string()];

    assert_ne!(
        ctx_a.context_key_with_root(Some(Path::new("/workspace-a"))),
        ctx_b.context_key_with_root(Some(Path::new("/workspace-b"))),
        "only the remap left side is root-normalized"
    );
}

#[test]
fn rustc_context_key_with_root_preserves_remap_right_side() {
    let mut ctx_a = make_rustc_context("/workspace-a/crates/demo/src/lib.rs", "2021");
    ctx_a.remap_path_prefixes = vec!["/workspace-a=.".to_string()];
    let mut ctx_b = make_rustc_context("/workspace-b/crates/demo/src/lib.rs", "2021");
    ctx_b.remap_path_prefixes = vec!["/workspace-b=/src".to_string()];

    assert_ne!(
        ctx_a.context_key_with_root(Some(Path::new("/workspace-a"))),
        ctx_b.context_key_with_root(Some(Path::new("/workspace-b"))),
        "different remap new prefixes must remain cache-significant"
    );
}

#[test]
fn rustc_context_key_with_root_keeps_malformed_remaps_distinct() {
    let mut ctx_a = make_rustc_context("/workspace-a/crates/demo/src/lib.rs", "2021");
    ctx_a.remap_path_prefixes = vec!["/workspace-a".to_string()];
    let mut ctx_b = make_rustc_context("/workspace-b/crates/demo/src/lib.rs", "2021");
    ctx_b.remap_path_prefixes = vec!["/workspace-b".to_string()];

    assert_ne!(
        ctx_a.context_key_with_root(Some(Path::new("/workspace-a"))),
        ctx_b.context_key_with_root(Some(Path::new("/workspace-b"))),
        "malformed remap values should not be root-normalized"
    );
}

#[test]
fn rustc_different_edition_different_key() {
    let k1 = make_rustc_context("/src/lib.rs", "2021").context_key();
    let k2 = make_rustc_context("/src/lib.rs", "2024").context_key();
    assert_ne!(k1, k2);
}

#[test]
fn rustc_different_cfg_different_key() {
    let mut ctx1 = make_rustc_context("/src/lib.rs", "2021");
    ctx1.cfgs = vec!["feature=\"std\"".to_string()];
    let mut ctx2 = make_rustc_context("/src/lib.rs", "2021");
    ctx2.cfgs = vec!["feature=\"alloc\"".to_string()];
    assert_ne!(ctx1.context_key(), ctx2.context_key());
}

#[test]
fn rustc_different_codegen_different_key() {
    let mut ctx1 = make_rustc_context("/src/lib.rs", "2021");
    ctx1.codegen_flags = vec!["opt-level=2".to_string()];
    let mut ctx2 = make_rustc_context("/src/lib.rs", "2021");
    ctx2.codegen_flags = vec!["opt-level=3".to_string()];
    assert_ne!(ctx1.context_key(), ctx2.context_key());
}

#[test]
fn rustc_cargo_metadata_affects_key() {
    let mut ctx1 = make_rustc_context("/src/lib.rs", "2021");
    ctx1.cargo_metadata = Some("worktree-a".to_string());
    let mut ctx2 = make_rustc_context("/src/lib.rs", "2021");
    ctx2.cargo_metadata = Some("worktree-b".to_string());
    assert_ne!(
        ctx1.context_key(),
        ctx2.context_key(),
        "-C metadata participates in crate disambiguation and must affect the key"
    );
}

#[test]
fn rustc_extra_filename_affects_key() {
    let mut ctx1 = make_rustc_context("/src/lib.rs", "2021");
    ctx1.extra_filename = Some("-aaa111".to_string());
    let mut ctx2 = make_rustc_context("/src/lib.rs", "2021");
    ctx2.extra_filename = Some("-bbb222".to_string());
    assert_ne!(
        ctx1.context_key(),
        ctx2.context_key(),
        "-C extra-filename controls emitted artifact names and must affect the key"
    );
}

#[test]
fn rustc_check_metadata_compat_key_matches_build_emit_superset() {
    let mut check = make_rustc_context("/src/lib.rs", "2021");
    check.emit_types = vec!["dep-info".to_string(), "metadata".to_string()];
    check.cargo_metadata = Some("check-metadata".to_string());
    check.extra_filename = Some("-check".to_string());
    check.extern_crates = vec![("dep".into(), "/target/check/libdep-check.rmeta".into())];

    let mut build = check.clone();
    build.emit_types = vec![
        "dep-info".to_string(),
        "metadata".to_string(),
        "link".to_string(),
    ];
    build.cargo_metadata = Some("build-metadata".to_string());
    build.extra_filename = Some("-build".to_string());
    build.extern_crates = vec![("dep".into(), "/target/build/libdep-build.rmeta".into())];

    assert_ne!(check.context_key(), build.context_key());
    assert_eq!(
        check.check_metadata_compat_key_with_root(None),
        build.check_metadata_compat_key_with_root(None),
        "check metadata should be able to probe build metadata+link via a separate alias"
    );
}

#[test]
fn rustc_check_metadata_compat_key_rejects_non_metadata_shapes() {
    let mut ctx = make_rustc_context("/src/lib.rs", "2021");
    ctx.emit_types = vec!["dep-info".to_string(), "link".to_string()];
    assert!(ctx.check_metadata_compat_key_with_root(None).is_none());

    ctx.emit_types = vec!["llvm-ir".to_string()];
    assert!(ctx.check_metadata_compat_key_with_root(None).is_none());

    ctx.emit_types = vec!["dep-info".to_string(), "metadata".to_string()];
    ctx.crate_types = vec!["proc-macro".to_string()];
    assert!(ctx.check_metadata_compat_key_with_root(None).is_none());
}

#[test]
fn rustc_context_key_differs_from_cc() {
    // The domain separation tags differ, so even identical-looking contexts
    // produce different keys.
    let cc_ctx = make_context("/src/lib.rs", &[], &[]);
    let rustc_ctx = make_rustc_context("/src/lib.rs", "2021");
    assert_ne!(
        cc_ctx.context_key(),
        rustc_ctx.context_key(),
        "C and Rust context keys must differ (domain separation)"
    );
}

#[test]
fn rustc_compiler_hash_affects_key() {
    let ctx1 = make_rustc_context("/src/lib.rs", "2021");
    let mut ctx2 = make_rustc_context("/src/lib.rs", "2021");
    ctx2.compiler_hash = Some(crate::hash::hash_bytes(b"rustc-1.94.1"));
    assert_ne!(
        ctx1.context_key(),
        ctx2.context_key(),
        "different compiler hash must produce different context key"
    );
}

#[test]
fn rustc_different_compiler_versions_different_key() {
    let mut ctx1 = make_rustc_context("/src/lib.rs", "2021");
    ctx1.compiler_hash = Some(crate::hash::hash_bytes(b"rustc-1.94.1"));
    let mut ctx2 = make_rustc_context("/src/lib.rs", "2021");
    ctx2.compiler_hash = Some(crate::hash::hash_bytes(b"rustc-1.94.2"));
    assert_ne!(ctx1.context_key(), ctx2.context_key());
}

#[test]
fn rustc_extern_crates_affect_key() {
    let ctx1 = make_rustc_context("/src/lib.rs", "2021");
    let mut ctx2 = make_rustc_context("/src/lib.rs", "2021");
    ctx2.extern_crates = vec![("serde".into(), "/deps/libserde.rlib".into())];
    assert_ne!(ctx1.context_key(), ctx2.context_key());
}

#[test]
fn rustc_different_extern_paths_different_key() {
    let mut ctx1 = make_rustc_context("/src/lib.rs", "2021");
    ctx1.extern_crates = vec![("a".into(), "/deps/liba_v1.rlib".into())];
    let mut ctx2 = make_rustc_context("/src/lib.rs", "2021");
    ctx2.extern_crates = vec![("a".into(), "/deps/liba_v2.rlib".into())];
    assert_ne!(
        ctx1.context_key(),
        ctx2.context_key(),
        "different extern paths must produce different context keys"
    );
}

#[test]
fn rustc_from_parsed_args() {
    let args = RustcParsedArgs {
        source_file: NormalizedPath::from("/src/lib.rs"),
        crate_name: Some("mylib".to_string()),
        crate_types: vec!["rlib".to_string(), "lib".to_string()],
        edition: Some("2021".to_string()),
        emit_types: vec!["link".to_string(), "dep-info".to_string()],
        cfgs: vec!["unix".to_string(), "feature=\"std\"".to_string()],
        check_cfgs: Vec::new(),
        codegen_flags: vec!["opt-level=2".to_string()],
        target: None,
        cap_lints: Some("allow".to_string()),
        externs: vec![
            ExternCrate {
                name: "serde".to_string(),
                path: NormalizedPath::from("/deps/libserde.rlib"),
            },
            ExternCrate {
                name: "log".to_string(),
                path: NormalizedPath::from("/deps/liblog.rlib"),
            },
        ],
        lint_flags: Vec::new(),
        unknown_flags: Vec::new(),
        out_dir: None,
        extra_filename: Some("-abc123".to_string()),
        cargo_metadata: Some("abc123".to_string()),
        incremental_dir: None,
        error_format: None,
        json_format: None,
        color: None,
        diagnostic_width: None,
        search_paths: Vec::new(),
        remap_path_prefixes: Vec::new(),
        sysroot: None,
        output_file: None,
    };
    let ctx = RustcCompileContext::from_parsed_args(&args, &[], None);
    // Crate types sorted
    assert_eq!(ctx.crate_types, vec!["lib", "rlib"]);
    // Emit types sorted
    assert_eq!(ctx.emit_types, vec!["dep-info", "link"]);
    // Extern crates extracted and sorted by name
    assert_eq!(ctx.extern_crates.len(), 2);
    assert_eq!(ctx.extern_crates[0].0, "log");
    assert_eq!(ctx.extern_crates[1].0, "serde");
    assert_eq!(ctx.cargo_metadata.as_deref(), Some("abc123"));
    assert_eq!(ctx.extra_filename.as_deref(), Some("-abc123"));
}

#[test]
fn rustc_artifact_key_changes_with_extern_content() {
    let ctx = make_rustc_context("/src/lib.rs", "2021");
    let ck = ctx.context_key();

    let src_hash = crate::hash::hash_bytes(b"source");
    let ext_hash_a = crate::hash::hash_bytes(b"extern A");
    let ext_hash_b = crate::hash::hash_bytes(b"extern B");

    let ak1 = compute_rustc_artifact_key(
        &ck,
        &mut [(NormalizedPath::from("/src/lib.rs"), src_hash)],
        &mut [("serde".to_string(), ext_hash_a)],
    );
    let ak2 = compute_rustc_artifact_key(
        &ck,
        &mut [(NormalizedPath::from("/src/lib.rs"), src_hash)],
        &mut [("serde".to_string(), ext_hash_b)],
    );
    assert_ne!(
        ak1, ak2,
        "different extern content should produce different artifact key"
    );
}

// ─── Cache-key path-independence tests (issues #139 / #396) ────────────────
//
// These tests pin the contract that cache keys are independent of the absolute
// path at which a workspace happens to live on disk. The same project checked
// out at `/tmp/proj-a` and `/tmp/proj-b` must produce the same rustc cache key,
// otherwise every `cargo {check,clippy,test}` after moving / re-cloning the
// repo cold-misses through the entire dep graph.

/// T1 — Two contexts that differ only in `CARGO_MANIFEST_DIR` must have
/// the same cache key. This is the headline regression: the same crate
/// checked out at two paths should not invalidate the cache.
#[test]
fn rustc_context_key_ignores_cargo_manifest_dir() {
    let ctx_a = make_rustc_context_with_env(vec![
        ("CARGO_MANIFEST_DIR".into(), "/tmp/proj-a/crates/foo".into()),
        ("CARGO_PKG_NAME".into(), "foo".into()),
        ("CARGO_PKG_VERSION".into(), "1.2.3".into()),
    ]);
    let ctx_b = make_rustc_context_with_env(vec![
        ("CARGO_MANIFEST_DIR".into(), "/tmp/proj-b/crates/foo".into()),
        ("CARGO_PKG_NAME".into(), "foo".into()),
        ("CARGO_PKG_VERSION".into(), "1.2.3".into()),
    ]);
    assert_eq!(
        ctx_a.context_key(),
        ctx_b.context_key(),
        "CARGO_MANIFEST_DIR is volatile (absolute path) and must NOT \
         contribute to the cache key; otherwise a project clone or rename \
         invalidates every dependent compile"
    );
}

/// T2 — Same idea for `CARGO_MANIFEST_PATH`. Cargo started exporting this
/// in newer versions; it's similarly an absolute path to `Cargo.toml`.
#[test]
fn rustc_context_key_ignores_cargo_manifest_path() {
    let ctx_a = make_rustc_context_with_env(vec![
        (
            "CARGO_MANIFEST_PATH".into(),
            "/tmp/proj-a/crates/foo/Cargo.toml".into(),
        ),
        ("CARGO_PKG_NAME".into(), "foo".into()),
        ("CARGO_PKG_VERSION".into(), "1.2.3".into()),
    ]);
    let ctx_b = make_rustc_context_with_env(vec![
        (
            "CARGO_MANIFEST_PATH".into(),
            "/tmp/proj-b/crates/foo/Cargo.toml".into(),
        ),
        ("CARGO_PKG_NAME".into(), "foo".into()),
        ("CARGO_PKG_VERSION".into(), "1.2.3".into()),
    ]);
    assert_eq!(
        ctx_a.context_key(),
        ctx_b.context_key(),
        "CARGO_MANIFEST_PATH is volatile (absolute path) and must NOT \
         contribute to the cache key"
    );
}

/// T2b (issue #396) — Two worktrees that pick different `CARGO_TARGET_DIR`
/// leaf names produce the same rustc cache key. This is the worktree-sharing
/// regression: agent workflows that spawn per-worktree target dirs would
/// otherwise cold-miss every compile even though source + flags match.
#[test]
fn rustc_context_key_ignores_cargo_target_dir() {
    let ctx_a = make_rustc_context_with_env(vec![
        (
            "CARGO_TARGET_DIR".into(),
            "/repo/.claude/worktrees/parent-cache-main-target".into(),
        ),
        ("CARGO_PKG_NAME".into(), "foo".into()),
        ("CARGO_PKG_VERSION".into(), "1.2.3".into()),
    ]);
    let ctx_b = make_rustc_context_with_env(vec![
        (
            "CARGO_TARGET_DIR".into(),
            "/repo/.claude/worktrees/parent-cache-sub-target".into(),
        ),
        ("CARGO_PKG_NAME".into(), "foo".into()),
        ("CARGO_PKG_VERSION".into(), "1.2.3".into()),
    ]);
    assert_eq!(
        ctx_a.context_key(),
        ctx_b.context_key(),
        "CARGO_TARGET_DIR is output-placement state and must NOT contribute \
         to the cache key; otherwise two worktrees with different target-dir \
         leaf names share no rustc cache (issue #396)"
    );
}

/// T2c (issue #396) — `from_parsed_args` is the second filter point. A
/// `RustcCompileContext` built through the public `from_parsed_args` entry
/// also must not carry `CARGO_TARGET_DIR` into `env_vars` (defense-in-depth
/// with the hash-time filter).
#[test]
fn rustc_from_parsed_args_drops_cargo_target_dir() {
    let args = RustcParsedArgs {
        source_file: NormalizedPath::from("/src/lib.rs"),
        crate_name: Some("mylib".to_string()),
        crate_types: vec!["lib".to_string()],
        edition: Some("2021".to_string()),
        emit_types: vec!["link".to_string()],
        cfgs: Vec::new(),
        check_cfgs: Vec::new(),
        codegen_flags: Vec::new(),
        target: None,
        cap_lints: None,
        externs: Vec::new(),
        lint_flags: Vec::new(),
        unknown_flags: Vec::new(),
        out_dir: None,
        extra_filename: None,
        cargo_metadata: None,
        incremental_dir: None,
        error_format: None,
        json_format: None,
        color: None,
        diagnostic_width: None,
        search_paths: Vec::new(),
        remap_path_prefixes: Vec::new(),
        sysroot: None,
        output_file: None,
    };
    let client_env = vec![
        ("CARGO_TARGET_DIR".to_string(), "/repo/target-a".to_string()),
        ("CARGO_PKG_NAME".to_string(), "foo".to_string()),
        ("CARGO_PKG_VERSION".to_string(), "1.2.3".to_string()),
    ];
    let ctx = RustcCompileContext::from_parsed_args(&args, &client_env, None);
    assert!(
        !ctx.env_vars.iter().any(|(k, _)| k == "CARGO_TARGET_DIR"),
        "from_parsed_args must drop CARGO_TARGET_DIR from env_vars; got {:?}",
        ctx.env_vars
    );
    assert!(
        ctx.env_vars.iter().any(|(k, _)| k == "CARGO_PKG_NAME"),
        "from_parsed_args must keep CARGO_PKG_NAME; got {:?}",
        ctx.env_vars
    );
}

/// T3 — Negative control: `CARGO_PKG_VERSION` MUST still affect the key
/// because `env!("CARGO_PKG_VERSION")` is embedded in compiled output.
/// This guards against an over-eager filter that strips too much.
#[test]
fn rustc_context_key_sensitive_to_cargo_pkg_version() {
    let ctx_a = make_rustc_context_with_env(vec![("CARGO_PKG_VERSION".into(), "1.2.3".into())]);
    let ctx_b = make_rustc_context_with_env(vec![("CARGO_PKG_VERSION".into(), "1.2.4".into())]);
    assert_ne!(
        ctx_a.context_key(),
        ctx_b.context_key(),
        "CARGO_PKG_VERSION feeds env!() macros and MUST be in the cache key"
    );
}

/// T4 — Extern rmeta paths that share a filename (and therefore the same
/// `metadata=` hash from cargo) but differ in their absolute directory
/// prefix must produce equal cache keys. This is the cascade-killer: when
/// a dep crate is rebuilt at the same content but in a different target
/// dir, all downstream crates should still hit.
#[test]
fn rustc_context_key_ignores_extern_directory_prefix() {
    let mut ctx_a = make_rustc_context("/src/lib.rs", "2021");
    ctx_a.extern_crates = vec![(
        "serde".into(),
        "/tmp/proj-a/target/debug/deps/libserde-abc123.rmeta".into(),
    )];
    let mut ctx_b = make_rustc_context("/src/lib.rs", "2021");
    ctx_b.extern_crates = vec![(
        "serde".into(),
        "/tmp/proj-b/target/debug/deps/libserde-abc123.rmeta".into(),
    )];
    assert_eq!(
        ctx_a.context_key(),
        ctx_b.context_key(),
        "extern rmeta paths with the same filename (= same cargo metadata \
         hash) but different absolute prefixes must produce equal cache \
         keys; otherwise relocating the workspace cascades through every \
         downstream crate"
    );
}

#[test]
fn rustc_artifact_key_stable() {
    let ctx = make_rustc_context("/src/lib.rs", "2021");
    let ck = ctx.context_key();

    let src_hash = crate::hash::hash_bytes(b"source");
    let ext_hash = crate::hash::hash_bytes(b"extern");

    let ak1 = compute_rustc_artifact_key(
        &ck,
        &mut [(NormalizedPath::from("/src/lib.rs"), src_hash)],
        &mut [("serde".to_string(), ext_hash)],
    );
    let ak2 = compute_rustc_artifact_key(
        &ck,
        &mut [(NormalizedPath::from("/src/lib.rs"), src_hash)],
        &mut [("serde".to_string(), ext_hash)],
    );
    assert_eq!(ak1, ak2);
}

#[test]
fn rustc_artifact_key_with_root_matches_equivalent_source_and_dependency_paths() {
    let ctx_a = make_rustc_context("/workspace-a/crates/demo/src/lib.rs", "2021");
    let ctx_b = make_rustc_context("/workspace-b/crates/demo/src/lib.rs", "2021");
    let root_a = Path::new("/workspace-a");
    let root_b = Path::new("/workspace-b");

    let ck_a = ctx_a.context_key_with_root(Some(root_a));
    let ck_b = ctx_b.context_key_with_root(Some(root_b));
    assert_eq!(ck_a, ck_b);

    let src_hash = crate::hash::hash_bytes(b"source");
    let dep_hash = crate::hash::hash_bytes(b"dependency");
    let ext_hash = crate::hash::hash_bytes(b"extern");

    let ak_a = compute_rustc_artifact_key_with_root(
        &ck_a,
        &mut [
            (
                NormalizedPath::from("/workspace-a/crates/demo/src/lib.rs"),
                src_hash,
            ),
            (
                NormalizedPath::from("/workspace-a/crates/demo/src/generated.rs"),
                dep_hash,
            ),
        ],
        &mut [("serde".to_string(), ext_hash)],
        Some(root_a),
    );
    let ak_b = compute_rustc_artifact_key_with_root(
        &ck_b,
        &mut [
            (
                NormalizedPath::from("/workspace-b/crates/demo/src/generated.rs"),
                dep_hash,
            ),
            (
                NormalizedPath::from("/workspace-b/crates/demo/src/lib.rs"),
                src_hash,
            ),
        ],
        &mut [("serde".to_string(), ext_hash)],
        Some(root_b),
    );

    assert_eq!(
        ak_a, ak_b,
        "source and dependency files under equivalent roots should hash relative to those roots"
    );
}
