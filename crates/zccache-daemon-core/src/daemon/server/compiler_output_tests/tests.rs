use super::*;

fn parsed_link_for_target(
    cwd: &Path,
    target: &str,
    crate_type: &str,
    packed: bool,
) -> crate::depgraph::RustcParsedArgs {
    let mut args = vec![
        "--crate-name=app".to_string(),
        format!("--crate-type={crate_type}"),
        "--emit=link".to_string(),
        "--out-dir=/repo/target/deps".to_string(),
        format!("--target={target}"),
        "src/main.rs".to_string(),
    ];
    if packed {
        args.push("-Csplit-debuginfo=packed".to_string());
        args.push("-Cdebuginfo=2".to_string());
    }
    crate::depgraph::parse_rustc_args(&args, cwd)
}

#[test]
fn linux_link_declares_packed_dwarf_sidecar() {
    let cwd = Path::new("/repo");
    let parsed = parsed_link_for_target(cwd, "x86_64-unknown-linux-gnu", "bin", true);
    let primary = Path::new("/repo/target/deps/app");

    let declared = rustc_expected_output_paths(&parsed, primary, cwd, None);

    assert!(
        declared
            .iter()
            .any(|path| path == &NormalizedPath::new("/repo/target/deps/app.dwp")),
        "Linux linked output must declare its packed DWARF sidecar: {declared:?}"
    );
}

#[test]
fn legacy_collection_includes_existing_packed_dwarf_sidecar() {
    let temp = tempfile::tempdir().unwrap();
    let primary = temp.path().join("app");
    let sidecar = temp.path().join("app.dwp");
    std::fs::write(&primary, b"image").unwrap();
    std::fs::write(&sidecar, b"packed-dwarf").unwrap();
    let parsed = parsed_link_for_target(temp.path(), "x86_64-unknown-linux-gnu", "bin", true);

    let collected = collect_rustc_output_files(&parsed, &primary, temp.path());

    assert!(
        collected.iter().any(|output| output.path == sidecar),
        "existing packed DWARF sidecar must be collected with the image"
    );
}

#[test]
fn packed_dwarf_declaration_is_target_and_link_kind_aware() {
    let cwd = Path::new("/repo");
    let primary = Path::new("/repo/target/deps/app");
    for parsed in [
        parsed_link_for_target(cwd, "x86_64-unknown-linux-gnu", "bin", false),
        parsed_link_for_target(cwd, "x86_64-unknown-linux-gnu", "rlib", true),
        parsed_link_for_target(cwd, "x86_64-pc-windows-msvc", "bin", true),
    ] {
        assert!(linux_packed_dwarf_sidecar_output_path(&parsed, primary).is_none());
    }

    let dylib = parsed_link_for_target(cwd, "x86_64-unknown-linux-gnu", "cdylib", true);
    assert_eq!(
        linux_packed_dwarf_sidecar_output_path(&dylib, Path::new("/repo/target/deps/libplugin.so")),
        Some(NormalizedPath::new("/repo/target/deps/libplugin.so.dwp"))
    );
}

#[cfg(test)]
mod dylint_sidecar_tests {
    use super::*;

    #[test]
    fn perf_dylint_cdylib_models_toolchain_sidecar_as_complete_output_set() {
        if crate::platform::host::is_windows() {
            return;
        }
        let cwd = Path::new("/repo");
        let out_dir = "/repo/target/dylint/libraries/nightly/release/deps";
        let args = vec![
            "--crate-name=lint".to_string(),
            "--crate-type=cdylib".to_string(),
            "--emit=link".to_string(),
            format!("--out-dir={out_dir}"),
            "-Clinker=/tools/dylint-link".to_string(),
            "src/lib.rs".to_string(),
        ];
        let parsed = crate::depgraph::parse_rustc_args(&args, cwd);
        let extension = if crate::platform::host::is_macos() {
            "dylib"
        } else {
            "so"
        };
        let primary = Path::new(out_dir).join(format!("liblint.{extension}"));
        let env = vec![
            ("CARGO_PKG_NAME".to_string(), "lint".to_string()),
            (
                "RUSTUP_TOOLCHAIN".to_string(),
                "nightly-2026-01-18-x86_64-unknown-linux-gnu".to_string(),
            ),
        ];

        let outputs = rustc_expected_output_paths(&parsed, &primary, cwd, Some(&env));
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0], NormalizedPath::new(&primary));
        assert!(outputs[1].ends_with(format!(
            "release/liblint@nightly-2026-01-18-x86_64-unknown-linux-gnu.{extension}"
        )));

        let without_identity = rustc_expected_output_paths(&parsed, &primary, cwd, None);
        assert_eq!(without_identity, vec![NormalizedPath::new(primary)]);
    }

    /// zackees/soldr#3044: widen the Dylint cdylib path gate to
    /// `dylint/tests` (dylint's own test-crate builds), not only
    /// `dylint/libraries`. This is the `dylint/tests` sibling of
    /// `perf_dylint_cdylib_models_toolchain_sidecar_as_complete_output_set`
    /// above -- every other input stays identical, only the out-dir tree
    /// changes. The libraries-tree assertions above are unchanged in
    /// substance, per soldr#3039's acceptance table.
    #[test]
    fn perf_dylint_tests_tree_cdylib_models_toolchain_sidecar() {
        if crate::platform::host::is_windows() {
            return;
        }
        let cwd = Path::new("/repo");
        let out_dir =
            "/repo/target/dylint/tests/ban_raw_process_creation/target/nightly/release/deps";
        let args = vec![
            "--crate-name=lint".to_string(),
            "--crate-type=cdylib".to_string(),
            "--emit=link".to_string(),
            format!("--out-dir={out_dir}"),
            "-Clinker=/tools/dylint-link".to_string(),
            "src/lib.rs".to_string(),
        ];
        let parsed = crate::depgraph::parse_rustc_args(&args, cwd);
        let extension = if crate::platform::host::is_macos() {
            "dylib"
        } else {
            "so"
        };
        let primary = Path::new(out_dir).join(format!("liblint.{extension}"));
        let env = vec![
            ("CARGO_PKG_NAME".to_string(), "lint".to_string()),
            (
                "RUSTUP_TOOLCHAIN".to_string(),
                "nightly-2026-01-18-x86_64-unknown-linux-gnu".to_string(),
            ),
        ];

        let outputs = rustc_expected_output_paths(&parsed, &primary, cwd, Some(&env));
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0], NormalizedPath::new(&primary));
        assert!(outputs[1].ends_with(format!(
            "release/liblint@nightly-2026-01-18-x86_64-unknown-linux-gnu.{extension}"
        )));

        let without_identity = rustc_expected_output_paths(&parsed, &primary, cwd, None);
        assert_eq!(without_identity, vec![NormalizedPath::new(primary)]);
    }

    /// zackees/soldr#3044: `is_dylint_cdylib_args` must also recognise the
    /// `dylint/tests` tree so the linker-hash key material
    /// (`add_dylint_linker_key_material`) is applied there too, not only
    /// under `dylint/libraries`.
    #[test]
    fn dylint_linker_key_material_is_applied_in_the_tests_tree() {
        if crate::platform::host::is_windows() {
            return;
        }
        let cwd = Path::new("/repo");
        let out_dir =
            "/repo/target/dylint/tests/ban_raw_process_creation/target/nightly/release/deps";
        let args = vec![
            "--crate-name=lint".to_string(),
            "--crate-type=cdylib".to_string(),
            "--emit=link".to_string(),
            format!("--out-dir={out_dir}"),
            "-Clinker=/tools/dylint-link".to_string(),
            "src/lib.rs".to_string(),
        ];
        let parsed = crate::depgraph::parse_rustc_args(&args, cwd);
        assert!(is_dylint_cdylib_args(&parsed));

        // Over-widening guard: a non-dylint-link linker under the same
        // tests-tree out-dir must not be treated as a dylint cdylib build.
        let non_dylint_linker_args = vec![
            "--crate-name=lint".to_string(),
            "--crate-type=cdylib".to_string(),
            "--emit=link".to_string(),
            format!("--out-dir={out_dir}"),
            "-Clinker=/tools/cc".to_string(),
            "src/lib.rs".to_string(),
        ];
        let non_dylint_linker_parsed =
            crate::depgraph::parse_rustc_args(&non_dylint_linker_args, cwd);
        assert!(!is_dylint_cdylib_args(&non_dylint_linker_parsed));

        // Over-widening guard: a non-empty extra-filename must not be
        // treated as a dylint cdylib build either -- this is the linker-hash
        // key material's isolation being proven still applies outside
        // `dylint/libraries`.
        let extra_filename_args = vec![
            "--crate-name=lint".to_string(),
            "--crate-type=cdylib".to_string(),
            "--emit=link".to_string(),
            format!("--out-dir={out_dir}"),
            "-Clinker=/tools/dylint-link".to_string(),
            "-Cextra-filename=-9a1b2c3d".to_string(),
            "src/lib.rs".to_string(),
        ];
        let extra_filename_parsed = crate::depgraph::parse_rustc_args(&extra_filename_args, cwd);
        assert!(!is_dylint_cdylib_args(&extra_filename_parsed));
    }

    /// zackees/soldr#3044 acceptance item 4. `out_dir` is deliberately
    /// excluded from the rustc context key (see the "non-cache-key state
    /// (out_dir excluded; ...)" comment in
    /// `crates/zccache-depgraph/src/context/mod.rs`), so this test does NOT
    /// assert that the libraries-tree and tests-tree keys differ by
    /// out-dir -- that assertion would be false and would encode a wrong
    /// model. It asserts the isolation that actually exists: per
    /// `dylint-link`/nightly identity, per driver-nightly compiler identity,
    /// and with-vs-without the dylint linker key material applied.
    #[test]
    fn dylint_cdylib_keys_stay_isolated_per_driver_and_linker() {
        if crate::platform::host::is_windows() {
            return;
        }
        let cwd = Path::new("/repo");
        let out_dir =
            "/repo/target/dylint/tests/ban_raw_process_creation/target/nightly/release/deps";
        let parse = || {
            let args = vec![
                "--crate-name=lint".to_string(),
                "--crate-type=cdylib".to_string(),
                "--emit=link".to_string(),
                format!("--out-dir={out_dir}"),
                "-Clinker=/tools/dylint-link".to_string(),
                "src/lib.rs".to_string(),
            ];
            crate::depgraph::parse_rustc_args(&args, cwd)
        };

        let key_with_linker_hash = |linker_hash: ContentHash, compiler_hash: ContentHash| {
            let mut parsed = parse();
            add_dylint_linker_key_material(&mut parsed, linker_hash);
            crate::depgraph::RustcCompileContext::from_parsed_args(&parsed, &[], compiler_hash)
                .context_key()
        };

        // (a) different `dylint-link`/nightly identities isolate the key.
        assert_ne!(
            key_with_linker_hash(
                ContentHash::from_bytes([1; 32]),
                ContentHash::from_bytes([9; 32])
            ),
            key_with_linker_hash(
                ContentHash::from_bytes([2; 32]),
                ContentHash::from_bytes([9; 32])
            ),
        );

        // (b) different driver-nightly (compiler) identities isolate the key.
        assert_ne!(
            key_with_linker_hash(
                ContentHash::from_bytes([1; 32]),
                ContentHash::from_bytes([9; 32])
            ),
            key_with_linker_hash(
                ContentHash::from_bytes([1; 32]),
                ContentHash::from_bytes([10; 32])
            ),
        );

        // (c) applying the dylint linker key material changes the key
        // relative to the same fixture without it -- this is why t5's
        // `is_dylint_cdylib_args` widening to `dylint/tests` matters for
        // correctness, not only for cache-hit rate.
        let without_material = crate::depgraph::RustcCompileContext::from_parsed_args(
            &parse(),
            &[],
            ContentHash::from_bytes([9; 32]),
        )
        .context_key();
        let with_material = key_with_linker_hash(
            ContentHash::from_bytes([1; 32]),
            ContentHash::from_bytes([9; 32]),
        );
        assert_ne!(without_material, with_material);
    }
}

#[test]
fn packed_dwarf_declaration_requires_link_and_enabled_debug_info() {
    let cwd = Path::new("/repo");
    let primary = Path::new("/repo/target/deps/app");
    for extra in [
        [
            "--emit=metadata",
            "-Csplit-debuginfo=packed",
            "-Cdebuginfo=2",
        ],
        ["--emit=link", "-Csplit-debuginfo=packed", "-Cdebuginfo=0"],
    ] {
        let mut args = vec![
            "--crate-name=app".to_string(),
            "--crate-type=bin".to_string(),
            "--target=x86_64-unknown-linux-gnu".to_string(),
            "--out-dir=/repo/target/deps".to_string(),
            "src/main.rs".to_string(),
        ];
        args.extend(extra.into_iter().map(str::to_string));
        let parsed = crate::depgraph::parse_rustc_args(&args, cwd);
        assert!(linux_packed_dwarf_sidecar_output_path(&parsed, primary).is_none());
    }
}

#[test]
fn packed_dwarf_declaration_uses_effective_last_codegen_values() {
    let cwd = Path::new("/repo");
    let primary = Path::new("/repo/target/deps/app");
    let parse = |split_values: [&str; 2]| {
        let mut args = vec![
            "--crate-name=app".to_string(),
            "--crate-type=bin".to_string(),
            "--emit=link".to_string(),
            "--target=x86_64-unknown-linux-gnu".to_string(),
            "-Cdebuginfo=2".to_string(),
            "src/main.rs".to_string(),
        ];
        args.extend(
            split_values
                .into_iter()
                .map(|value| format!("-Csplit-debuginfo={value}")),
        );
        crate::depgraph::parse_rustc_args(&args, cwd)
    };

    assert!(linux_packed_dwarf_sidecar_output_path(&parse(["packed", "off"]), primary).is_none());
    assert!(linux_packed_dwarf_sidecar_output_path(&parse(["off", "packed"]), primary).is_some());
}

#[test]
fn packed_dwarf_declaration_honors_debug_shorthand_precedence() {
    let cwd = Path::new("/repo");
    let primary = Path::new("/repo/target/deps/app");
    let parse = |debug_args: &[&str]| {
        let mut args = vec![
            "--crate-name=app".to_string(),
            "--crate-type=bin".to_string(),
            "--emit=link".to_string(),
            "--target=x86_64-unknown-linux-gnu".to_string(),
            "-Csplit-debuginfo=packed".to_string(),
            "src/main.rs".to_string(),
        ];
        args.extend(debug_args.iter().map(|arg| (*arg).to_string()));
        crate::depgraph::parse_rustc_args(&args, cwd)
    };

    assert!(linux_packed_dwarf_sidecar_output_path(&parse(&["-g"]), primary).is_some());
    assert!(
        linux_packed_dwarf_sidecar_output_path(&parse(&["-g", "-Cdebuginfo=0"]), primary).is_none()
    );
    assert!(
        linux_packed_dwarf_sidecar_output_path(&parse(&["-Cdebuginfo=0", "-g"]), primary).is_some()
    );
}

#[test]
fn dylint_key_material_preserves_repeated_codegen_precedence() {
    let cwd = Path::new("/repo");
    let parse = |values: [&str; 2]| {
        let args = vec![
            format!("-Copt-level={}", values[0]),
            format!("-Copt-level={}", values[1]),
            "-Clink-arg=z-last-lexically".to_string(),
            "-Clink-arg=a-first-lexically".to_string(),
            "src/lib.rs".to_string(),
        ];
        crate::depgraph::parse_rustc_args(&args, cwd)
    };
    let key = |values| {
        let mut parsed = parse(values);
        add_dylint_linker_key_material(&mut parsed, ContentHash::from_bytes([7; 32]));
        crate::depgraph::RustcCompileContext::from_parsed_args(
            &parsed,
            &[],
            ContentHash::from_bytes([9; 32]),
        )
        .context_key()
    };

    assert_ne!(key(["2", "3"]), key(["3", "2"]));
}

/// soldr#2148. Deliberately NOT `cfg(not(target_os = "windows"))` like the
/// dylint sidecar tests above: `msvc_pdb_sidecar_output_path` is pure path
/// manipulation, and Windows is precisely where its absence was the bug.
#[cfg(test)]
mod pdb_sidecar_tests {
    use super::*;

    #[test]
    fn msvc_pdb_is_declared_for_linked_images_only() {
        // soldr#2148: a cached build produced the .exe without its .pdb, so
        // crash dumps resolved to `module+0xNNNN`. The pdb was never in the
        // output model, so it was never staged, stored or replayed.
        for image in ["app.exe", "plugin.dll", "APP.EXE"] {
            let pdb = msvc_pdb_sidecar_output_path(Path::new(image))
                .unwrap_or_else(|| panic!("{image} should declare a pdb"));
            assert_eq!(
                pdb.extension().and_then(|e| e.to_str()),
                Some("pdb"),
                "{image} -> {pdb:?}"
            );
        }

        // Artifacts that never have one. Declaring a pdb for these would be
        // harmless (missing outputs are filtered at collection) but it would
        // also be a lie about what the compile produces.
        for other in ["libfoo.rlib", "libfoo.rmeta", "libfoo.a", "foo.d", "noext"] {
            assert!(
                msvc_pdb_sidecar_output_path(Path::new(other)).is_none(),
                "{other} must not declare a pdb"
            );
        }
    }

    /// soldr#2347: the pdb declaration is target-aware. A windows-gnu
    /// image is linked by mingw (DWARF in the image, no pdb ever); the
    /// staged plan hard-fails materialization on a declared output that
    /// never appears, which killed every Linux-hosted
    /// `--target x86_64-pc-windows-gnu` linked-image compile.
    #[test]
    fn pdb_declaration_is_msvc_target_only() {
        let cwd = Path::new("/repo");
        let base = |target: Option<&str>| {
            let mut args = vec![
                "--crate-name=wg".to_string(),
                "--crate-type=bin".to_string(),
                "--emit=link".to_string(),
                "--out-dir=/repo/target/deps".to_string(),
                "src/main.rs".to_string(),
            ];
            if let Some(target) = target {
                args.push(format!("--target={target}"));
            }
            crate::depgraph::parse_rustc_args(&args, cwd)
        };

        let msvc = base(Some("x86_64-pc-windows-msvc"));
        assert!(msvc_target_writes_pdb(&msvc));
        let primary = Path::new("/repo/target/deps/wg.exe");
        let declared = rustc_expected_output_paths(&msvc, primary, cwd, None);
        assert!(
            declared
                .iter()
                .any(|p| p.extension() == Some("pdb".as_ref())),
            "msvc target must declare the pdb sidecar: {declared:?}"
        );

        let gnu = base(Some("x86_64-pc-windows-gnu"));
        assert!(!msvc_target_writes_pdb(&gnu));
        let declared = rustc_expected_output_paths(&gnu, primary, cwd, None);
        assert!(
            !declared.iter().any(|p| p.extension() == Some("pdb".as_ref())),
            "windows-gnu never writes a pdb; declaring one hard-fails the              staged materialization (soldr#2347): {declared:?}"
        );

        let aarch_gnu = base(Some("aarch64-pc-windows-gnullvm"));
        assert!(!msvc_target_writes_pdb(&aarch_gnu));
    }
}
