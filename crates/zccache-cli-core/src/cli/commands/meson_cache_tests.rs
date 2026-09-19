use super::*;

#[cfg(unix)]
fn version_probe_fixture(body: &str) -> (tempfile::TempDir, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("probe fixture directory");
    let path = dir.path().join("meson fixture");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("fixture script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
        .expect("fixture permissions");
    (dir, path)
}

#[cfg(unix)]
#[test]
fn canonical_version_probe_passes_literal_argument_and_trims_output() {
    let (_dir, path) = version_probe_fixture(
        "[ \"$#\" = 1 ] && [ \"$1\" = --version ] || exit 9; printf ' 1.2.3\\n'",
    );
    assert_eq!(
        capture_meson_version(&path).expect("probe succeeds"),
        "1.2.3"
    );
}

#[cfg(unix)]
#[test]
fn canonical_version_probe_rejects_failed_tool() {
    let (_dir, path) = version_probe_fixture("printf misleading-version; exit 7");
    let error = capture_meson_version(&path).expect_err("failed probe is not an identity");
    assert_eq!(error.kind(), std::io::ErrorKind::Other);
    assert!(error.to_string().contains("exit=7"));
}

#[cfg(unix)]
#[test]
fn canonical_version_probe_rejects_oversized_stdout_or_stderr() {
    // Both channels share the cap. In particular a short plausible
    // version on stdout must not hide excessive diagnostics on stderr.
    for script in [
        "dd if=/dev/zero bs=4096 count=257 2>/dev/null",
        "printf '1.2.3\\n'; (dd if=/dev/zero bs=4096 count=257 2>/dev/null) >&2",
    ] {
        let (_dir, path) = version_probe_fixture(script);
        let error = capture_meson_version(&path).expect_err("aggregate output cap enforced");
        assert_eq!(error.kind(), std::io::ErrorKind::FileTooLarge);
    }
}

#[test]
fn canonical_version_probe_preserves_missing_executable_error() {
    let dir = tempfile::tempdir().expect("fixture directory");
    let error = capture_meson_version(&dir.path().join("missing-meson"))
        .expect_err("missing executable must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn allowlist_keeps_meson_configure_outputs() {
    assert!(is_configure_output("build.ninja"));
    assert!(is_configure_output("compile_commands.json"));
    assert!(is_configure_output("meson-info/intro-targets.json"));
    assert!(is_configure_output("meson-private/coredata.dat"));
    assert!(is_configure_output("meson-logs/meson-log.txt"));
}

#[test]
fn allowlist_rejects_build_outputs_and_pch_sidecars() {
    // Issue #710 reproducer paths from the FastLED build dir:
    assert!(!is_configure_output("tests/test_pch.h.pch"));
    assert!(!is_configure_output("tests/test_pch.h.pch.input_hash"));
    assert!(!is_configure_output("tests/test_pch.h.d.cache"));
    // Generic ninja outputs:
    assert!(!is_configure_output("tests/foo.obj"));
    assert!(!is_configure_output("tests/foo.o"));
    assert!(!is_configure_output("libfastled.a"));
    assert!(!is_configure_output("libfastled.dll"));
    assert!(!is_configure_output("libfastled.so"));
    assert!(!is_configure_output("test_pch.exe"));
    assert!(!is_configure_output("test_pch.pdb"));
    // Subdirs that ninja owns:
    assert!(!is_configure_output("examples/Blink/Blink.dll"));
    assert!(!is_configure_output("subprojects/lib/whatever.obj"));
    // Ninja state — not configure output:
    assert!(!is_configure_output(".ninja_log"));
    assert!(!is_configure_output(".ninja_deps"));
}

#[test]
fn dir_allowlist_lets_walker_skip_target_subdirs() {
    assert!(is_configure_output_dir("meson-info"));
    assert!(is_configure_output_dir("meson-private"));
    assert!(is_configure_output_dir("meson-logs"));
    assert!(is_configure_output_dir("meson-info/sub"));
    assert!(!is_configure_output_dir("tests"));
    assert!(!is_configure_output_dir("examples"));
    assert!(!is_configure_output_dir("subprojects"));
    assert!(!is_configure_output_dir("CMakeFiles"));
}

/// Build a length-prefixed restore payload from a slice of
/// `(rel_path, content)` pairs in the same wire format that
/// [`archive_dir`] emits and [`restore_from_cache`] consumes.
fn write_payload(payload_path: &Path, entries: &[(&str, &[u8])]) {
    use std::io::Write;
    let f = std::fs::File::create(payload_path).unwrap();
    let mut writer = std::io::BufWriter::new(f);
    for (rel, content) in entries {
        let rel_bytes = rel.as_bytes();
        writer
            .write_all(&(rel_bytes.len() as u32).to_le_bytes())
            .unwrap();
        writer.write_all(rel_bytes).unwrap();
        writer
            .write_all(&(content.len() as u64).to_le_bytes())
            .unwrap();
        writer.write_all(content).unwrap();
    }
    writer.flush().unwrap();
}

/// Issue #749 — RED before, GREEN after the targeted-wipe fix.
///
/// A user-owned file the caller placed in the build dir before
/// calling `zccache meson configure` (e.g. FastLED's
/// `meson_native.txt`) MUST survive a successful cache restore.
/// The v3 capture allowlist (`is_configure_output`) does not include
/// that file, so the pre-fix blanket `remove_dir_all` deleted it
/// outright; FastLED/FastLED#3048 is the user-visible symptom.
#[test]
fn restore_preserves_user_owned_file_outside_allowlist() {
    let tmp = tempfile::tempdir().unwrap();
    let build_abs = tmp.path().join("build");
    std::fs::create_dir_all(&build_abs).unwrap();

    // Caller writes its meson_native.txt into the build dir *before*
    // invoking the cached configure. This file is NOT in the v3
    // allowlist (`is_configure_output` returns false for it).
    let native_file = build_abs.join("meson_native.txt");
    let native_content = b"[binaries]\nc = 'clang'\n";
    std::fs::write(&native_file, native_content).unwrap();
    assert!(!is_configure_output("meson_native.txt"));

    // Payload contains only allowlisted configure outputs.
    let payload_path = tmp.path().join("payload");
    let stdout_path = tmp.path().join("stdout.bin");
    let stderr_path = tmp.path().join("stderr.bin");
    write_payload(
        &payload_path,
        &[
            ("build.ninja", b"# regenerated by restore"),
            ("meson-private/coredata.dat", b"\0\0coredata"),
        ],
    );
    std::fs::write(&stdout_path, b"").unwrap();
    std::fs::write(&stderr_path, b"").unwrap();

    restore_from_cache(&payload_path, &stdout_path, &stderr_path, &build_abs)
        .expect("restore should succeed");

    // The cached files landed.
    assert_eq!(
        std::fs::read(build_abs.join("build.ninja")).unwrap(),
        b"# regenerated by restore"
    );
    assert_eq!(
        std::fs::read(build_abs.join("meson-private/coredata.dat")).unwrap(),
        b"\0\0coredata"
    );

    // The user-owned file is intact. Pre-fix this fails because the
    // blanket `remove_dir_all(build_abs)` wiped it before extracting.
    assert!(
        native_file.exists(),
        "meson_native.txt was deleted by the restore — FastLED/FastLED#3048"
    );
    assert_eq!(std::fs::read(&native_file).unwrap(), native_content);
}

/// Issue #749 — RED before, GREEN after the extract-then-swap fix.
///
/// A restore whose payload is corrupt MUST leave the destination in
/// its pre-restore state, not a half-restored mix. Pre-fix the
/// blanket wipe ran first, so a payload that fails mid-extract left
/// the dest empty even though the operation was rolled back at the
/// caller's "fall back to fresh setup" level.
#[test]
fn restore_failure_leaves_destination_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let build_abs = tmp.path().join("build");
    std::fs::create_dir_all(&build_abs).unwrap();
    let native_file = build_abs.join("meson_native.txt");
    let native_content = b"[binaries]\nc = 'clang'\n";
    std::fs::write(&native_file, native_content).unwrap();

    // Truncated payload: length header announces a path of 16 bytes
    // but only 4 follow — the extract loop will hit
    // `UnexpectedEof` deep inside the read and return Err.
    let payload_path = tmp.path().join("payload");
    let stdout_path = tmp.path().join("stdout.bin");
    let stderr_path = tmp.path().join("stderr.bin");
    let mut bad = Vec::new();
    bad.extend_from_slice(&16u32.to_le_bytes()); // claims 16-byte path
    bad.extend_from_slice(b"abcd"); // ...but only 4 bytes
    std::fs::write(&payload_path, &bad).unwrap();
    std::fs::write(&stdout_path, b"").unwrap();
    std::fs::write(&stderr_path, b"").unwrap();

    let result = restore_from_cache(&payload_path, &stdout_path, &stderr_path, &build_abs);
    assert!(
        result.is_err(),
        "truncated payload should surface as Err so the caller can fall back"
    );

    // Pre-fix this fails because the wipe ran before the extract
    // error, so the destination is empty.
    assert!(
        native_file.exists(),
        "meson_native.txt must survive a failed restore — FastLED/FastLED#3048 root cause"
    );
    assert_eq!(std::fs::read(&native_file).unwrap(), native_content);
}

#[test]
fn no_op_reconfigure_detected_in_stdout() {
    let sample =
        b"The Meson build system\nVersion: 1.6.0\nDirectory already configured.\n\nJust run your build command (e.g. ninja) and Meson will regenerate as necessary.\n";
    assert!(stdout_contains_already_configured(sample));
}

#[test]
fn no_op_detection_does_not_false_positive_on_normal_configure() {
    let normal = b"The Meson build system\nVersion: 1.6.0\nSource dir: /tmp/src\nBuild dir: /tmp/build\nBuild type: native build\n";
    assert!(!stdout_contains_already_configured(normal));
}

#[test]
fn archive_dir_only_captures_allowlisted_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Configure outputs that SHOULD be captured:
    std::fs::write(root.join("build.ninja"), b"# ninja").unwrap();
    std::fs::write(root.join("compile_commands.json"), b"[]").unwrap();
    std::fs::create_dir_all(root.join("meson-info")).unwrap();
    std::fs::write(root.join("meson-info/intro-targets.json"), b"[]").unwrap();
    std::fs::create_dir_all(root.join("meson-private")).unwrap();
    std::fs::write(root.join("meson-private/coredata.dat"), b"\0\0").unwrap();
    std::fs::create_dir_all(root.join("meson-logs")).unwrap();
    std::fs::write(root.join("meson-logs/meson-log.txt"), b"ok").unwrap();

    // Poison the dir with the actual #710 reproducer files — these
    // MUST be skipped:
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(root.join("tests/test_pch.h.pch"), b"STALEPCH").unwrap();
    std::fs::write(root.join("tests/test_pch.h.pch.input_hash"), b"deadbeef").unwrap();
    std::fs::write(root.join("tests/test_pch.h.d.cache"), b"depcache").unwrap();
    std::fs::write(root.join("tests/foo.obj"), b"OBJECTBYTES").unwrap();
    std::fs::create_dir_all(root.join("examples/Blink")).unwrap();
    std::fs::write(root.join("examples/Blink/Blink.dll"), b"DLLBYTES").unwrap();
    std::fs::write(root.join(".ninja_log"), b"log").unwrap();

    let tar_path = tmp.path().join("out.tar");
    {
        let f = std::fs::File::create(&tar_path).unwrap();
        let mut writer = std::io::BufWriter::new(f);
        archive_dir(root, root, &mut writer).unwrap();
        std::io::Write::flush(&mut writer).unwrap();
    }

    // Read back the archive and collect captured rel paths.
    let mut captured: Vec<String> = Vec::new();
    let f = std::fs::File::open(&tar_path).unwrap();
    let mut reader = std::io::BufReader::new(f);
    loop {
        use std::io::Read;
        let mut len_buf = [0u8; 4];
        match reader.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => panic!("read failed: {e}"),
        }
        let path_len = u32::from_le_bytes(len_buf) as usize;
        let mut path_bytes = vec![0u8; path_len];
        reader.read_exact(&mut path_bytes).unwrap();
        let rel = String::from_utf8(path_bytes).unwrap();
        let mut content_len_buf = [0u8; 8];
        reader.read_exact(&mut content_len_buf).unwrap();
        let content_len = u64::from_le_bytes(content_len_buf) as usize;
        let mut content = vec![0u8; content_len];
        reader.read_exact(&mut content).unwrap();
        captured.push(rel);
    }
    captured.sort();

    // Everything captured must be configure output.
    for rel in &captured {
        assert!(
            is_configure_output(rel),
            "archive captured a non-configure path: {rel}"
        );
    }
    // None of the #710 poison files made it in.
    for poison in &[
        "tests/test_pch.h.pch",
        "tests/test_pch.h.pch.input_hash",
        "tests/test_pch.h.d.cache",
        "tests/foo.obj",
        "examples/Blink/Blink.dll",
        ".ninja_log",
    ] {
        assert!(
            !captured.iter().any(|r| r == poison),
            "archive captured poison file: {poison}"
        );
    }
    // And the real configure outputs made it in.
    for needed in &[
        "build.ninja",
        "compile_commands.json",
        "meson-info/intro-targets.json",
        "meson-private/coredata.dat",
        "meson-logs/meson-log.txt",
    ] {
        assert!(
            captured.iter().any(|r| r == needed),
            "archive missed required configure output: {needed}"
        );
    }
}
