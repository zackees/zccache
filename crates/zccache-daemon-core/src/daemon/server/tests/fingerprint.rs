//! Tests for `request_fingerprint`, worktree-root resolution, and the
//! effective-args + link-flag normalization helpers. Most assertions
//! check that two equivalent invocations across different roots hash
//! to the same fingerprint, and that genuinely-different invocations
//! stay distinct.

use super::super::*;

#[test]
fn rustc_dylint_libs_env_dep_uses_synthetic_content_identity() {
    let env = vec![
        (
            crate::compiler::DYLINT_LIBS_ENV.to_string(),
            r#"["/checkout-a/target/libfixture.so"]"#.to_string(),
        ),
        (
            crate::compiler::DYLINT_CACHE_INPUT_HASH_ENV.to_string(),
            "content-identity".to_string(),
        ),
    ];

    assert_eq!(
        rustc_env_dep_value(Some(&env), crate::compiler::DYLINT_LIBS_ENV),
        Some("content-identity")
    );
}

#[test]
fn rustc_env_dep_value_preserves_non_dylint_and_unprepared_values() {
    let env = vec![
        (
            crate::compiler::DYLINT_LIBS_ENV.to_string(),
            r#"["/checkout-a/target/libfixture.so"]"#.to_string(),
        ),
        ("BUILD_STAMP".to_string(), String::new()),
    ];

    assert_eq!(
        rustc_env_dep_value(Some(&env), crate::compiler::DYLINT_LIBS_ENV),
        Some(r#"["/checkout-a/target/libfixture.so"]"#)
    );
    assert_eq!(rustc_env_dep_value(Some(&env), "BUILD_STAMP"), Some(""));
}

#[cfg(windows)]
#[test]
fn request_fingerprint_normalizes_equivalent_windows_paths() {
    let args = vec!["-c".to_string(), "src/main.cpp".to_string()];
    let a = request_fingerprint(
        Path::new(r"C:\LLVM\bin\clang++.exe"),
        &args,
        Path::new(r"C:\Work\Project"),
        None,
        None,
    );
    let b = request_fingerprint(
        Path::new("c:/llvm/bin/clang++.exe"),
        &args,
        Path::new("c:/work/project"),
        None,
        None,
    );
    assert_eq!(a, b);
}

#[test]
fn find_git_root_detects_git_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    let nested = root.join("crates/demo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(&nested).unwrap();

    assert_eq!(find_git_root(&nested), Some(root.into()));
}

#[test]
fn path_remap_auto_enabled_recognizes_auto_case_insensitive() {
    // Issue #353 prerequisite: confirm the auto-detection helper recognizes
    // the env value the cwd-fallback branch keys on.
    let env = vec![("ZCCACHE_PATH_REMAP".to_string(), "auto".to_string())];
    assert!(path_remap_auto_enabled(Some(&env)));
    let env = vec![("ZCCACHE_PATH_REMAP".to_string(), "AUTO".to_string())];
    assert!(path_remap_auto_enabled(Some(&env)));
    let env = vec![("ZCCACHE_PATH_REMAP".to_string(), "off".to_string())];
    assert!(!path_remap_auto_enabled(Some(&env)));
    assert!(!path_remap_auto_enabled(None));
}

#[test]
fn diag_path_remap_state_tags() {
    // Issue #353: the `auto_no_git` tag is the one observers grep for to
    // distinguish "remap fired with cwd fallback" from "remap silently
    // skipped because no env var".
    let auto = vec![("ZCCACHE_PATH_REMAP".to_string(), "auto".to_string())];
    assert_eq!(diag_path_remap_state(Some(&auto), true), "auto");
    assert_eq!(diag_path_remap_state(Some(&auto), false), "auto_no_git");
    assert_eq!(diag_path_remap_state(None, true), "off");
    assert_eq!(diag_path_remap_state(None, false), "off");
}

#[test]
fn resolve_worktree_root_prefers_client_env_override() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("repo/subdir");
    let override_root = tmp.path().join("override-root");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&override_root).unwrap();
    std::fs::create_dir_all(tmp.path().join("repo/.git")).unwrap();
    let env = vec![(
        WORKTREE_ROOT_ENV.to_string(),
        override_root.to_string_lossy().into_owned(),
    )];

    assert_eq!(
        resolve_worktree_root(&cwd, Some(&env)),
        Some(override_root.into())
    );
}

#[test]
fn request_fingerprint_matches_equivalent_roots_for_safe_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let root_a = tmp.path().join("workspace-a");
    let root_b = tmp.path().join("workspace-b");
    let include_a = root_a.join("include");
    let include_b = root_b.join("include");
    let source_a = root_a.join("src/main.cpp");
    let source_b = root_b.join("src/main.cpp");
    let output_a = root_a.join("build/main.o");
    let output_b = root_b.join("build/main.o");

    let args_a = vec![
        "-I".to_string(),
        include_a.to_string_lossy().into_owned(),
        "-c".to_string(),
        source_a.to_string_lossy().into_owned(),
        "-o".to_string(),
        output_a.to_string_lossy().into_owned(),
    ];
    let args_b = vec![
        "-I".to_string(),
        include_b.to_string_lossy().into_owned(),
        "-c".to_string(),
        source_b.to_string_lossy().into_owned(),
        "-o".to_string(),
        output_b.to_string_lossy().into_owned(),
    ];

    let a = request_fingerprint(
        Path::new("/usr/bin/clang++"),
        &args_a,
        &root_a,
        Some(&root_a),
        None,
    );
    let b = request_fingerprint(
        Path::new("/usr/bin/clang++"),
        &args_b,
        &root_b,
        Some(&root_b),
        None,
    );

    assert_eq!(a, b);
}

#[test]
fn request_fingerprint_keeps_external_paths_distinct() {
    let args_a = vec!["-I".to_string(), "/external-a/include".to_string()];
    let args_b = vec!["-I".to_string(), "/external-b/include".to_string()];

    let a = request_fingerprint(
        Path::new("/usr/bin/clang++"),
        &args_a,
        Path::new("/workspace-a"),
        Some(Path::new("/workspace-a")),
        None,
    );
    let b = request_fingerprint(
        Path::new("/usr/bin/clang++"),
        &args_b,
        Path::new("/workspace-b"),
        Some(Path::new("/workspace-b")),
        None,
    );

    assert_ne!(a, b);
}

#[test]
fn request_fingerprint_normalizes_cc_prefix_map_old_side() {
    let tmp = tempfile::tempdir().unwrap();
    let root_a = tmp.path().join("workspace-a");
    let root_b = tmp.path().join("workspace-b");
    let args_a = vec![format!("-ffile-prefix-map={}=.", root_a.display())];
    let args_b = vec![format!("-ffile-prefix-map={}=.", root_b.display())];

    let a = request_fingerprint(
        Path::new("/usr/bin/clang++"),
        &args_a,
        &root_a,
        Some(&root_a),
        None,
    );
    let b = request_fingerprint(
        Path::new("/usr/bin/clang++"),
        &args_b,
        &root_b,
        Some(&root_b),
        None,
    );

    assert_eq!(a, b);
}

#[test]
fn request_fingerprint_normalizes_rust_remap_detached_old_side() {
    let tmp = tempfile::tempdir().unwrap();
    let root_a = tmp.path().join("workspace-a");
    let root_b = tmp.path().join("workspace-b");
    let args_a = vec![
        "--remap-path-prefix".to_string(),
        format!("{}=.", root_a.display()),
    ];
    let args_b = vec![
        "--remap-path-prefix".to_string(),
        format!("{}=.", root_b.display()),
    ];

    let a = request_fingerprint(Path::new("rustc"), &args_a, &root_a, Some(&root_a), None);
    let b = request_fingerprint(Path::new("rustc"), &args_b, &root_b, Some(&root_b), None);

    assert_eq!(a, b);
}

#[test]
fn request_fingerprint_normalizes_rust_remap_equals_old_side() {
    let tmp = tempfile::tempdir().unwrap();
    let root_a = tmp.path().join("workspace-a");
    let root_b = tmp.path().join("workspace-b");
    let args_a = vec![format!("--remap-path-prefix={}=.", root_a.display())];
    let args_b = vec![format!("--remap-path-prefix={}=.", root_b.display())];

    let a = request_fingerprint(Path::new("rustc"), &args_a, &root_a, Some(&root_a), None);
    let b = request_fingerprint(Path::new("rustc"), &args_b, &root_b, Some(&root_b), None);

    assert_eq!(a, b);
}

#[test]
fn request_fingerprint_preserves_rust_remap_new_prefixes() {
    let tmp = tempfile::tempdir().unwrap();
    let root_a = tmp.path().join("workspace-a");
    let root_b = tmp.path().join("workspace-b");
    let args_a = vec![format!("--remap-path-prefix={}=.", root_a.display())];
    let args_b = vec![format!("--remap-path-prefix={}=/src", root_b.display())];

    let a = request_fingerprint(Path::new("rustc"), &args_a, &root_a, Some(&root_a), None);
    let b = request_fingerprint(Path::new("rustc"), &args_b, &root_b, Some(&root_b), None);

    assert_ne!(a, b);
}

#[test]
fn request_fingerprint_keeps_malformed_rust_remap_detached_values_distinct() {
    let tmp = tempfile::tempdir().unwrap();
    let root_a = tmp.path().join("workspace-a");
    let root_b = tmp.path().join("workspace-b");
    let args_a = vec![
        "--remap-path-prefix".to_string(),
        root_a.to_string_lossy().into_owned(),
    ];
    let args_b = vec![
        "--remap-path-prefix".to_string(),
        root_b.to_string_lossy().into_owned(),
    ];

    let a = request_fingerprint(Path::new("rustc"), &args_a, &root_a, Some(&root_a), None);
    let b = request_fingerprint(Path::new("rustc"), &args_b, &root_b, Some(&root_b), None);

    assert_ne!(a, b);
}

#[test]
fn effective_compile_args_reuses_owned_strings_when_no_maps_are_added() {
    let args = vec![
        "-c".to_string(),
        "src/main.cc".to_string(),
        "-DBENCH_DEFINE_0001=1".to_string(),
    ];
    let string_buffers: Vec<*const u8> = args.iter().map(|arg| arg.as_ptr()).collect();

    let effective = effective_compile_args(
        args,
        Path::new("/usr/bin/clang++"),
        Path::new("/workspace"),
        None,
        None,
    );

    assert_eq!(
        effective.iter().map(|arg| arg.as_ptr()).collect::<Vec<_>>(),
        string_buffers,
        "warm response-file preparation must not deep-clone every expanded argument",
    );
}

#[test]
fn effective_compile_args_auto_adds_root_and_cwd_maps() {
    // Issue #474: alongside `-ffile-prefix-map`, the auto-inject now
    // also emits `-fmacro-prefix-map` and `-fdebug-prefix-map` for both
    // the worktree root and the cwd. Older clang (< 10) and some gcc
    // versions don't treat `-ffile-prefix-map` as the umbrella; the
    // explicit triplet ensures `__FILE__` strings and DWARF debug info
    // are both scrubbed on every supported toolchain.
    let tmp = tempfile::tempdir().unwrap();
    let root_path = tmp.path().join("workspace");
    let cwd = root_path.join("build");
    let root = NormalizedPath::new(&root_path);
    let env = vec![(PATH_REMAP_ENV.to_string(), "auto".to_string())];
    let args = vec!["-c".to_string(), "src/main.cc".to_string()];

    let effective = effective_compile_args(
        args,
        Path::new("/usr/bin/clang++"),
        &cwd,
        Some(&root),
        Some(&env),
    );

    assert!(effective.contains(&"-c".to_string()));
    for flag in &[
        "-ffile-prefix-map",
        "-fmacro-prefix-map",
        "-fdebug-prefix-map",
    ] {
        let root_arg = format!("{flag}={}=.", root_path.display());
        let cwd_arg = format!("{flag}={}=.", cwd.display());
        assert!(
            effective.contains(&root_arg),
            "expected {root_arg:?} in {effective:?}"
        );
        assert!(
            effective.contains(&cwd_arg),
            "expected {cwd_arg:?} in {effective:?}"
        );
    }
    // Order invariant: root-mapped flags precede cwd-mapped flags.
    let root_file_pos = effective
        .iter()
        .position(|a| a == &format!("-ffile-prefix-map={}=.", root_path.display()))
        .unwrap();
    let cwd_file_pos = effective
        .iter()
        .position(|a| a == &format!("-ffile-prefix-map={}=.", cwd.display()))
        .unwrap();
    assert!(root_file_pos < cwd_file_pos);
}

#[test]
fn effective_compile_args_auto_cc_maps_are_fallbacks_before_user_maps() {
    let tmp = tempfile::tempdir().unwrap();
    let root_path = tmp.path().join("workspace");
    let subtree = root_path.join("src/generated");
    let root = NormalizedPath::new(&root_path);
    let env = vec![(PATH_REMAP_ENV.to_string(), "auto".to_string())];
    let user_map = format!("-ffile-prefix-map={}=/generated", subtree.display());
    let args = vec![
        user_map.clone(),
        "-c".to_string(),
        "src/main.cc".to_string(),
    ];

    let effective = effective_compile_args(
        args,
        Path::new("/usr/bin/clang++"),
        &root_path,
        Some(&root),
        Some(&env),
    );

    assert_eq!(
        effective[0],
        format!("-ffile-prefix-map={}=.", root_path.display())
    );
    let user_map_pos = effective.iter().position(|arg| arg == &user_map).unwrap();
    assert!(
        user_map_pos > 0,
        "user-supplied narrower map must remain after the auto root fallback"
    );
}

#[test]
fn effective_compile_args_auto_cc_debug_map_does_not_suppress_file_map() {
    let tmp = tempfile::tempdir().unwrap();
    let root_path = tmp.path().join("workspace");
    let root = NormalizedPath::new(&root_path);
    let env = vec![(PATH_REMAP_ENV.to_string(), "auto".to_string())];
    let debug_map = format!("-fdebug-prefix-map={}=/debug", root_path.display());
    let args = vec![
        debug_map.clone(),
        "-c".to_string(),
        "src/main.cc".to_string(),
    ];

    let effective = effective_compile_args(
        args,
        Path::new("/usr/bin/clang++"),
        &root_path,
        Some(&root),
        Some(&env),
    );

    assert_eq!(
        effective[0],
        format!("-ffile-prefix-map={}=.", root_path.display())
    );
    assert!(effective.contains(&debug_map));
}

#[test]
fn effective_compile_args_auto_adds_rust_root_remap_as_fallback() {
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
        args,
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
        ]
    );
}

#[test]
fn effective_compile_args_auto_rust_remap_is_before_user_subtree_remap() {
    let tmp = tempfile::tempdir().unwrap();
    let root_path = tmp.path().join("workspace");
    let subtree = root_path.join("src/generated");
    let root = NormalizedPath::new(&root_path);
    let env = vec![(PATH_REMAP_ENV.to_string(), "auto".to_string())];
    let user_remap = format!("--remap-path-prefix={}=/generated", subtree.display());
    let args = vec![user_remap.clone(), "src/lib.rs".to_string()];

    let effective = effective_compile_args(
        args,
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
        ]
    );
    let user_remap_pos = effective.iter().position(|arg| arg == &user_remap).unwrap();
    assert!(
        user_remap_pos > 1,
        "user-supplied narrower remap must remain after the auto root fallback"
    );
}

#[test]
fn effective_compile_args_auto_keeps_existing_rust_root_remap() {
    let tmp = tempfile::tempdir().unwrap();
    let root_path = tmp.path().join("workspace");
    let root = NormalizedPath::new(&root_path);
    let env = vec![(PATH_REMAP_ENV.to_string(), "auto".to_string())];
    let args = vec![
        format!("--remap-path-prefix={}=/src", root_path.display()),
        "src/lib.rs".to_string(),
    ];

    let effective = effective_compile_args(
        args.clone(),
        Path::new("clippy-driver"),
        &root_path,
        Some(&root),
        Some(&env),
    );

    assert_eq!(effective, args);
}

#[test]
fn link_flag_normalization_keeps_outputs_root_specific() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace-a");
    let lib = root.join("lib");
    let version_map = root.join("link/version.map");
    let more_lib = root.join("more-lib");
    let wasm_map = root.join("link/wasm.map");
    let app_map = root.join("build/app.map");
    let app_lib = root.join("build/app.lib");
    let app_pdb = root.join("build/app.pdb");
    let app_def = root.join("link/app.def");
    let flags = vec![
        "-L".to_string(),
        lib.to_string_lossy().into_owned(),
        "--version-script".to_string(),
        version_map.to_string_lossy().into_owned(),
        format!(
            "-Wl,-L,{},--version-script,{}",
            more_lib.display(),
            wasm_map.display()
        ),
        format!("-Wl,-Map,{}", app_map.display()),
        format!("/IMPLIB:{}", app_lib.display()),
        format!("/PDB:{}", app_pdb.display()),
        format!("/DEF:{}", app_def.display()),
    ];

    let normalized = normalize_link_cache_flags_for_key(&flags, Some(&root));

    assert_eq!(normalized[1], "$ZCCACHE_WORKTREE_ROOT/lib");
    assert_eq!(normalized[3], "$ZCCACHE_WORKTREE_ROOT/link/version.map");
    assert_eq!(
        normalized[4],
        "-Wl,-L,$ZCCACHE_WORKTREE_ROOT/more-lib,--version-script,$ZCCACHE_WORKTREE_ROOT/link/wasm.map"
    );
    assert_eq!(normalized[5], format!("-Wl,-Map,{}", app_map.display()));
    assert_eq!(normalized[6], format!("/IMPLIB:{}", app_lib.display()));
    assert_eq!(normalized[7], format!("/PDB:{}", app_pdb.display()));
    assert_eq!(normalized[8], "/DEF:$ZCCACHE_WORKTREE_ROOT/link/app.def");
}

#[test]
fn request_fingerprint_includes_rust_key_env() {
    let args = vec!["src/lib.rs".to_string()];
    let env_a = vec![("CARGO_PKG_VERSION".to_string(), "1.0.0".to_string())];
    let env_b = vec![("CARGO_PKG_VERSION".to_string(), "1.0.1".to_string())];

    let a = request_fingerprint(
        Path::new("/usr/bin/rustc"),
        &args,
        Path::new("/workspace"),
        Some(Path::new("/workspace")),
        Some(&env_a),
    );
    let b = request_fingerprint(
        Path::new("/usr/bin/rustc"),
        &args,
        Path::new("/workspace"),
        Some(Path::new("/workspace")),
        Some(&env_b),
    );

    assert_ne!(a, b);
}

#[test]
fn request_fingerprint_includes_internal_dylint_cache_salt() {
    let args = vec![
        "/toolchains/nightly/bin/rustc".to_string(),
        "src/lib.rs".to_string(),
    ];
    let env_a = vec![(
        crate::compiler::DYLINT_CACHE_INPUT_HASH_ENV.to_string(),
        "library-v1".to_string(),
    )];
    let env_b = vec![(
        crate::compiler::DYLINT_CACHE_INPUT_HASH_ENV.to_string(),
        "library-v2".to_string(),
    )];

    let a = request_fingerprint(
        Path::new("/tmp/dylint-driver"),
        &args,
        Path::new("/workspace"),
        Some(Path::new("/workspace")),
        Some(&env_a),
    );
    let b = request_fingerprint(
        Path::new("/tmp/dylint-driver"),
        &args,
        Path::new("/workspace"),
        Some(Path::new("/workspace")),
        Some(&env_b),
    );

    assert_ne!(a, b);
}

/// Dependency policy changes the effective C/C++ manifest and must therefore
/// partition the request-level cache as well as the depgraph context key.
#[test]
fn request_fingerprint_separates_dependency_policy_env() {
    let args = vec!["-c".to_string(), "src/main.cpp".to_string()];
    let safe = vec![("ZCCACHE_SCAN_SYSTEM_HEADERS".to_string(), "1".to_string())];
    let fast = vec![("ZCCACHE_FAST".to_string(), "1".to_string())];

    let safe_key = request_fingerprint(
        Path::new("/usr/bin/clang++"),
        &args,
        Path::new("/workspace"),
        Some(Path::new("/workspace")),
        Some(&safe),
    );
    let fast_key = request_fingerprint(
        Path::new("/usr/bin/clang++"),
        &args,
        Path::new("/workspace"),
        Some(Path::new("/workspace")),
        Some(&fast),
    );

    assert_ne!(safe_key, fast_key);
}

#[test]
fn request_fingerprint_preserves_raw_user_depfile_paths() {
    let root_a = Path::new("/repo/worktree-a");
    let root_b = Path::new("/repo/worktree-b");
    let args_a = vec![
        "-c".into(),
        "/repo/worktree-a/src/main.cpp".into(),
        "-o".into(),
        "/repo/worktree-a/main.o".into(),
        "-MD".into(),
        "-MF".into(),
        "/repo/worktree-a/deps.d".into(),
    ];
    let mut args_b = args_a.clone();
    args_b[1] = "/repo/worktree-b/src/main.cpp".into();
    args_b[3] = "/repo/worktree-b/main.o".into();
    args_b[6] = "/repo/worktree-b/deps.d".into();

    let a = request_fingerprint(Path::new("clang++"), &args_a, root_a, Some(root_a), None);
    let b = request_fingerprint(Path::new("clang++"), &args_b, root_b, Some(root_b), None);
    assert_ne!(a, b, "verbatim depfile prerequisites differ across roots");

    args_b[1] = args_a[1].clone();
    args_b[3] = args_a[3].clone();
    args_b[5] = "-MF-".into();
    args_b.remove(6);
    let stdout = request_fingerprint(Path::new("clang++"), &args_b, root_a, Some(root_a), None);
    assert_ne!(a, stdout);
}

/// Issue #396 — `CARGO_TARGET_DIR` is output-placement state and must NOT
/// alter the request fingerprint. Without this filter, two worktrees whose
/// only difference is the target-dir leaf name produce divergent fingerprints
/// and never share the request-level fast path, even with
/// `ZCCACHE_PATH_REMAP=auto`.
#[test]
fn request_fingerprint_ignores_cargo_target_dir() {
    let args = vec!["src/lib.rs".to_string()];
    let env_a = vec![
        ("CARGO_PKG_VERSION".to_string(), "1.0.0".to_string()),
        (
            "CARGO_TARGET_DIR".to_string(),
            "/repo/.claude/worktrees/parent-cache-main-target".to_string(),
        ),
    ];
    let env_b = vec![
        ("CARGO_PKG_VERSION".to_string(), "1.0.0".to_string()),
        (
            "CARGO_TARGET_DIR".to_string(),
            "/repo/.claude/worktrees/parent-cache-sub-target".to_string(),
        ),
    ];

    let a = request_fingerprint(
        Path::new("/usr/bin/rustc"),
        &args,
        Path::new("/workspace"),
        Some(Path::new("/workspace")),
        Some(&env_a),
    );
    let b = request_fingerprint(
        Path::new("/usr/bin/rustc"),
        &args,
        Path::new("/workspace"),
        Some(Path::new("/workspace")),
        Some(&env_b),
    );

    assert_eq!(
        a, b,
        "CARGO_TARGET_DIR is output-placement state and must NOT enter the \
         request fingerprint; worktrees with different target-dir leaf names \
         should share the request-level fast path (issue #396)"
    );
}

#[test]
fn msvc_cl_environment_salts_request_fingerprint_case_insensitively() {
    let args = vec!["/c".to_string(), "main.c".to_string()];
    let common = vec![("PATH".to_string(), "C:\\msvc".to_string())];
    let with_cl = vec![
        ("cl".to_string(), "/DANSWER=42".to_string()),
        ("_CL_".to_string(), "/Z7".to_string()),
    ];
    let equivalent_case = vec![
        ("CL".to_string(), "/DANSWER=42".to_string()),
        ("_cl_".to_string(), "/Z7".to_string()),
    ];

    let base = request_fingerprint(
        Path::new("cl.exe"),
        &args,
        Path::new("."),
        None,
        Some(&common),
    );
    let keyed = request_fingerprint(
        Path::new("cl.exe"),
        &args,
        Path::new("."),
        None,
        Some(&with_cl),
    );
    let same = request_fingerprint(
        Path::new("cl.exe"),
        &args,
        Path::new("."),
        None,
        Some(&equivalent_case),
    );

    assert_ne!(
        base, keyed,
        "CL and _CL_ affect cl.exe output and must salt the request key"
    );
    assert_eq!(
        keyed, same,
        "Windows CL environment names are case-insensitive"
    );
}

#[test]
fn request_fingerprint_keeps_external_out_dirs_distinct() {
    let args_a = vec![
        "--crate-name".to_string(),
        "dep".to_string(),
        "--out-dir".to_string(),
        "/external/agent-a-target".to_string(),
        "src/dep.rs".to_string(),
    ];
    let args_b = vec![
        "--crate-name".to_string(),
        "dep".to_string(),
        "--out-dir".to_string(),
        "/external/agent-b-target".to_string(),
        "src/dep.rs".to_string(),
    ];
    let env_a = vec![
        ("CARGO_PKG_VERSION".to_string(), "1.0.0".to_string()),
        (
            "CARGO_TARGET_DIR".to_string(),
            "/external/agent-a-target".to_string(),
        ),
    ];
    let env_b = vec![
        ("CARGO_PKG_VERSION".to_string(), "1.0.0".to_string()),
        (
            "CARGO_TARGET_DIR".to_string(),
            "/external/agent-b-target".to_string(),
        ),
    ];

    let a = request_fingerprint(
        Path::new("/usr/bin/rustc"),
        &args_a,
        Path::new("/workspace"),
        Some(Path::new("/workspace")),
        Some(&env_a),
    );
    let b = request_fingerprint(
        Path::new("/usr/bin/rustc"),
        &args_b,
        Path::new("/workspace"),
        Some(Path::new("/workspace")),
        Some(&env_b),
    );

    assert_ne!(
        a, b,
        "target dirs outside ZCCACHE_WORKTREE_ROOT remain request-key distinct \
         because their --out-dir paths are not root-relative"
    );
}
