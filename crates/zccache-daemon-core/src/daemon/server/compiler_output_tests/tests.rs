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
