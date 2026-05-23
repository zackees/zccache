//! Synthetic C++ project generation, warmup, and single/multi compile helpers
//! across bare clang, sccache, and zccache (including a `with_env` variant
//! used by the sibling-remap benchmarks).

use std::path::Path;
use std::time::{Duration, Instant};
use zccache_protocol::{Request, Response};

use super::common::{clean_objects, ClientConn, NUM_FILES};
use super::response_file::generate_response_files;

pub fn source_names() -> Vec<String> {
    (0..NUM_FILES).map(|i| format!("unit_{i:03}.cpp")).collect()
}

pub fn absolute_cpp_source_names(dir: &Path) -> Vec<String> {
    (0..NUM_FILES)
        .map(|i| {
            dir.join(format!("unit_{i:03}.cpp"))
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// Generate NUM_FILES lightweight C++ source files with a shared header.
pub fn generate_project(dir: &Path) {
    generate_cpp_project(dir, false);
}

pub fn generate_project_with_file_tags(dir: &Path) {
    generate_cpp_project(dir, true);
}

pub fn generate_cpp_project(dir: &Path, with_file_tags: bool) {
    let incdir = dir.join("include");
    std::fs::create_dir_all(&incdir).unwrap();

    std::fs::write(
        incdir.join("common.h"),
        r#"#pragma once
#include <cstdint>
namespace bench {
  template<typename T>
  inline T clamp(T v, T lo, T hi) { return v < lo ? lo : v > hi ? hi : v; }
}
"#,
    )
    .unwrap();

    for i in 0..NUM_FILES {
        let file_tag = if with_file_tags {
            format!(
                r#"  static const char *file_tag_{i:03}(void) {{ return __FILE__; }}
"#
            )
        } else {
            String::new()
        };
        let content = format!(
            r#"#include "common.h"
#include <cmath>
namespace unit_{i:03} {{
{file_tag}
  double compute(int n) {{ return std::sin(n * 0.{i:03}1); }}
  double accumulate(int n) {{
    double s = 0;
    for (int j = 0; j < n; ++j) s += compute(j);
    return s;
  }}
}}
"#,
        );
        std::fs::write(dir.join(format!("unit_{i:03}.cpp")), content).unwrap();
    }
}

/// Run clang on one file to warm the OS page cache (compiler binary + system headers).
/// This normalizes page cache state before each cold measurement so all tools
/// start from the same baseline.
pub fn warmup_compiler(compiler: &str, dir: &Path) {
    let src = dir.join("unit_000.cpp");
    let obj = dir.join("_warmup.o");
    let output = std::process::Command::new(compiler)
        .args(["-c", "-Iinclude", "-O2", "-std=c++17"])
        .arg(&src)
        .arg("-o")
        .arg(&obj)
        .current_dir(dir)
        .output()
        .expect("warmup compile failed to spawn");
    assert!(
        output.status.success(),
        "C++ warmup compile failed: status={:?}\ncompiler={compiler}\ndir={dir:?}\nsrc exists={}\ninclude exists={}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        src.exists(),
        dir.join("include").is_dir(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let _ = std::fs::remove_file(&obj);
}

/// Delete all files in dir and regenerate the project from scratch.
pub fn nuke_and_regenerate(dir: &Path) {
    // Remove everything inside the directory
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path).unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
        }
    }
    generate_project(dir);
}

/// Delete all files in dir and regenerate project + response files.
pub fn nuke_and_regenerate_with_rsp(dir: &Path) {
    nuke_and_regenerate(dir);
    generate_response_files(dir);
}

// ── zccache benchmarks (in-process daemon, no subprocess overhead) ──────

pub async fn zccache_compile_single(
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
                    "-Iinclude".into(),
                    "-O2".into(),
                    "-std=c++17".into(),
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
                assert_eq!(exit_code, 0, "compile failed for {src}");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }
    }
    start.elapsed()
}

pub async fn zccache_compile_multi(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    cwd: &str,
    sources: &[String],
) -> Duration {
    clean_objects(Path::new(cwd));
    let mut args: Vec<String> = vec!["-c".into()];
    args.extend(sources.iter().cloned());
    args.extend(["-Iinclude".into(), "-O2".into(), "-std=c++17".into()]);

    let start = Instant::now();
    client
        .send(&Request::Compile {
            session_id: session_id.to_string(),
            args,
            cwd: cwd.into(),
            compiler: compiler.to_string().into(),
            env: None,
            stdin: Vec::new(),
        })
        .await
        .unwrap();
    match client.recv().await.unwrap() {
        Some(Response::CompileResult { exit_code, .. }) => {
            assert_eq!(exit_code, 0, "multi-file compile failed");
        }
        other => panic!("expected CompileResult, got: {other:?}"),
    }
    start.elapsed()
}

// ── sccache benchmark (subprocess) ──────────────────────────────────────

pub fn sccache_compile_single(
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
                "-Iinclude",
                "-O2",
                "-std=c++17",
            ])
            .current_dir(cwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("failed to run sccache");
        assert!(status.success(), "sccache compile failed for {src}");
    }
    start.elapsed()
}

pub fn sccache_compile_multi(
    sccache: &Path,
    compiler: &str,
    cwd: &Path,
    sources: &[String],
) -> Duration {
    clean_objects(cwd);
    let mut cmd = std::process::Command::new(sccache);
    cmd.arg(compiler).arg("-c");
    for src in sources {
        cmd.arg(src);
    }
    cmd.args(["-Iinclude", "-O2", "-std=c++17"]);
    cmd.current_dir(cwd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let start = Instant::now();
    let status = cmd.status().expect("failed to run sccache");
    assert!(status.success(), "sccache multi-file compile failed");
    start.elapsed()
}

// ── Baseline (direct compiler, no cache) ────────────────────────────────

pub fn baseline_single(compiler: &str, cwd: &Path, sources: &[String]) -> Duration {
    clean_objects(cwd);
    let start = Instant::now();
    for src in sources {
        let status = std::process::Command::new(compiler)
            .args([
                "-c",
                src,
                "-o",
                &src.replace(".cpp", ".o"),
                "-Iinclude",
                "-O2",
                "-std=c++17",
            ])
            .current_dir(cwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("failed to run compiler");
        assert!(status.success(), "compile failed for {src}");
    }
    start.elapsed()
}

pub fn baseline_multi(compiler: &str, cwd: &Path, sources: &[String]) -> Duration {
    clean_objects(cwd);
    let mut cmd = std::process::Command::new(compiler);
    cmd.arg("-c");
    for src in sources {
        cmd.arg(src);
    }
    cmd.args(["-Iinclude", "-O2", "-std=c++17"]);
    cmd.current_dir(cwd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let start = Instant::now();
    let status = cmd.status().expect("failed to run compiler");
    assert!(status.success(), "multi-file compile failed");
    start.elapsed()
}

pub async fn zccache_compile_cpp_single_with_env(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    cwd: &str,
    sources: &[String],
    env: Vec<(String, String)>,
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
                    "-Iinclude".into(),
                    "-O2".into(),
                    "-std=c++17".into(),
                ],
                cwd: cwd.into(),
                compiler: compiler.to_string().into(),
                env: Some(env.clone()),
                stdin: Vec::new(),
            })
            .await
            .unwrap();
        match client.recv().await.unwrap() {
            Some(Response::CompileResult { exit_code, .. }) => {
                assert_eq!(exit_code, 0, "compile failed for {src}");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }
    }
    start.elapsed()
}
