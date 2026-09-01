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

    /// soldr#2349: this used to bail out with `if is_windows() { return; }`
    /// because `dylint_library_sidecar_output_path` unconditionally
    /// returned `None` on a Windows host, making `outputs.len()` 1 instead
    /// of 2. The gate is gone, so this now runs — and must pass — on every
    /// host in the CI matrix, asserting the host-appropriate sidecar name.
    #[test]
    fn perf_dylint_cdylib_models_toolchain_sidecar_as_complete_output_set() {
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
        // The synthetic primary's own name is not load-bearing here — the
        // sidecar function only walks its *parent* directory to find
        // `sidecar_dir`, then names the sidecar from `crate_name` — so a
        // fixed non-Windows-style stand-in name is fine even when this test
        // happens to run on a real Windows host.
        let primary = Path::new(out_dir).join("liblint.so");
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
        let expected_sidecar_suffix = if crate::platform::host::is_windows() {
            "release/lint@nightly-2026-01-18-x86_64-unknown-linux-gnu.dll".to_string()
        } else if crate::platform::host::is_macos() {
            "release/liblint@nightly-2026-01-18-x86_64-unknown-linux-gnu.dylib".to_string()
        } else {
            "release/liblint@nightly-2026-01-18-x86_64-unknown-linux-gnu.so".to_string()
        };
        assert!(
            outputs[1].ends_with(&expected_sidecar_suffix),
            "expected sidecar ending in {expected_sidecar_suffix}, got {:?}",
            outputs[1]
        );

        let without_identity = rustc_expected_output_paths(&parsed, &primary, cwd, None);
        assert_eq!(without_identity, vec![NormalizedPath::new(primary)]);
    }

    /// Pure-function coverage for the sidecar filename split, exercised
    /// directly with an explicit `HostFamily` (soldr#2349) so the Windows
    /// naming — no `lib` prefix, `.dll` extension — is proven from Linux
    /// CI rather than only on a real Windows runner. Production callers
    /// resolve the real host via `HostFamily::current()`.
    #[test]
    fn dylint_sidecar_name_matches_host_convention() {
        let cwd = Path::new("/repo");
        let out_dir = "/repo/target/dylint/libraries/nightly/release/deps";
        let args = vec![
            "--crate-name=lint".to_string(),
            "--crate-type=cdylib".to_string(),
            "--emit=link".to_string(),
            format!("--out-dir={out_dir}"),
            "-Clinker=/tools/dylint-link.exe".to_string(),
            "src/lib.rs".to_string(),
        ];
        let parsed = crate::depgraph::parse_rustc_args(&args, cwd);
        let primary = Path::new(out_dir).join("lint.dll");
        let env = vec![
            ("CARGO_PKG_NAME".to_string(), "lint".to_string()),
            (
                "RUSTUP_TOOLCHAIN".to_string(),
                "nightly-2026-01-18-x86_64-pc-windows-msvc".to_string(),
            ),
        ];

        let windows_sidecar = dylint_library_sidecar_output_path(
            &parsed,
            &primary,
            cwd,
            Some(&env),
            HostFamily::Windows,
        )
        .expect("windows sidecar should resolve when identity is complete");
        assert!(
            windows_sidecar.ends_with("release/lint@nightly-2026-01-18-x86_64-pc-windows-msvc.dll")
        );
        assert!(
            !windows_sidecar.to_string_lossy().contains("liblint"),
            "Windows sidecar must not carry the unix `lib` prefix: {windows_sidecar:?}"
        );

        let macos_sidecar = dylint_library_sidecar_output_path(
            &parsed,
            &primary,
            cwd,
            Some(&env),
            HostFamily::MacOs,
        )
        .expect("macos sidecar should resolve when identity is complete");
        assert!(macos_sidecar
            .ends_with("release/liblint@nightly-2026-01-18-x86_64-pc-windows-msvc.dylib"));

        let other_sidecar = dylint_library_sidecar_output_path(
            &parsed,
            &primary,
            cwd,
            Some(&env),
            HostFamily::Other,
        )
        .expect("linux sidecar should resolve when identity is complete");
        assert!(
            other_sidecar.ends_with("release/liblint@nightly-2026-01-18-x86_64-pc-windows-msvc.so")
        );
    }

    /// A missing toolchain-qualified sidecar identity must still fail
    /// closed to non-cacheable on Windows, exactly as it already does on
    /// Linux/macOS (soldr#2349 must not weaken the existing fail-closed
    /// contract while extending it).
    #[test]
    fn dylint_cdylib_windows_missing_identity_is_incomplete() {
        let cwd = Path::new("/repo");
        let out_dir = "/repo/target/dylint/libraries/nightly/release/deps";
        let args = vec![
            "--crate-name=lint".to_string(),
            "--crate-type=cdylib".to_string(),
            "--emit=link".to_string(),
            format!("--out-dir={out_dir}"),
            "-Clinker=/tools/dylint-link.exe".to_string(),
            "src/lib.rs".to_string(),
        ];
        let parsed = crate::depgraph::parse_rustc_args(&args, cwd);
        let primary = Path::new(out_dir).join("lint.dll");

        // No client_env at all -- CARGO_PKG_NAME/RUSTUP_TOOLCHAIN unknown.
        assert!(dylint_library_sidecar_output_path(
            &parsed,
            &primary,
            cwd,
            None,
            HostFamily::Windows
        )
        .is_none());
        assert!(!dylint_cdylib_has_complete_output_identity(
            &parsed, &primary, cwd, None
        ));

        // CARGO_PKG_NAME present but RUSTUP_TOOLCHAIN missing.
        let partial_env = vec![("CARGO_PKG_NAME".to_string(), "lint".to_string())];
        assert!(dylint_library_sidecar_output_path(
            &parsed,
            &primary,
            cwd,
            Some(&partial_env),
            HostFamily::Windows
        )
        .is_none());
        assert!(!dylint_cdylib_has_complete_output_identity(
            &parsed,
            &primary,
            cwd,
            Some(&partial_env)
        ));
    }

    /// A Windows cdylib that is not a Dylint lint library (no
    /// `dylint-link` linker) must still be refused by the daemon-side
    /// mirror gate, matching the compiler-crate side.
    #[test]
    fn is_dylint_cdylib_args_refuses_non_dylint_windows_cdylib() {
        let cwd = Path::new("/repo");
        let out_dir = "/repo/target/dylint/libraries/nightly/release/deps";
        let args = vec![
            "--crate-name=extmod".to_string(),
            "--crate-type=cdylib".to_string(),
            "--emit=link".to_string(),
            format!("--out-dir={out_dir}"),
            "-Clinker=/tools/link.exe".to_string(),
            "src/lib.rs".to_string(),
        ];
        let parsed = crate::depgraph::parse_rustc_args(&args, cwd);
        assert!(!is_dylint_cdylib_args(&parsed));
    }

    /// A Windows Dylint cdylib invocation is recognized by the daemon-side
    /// mirror gate — the counterpart to
    /// `zccache-compiler`'s `rustc_dylint_library_cdylib_is_cacheable`.
    /// `dylint-link.exe`'s `.exe` suffix must not break the basename match.
    #[test]
    fn is_dylint_cdylib_args_accepts_windows_dylint_cdylib() {
        let cwd = Path::new("/repo");
        let out_dir = "/repo/target/dylint/libraries/nightly/release/deps";
        let args = vec![
            "--crate-name=lint".to_string(),
            "--crate-type=cdylib".to_string(),
            "--emit=link".to_string(),
            format!("--out-dir={out_dir}"),
            "-Clinker=/tools/dylint-link.exe".to_string(),
            "src/lib.rs".to_string(),
        ];
        let parsed = crate::depgraph::parse_rustc_args(&args, cwd);
        assert!(is_dylint_cdylib_args(&parsed));
    }
}

/// soldr#2349: the MSVC import-library pair (`<dll>.lib` + `.exp`) beside a
/// linked Windows DLL. Uses an explicit `--target` throughout (rather than
/// a host parameter) because `msvc_target_writes_pdb` already resolves the
/// MSVC-ness from the target triple when one is given — the same testable
/// seam the existing `pdb_sidecar_tests` module below relies on.
#[cfg(test)]
mod msvc_implib_sidecar_tests {
    use super::*;

    fn parsed_cdylib_for_target(cwd: &Path, target: &str) -> crate::depgraph::RustcParsedArgs {
        let args = vec![
            "--crate-name=lint".to_string(),
            "--crate-type=cdylib".to_string(),
            "--emit=link".to_string(),
            "--out-dir=/repo/target/dylint/libraries/nightly/release/deps".to_string(),
            format!("--target={target}"),
            "-Clinker=/tools/dylint-link".to_string(),
            "src/lib.rs".to_string(),
        ];
        crate::depgraph::parse_rustc_args(&args, cwd)
    }

    /// The naming function itself still resolves the pair correctly for an
    /// MSVC-target DLL -- this is what `collect_rustc_output_files` (the
    /// opportunistic, tolerant path) uses to look for the files on disk.
    #[test]
    fn msvc_target_resolves_implib_and_exp_names_beside_dll() {
        let cwd = Path::new("/repo");
        let parsed = parsed_cdylib_for_target(cwd, "x86_64-pc-windows-msvc");
        let primary = Path::new("/repo/target/dylint/libraries/nightly/release/deps/lint.dll");

        let [implib, exp] = msvc_dll_implib_sidecar_output_paths(&parsed, primary)
            .expect("MSVC-target DLL must resolve an import-lib pair name");
        assert_eq!(
            implib,
            NormalizedPath::new("/repo/target/dylint/libraries/nightly/release/deps/lint.dll.lib")
        );
        assert_eq!(
            exp,
            NormalizedPath::new("/repo/target/dylint/libraries/nightly/release/deps/lint.dll.exp")
        );
    }

    /// soldr#2349 (post-review revision): the import-lib pair must NOT be
    /// part of the staged plan's *required* output declaration. A staged
    /// output that is declared but never materializes hard-fails the whole
    /// compile via `StagedCompilePlan::materialize` (soldr#2347's failure
    /// class), and the `<dll>.lib` naming was never confirmed against a
    /// live Windows build. Requiring it would turn an unverified filename
    /// guess into a build-breaking risk on the exact platform this change
    /// targets, for a file nothing reads back. See the doc comment on
    /// `msvc_dll_implib_sidecar_output_paths`.
    #[test]
    fn staged_declaration_never_requires_the_implib_pair() {
        let cwd = Path::new("/repo");
        let parsed = parsed_cdylib_for_target(cwd, "x86_64-pc-windows-msvc");
        let primary = Path::new("/repo/target/dylint/libraries/nightly/release/deps/lint.dll");

        let declared = rustc_expected_output_paths(&parsed, primary, cwd, None);
        assert!(
            !declared
                .iter()
                .any(|p| p.extension() == Some("lib".as_ref())),
            "staged declaration must not require the import lib: {declared:?}"
        );
        assert!(
            !declared
                .iter()
                .any(|p| p.extension() == Some("exp".as_ref())),
            "staged declaration must not require the .exp file: {declared:?}"
        );
    }

    /// soldr#2349: `msvc_dll_implib_sidecar_output_paths` (and therefore the
    /// opportunistic `collect_rustc_output_files` path that calls it) is
    /// deliberately scoped OFF proc-macro, even though a Windows proc-macro
    /// is also a `/DLL`-mode MSVC link and would otherwise match the
    /// extension/target gate. Proc-macro Windows caching is a shipped,
    /// working lane; there's no reason to touch its collected output set
    /// on an unverified filename guess for a file nothing reads back. (The
    /// staged declaration is unconditionally excluded for every crate type
    /// now — see `staged_declaration_never_requires_the_implib_pair` — so
    /// the crate-type scope matters only for this tolerant collection
    /// path.) See the doc comment on `msvc_dll_implib_sidecar_output_paths`.
    #[test]
    fn proc_macro_never_resolves_implib_even_on_msvc_target() {
        let cwd = Path::new("/repo");
        let args = vec![
            "--crate-name=serde_derive".to_string(),
            "--crate-type=proc-macro".to_string(),
            "--emit=link".to_string(),
            "--out-dir=/repo/target/deps".to_string(),
            "--target=x86_64-pc-windows-msvc".to_string(),
            "src/lib.rs".to_string(),
        ];
        let parsed = crate::depgraph::parse_rustc_args(&args, cwd);
        let primary = Path::new("/repo/target/deps/serde_derive.dll");
        assert!(msvc_dll_implib_sidecar_output_paths(&parsed, primary).is_none());
    }

    #[test]
    fn windows_gnu_target_never_declares_implib() {
        // mingw's `--out-implib` naming differs entirely (`.dll.a`, via
        // `-Wl,--out-implib=`) and is not modeled by this MSVC-specific
        // helper — see the risk list in the task report. Declaring the
        // MSVC name here would hard-fail staged materialization exactly
        // like the pdb case (soldr#2347).
        let cwd = Path::new("/repo");
        let parsed = parsed_cdylib_for_target(cwd, "x86_64-pc-windows-gnu");
        let primary = Path::new("/repo/target/dylint/libraries/nightly/release/deps/lint.dll");
        assert!(msvc_dll_implib_sidecar_output_paths(&parsed, primary).is_none());
    }

    #[test]
    fn non_dll_primary_never_declares_implib() {
        let cwd = Path::new("/repo");
        let parsed = parsed_cdylib_for_target(cwd, "x86_64-pc-windows-msvc");
        let exe_primary = Path::new("/repo/target/dylint/libraries/nightly/release/deps/app.exe");
        assert!(msvc_dll_implib_sidecar_output_paths(&parsed, exe_primary).is_none());
    }

    #[test]
    fn legacy_collection_includes_existing_implib_pair() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("lint.dll");
        let implib = temp.path().join("lint.dll.lib");
        let exp = temp.path().join("lint.dll.exp");
        std::fs::write(&primary, b"image").unwrap();
        std::fs::write(&implib, b"implib").unwrap();
        std::fs::write(&exp, b"exp").unwrap();
        let parsed = parsed_cdylib_for_target(temp.path(), "x86_64-pc-windows-msvc");

        let collected = collect_rustc_output_files(&parsed, &primary, temp.path());

        assert!(collected.iter().any(|output| output.path == implib));
        assert!(collected.iter().any(|output| output.path == exp));
    }

    /// The counterpart to `legacy_collection_includes_existing_implib_pair`:
    /// when the import lib / `.exp` are absent (a wrong filename guess, a
    /// windows-gnu host, or a DLL with debug info that genuinely has no
    /// import lib), collection is a silent no-op rather than a failure --
    /// this is the entire point of keeping the feature opportunistic
    /// instead of a staged-required output. Only the primary DLL is
    /// collected.
    #[test]
    fn legacy_collection_is_a_noop_when_implib_pair_absent() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("lint.dll");
        std::fs::write(&primary, b"image").unwrap();
        let parsed = parsed_cdylib_for_target(temp.path(), "x86_64-pc-windows-msvc");

        let collected = collect_rustc_output_files(&parsed, &primary, temp.path());

        // `RustcOutputFile` has no `Debug`, so name the paths explicitly rather
        // than formatting the collection.
        let names: Vec<_> = collected.iter().map(|file| file.path.clone()).collect();
        assert_eq!(
            collected.len(),
            1,
            "no import-lib/.exp on disk must not synthesize entries: {names:?}"
        );
        assert_eq!(collected[0].path, primary);
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
