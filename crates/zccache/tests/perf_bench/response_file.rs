//! Response-file generation and `_rsp` variants of the single/multi compile
//! helpers (bare clang, sccache, zccache).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::panic_in_result_fn, clippy::unwrap_in_result)]

use std::path::Path;
use std::time::{Duration, Instant};
use zccache::protocol::{Request, Response};

use super::common::{clean_objects, ClientConn, NUM_FILES, RSP_NUM_DEFINES, RSP_NUM_INCLUDES};

// ── Response-file generation ─────────────────────────────────────────────

/// Generate a large response file hierarchy that exercises the expansion path.
///
/// Layout:
///   flags.rsp          — top-level: @warnings.rsp, @defines.rsp, -Iinclude, -O2, -std=c++17
///   warnings.rsp       — ~30 warning flags
///   defines.rsp        — RSP_NUM_DEFINES -D flags + RSP_NUM_INCLUDES -I flags
///   sources_multi.rsp  — @flags.rsp + all source file names (for multi-file rsp mode)
///
/// The total expanded arg count is ~300+ flags per compilation, which is realistic
/// for large build systems (CMake, Bazel) that pass everything via response files.
pub fn generate_response_files(dir: &Path) {
    // warnings.rsp — realistic warning flags
    let warnings = [
        "-Wall",
        "-Wextra",
        "-Wpedantic",
        "-Wconversion",
        "-Wshadow",
        "-Wold-style-cast",
        "-Wcast-align",
        "-Wunused",
        "-Woverloaded-virtual",
        "-Wnon-virtual-dtor",
        "-Wformat=2",
        "-Wmisleading-indentation",
        "-Wduplicated-cond",
        "-Wduplicated-branches",
        "-Wlogical-op",
        "-Wnull-dereference",
        "-Wuseless-cast",
        "-Wdouble-promotion",
        "-Wno-unused-parameter",
        "-Wno-missing-field-initializers",
        "-Werror=return-type",
        "-Werror=implicit-fallthrough",
        "-Wno-sign-conversion",
        "-Wno-shorten-64-to-32",
        "-Wno-c++98-compat",
        "-Wno-c++98-compat-pedantic",
        "-Wno-global-constructors",
        "-Wno-exit-time-destructors",
        "-Wno-padded",
        "-Wno-weak-vtables",
    ];
    std::fs::write(dir.join("warnings.rsp"), warnings.join("\n")).unwrap();

    // defines.rsp — many -D and -I flags to make it large
    let mut defines_content = String::with_capacity(16 * 1024);
    for i in 0..RSP_NUM_DEFINES {
        defines_content.push_str(&format!("-DBENCH_DEFINE_{i:04}={i}\n"));
    }
    for i in 0..RSP_NUM_INCLUDES {
        // Synthetic include paths (won't be used by compiler, but exercises arg parsing)
        defines_content.push_str(&format!("-Isynthetic/include/path_{i:03}\n"));
    }
    std::fs::write(dir.join("defines.rsp"), &defines_content).unwrap();

    // flags.rsp — top-level: nests warnings + defines, adds real compile flags
    std::fs::write(
        dir.join("flags.rsp"),
        "@warnings.rsp\n@defines.rsp\n-Iinclude\n-O2\n-std=c++17\n",
    )
    .unwrap();

    // sources_multi.rsp — all sources + flags (for multi-file rsp mode)
    let mut multi_content = String::from("@flags.rsp\n-c\n");
    for i in 0..NUM_FILES {
        multi_content.push_str(&format!("unit_{i:03}.cpp\n"));
    }
    std::fs::write(dir.join("sources_multi.rsp"), &multi_content).unwrap();
}

// ── Response-file benchmarks: baseline ──────────────────────────────────

pub fn baseline_single_rsp(compiler: &str, cwd: &Path, sources: &[String]) -> Duration {
    clean_objects(cwd);
    let start = Instant::now();
    for src in sources {
        let status = std::process::Command::new(compiler)
            .args(["-c", src, "-o", &src.replace(".cpp", ".o"), "@flags.rsp"])
            .current_dir(cwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("failed to run compiler with rsp");
        assert!(status.success(), "rsp compile failed for {src}");
    }
    start.elapsed()
}

pub fn baseline_multi_rsp(compiler: &str, cwd: &Path) -> Duration {
    clean_objects(cwd);
    let start = Instant::now();
    let status = std::process::Command::new(compiler)
        .arg("@sources_multi.rsp")
        .current_dir(cwd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("failed to run compiler with multi rsp");
    assert!(status.success(), "multi-file rsp compile failed");
    start.elapsed()
}

// ── Response-file benchmarks: sccache ───────────────────────────────────

pub fn sccache_compile_single_rsp(
    sccache: &Path,
    compiler: &str,
    cwd: &Path,
    sources: &[String],
) -> Duration {
    clean_objects(cwd);
    let start = Instant::now();
    for src in sources {
        let status = std::process::Command::new(sccache)
            .args([
                compiler,
                "-c",
                src,
                "-o",
                &src.replace(".cpp", ".o"),
                "@flags.rsp",
            ])
            .current_dir(cwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("failed to run sccache with rsp");
        assert!(status.success(), "sccache rsp compile failed for {src}");
    }
    start.elapsed()
}

pub fn sccache_compile_multi_rsp(sccache: &Path, compiler: &str, cwd: &Path) -> Duration {
    clean_objects(cwd);
    let mut cmd = std::process::Command::new(sccache);
    cmd.arg(compiler).arg("@sources_multi.rsp");
    cmd.current_dir(cwd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let start = Instant::now();
    let status = cmd.status().expect("failed to run sccache with multi rsp");
    assert!(status.success(), "sccache multi-file rsp compile failed");
    start.elapsed()
}

// ── Response-file benchmarks: zccache ───────────────────────────────────

pub async fn zccache_compile_single_rsp(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    cwd: &str,
    sources: &[String],
) -> Duration {
    clean_objects(Path::new(cwd));
    let start = Instant::now();
    for src in sources {
        client
            .send(&Request::Compile {
                session_id: session_id.to_string(),
                args: vec![
                    "-c".into(),
                    src.clone(),
                    "-o".into(),
                    src.replace(".cpp", ".o"),
                    "@flags.rsp".into(),
                ],
                cwd: cwd.into(),
                compiler: compiler.to_string().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();
        match client.recv().await.unwrap() {
            Some(Response::CompileResult { exit_code, .. }) => {
                assert_eq!(exit_code, 0, "rsp compile failed for {src}");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }
    }
    start.elapsed()
}

pub async fn zccache_compile_multi_rsp(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    cwd: &str,
) -> Duration {
    clean_objects(Path::new(cwd));
    let start = Instant::now();
    client
        .send(&Request::Compile {
            session_id: session_id.to_string(),
            args: vec!["@sources_multi.rsp".into()],
            cwd: cwd.into(),
            compiler: compiler.to_string().into(),
            env: None,
            stdin: Vec::new(),
        })
        .await
        .unwrap();
    match client.recv().await.unwrap() {
        Some(Response::CompileResult { exit_code, .. }) => {
            assert_eq!(exit_code, 0, "multi-file rsp compile failed");
        }
        other => panic!("expected CompileResult, got: {other:?}"),
    }
    start.elapsed()
}
