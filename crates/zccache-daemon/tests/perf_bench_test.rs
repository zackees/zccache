//! Performance benchmark: warm-cache compilation latency.
//!
//! Four benchmarks:
//!   - `perf_c_zccache_vs_bare`: C single-file compilation (inline args)
//!   - `perf_warm_cache_zccache_vs_sccache`: C++ single-file vs multi-file (inline args)
//!   - `perf_response_file`: C++ same workload but args passed via large nested response files
//!   - `perf_rustc_zccache_vs_sccache`: Rust single-file compilation (lib crates)
//!
//! Each tool gets its own fresh zccache-prefixed tempdir to avoid OS page cache cross-contamination.
//!
//! Run with: soldr cargo test -p zccache-daemon --test perf_bench_test -- --nocapture --ignored

use std::path::Path;
use std::time::{Duration, Instant};
use zccache_core::NormalizedPath;
use zccache_daemon::DaemonServer;
use zccache_protocol::{Request, Response};

#[cfg(unix)]
type ClientConn = zccache_ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache_ipc::IpcClientConnection;

const NUM_FILES: usize = 50;
const WARM_TRIALS: usize = 5;

struct EnvVarGuard {
    name: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set_path(name: &'static str, value: &Path) -> Self {
        let previous = std::env::var_os(name);
        std::env::set_var(name, value);
        Self { name, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            std::env::set_var(self.name, previous);
        } else {
            std::env::remove_var(self.name);
        }
    }
}

async fn start_daemon() -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let endpoint = zccache_ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (endpoint, handle, shutdown)
}

fn find_sccache() -> Option<NormalizedPath> {
    for path in &["sccache", "C:/tools/python13/Scripts/sccache.exe"] {
        let p = NormalizedPath::new(path);
        if p.exists() {
            return Some(p);
        }
        if let Ok(output) = std::process::Command::new(path).arg("--version").output() {
            if output.status.success() {
                return Some(p);
            }
        }
    }
    None
}

fn find_empp() -> Option<NormalizedPath> {
    if let Some(p) = zccache_test_support::find_on_path("em++") {
        return Some(p);
    }
    let extra: &[&str] = if cfg!(windows) {
        &[
            "C:/emsdk/upstream/emscripten",
            "C:/Program Files/emsdk/upstream/emscripten",
        ]
    } else {
        &[
            "/usr/local/emsdk/upstream/emscripten",
            "/opt/emsdk/upstream/emscripten",
        ]
    };
    let suffix = if cfg!(windows) { ".bat" } else { "" };
    for dir in extra {
        let candidate = NormalizedPath::new(format!("{dir}/em++{suffix}"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Number of synthetic `-D` defines in the large response file.
const RSP_NUM_DEFINES: usize = 200;
/// Number of synthetic `-I` include paths in the large response file.
const RSP_NUM_INCLUDES: usize = 50;

/// Generate NUM_FILES lightweight C++ source files with a shared header.
fn generate_project(dir: &Path) {
    generate_cpp_project(dir, false);
}

fn generate_project_with_file_tags(dir: &Path) {
    generate_cpp_project(dir, true);
}

fn generate_cpp_project(dir: &Path, with_file_tags: bool) {
    let incdir = dir.join("include");
    std::fs::create_dir_all(&incdir).unwrap();

    std::fs::write(
        incdir.join("common.h"),
        r#"#pragma once
#include <vector>
#include <string>
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
  std::vector<double> build(int n) {{
    std::vector<double> v(n);
    for (int j = 0; j < n; ++j) v[j] = compute(j);
    return v;
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
fn warmup_compiler(compiler: &str, dir: &Path) {
    let src = dir.join("unit_000.cpp");
    let obj = dir.join("_warmup.o");
    let status = std::process::Command::new(compiler)
        .args(["-c", "-Iinclude", "-O2", "-std=c++17"])
        .arg(&src)
        .arg("-o")
        .arg(&obj)
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("warmup compile failed");
    assert!(status.success(), "warmup compile failed");
    let _ = std::fs::remove_file(&obj);
}

/// Delete all files in dir and regenerate the project from scratch.
fn nuke_and_regenerate(dir: &Path) {
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
fn nuke_and_regenerate_with_rsp(dir: &Path) {
    nuke_and_regenerate(dir);
    generate_response_files(dir);
}

fn clean_objects(dir: &Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("o") {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn clear_dir_contents(dir: &Path) {
    if !dir.exists() {
        std::fs::create_dir_all(dir).unwrap();
        return;
    }
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path).unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
        }
    }
}

fn source_names() -> Vec<String> {
    (0..NUM_FILES).map(|i| format!("unit_{i:03}.cpp")).collect()
}

fn c_source_names() -> Vec<String> {
    (0..NUM_FILES).map(|i| format!("unit_{i:03}.c")).collect()
}

fn generate_c_project(dir: &Path) {
    let incdir = dir.join("include");
    std::fs::create_dir_all(&incdir).unwrap();

    std::fs::write(
        incdir.join("common_c.h"),
        r#"#pragma once
#include <stdint.h>
#include <math.h>

static inline uint64_t bench_mix(uint64_t x) {
    x ^= x >> 33;
    x *= 0xff51afd7ed558ccdULL;
    x ^= x >> 33;
    x *= 0xc4ceb9fe1a85ec53ULL;
    x ^= x >> 33;
    return x;
}
"#,
    )
    .unwrap();

    for i in 0..NUM_FILES {
        let content = format!(
            r#"#include "common_c.h"

double compute_{i:03}(int n) {{
    double acc = (double)n * 0.{i:03}1;
    for (int j = 0; j < 32; ++j) {{
        acc += sin((double)bench_mix((uint64_t)(n + j + {i})) * 1e-18);
    }}
    return acc;
}}
"#,
        );
        std::fs::write(dir.join(format!("unit_{i:03}.c")), content).unwrap();
    }
}

fn nuke_and_regenerate_c(dir: &Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path).unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
        }
    }
    generate_c_project(dir);
}

fn warmup_c_compiler(compiler: &str, dir: &Path) {
    let src = dir.join("unit_000.c");
    let obj = dir.join("_warmup.o");
    let status = std::process::Command::new(compiler)
        .args(["-c", "-Iinclude", "-O2", "-std=c11"])
        .arg(&src)
        .arg("-o")
        .arg(&obj)
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("C warmup compile failed");
    assert!(status.success(), "C warmup compile failed");
    let _ = std::fs::remove_file(&obj);
}

fn baseline_c_single(compiler: &str, cwd: &Path, sources: &[String]) -> Duration {
    clean_objects(cwd);
    let start = Instant::now();
    for src in sources {
        let status = std::process::Command::new(compiler)
            .args([
                "-c",
                src,
                "-o",
                &src.replace(".c", ".o"),
                "-Iinclude",
                "-O2",
                "-std=c11",
            ])
            .current_dir(cwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("failed to run C compiler");
        assert!(status.success(), "C compile failed for {src}");
    }
    start.elapsed()
}

fn sccache_compile_c_single(
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
                &src.replace(".c", ".o"),
                "-Iinclude",
                "-O2",
                "-std=c11",
            ])
            .current_dir(cwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("failed to run sccache for C");
        assert!(status.success(), "sccache C compile failed for {src}");
    }
    start.elapsed()
}

async fn zccache_compile_c_single(
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
                    src.replace(".c", ".o"),
                    "-Iinclude".into(),
                    "-O2".into(),
                    "-std=c11".into(),
                ],
                cwd: cwd.into(),
                compiler: compiler.to_string().into(),
                env: None,
            })
            .await
            .unwrap();
        match client.recv().await.unwrap() {
            Some(Response::CompileResult { exit_code, .. }) => {
                assert_eq!(exit_code, 0, "C compile failed for {src}");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }
    }
    start.elapsed()
}

// ── zccache benchmarks (in-process daemon, no subprocess overhead) ──────

async fn zccache_compile_single(
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

async fn zccache_compile_multi(
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

fn sccache_compile_single(
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

fn sccache_compile_multi(
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

fn baseline_single(compiler: &str, cwd: &Path, sources: &[String]) -> Duration {
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

fn baseline_multi(compiler: &str, cwd: &Path, sources: &[String]) -> Duration {
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
fn generate_response_files(dir: &Path) {
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

fn baseline_single_rsp(compiler: &str, cwd: &Path, sources: &[String]) -> Duration {
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

fn baseline_multi_rsp(compiler: &str, cwd: &Path) -> Duration {
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

fn sccache_compile_single_rsp(
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

fn sccache_compile_multi_rsp(sccache: &Path, compiler: &str, cwd: &Path) -> Duration {
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

async fn zccache_compile_single_rsp(
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

async fn zccache_compile_multi_rsp(
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

// ── Reporting ───────────────────────────────────────────────────────────

fn median(times: &[Duration]) -> Duration {
    let mut sorted: Vec<Duration> = times.to_vec();
    sorted.sort();
    sorted[sorted.len() / 2]
}

fn fmt_dur(d: Duration) -> String {
    format!("{:.3}s", d.as_secs_f64())
}

fn print_trials(label: &str, times: &[Duration]) {
    print_trials_per(label, times, None);
}

/// Like `print_trials` but also reports per-call latency when `files_per_trial`
/// is known. Useful when a single trial sums N sequential cache lookups so the
/// reader can see whether the per-call cost is in the expected ~1ms range or
/// taking a slow path.
fn print_trials_per(label: &str, times: &[Duration], files_per_trial: Option<usize>) {
    let med = median(times);
    let min = times.iter().min().unwrap();
    let max = times.iter().max().unwrap();
    if let Some(n) = files_per_trial {
        let per_call_ms = (med.as_secs_f64() / n as f64) * 1000.0;
        eprintln!(
            "        {label:<14}{} ({} \u{2013} {}) -> {:.2} ms/call \u{00d7} {n}",
            fmt_dur(med),
            fmt_dur(*min),
            fmt_dur(*max),
            per_call_ms,
        );
    } else {
        eprintln!(
            "        {label:<14}{} ({} \u{2013} {})",
            fmt_dur(med),
            fmt_dur(*min),
            fmt_dur(*max),
        );
    }
}

fn fmt_ratio(baseline: Duration, test: Duration, bold: bool) -> String {
    let ratio = baseline.as_secs_f64() / test.as_secs_f64();
    let text = if ratio >= 10.0 {
        format!("{ratio:.0}x faster")
    } else if ratio >= 1.05 {
        format!("{ratio:.1}x faster")
    } else if ratio > 0.95 {
        "~same".to_string()
    } else {
        let inv = 1.0 / ratio;
        if inv >= 10.0 {
            format!("{inv:.0}x slower")
        } else {
            format!("{inv:.1}x slower")
        }
    };
    if bold && ratio >= 2.0 {
        format!("**{text}**")
    } else {
        text
    }
}

// ── Main benchmark ──────────────────────────────────────────────────────

// -- Link/archive benchmark helpers -----------------------------------------

fn find_archiver() -> Option<NormalizedPath> {
    zccache_test_support::find_on_path("ar")
        .or_else(|| zccache_test_support::find_on_path("llvm-ar"))
}

fn bench_exe_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

fn remove_path_if_exists(path: &Path) {
    if path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

fn remove_output_and_sidecars(output: &Path) {
    remove_path_if_exists(output);
    let Some(parent) = output.parent() else {
        return;
    };
    let Some(stem) = output.file_stem().and_then(|s| s.to_str()) else {
        return;
    };
    for ext in [
        "a", "data", "dSYM", "exe", "html", "js", "lib", "map", "pdb", "wasm",
    ] {
        remove_path_if_exists(&parent.join(format!("{stem}.{ext}")));
    }
}

fn clean_link_outputs(cwd: &Path, outputs: &[String]) {
    for output in outputs {
        let path = Path::new(output);
        if path.is_absolute() {
            remove_output_and_sidecars(path);
        } else {
            remove_output_and_sidecars(&cwd.join(path));
        }
    }
}

fn command_failure(description: &str, output: &std::process::Output) -> String {
    format!(
        "{description} failed with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn try_run_tool(tool: &Path, args: &[String], cwd: &Path, description: &str) -> Result<(), String> {
    let output = std::process::Command::new(tool)
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("failed to run {description}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_failure(description, &output))
    }
}

fn run_tool_timed(tool: &Path, args: &[String], cwd: &Path, description: &str) -> Duration {
    let start = Instant::now();
    let output = std::process::Command::new(tool)
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("failed to run {description}: {e}"));
    let elapsed = start.elapsed();
    assert!(
        output.status.success(),
        "{}",
        command_failure(description, &output)
    );
    elapsed
}

fn try_run_sccache_tool_timed(
    sccache: &Path,
    tool: &Path,
    args: &[String],
    cwd: &Path,
    description: &str,
) -> Result<Duration, String> {
    let start = Instant::now();
    let output = std::process::Command::new(sccache)
        .arg(tool)
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("failed to run {description}: {e}"))?;
    let elapsed = start.elapsed();
    if output.status.success() {
        Ok(elapsed)
    } else {
        Err(command_failure(description, &output))
    }
}

fn start_fresh_sccache(sccache: &Path, cache_dir: &Path) -> String {
    let cache_dir_str = cache_dir.to_string_lossy().into_owned();
    std::env::set_var("SCCACHE_DIR", &cache_dir_str);
    let _ = std::process::Command::new(sccache)
        .arg("--stop-server")
        .env("SCCACHE_DIR", &cache_dir_str)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    clear_dir_contents(cache_dir);
    let _ = std::process::Command::new(sccache)
        .arg("--start-server")
        .env("SCCACHE_DIR", &cache_dir_str)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    cache_dir_str
}

fn stop_sccache(sccache: &Path) {
    let _ = std::process::Command::new(sccache)
        .arg("--stop-server")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    std::env::remove_var("SCCACHE_DIR");
}

async fn clear_zccache(client: &mut ClientConn) {
    client.send(&Request::Clear).await.unwrap();
    match client.recv().await.unwrap() {
        Some(Response::Cleared { .. }) => {}
        other => panic!("expected Cleared, got: {other:?}"),
    }
}

async fn run_zccache_link_timed(
    client: &mut ClientConn,
    tool: &Path,
    args: &[String],
    cwd: &Path,
    expected_cached: bool,
    description: &str,
) -> Duration {
    let start = Instant::now();
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),
            tool: tool.to_string_lossy().into_owned().into(),
            args: args.to_vec(),
            cwd: cwd.to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();
    let elapsed = start.elapsed();
    match client.recv().await.unwrap() {
        Some(Response::LinkResult {
            exit_code,
            stderr,
            cached,
            warning,
            ..
        }) => {
            assert_eq!(
                exit_code,
                0,
                "{description} failed:\n{}",
                String::from_utf8_lossy(&stderr)
            );
            assert_eq!(
                cached, expected_cached,
                "{description} cached={cached}, expected {expected_cached}; warning={warning:?}"
            );
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }
    elapsed
}

struct LinkBenchResult {
    scenario: &'static str,
    bare_cold: Duration,
    bare_warm: Duration,
    sccache_cold: Option<Duration>,
    sccache_warm: Option<Vec<Duration>>,
    zccache_cold: Duration,
    zccache_warm: Vec<Duration>,
}

async fn measure_ephemeral_link_scenario(
    scenario: &'static str,
    tool: &Path,
    args: &[String],
    outputs: &[String],
    bare_dir: &Path,
    sccache_dir: &Path,
    zccache_dir: &Path,
) -> LinkBenchResult {
    eprintln!("  Scenario: {scenario}");
    eprintln!();

    eprintln!("  [1/3] Bare {}", tool.display());
    clean_link_outputs(bare_dir, outputs);
    let _ = run_tool_timed(tool, args, bare_dir, "bare link warmup");
    clean_link_outputs(bare_dir, outputs);
    let bare_cold = run_tool_timed(tool, args, bare_dir, "bare cold link");
    eprintln!("        cold: {}", fmt_dur(bare_cold));
    let mut bare_warm = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        clean_link_outputs(bare_dir, outputs);
        bare_warm.push(run_tool_timed(tool, args, bare_dir, "bare warm link"));
    }
    print_trials("warm:", &bare_warm);
    eprintln!();

    let (sccache_cold, sccache_warm) = if let Some(sccache_bin) = find_sccache() {
        let sc_cache_dir = zccache_test_support::temp_cache_dir().unwrap();
        let _cache_dir = start_fresh_sccache(&sccache_bin, sc_cache_dir.path());
        eprintln!("  [2/3] sccache ({})", sccache_bin.display());

        clean_link_outputs(sccache_dir, outputs);
        let cold = match try_run_sccache_tool_timed(
            &sccache_bin,
            tool,
            args,
            sccache_dir,
            "sccache cold link",
        ) {
            Ok(duration) => duration,
            Err(error) => {
                eprintln!(
                    "        sccache link passthrough failed; using direct tool as no-cache baseline\n        {}",
                    error.lines().next().unwrap_or("unknown failure")
                );
                run_tool_timed(tool, args, sccache_dir, "direct no-cache cold link")
            }
        };
        eprintln!("        cold: {}", fmt_dur(cold));

        let mut passthrough_supported = true;
        let mut warm = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            clean_link_outputs(sccache_dir, outputs);
            let duration = if passthrough_supported {
                match try_run_sccache_tool_timed(
                    &sccache_bin,
                    tool,
                    args,
                    sccache_dir,
                    "sccache warm link",
                ) {
                    Ok(duration) => duration,
                    Err(_) => {
                        passthrough_supported = false;
                        run_tool_timed(tool, args, sccache_dir, "direct no-cache warm link")
                    }
                }
            } else {
                run_tool_timed(tool, args, sccache_dir, "direct no-cache warm link")
            };
            warm.push(duration);
        }
        print_trials("warm:", &warm);
        stop_sccache(&sccache_bin);
        eprintln!();
        (Some(cold), Some(warm))
    } else {
        eprintln!("  [2/3] sccache: not found, skipping");
        eprintln!();
        (None, None)
    };

    eprintln!("  [3/3] zccache");
    clean_link_outputs(zccache_dir, outputs);
    let _ = run_tool_timed(tool, args, zccache_dir, "zccache linker warmup");
    clean_link_outputs(zccache_dir, outputs);

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    clear_zccache(&mut client).await;

    let zccache_cold = run_zccache_link_timed(
        &mut client,
        tool,
        args,
        zccache_dir,
        false,
        "zccache cold link",
    )
    .await;
    eprintln!("        cold: {}", fmt_dur(zccache_cold));
    let mut zccache_warm = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        clean_link_outputs(zccache_dir, outputs);
        zccache_warm.push(
            run_zccache_link_timed(
                &mut client,
                tool,
                args,
                zccache_dir,
                true,
                "zccache warm link",
            )
            .await,
        );
    }
    print_trials("warm:", &zccache_warm);
    shutdown.notify_one();
    server_handle.await.unwrap();
    eprintln!();

    LinkBenchResult {
        scenario,
        bare_cold,
        bare_warm: median(&bare_warm),
        sccache_cold,
        sccache_warm,
        zccache_cold,
        zccache_warm,
    }
}

fn print_link_benchmark_table(title: &str, bare_label: &str, results: &[LinkBenchResult]) {
    let dash = "\u{2014}";
    eprintln!();
    eprintln!("{title}");
    eprintln!();
    eprintln!("| Scenario | {bare_label} | sccache | zccache | vs sccache | vs {bare_label} |");
    eprintln!("|:---------|----------:|--------:|--------:|-----------:|--------------:|");
    for result in results {
        let cold_sccache = result.sccache_cold.map(fmt_dur);
        let cold_vs_sccache = result
            .sccache_cold
            .map(|duration| fmt_ratio(duration, result.zccache_cold, false));
        let cold_vs_bare = fmt_ratio(result.bare_cold, result.zccache_cold, false);
        eprintln!(
            "| {}, Cold | {} | {} | {} | {} | {} |",
            result.scenario,
            fmt_dur(result.bare_cold),
            cold_sccache.as_deref().unwrap_or(dash),
            fmt_dur(result.zccache_cold),
            cold_vs_sccache.as_deref().unwrap_or(dash),
            cold_vs_bare,
        );

        let zccache_warm = median(&result.zccache_warm);
        let warm_sccache = result
            .sccache_warm
            .as_ref()
            .map(|times| fmt_dur(median(times)));
        let warm_vs_sccache = result
            .sccache_warm
            .as_ref()
            .map(|times| fmt_ratio(median(times), zccache_warm, true));
        let warm_vs_bare = fmt_ratio(result.bare_warm, zccache_warm, true);
        eprintln!(
            "| {}, Warm | {} | {} | **{}** | {} | {} |",
            result.scenario,
            fmt_dur(result.bare_warm),
            warm_sccache.as_deref().unwrap_or(dash),
            fmt_dur(zccache_warm),
            warm_vs_sccache.as_deref().unwrap_or(dash),
            warm_vs_bare,
        );
    }
    eprintln!();
    eprintln!(
        "> **Cold** = first link/archive with an empty zccache. **Warm** = median of {WARM_TRIALS} subsequent cached output restores."
    );
    eprintln!();
}

fn fake_archive_object_names() -> Vec<String> {
    (0..NUM_FILES).map(|i| format!("unit_{i:03}.o")).collect()
}

fn prepare_fake_archive_inputs(dir: &Path) -> Vec<String> {
    clear_dir_contents(dir);
    let names = fake_archive_object_names();
    for (i, name) in names.iter().enumerate() {
        let mut content = Vec::with_capacity(4096);
        for n in 0..128 {
            content.extend_from_slice(format!("fake c object {i:03} record {n:03}\n").as_bytes());
        }
        std::fs::write(dir.join(name), content).unwrap();
    }
    names
}

fn archive_link_args(output: &str, objects: &[String]) -> Vec<String> {
    let mut args = vec!["rcsD".to_string(), output.to_string()];
    args.extend(objects.iter().cloned());
    args
}

fn prepare_cpp_link_inputs(compiler: &str, dir: &Path) -> Result<Vec<String>, String> {
    clear_dir_contents(dir);
    generate_project(dir);
    std::fs::write(dir.join("main.cpp"), "int main() { return 0; }\n").unwrap();

    let mut objects = Vec::with_capacity(NUM_FILES + 1);
    for src in source_names() {
        let obj = src.replace(".cpp", ".o");
        let args = vec![
            "-c".to_string(),
            src.clone(),
            "-o".to_string(),
            obj.clone(),
            "-Iinclude".to_string(),
            "-O2".to_string(),
            "-std=c++17".to_string(),
        ];
        try_run_tool(Path::new(compiler), &args, dir, "compile C++ link input")?;
        objects.push(obj);
    }
    let args = vec![
        "-c".to_string(),
        "main.cpp".to_string(),
        "-o".to_string(),
        "main.o".to_string(),
        "-O2".to_string(),
        "-std=c++17".to_string(),
    ];
    try_run_tool(
        Path::new(compiler),
        &args,
        dir,
        "compile C++ main link input",
    )?;
    objects.push("main.o".to_string());
    Ok(objects)
}

fn driver_link_args(output: &str, objects: &[String]) -> Vec<String> {
    let mut args = vec!["-o".to_string(), output.to_string()];
    args.extend(objects.iter().cloned());
    args
}

fn rust_final_output_name() -> String {
    if cfg!(windows) {
        "rust_link_app.lib".to_string()
    } else {
        "librust_link_app.a".to_string()
    }
}

fn rust_rlib_path(index: usize) -> String {
    format!("deps/libunit_{index:03}-unit_{index:03}.rlib")
}

fn rust_final_link_args(output: &str) -> Vec<String> {
    let mut args = vec![
        "--edition".to_string(),
        "2021".to_string(),
        "--crate-type".to_string(),
        "staticlib".to_string(),
        "--crate-name".to_string(),
        "rust_link_app".to_string(),
        "--emit=link".to_string(),
        "-C".to_string(),
        "metadata=rust_link_app".to_string(),
        "-L".to_string(),
        "dependency=deps".to_string(),
        "lib.rs".to_string(),
        "-o".to_string(),
        output.to_string(),
    ];
    for i in 0..RUSTC_NUM_FILES {
        args.push("--extern".to_string());
        args.push(format!("unit_{i:03}={}", rust_rlib_path(i)));
    }
    args
}

fn prepare_rust_link_inputs(rustc: &str, dir: &Path) -> Result<(), String> {
    clear_dir_contents(dir);
    generate_rust_project(dir);
    let srcs = rust_source_names();
    run_rustc_batch(rustc, dir, &srcs, rustc_args_for);
    for i in 0..RUSTC_NUM_FILES {
        let rlib = dir.join(rust_rlib_path(i));
        if !rlib.exists() {
            return Err(format!("expected rlib missing: {}", rlib.display()));
        }
    }

    let mut lib_rs = String::new();
    for i in 0..RUSTC_NUM_FILES {
        lib_rs.push_str(&format!("extern crate unit_{i:03};\n"));
    }
    lib_rs.push_str(
        "\n#[no_mangle]\npub extern \"C\" fn zccache_link_entry() -> f64 {\n    let mut acc = 0.0_f64;\n",
    );
    for i in 0..RUSTC_NUM_FILES {
        lib_rs.push_str(&format!("    acc += unit_{i:03}::compute_{i:03}({i});\n"));
    }
    lib_rs.push_str("    acc\n}\n");
    std::fs::write(dir.join("lib.rs"), lib_rs).unwrap();
    Ok(())
}

fn clean_rust_final_output(cwd: &Path, output: &str) {
    clean_link_outputs(cwd, &[output.to_string()]);
}

fn run_rust_final_link_timed(
    rustc: &Path,
    args: &[String],
    cwd: &Path,
    output: &str,
    description: &str,
) -> Duration {
    clean_rust_final_output(cwd, output);
    run_tool_timed(rustc, args, cwd, description)
}

fn try_run_sccache_rust_final_link_timed(
    sccache: &Path,
    rustc: &Path,
    args: &[String],
    cwd: &Path,
    output: &str,
    description: &str,
) -> Result<Duration, String> {
    clean_rust_final_output(cwd, output);
    try_run_sccache_tool_timed(sccache, rustc, args, cwd, description)
}

async fn run_zccache_rust_final_link_timed(
    client: &mut ClientConn,
    session_id: &str,
    rustc: &Path,
    args: &[String],
    cwd: &Path,
    output: &str,
    expected_cached: bool,
) -> Duration {
    clean_rust_final_output(cwd, output);
    let start = Instant::now();
    client
        .send(&Request::Compile {
            session_id: session_id.to_string(),
            args: args.to_vec(),
            cwd: cwd.to_string_lossy().into_owned().into(),
            compiler: rustc.to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();
    let elapsed = start.elapsed();
    match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code,
            stderr,
            cached,
            ..
        }) => {
            assert_eq!(
                exit_code,
                0,
                "zccache Rust link failed:\n{}",
                String::from_utf8_lossy(&stderr)
            );
            assert_eq!(
                cached, expected_cached,
                "zccache Rust link cached={cached}, expected {expected_cached}"
            );
        }
        other => panic!("expected CompileResult, got: {other:?}"),
    }
    elapsed
}

#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- perf_c_zccache_vs_bare --nocapture --ignored
async fn perf_c_zccache_vs_bare() {
    zccache_test_support::ensure_clang_tool_chain_on_path();
    let compiler_path = match zccache_test_support::find_on_path("clang") {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no C compiler found");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();
    let sources = c_source_names();

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  C COMPILATION BENCHMARK");
    eprintln!("  {NUM_FILES} .c files | {WARM_TRIALS} warm trials | each tool in its own tempdir");
    eprintln!("  Compiler: {compiler}");
    eprintln!("================================================================");
    eprintln!();

    let bl_dir = zccache_test_support::temp_cache_dir().unwrap();
    generate_c_project(bl_dir.path());

    eprintln!("  [1/2] Bare clang");
    nuke_and_regenerate_c(bl_dir.path());
    warmup_c_compiler(&compiler, bl_dir.path());
    let bl_cold = baseline_c_single(&compiler, bl_dir.path(), &sources);
    eprintln!("        cold:  {}", fmt_dur(bl_cold));

    let bl_warm = baseline_c_single(&compiler, bl_dir.path(), &sources);
    eprintln!("        warm:  {}", fmt_dur(bl_warm));
    eprintln!();
    drop(bl_dir);

    let sccache_cold;
    let sccache_warm;
    if let Some(sccache_bin) = find_sccache() {
        let sc_dir = zccache_test_support::temp_cache_dir().unwrap();
        generate_c_project(sc_dir.path());

        let sc_cache_dir = zccache_test_support::temp_cache_dir().unwrap();
        let sc_cache_str = sc_cache_dir.path().to_string_lossy().into_owned();
        std::env::set_var("SCCACHE_DIR", &sc_cache_str);

        eprintln!("  [2/3] sccache ({})", sccache_bin.display());
        let _ = std::process::Command::new(&sccache_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if sc_cache_dir.path().exists() {
            let _ = std::fs::remove_dir_all(sc_cache_dir.path());
            let _ = std::fs::create_dir_all(sc_cache_dir.path());
        }
        let _ = std::process::Command::new(&sccache_bin)
            .arg("--start-server")
            .env("SCCACHE_DIR", &sc_cache_str)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        nuke_and_regenerate_c(sc_dir.path());
        warmup_c_compiler(&compiler, sc_dir.path());
        let cold = sccache_compile_c_single(&sccache_bin, &compiler, sc_dir.path(), &sources);
        eprintln!("        cold:  {}", fmt_dur(cold));
        sccache_cold = Some(cold);

        let mut times = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            times.push(sccache_compile_c_single(
                &sccache_bin,
                &compiler,
                sc_dir.path(),
                &sources,
            ));
        }
        print_trials("warm:", &times);
        sccache_warm = Some(times);

        let _ = std::process::Command::new(&sccache_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        std::env::remove_var("SCCACHE_DIR");
        eprintln!();
    } else {
        eprintln!("  [2/3] sccache: not found, skipping");
        eprintln!();
        sccache_cold = None;
        sccache_warm = None;
    }

    let zc_dir = zccache_test_support::temp_cache_dir().unwrap();
    generate_c_project(zc_dir.path());
    let zc_cwd = zc_dir.path().to_string_lossy().into_owned();

    eprintln!("  [3/3] zccache");
    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: zc_cwd.clone().into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
        })
        .await
        .unwrap();
    let session_id = match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };

    nuke_and_regenerate_c(zc_dir.path());
    warmup_c_compiler(&compiler, zc_dir.path());
    let zc_cold =
        zccache_compile_c_single(&mut client, &session_id, &compiler, &zc_cwd, &sources).await;
    eprintln!("        cold:  {}", fmt_dur(zc_cold));

    let mut zc_warm = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_warm.push(
            zccache_compile_c_single(&mut client, &session_id, &compiler, &zc_cwd, &sources).await,
        );
    }
    print_trials("warm:", &zc_warm);

    client
        .send(&Request::SessionEnd {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let _ = client.recv::<Response>().await;

    shutdown.notify_one();
    server_handle.await.unwrap();

    let zc_warm_med = median(&zc_warm);
    let vs_bare_cold = fmt_ratio(bl_cold, zc_cold, false);
    let vs_bare_warm = fmt_ratio(bl_warm, zc_warm_med, true);
    let dash = "\u{2014}";
    let sccache_cold_str = sccache_cold.map(fmt_dur);
    let sccache_warm_str = sccache_warm.as_ref().map(|times| fmt_dur(median(times)));
    let vs_sccache_cold = sccache_cold.map(|duration| fmt_ratio(duration, zc_cold, false));
    let vs_sccache_warm = sccache_warm
        .as_ref()
        .map(|times| fmt_ratio(median(times), zc_warm_med, true));

    eprintln!();
    eprintln!("## C Benchmark: {NUM_FILES} .c files, {WARM_TRIALS} warm trials");
    eprintln!();
    eprintln!("| Scenario | Bare clang | sccache | zccache | vs sccache | vs bare clang |");
    eprintln!("|:---------|----------:|--------:|--------:|-----------:|--------------:|");
    eprintln!(
        "| Single-file, Cold | {} | {} | {} | {} | {} |",
        fmt_dur(bl_cold),
        sccache_cold_str.as_deref().unwrap_or(dash),
        fmt_dur(zc_cold),
        vs_sccache_cold.as_deref().unwrap_or(dash),
        vs_bare_cold,
    );
    eprintln!(
        "| Single-file, Warm | {} | {} | **{}** | {} | {} |",
        fmt_dur(bl_warm),
        sccache_warm_str.as_deref().unwrap_or(dash),
        fmt_dur(zc_warm_med),
        vs_sccache_warm.as_deref().unwrap_or(dash),
        vs_bare_warm,
    );
    eprintln!();
    eprintln!("> **Cold** = first compile (empty cache). **Warm** = median of {WARM_TRIALS} subsequent runs.");
    eprintln!();
}

#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- --nocapture --ignored
async fn perf_warm_cache_zccache_vs_sccache() {
    let compiler_path = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no C++ compiler found");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();
    let sources = source_names();

    eprintln!();
    eprintln!("\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}");
    eprintln!("  WARM-CACHE BENCHMARK");
    eprintln!("  {NUM_FILES} C++ files \u{00b7} {WARM_TRIALS} warm trials \u{00b7} each tool in its own tempdir");
    eprintln!("  Compiler: {compiler}");
    eprintln!("\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}");
    eprintln!();

    // ── Baseline (fresh dir) ──────────────────────────────────────────
    let bl_dir = zccache_test_support::temp_cache_dir().unwrap();
    generate_project(bl_dir.path());

    eprintln!("  [1/3] Bare clang (baseline)");

    nuke_and_regenerate(bl_dir.path());
    warmup_compiler(&compiler, bl_dir.path());
    let bl_cold_single = baseline_single(&compiler, bl_dir.path(), &sources);
    eprintln!("        single cold:  {}", fmt_dur(bl_cold_single));

    let bl_warm_single = baseline_single(&compiler, bl_dir.path(), &sources);
    eprintln!("        single warm:  {}", fmt_dur(bl_warm_single));

    nuke_and_regenerate(bl_dir.path());
    warmup_compiler(&compiler, bl_dir.path());
    let bl_cold_multi = baseline_multi(&compiler, bl_dir.path(), &sources);
    eprintln!("        multi cold:   {}", fmt_dur(bl_cold_multi));

    let bl_warm_multi = baseline_multi(&compiler, bl_dir.path(), &sources);
    eprintln!("        multi warm:   {}", fmt_dur(bl_warm_multi));
    eprintln!();

    drop(bl_dir);

    // ── sccache (fresh dir) ───────────────────────────────────────────
    let sccache_cold_single;
    let sccache_cold_multi;
    let sccache_single_times;
    let sccache_multi_times;

    if let Some(sccache_bin) = find_sccache() {
        let sc_dir = zccache_test_support::temp_cache_dir().unwrap();
        generate_project(sc_dir.path());

        // Use a fresh cache dir so previous sccache usage doesn't pollute results.
        let sc_cache_dir = zccache_test_support::temp_cache_dir().unwrap();
        let sc_cache_str = sc_cache_dir.path().to_string_lossy().into_owned();

        // Set SCCACHE_DIR for this process so both server and client see it.
        std::env::set_var("SCCACHE_DIR", &sc_cache_str);

        eprintln!("  [2/3] sccache ({})", sccache_bin.display());

        // Helper: stop server, purge disk cache, restart with fresh SCCACHE_DIR.
        let stop_purge_start = |sccache: &Path, cache_dir: &str| {
            let _ = std::process::Command::new(sccache)
                .arg("--stop-server")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            // Purge disk cache
            let cache_path = std::path::Path::new(cache_dir);
            if cache_path.exists() {
                let _ = std::fs::remove_dir_all(cache_path);
                let _ = std::fs::create_dir_all(cache_path);
            }
            let _ = std::process::Command::new(sccache)
                .arg("--start-server")
                .env("SCCACHE_DIR", cache_dir)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        };

        // Start fresh server with isolated cache dir
        stop_purge_start(&sccache_bin, &sc_cache_str);

        // Cold single-file: nuke dir, regenerate, warmup clang, compile (cache empty)
        nuke_and_regenerate(sc_dir.path());
        warmup_compiler(&compiler, sc_dir.path());
        eprint!("        single cold:  ");
        let cold_s = sccache_compile_single(&sccache_bin, &compiler, sc_dir.path(), &sources);
        eprintln!("{}", fmt_dur(cold_s));
        sccache_cold_single = Some(cold_s);

        // Warm trials: single-file (cache populated from cold pass)
        let mut times = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            times.push(sccache_compile_single(
                &sccache_bin,
                &compiler,
                sc_dir.path(),
                &sources,
            ));
        }
        print_trials("single warm:", &times);
        sccache_single_times = Some(times);

        stop_purge_start(&sccache_bin, &sc_cache_str);

        nuke_and_regenerate(sc_dir.path());
        warmup_compiler(&compiler, sc_dir.path());
        eprint!("        multi cold:   ");
        let cold_m = sccache_compile_multi(&sccache_bin, &compiler, sc_dir.path(), &sources);
        eprintln!("{}", fmt_dur(cold_m));
        sccache_cold_multi = Some(cold_m);

        // Warm trials: multi-file (sccache can't cache this — passes through)
        let mut times = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            times.push(sccache_compile_multi(
                &sccache_bin,
                &compiler,
                sc_dir.path(),
                &sources,
            ));
        }
        print_trials("multi warm:", &times);
        sccache_multi_times = Some(times);

        let _ = std::process::Command::new(&sccache_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        std::env::remove_var("SCCACHE_DIR");
        drop(sc_dir);
        drop(sc_cache_dir);
        eprintln!();
    } else {
        eprintln!("  [2/3] sccache: not found, skipping");
        eprintln!();
        sccache_cold_single = None;
        sccache_cold_multi = None;
        sccache_single_times = None;
        sccache_multi_times = None;
    }

    // ── zccache (fresh dir, in-process daemon) ────────────────────────
    let zc_dir = zccache_test_support::temp_cache_dir().unwrap();
    generate_project(zc_dir.path());
    let zc_cwd = zc_dir.path().to_string_lossy().into_owned();

    eprintln!("  [3/3] zccache (in-process daemon)");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    // Start session
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: zc_cwd.clone().into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
        })
        .await
        .unwrap();
    let session_id = match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };

    nuke_and_regenerate(zc_dir.path());
    warmup_compiler(&compiler, zc_dir.path());
    eprint!("        single cold:  ");
    let zc_cold_single =
        zccache_compile_single(&mut client, &session_id, &compiler, &zc_cwd, &sources).await;
    eprintln!("{}", fmt_dur(zc_cold_single));

    let mut zc_single_times = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_single_times.push(
            zccache_compile_single(&mut client, &session_id, &compiler, &zc_cwd, &sources).await,
        );
    }
    print_trials("single warm:", &zc_single_times);

    client.send(&Request::Clear).await.unwrap();
    let _ = client.recv::<Response>().await;
    nuke_and_regenerate(zc_dir.path());
    warmup_compiler(&compiler, zc_dir.path());

    eprint!("        multi cold:   ");
    let zc_cold_multi =
        zccache_compile_multi(&mut client, &session_id, &compiler, &zc_cwd, &sources).await;
    eprintln!("{}", fmt_dur(zc_cold_multi));

    let mut zc_multi_times = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_multi_times.push(
            zccache_compile_multi(&mut client, &session_id, &compiler, &zc_cwd, &sources).await,
        );
    }
    print_trials("multi warm:", &zc_multi_times);

    // End session
    client
        .send(&Request::SessionEnd {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let _ = client.recv::<Response>().await;

    shutdown.notify_one();
    server_handle.await.unwrap();

    // ── Summary ─────────────────────────────────────────────────────
    let zc_single_med = median(&zc_single_times);
    let zc_multi_med = median(&zc_multi_times);

    let scc_single_str = sccache_single_times.as_ref().map(|t| fmt_dur(median(t)));
    let scc_multi_str = sccache_multi_times.as_ref().map(|t| fmt_dur(median(t)));
    let scc_cold_s_str = sccache_cold_single.map(fmt_dur);
    let scc_cold_m_str = sccache_cold_multi.map(fmt_dur);
    let dash = "\u{2014}";

    eprintln!();
    eprintln!("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}");
    eprintln!("  RESULTS");
    eprintln!("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}");
    eprintln!();
    eprintln!("## Benchmark: {NUM_FILES} C++ files, {WARM_TRIALS} warm trials");
    eprintln!();
    eprintln!("| Scenario | Bare Clang | sccache | zccache | vs sccache | vs bare clang |");
    eprintln!("|:---------|----------:|--------:|--------:|-----------:|--------------:|");

    // Single-file, Cold
    let scc_cs = scc_cold_s_str.as_deref().unwrap_or(dash);
    let vs_scc_cold_s = sccache_cold_single.map(|t| fmt_ratio(t, zc_cold_single, false));
    let vs_bare_cold_s = fmt_ratio(bl_cold_single, zc_cold_single, false);
    eprintln!(
        "| Single-file, Cold | {} | {} | {} | {} | {} |",
        fmt_dur(bl_cold_single),
        scc_cs,
        fmt_dur(zc_cold_single),
        vs_scc_cold_s.as_deref().unwrap_or(dash),
        vs_bare_cold_s,
    );

    // Single-file, Warm
    let scc_ws = scc_single_str.as_deref().unwrap_or(dash);
    let vs_scc_warm_s = sccache_single_times
        .as_ref()
        .map(|t| fmt_ratio(median(t), zc_single_med, true));
    let vs_bare_warm_s = fmt_ratio(bl_warm_single, zc_single_med, true);
    eprintln!(
        "| Single-file, Warm | {} | {} | **{}** | {} | {} |",
        fmt_dur(bl_warm_single),
        scc_ws,
        fmt_dur(zc_single_med),
        vs_scc_warm_s.as_deref().unwrap_or(dash),
        vs_bare_warm_s,
    );

    // Multi-file, Cold
    let scc_cm = scc_cold_m_str.as_deref().unwrap_or(dash);
    let vs_scc_cold_m = sccache_cold_multi.map(|t| fmt_ratio(t, zc_cold_multi, false));
    let vs_bare_cold_m = fmt_ratio(bl_cold_multi, zc_cold_multi, false);
    eprintln!(
        "| Multi-file, Cold | {} | {} | {} | {} | {} |",
        fmt_dur(bl_cold_multi),
        scc_cm,
        fmt_dur(zc_cold_multi),
        vs_scc_cold_m.as_deref().unwrap_or(dash),
        vs_bare_cold_m,
    );

    // Multi-file, Warm
    let scc_wm = scc_multi_str.as_deref().unwrap_or(dash);
    let vs_scc_warm_m = sccache_multi_times
        .as_ref()
        .map(|t| fmt_ratio(median(t), zc_multi_med, true));
    let vs_bare_warm_m = fmt_ratio(bl_warm_multi, zc_multi_med, true);
    eprintln!(
        "| Multi-file, Warm | {} | {} | **{}** | {} | {} |",
        fmt_dur(bl_warm_multi),
        scc_wm,
        fmt_dur(zc_multi_med),
        vs_scc_warm_m.as_deref().unwrap_or(dash),
        vs_bare_warm_m,
    );

    eprintln!();
    eprintln!("> **Cold** = first compile (empty cache). **Warm** = median of {WARM_TRIALS} subsequent runs.");
    eprintln!("> Single-file = {NUM_FILES} sequential `clang++ -c unit.cpp` invocations. Multi-file = one `clang++ -c *.cpp` invocation.");
    if sccache_multi_times.is_some() {
        eprintln!("> sccache cannot cache multi-file compilations \u{2014} its \"warm\" multi-file time is a full recompile.");
    }

    // ── Bottom Line ─────────────────────────────────────────────────
    eprintln!();
    eprintln!("### Bottom Line");
    eprintln!();
    let single_vs_clang = bl_warm_single.as_secs_f64() / zc_single_med.as_secs_f64();
    let multi_vs_clang = bl_warm_multi.as_secs_f64() / zc_multi_med.as_secs_f64();
    if let Some(ref t) = sccache_single_times {
        let single_vs_scc = median(t).as_secs_f64() / zc_single_med.as_secs_f64();
        eprintln!(
            "  Warm single-file:  {single_vs_clang:.0}x faster than clang, {single_vs_scc:.0}x faster than sccache"
        );
    } else {
        eprintln!("  Warm single-file:  {single_vs_clang:.0}x faster than clang");
    }
    if let Some(ref t) = sccache_multi_times {
        let multi_vs_scc = median(t).as_secs_f64() / zc_multi_med.as_secs_f64();
        eprintln!(
            "  Warm multi-file:   {multi_vs_clang:.0}x faster than clang, {multi_vs_scc:.0}x faster than sccache"
        );
    } else {
        eprintln!("  Warm multi-file:   {multi_vs_clang:.0}x faster than clang");
    }
    eprintln!();
}

// ── Response-file benchmark (separate test) ─────────────────────────────

#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- perf_response_file --nocapture --ignored
async fn perf_response_file() {
    let compiler_path = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no C++ compiler found");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();
    let sources = source_names();

    eprintln!();
    eprintln!("\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}");
    eprintln!("  RESPONSE-FILE BENCHMARK");
    eprintln!(
        "  {NUM_FILES} C++ files \u{00b7} {WARM_TRIALS} warm trials \u{00b7} ~{} expanded args per compile",
        RSP_NUM_DEFINES + RSP_NUM_INCLUDES + 30 + 3,
    );
    eprintln!("  Compiler: {compiler}");
    eprintln!("\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}");
    eprintln!();

    // ── Baseline RSP (fresh dir) ─────────────────────────────────────
    let bl_dir = zccache_test_support::temp_cache_dir().unwrap();
    generate_project(bl_dir.path());
    generate_response_files(bl_dir.path());

    eprintln!("  [1/3] Bare clang (baseline)");

    nuke_and_regenerate_with_rsp(bl_dir.path());
    warmup_compiler(&compiler, bl_dir.path());
    let bl_cold_single = baseline_single_rsp(&compiler, bl_dir.path(), &sources);
    eprintln!("        single cold:  {}", fmt_dur(bl_cold_single));

    let bl_warm_single = baseline_single_rsp(&compiler, bl_dir.path(), &sources);
    eprintln!("        single warm:  {}", fmt_dur(bl_warm_single));

    nuke_and_regenerate_with_rsp(bl_dir.path());
    warmup_compiler(&compiler, bl_dir.path());
    let bl_cold_multi = baseline_multi_rsp(&compiler, bl_dir.path());
    eprintln!("        multi cold:   {}", fmt_dur(bl_cold_multi));

    let bl_warm_multi = baseline_multi_rsp(&compiler, bl_dir.path());
    eprintln!("        multi warm:   {}", fmt_dur(bl_warm_multi));
    eprintln!();
    drop(bl_dir);

    // ── sccache RSP (fresh dir) ──────────────────────────────────────
    let sccache_cold_single;
    let sccache_cold_multi;
    let sccache_single_times;
    let sccache_multi_times;

    if let Some(sccache_bin) = find_sccache() {
        let sc_dir = zccache_test_support::temp_cache_dir().unwrap();
        generate_project(sc_dir.path());
        generate_response_files(sc_dir.path());

        let sc_cache_dir = zccache_test_support::temp_cache_dir().unwrap();
        let sc_cache_str = sc_cache_dir.path().to_string_lossy().into_owned();
        std::env::set_var("SCCACHE_DIR", &sc_cache_str);

        eprintln!("  [2/3] sccache ({})", sccache_bin.display());

        let stop_purge_start = |sccache: &Path, cache_dir: &str| {
            let _ = std::process::Command::new(sccache)
                .arg("--stop-server")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            let cache_path = std::path::Path::new(cache_dir);
            if cache_path.exists() {
                let _ = std::fs::remove_dir_all(cache_path);
                let _ = std::fs::create_dir_all(cache_path);
            }
            let _ = std::process::Command::new(sccache)
                .arg("--start-server")
                .env("SCCACHE_DIR", cache_dir)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        };

        stop_purge_start(&sccache_bin, &sc_cache_str);

        nuke_and_regenerate_with_rsp(sc_dir.path());
        warmup_compiler(&compiler, sc_dir.path());
        eprint!("        single cold:  ");
        let cold_s = sccache_compile_single_rsp(&sccache_bin, &compiler, sc_dir.path(), &sources);
        eprintln!("{}", fmt_dur(cold_s));
        sccache_cold_single = Some(cold_s);

        let mut times = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            times.push(sccache_compile_single_rsp(
                &sccache_bin,
                &compiler,
                sc_dir.path(),
                &sources,
            ));
        }
        print_trials("single warm:", &times);
        sccache_single_times = Some(times);

        stop_purge_start(&sccache_bin, &sc_cache_str);

        nuke_and_regenerate_with_rsp(sc_dir.path());
        warmup_compiler(&compiler, sc_dir.path());
        eprint!("        multi cold:   ");
        let cold_m = sccache_compile_multi_rsp(&sccache_bin, &compiler, sc_dir.path());
        eprintln!("{}", fmt_dur(cold_m));
        sccache_cold_multi = Some(cold_m);

        let mut times = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            times.push(sccache_compile_multi_rsp(
                &sccache_bin,
                &compiler,
                sc_dir.path(),
            ));
        }
        print_trials("multi warm:", &times);
        sccache_multi_times = Some(times);

        let _ = std::process::Command::new(&sccache_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        std::env::remove_var("SCCACHE_DIR");
        drop(sc_dir);
        drop(sc_cache_dir);
        eprintln!();
    } else {
        eprintln!("  [2/3] sccache: not found, skipping");
        eprintln!();
        sccache_cold_single = None;
        sccache_cold_multi = None;
        sccache_single_times = None;
        sccache_multi_times = None;
    }

    // ── zccache RSP (fresh dir, in-process daemon) ───────────────────
    let zc_dir = zccache_test_support::temp_cache_dir().unwrap();
    generate_project(zc_dir.path());
    generate_response_files(zc_dir.path());
    let zc_cwd = zc_dir.path().to_string_lossy().into_owned();

    eprintln!("  [3/3] zccache (in-process daemon)");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: zc_cwd.clone().into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
        })
        .await
        .unwrap();
    let session_id = match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };

    nuke_and_regenerate_with_rsp(zc_dir.path());
    warmup_compiler(&compiler, zc_dir.path());
    eprint!("        single cold:  ");
    let zc_cold_single =
        zccache_compile_single_rsp(&mut client, &session_id, &compiler, &zc_cwd, &sources).await;
    eprintln!("{}", fmt_dur(zc_cold_single));

    let mut zc_single_times = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_single_times.push(
            zccache_compile_single_rsp(&mut client, &session_id, &compiler, &zc_cwd, &sources)
                .await,
        );
    }
    print_trials("single warm:", &zc_single_times);

    client.send(&Request::Clear).await.unwrap();
    let _ = client.recv::<Response>().await;
    nuke_and_regenerate_with_rsp(zc_dir.path());
    warmup_compiler(&compiler, zc_dir.path());

    eprint!("        multi cold:   ");
    let zc_cold_multi =
        zccache_compile_multi_rsp(&mut client, &session_id, &compiler, &zc_cwd).await;
    eprintln!("{}", fmt_dur(zc_cold_multi));

    let mut zc_multi_times = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_multi_times
            .push(zccache_compile_multi_rsp(&mut client, &session_id, &compiler, &zc_cwd).await);
    }
    print_trials("multi warm:", &zc_multi_times);

    // End session
    client
        .send(&Request::SessionEnd {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let _ = client.recv::<Response>().await;

    shutdown.notify_one();
    server_handle.await.unwrap();

    // ── Summary ─────────────────────────────────────────────────────
    let zc_single_med = median(&zc_single_times);
    let zc_multi_med = median(&zc_multi_times);

    let scc_single_str = sccache_single_times.as_ref().map(|t| fmt_dur(median(t)));
    let scc_multi_str = sccache_multi_times.as_ref().map(|t| fmt_dur(median(t)));
    let scc_cold_s_str = sccache_cold_single.map(fmt_dur);
    let scc_cold_m_str = sccache_cold_multi.map(fmt_dur);
    let dash = "\u{2014}";

    eprintln!();
    eprintln!("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}");
    eprintln!("  RESULTS");
    eprintln!("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}");
    eprintln!();
    eprintln!(
        "## Response-File Benchmark: {NUM_FILES} C++ files, ~{} expanded args, {WARM_TRIALS} warm trials",
        RSP_NUM_DEFINES + RSP_NUM_INCLUDES + 30 + 3,
    );
    eprintln!();
    eprintln!("| Scenario | Bare Clang | sccache | zccache | vs sccache | vs bare clang |");
    eprintln!("|:---------|----------:|--------:|--------:|-----------:|--------------:|");

    // Single-file RSP, Cold
    let scc_cs = scc_cold_s_str.as_deref().unwrap_or(dash);
    let vs_scc_cold_s = sccache_cold_single.map(|t| fmt_ratio(t, zc_cold_single, false));
    let vs_bare_cold_s = fmt_ratio(bl_cold_single, zc_cold_single, false);
    eprintln!(
        "| Single-file RSP, Cold | {} | {} | {} | {} | {} |",
        fmt_dur(bl_cold_single),
        scc_cs,
        fmt_dur(zc_cold_single),
        vs_scc_cold_s.as_deref().unwrap_or(dash),
        vs_bare_cold_s,
    );

    // Single-file RSP, Warm
    let scc_ws = scc_single_str.as_deref().unwrap_or(dash);
    let vs_scc_warm_s = sccache_single_times
        .as_ref()
        .map(|t| fmt_ratio(median(t), zc_single_med, true));
    let vs_bare_warm_s = fmt_ratio(bl_warm_single, zc_single_med, true);
    eprintln!(
        "| Single-file RSP, Warm | {} | {} | **{}** | {} | {} |",
        fmt_dur(bl_warm_single),
        scc_ws,
        fmt_dur(zc_single_med),
        vs_scc_warm_s.as_deref().unwrap_or(dash),
        vs_bare_warm_s,
    );

    // Multi-file RSP, Cold
    let scc_cm = scc_cold_m_str.as_deref().unwrap_or(dash);
    let vs_scc_cold_m = sccache_cold_multi.map(|t| fmt_ratio(t, zc_cold_multi, false));
    let vs_bare_cold_m = fmt_ratio(bl_cold_multi, zc_cold_multi, false);
    eprintln!(
        "| Multi-file RSP, Cold | {} | {} | {} | {} | {} |",
        fmt_dur(bl_cold_multi),
        scc_cm,
        fmt_dur(zc_cold_multi),
        vs_scc_cold_m.as_deref().unwrap_or(dash),
        vs_bare_cold_m,
    );

    // Multi-file RSP, Warm
    let scc_wm = scc_multi_str.as_deref().unwrap_or(dash);
    let vs_scc_warm_m = sccache_multi_times
        .as_ref()
        .map(|t| fmt_ratio(median(t), zc_multi_med, true));
    let vs_bare_warm_m = fmt_ratio(bl_warm_multi, zc_multi_med, true);
    eprintln!(
        "| Multi-file RSP, Warm | {} | {} | **{}** | {} | {} |",
        fmt_dur(bl_warm_multi),
        scc_wm,
        fmt_dur(zc_multi_med),
        vs_scc_warm_m.as_deref().unwrap_or(dash),
        vs_bare_warm_m,
    );

    eprintln!();
    eprintln!("> **Cold** = first compile (empty cache). **Warm** = median of {WARM_TRIALS} subsequent runs.");
    eprintln!(
        "> All args passed via nested response files: flags.rsp -> @warnings.rsp + @defines.rsp"
    );
    eprintln!("> {RSP_NUM_DEFINES} -D defines + {RSP_NUM_INCLUDES} -I paths + 30 warning flags = ~{} total expanded args per compile.",
        RSP_NUM_DEFINES + RSP_NUM_INCLUDES + 30 + 3);

    // ── Bottom Line ─────────────────────────────────────────────────
    eprintln!();
    eprintln!("### Bottom Line");
    eprintln!();
    let single_vs_clang = bl_warm_single.as_secs_f64() / zc_single_med.as_secs_f64();
    let multi_vs_clang = bl_warm_multi.as_secs_f64() / zc_multi_med.as_secs_f64();
    if let Some(ref t) = sccache_single_times {
        let single_vs_scc = median(t).as_secs_f64() / zc_single_med.as_secs_f64();
        eprintln!(
            "  Warm single-file:  {single_vs_clang:.0}x faster than clang, {single_vs_scc:.0}x faster than sccache"
        );
    } else {
        eprintln!("  Warm single-file:  {single_vs_clang:.0}x faster than clang");
    }
    if let Some(ref t) = sccache_multi_times {
        let multi_vs_scc = median(t).as_secs_f64() / zc_multi_med.as_secs_f64();
        eprintln!(
            "  Warm multi-file:   {multi_vs_clang:.0}x faster than clang, {multi_vs_scc:.0}x faster than sccache"
        );
    } else {
        eprintln!("  Warm multi-file:   {multi_vs_clang:.0}x faster than clang");
    }
    eprintln!();
}

// ═════════════════════════════════════════════════════════════════════════════
// Rust (rustc) benchmark: zccache vs sccache vs bare rustc
// ═════════════════════════════════════════════════════════════════════════════

const RUSTC_NUM_FILES: usize = 50;
const RUSTC_WARM_TRIALS: usize = 5;

fn generate_rust_project(dir: &Path) {
    // Create output directory (mimics cargo's target/debug/deps)
    std::fs::create_dir_all(dir.join("deps")).unwrap();
    for i in 0..RUSTC_NUM_FILES {
        let content = format!(
            r#"pub fn compute_{i:03}(n: i32) -> f64 {{
    let mut acc = n as f64;
    for j in 0..10 {{
        acc = (acc * 0.{i:03}1 + j as f64).sin().abs();
    }}
    acc
}}

pub fn transform_{i:03}(data: &[f64]) -> Vec<f64> {{
    data.iter().map(|&x| compute_{i:03}(x as i32) * x).collect()
}}
"#,
        );
        std::fs::write(dir.join(format!("unit_{i:03}.rs")), content).unwrap();
    }
}

fn rust_source_names() -> Vec<String> {
    (0..RUSTC_NUM_FILES)
        .map(|i| format!("unit_{i:03}.rs"))
        .collect()
}

fn clean_rlibs(dir: &Path) {
    let deps = dir.join("deps");
    if deps.is_dir() {
        for entry in std::fs::read_dir(&deps).unwrap() {
            let path = entry.unwrap().path();
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if matches!(ext, "rlib" | "rmeta" | "d") {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
}

fn warmup_rustc(rc: &str, dir: &Path) {
    let src = dir.join("unit_000.rs");
    let deps = dir.join("deps");
    let s = std::process::Command::new(rc)
        .args([
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "warmup",
            "--emit=dep-info,metadata,link",
            "-C",
            "metadata=warm",
            "-C",
            "extra-filename=-warm",
        ])
        .arg(&src)
        .arg("--out-dir")
        .arg(&deps)
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("rustc warmup failed");
    assert!(s.success());
    clean_rlibs(dir);
}

/// Common rustc args that match what `cargo build` passes.
/// Uses --out-dir (required by sccache), --emit=dep-info,metadata,link,
/// and -C metadata/-C extra-filename for output naming.
fn rustc_args_for(cn: &str, src: &str, deps_dir: &str) -> Vec<String> {
    vec![
        "--edition".into(),
        "2021".into(),
        "--crate-type".into(),
        "lib".into(),
        "--crate-name".into(),
        cn.into(),
        "--emit=dep-info,metadata,link".into(),
        "-C".into(),
        format!("metadata={cn}"),
        "-C".into(),
        format!("extra-filename=-{cn}"),
        "--out-dir".into(),
        deps_dir.into(),
        src.into(),
    ]
}

/// Rustc args matching what `cargo check` passes: --emit=dep-info,metadata (no link).
/// Produces only .rmeta + .d files (no .rlib).
fn rustc_check_args_for(cn: &str, src: &str, deps_dir: &str) -> Vec<String> {
    vec![
        "--edition".into(),
        "2021".into(),
        "--crate-type".into(),
        "lib".into(),
        "--crate-name".into(),
        cn.into(),
        "--emit=dep-info,metadata".into(),
        "-C".into(),
        format!("metadata={cn}"),
        "-C".into(),
        format!("extra-filename=-{cn}"),
        "--out-dir".into(),
        deps_dir.into(),
        src.into(),
    ]
}

/// Run a batch of rustc compilations using the given arg builder.
fn run_rustc_batch(
    rc: &str,
    cwd: &Path,
    srcs: &[String],
    args_fn: fn(&str, &str, &str) -> Vec<String>,
) -> Duration {
    clean_rlibs(cwd);
    let deps = cwd.join("deps");
    let deps_s = deps.to_string_lossy().to_string();
    let start = Instant::now();
    for (i, src) in srcs.iter().enumerate() {
        let cn = format!("unit_{i:03}");
        let args = args_fn(&cn, src, &deps_s);
        let s = std::process::Command::new(rc)
            .args(&args)
            .current_dir(cwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(s.success(), "rustc failed for {src}");
    }
    start.elapsed()
}

fn run_sccache_rustc_batch(
    scc: &Path,
    rc: &str,
    cwd: &Path,
    srcs: &[String],
    args_fn: fn(&str, &str, &str) -> Vec<String>,
) -> Duration {
    clean_rlibs(cwd);
    let deps = cwd.join("deps");
    let deps_s = deps.to_string_lossy().to_string();
    let start = Instant::now();
    for (i, src) in srcs.iter().enumerate() {
        let cn = format!("unit_{i:03}");
        let args = args_fn(&cn, src, &deps_s);
        let s = std::process::Command::new(scc)
            .arg(rc)
            .args(&args)
            .current_dir(cwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(s.success(), "sccache rustc failed for {src}");
    }
    start.elapsed()
}

async fn run_zccache_rustc_batch(
    client: &mut ClientConn,
    sid: &str,
    rc: &str,
    cwd: &str,
    srcs: &[String],
    args_fn: fn(&str, &str, &str) -> Vec<String>,
) -> Duration {
    clean_rlibs(Path::new(cwd));
    let deps = Path::new(cwd).join("deps");
    let deps_s = deps.to_string_lossy().to_string();
    let start = Instant::now();
    for (i, src) in srcs.iter().enumerate() {
        let cn = format!("unit_{i:03}");
        let args = args_fn(&cn, src, &deps_s);
        client
            .send(&Request::Compile {
                session_id: sid.to_string(),
                args,
                cwd: cwd.into(),
                compiler: rc.to_string().into(),
                env: None,
            })
            .await
            .unwrap();
        match client.recv().await.unwrap() {
            Some(Response::CompileResult { exit_code, .. }) => {
                assert_eq!(exit_code, 0, "zccache rustc failed for {src}")
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }
    }
    start.elapsed()
}

/// Rust compilation: bare rustc vs sccache vs zccache, 50 independent .rs lib files.
/// Tests both `cargo build` (emit link+metadata+dep-info) and `cargo check` (emit metadata+dep-info) modes.
#[tokio::test]
#[ignore]
async fn perf_rustc_zccache_vs_sccache() {
    let rustc_path = match zccache_test_support::find_rustc() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: rustc not found");
            return;
        }
    };
    let rc = rustc_path.to_string_lossy().to_string();
    let srcs = rust_source_names();

    eprintln!();
    eprintln!("\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}");
    eprintln!("  RUST COMPILATION BENCHMARK");
    eprintln!("  {RUSTC_NUM_FILES} .rs files \u{00b7} {RUSTC_WARM_TRIALS} warm trials \u{00b7} each tool in its own tempdir");
    eprintln!("  Compiler: {rc}");
    eprintln!("  Modes: build (--emit=dep-info,metadata,link) + check (--emit=dep-info,metadata)");
    eprintln!("\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}");
    eprintln!();

    // Helper: run a mode (build or check) through all 3 tools.
    // ─── Build mode (--emit=dep-info,metadata,link) ─────────────────────
    eprintln!("  ─── Build mode (cargo build) ───");
    eprintln!();

    let bl_dir = zccache_test_support::temp_cache_dir().unwrap();
    generate_rust_project(bl_dir.path());
    eprintln!("  [1/3] Bare rustc");
    warmup_rustc(&rc, bl_dir.path());
    let build_bl_cold = run_rustc_batch(&rc, bl_dir.path(), &srcs, rustc_args_for);
    eprintln!("        cold:  {}", fmt_dur(build_bl_cold));
    let build_bl_warm = run_rustc_batch(&rc, bl_dir.path(), &srcs, rustc_args_for);
    eprintln!("        warm:  {}", fmt_dur(build_bl_warm));
    eprintln!();
    drop(bl_dir);

    let build_sc_cold;
    let build_sc_warm;
    if let Some(ref scc_bin) = find_sccache() {
        let sd = zccache_test_support::temp_cache_dir().unwrap();
        generate_rust_project(sd.path());
        let scd = zccache_test_support::temp_cache_dir().unwrap();
        let scd_s = scd.path().to_string_lossy().into_owned();
        std::env::set_var("SCCACHE_DIR", &scd_s);
        eprintln!("  [2/3] sccache");
        let _ = std::process::Command::new(scc_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if scd.path().exists() {
            let _ = std::fs::remove_dir_all(scd.path());
            let _ = std::fs::create_dir_all(scd.path());
        }
        let _ = std::process::Command::new(scc_bin)
            .arg("--start-server")
            .env("SCCACHE_DIR", &scd_s)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        warmup_rustc(&rc, sd.path());
        let c = run_sccache_rustc_batch(scc_bin, &rc, sd.path(), &srcs, rustc_args_for);
        eprintln!("        cold:  {}", fmt_dur(c));
        build_sc_cold = Some(c);
        let mut t = Vec::with_capacity(RUSTC_WARM_TRIALS);
        for _ in 0..RUSTC_WARM_TRIALS {
            t.push(run_sccache_rustc_batch(
                scc_bin,
                &rc,
                sd.path(),
                &srcs,
                rustc_args_for,
            ));
        }
        print_trials("warm:", &t);
        build_sc_warm = Some(t);
        let _ = std::process::Command::new(scc_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        std::env::remove_var("SCCACHE_DIR");
        eprintln!();
    } else {
        eprintln!("  [2/3] sccache: not found, skipping\n");
        build_sc_cold = None;
        build_sc_warm = None;
    }

    let zd = zccache_test_support::temp_cache_dir().unwrap();
    generate_rust_project(zd.path());
    let zc = zd.path().to_string_lossy().into_owned();
    eprintln!("  [3/3] zccache");
    let (ep, sh, sd) = start_daemon().await;
    let mut cl = zccache_ipc::connect(&ep).await.unwrap();
    cl.send(&Request::SessionStart {
        client_pid: std::process::id(),
        working_dir: zc.clone().into(),
        log_file: None,
        track_stats: true,
        journal_path: None,
    })
    .await
    .unwrap();
    let sid = match cl.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };
    warmup_rustc(&rc, zd.path());
    let build_zc_cold =
        run_zccache_rustc_batch(&mut cl, &sid, &rc, &zc, &srcs, rustc_args_for).await;
    eprintln!("        cold:  {}", fmt_dur(build_zc_cold));
    let mut build_zc_warm = Vec::with_capacity(RUSTC_WARM_TRIALS);
    for _ in 0..RUSTC_WARM_TRIALS {
        build_zc_warm
            .push(run_zccache_rustc_batch(&mut cl, &sid, &rc, &zc, &srcs, rustc_args_for).await);
    }
    print_trials("warm:", &build_zc_warm);
    eprintln!();

    // ─── Check mode (--emit=dep-info,metadata) ──────────────────────────
    eprintln!("  ─── Check mode (cargo check) ───");
    eprintln!();

    let bl_dir2 = zccache_test_support::temp_cache_dir().unwrap();
    generate_rust_project(bl_dir2.path());
    eprintln!("  [1/3] Bare rustc");
    warmup_rustc(&rc, bl_dir2.path());
    let check_bl_cold = run_rustc_batch(&rc, bl_dir2.path(), &srcs, rustc_check_args_for);
    eprintln!("        cold:  {}", fmt_dur(check_bl_cold));
    let check_bl_warm = run_rustc_batch(&rc, bl_dir2.path(), &srcs, rustc_check_args_for);
    eprintln!("        warm:  {}", fmt_dur(check_bl_warm));
    eprintln!();
    drop(bl_dir2);

    let check_sc_cold;
    let check_sc_warm;
    if let Some(ref scc_bin) = find_sccache() {
        let sd = zccache_test_support::temp_cache_dir().unwrap();
        generate_rust_project(sd.path());
        let scd = zccache_test_support::temp_cache_dir().unwrap();
        let scd_s = scd.path().to_string_lossy().into_owned();
        std::env::set_var("SCCACHE_DIR", &scd_s);
        eprintln!("  [2/3] sccache");
        let _ = std::process::Command::new(scc_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if scd.path().exists() {
            let _ = std::fs::remove_dir_all(scd.path());
            let _ = std::fs::create_dir_all(scd.path());
        }
        let _ = std::process::Command::new(scc_bin)
            .arg("--start-server")
            .env("SCCACHE_DIR", &scd_s)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        warmup_rustc(&rc, sd.path());
        let c = run_sccache_rustc_batch(scc_bin, &rc, sd.path(), &srcs, rustc_check_args_for);
        eprintln!("        cold:  {}", fmt_dur(c));
        check_sc_cold = Some(c);
        let mut t = Vec::with_capacity(RUSTC_WARM_TRIALS);
        for _ in 0..RUSTC_WARM_TRIALS {
            t.push(run_sccache_rustc_batch(
                scc_bin,
                &rc,
                sd.path(),
                &srcs,
                rustc_check_args_for,
            ));
        }
        print_trials("warm:", &t);
        check_sc_warm = Some(t);
        let _ = std::process::Command::new(scc_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        std::env::remove_var("SCCACHE_DIR");
        eprintln!();
    } else {
        eprintln!("  [2/3] sccache: not found, skipping\n");
        check_sc_cold = None;
        check_sc_warm = None;
    }

    // Reuse zccache daemon — clear cache for fresh check-mode measurement
    cl.send(&Request::Clear).await.unwrap();
    let _ = cl.recv::<Response>().await;
    generate_rust_project(zd.path());
    eprintln!("  [3/3] zccache");
    warmup_rustc(&rc, zd.path());
    let check_zc_cold =
        run_zccache_rustc_batch(&mut cl, &sid, &rc, &zc, &srcs, rustc_check_args_for).await;
    eprintln!("        cold:  {}", fmt_dur(check_zc_cold));
    let mut check_zc_warm = Vec::with_capacity(RUSTC_WARM_TRIALS);
    for _ in 0..RUSTC_WARM_TRIALS {
        check_zc_warm.push(
            run_zccache_rustc_batch(&mut cl, &sid, &rc, &zc, &srcs, rustc_check_args_for).await,
        );
    }
    print_trials("warm:", &check_zc_warm);

    cl.send(&Request::SessionEnd { session_id: sid })
        .await
        .unwrap();
    let _ = cl.recv::<Response>().await;
    sd.notify_one();
    sh.await.unwrap();

    // ── Results table ──────────────────────────────────────────────────
    let dash = "\u{2014}";
    let build_zm = median(&build_zc_warm);
    let check_zm = median(&check_zc_warm);

    eprintln!();
    eprintln!("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}");
    eprintln!("  RESULTS");
    eprintln!("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}");
    eprintln!();
    eprintln!("## Rust Benchmark: {RUSTC_NUM_FILES} .rs files, {RUSTC_WARM_TRIALS} warm trials");
    eprintln!();
    eprintln!("| Scenario | Bare rustc | sccache | zccache | vs sccache | vs bare rustc |");
    eprintln!("|:---------|----------:|--------:|--------:|-----------:|--------------:|");

    // Helper closure for table rows
    let row = |label: &str, bl: Duration, sc: Option<Duration>, zc: Duration, bold: bool| {
        let sc_s = sc.map(fmt_dur);
        let sc_str = sc_s.as_deref().unwrap_or(dash);
        let vs_sc = sc.map(|s| fmt_ratio(s, zc, bold));
        let vs_bl = fmt_ratio(bl, zc, bold);
        let zc_fmt = if bold {
            format!("**{}**", fmt_dur(zc))
        } else {
            fmt_dur(zc)
        };
        eprintln!(
            "| {} | {} | {} | {} | {} | {} |",
            label,
            fmt_dur(bl),
            sc_str,
            zc_fmt,
            vs_sc.as_deref().unwrap_or(dash),
            vs_bl
        );
    };

    row(
        "Build, Cold",
        build_bl_cold,
        build_sc_cold,
        build_zc_cold,
        false,
    );
    row(
        "Build, Warm",
        build_bl_warm,
        build_sc_warm.as_ref().map(|t| median(t)),
        build_zm,
        true,
    );
    row(
        "Check, Cold",
        check_bl_cold,
        check_sc_cold,
        check_zc_cold,
        false,
    );
    row(
        "Check, Warm",
        check_bl_warm,
        check_sc_warm.as_ref().map(|t| median(t)),
        check_zm,
        true,
    );

    eprintln!();
    eprintln!("> **Build** = `--emit=dep-info,metadata,link` (cargo build). **Check** = `--emit=dep-info,metadata` (cargo check).");
    eprintln!("> **Cold** = first compile (empty cache). **Warm** = median of {RUSTC_WARM_TRIALS} subsequent runs.");

    eprintln!();
    eprintln!("### Bottom Line");
    eprintln!();
    let bld_vs_rc = build_bl_warm.as_secs_f64() / build_zm.as_secs_f64();
    let chk_vs_rc = check_bl_warm.as_secs_f64() / check_zm.as_secs_f64();
    if let Some(ref t) = build_sc_warm {
        let bld_vs_sc = median(t).as_secs_f64() / build_zm.as_secs_f64();
        eprintln!("  Build warm:  {bld_vs_rc:.1}x faster than bare rustc, {bld_vs_sc:.1}x faster than sccache");
    } else {
        eprintln!("  Build warm:  {bld_vs_rc:.1}x faster than bare rustc");
    }
    if let Some(ref t) = check_sc_warm {
        let chk_vs_sc = median(t).as_secs_f64() / check_zm.as_secs_f64();
        eprintln!("  Check warm:  {chk_vs_rc:.1}x faster than bare rustc, {chk_vs_sc:.1}x faster than sccache");
    } else {
        eprintln!("  Check warm:  {chk_vs_rc:.1}x faster than bare rustc");
    }
    eprintln!();
}

// ═════════════════════════════════════════════════════════════════════════════
// Sibling git workspace + ZCCACHE_PATH_REMAP=auto benchmarks.
//
// These benchmarks measure warm-state compile latency when zccache shares cache
// entries across two sibling git roots via path-remap auto. Bare and sccache
// run their normal same-workspace warm trials in workspace B (they cannot
// share across sibling roots). zccache is primed from workspace A, then warm
// trials measure compiles in workspace B that should hit the sibling cache.
// ═════════════════════════════════════════════════════════════════════════════

fn make_git_workspace(dir: &Path) {
    std::fs::create_dir_all(dir.join(".git")).unwrap();
}

fn path_remap_auto_env() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(key, value)| {
            let key = key.into_string().ok()?;
            let value = value.into_string().ok()?;
            let is_zccache_root = key.eq_ignore_ascii_case("ZCCACHE_WORKTREE_ROOT");
            let is_zccache_remap = key.eq_ignore_ascii_case("ZCCACHE_PATH_REMAP");
            (!is_zccache_root && !is_zccache_remap).then_some((key, value))
        })
        .collect();
    env.push(("ZCCACHE_PATH_REMAP".to_string(), "auto".to_string()));
    env
}

async fn zccache_compile_cpp_single_with_env(
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

async fn start_zccache_session(client: &mut ClientConn, working_dir: &str) -> String {
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: working_dir.into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
        })
        .await
        .unwrap();
    match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    }
}

async fn end_zccache_session(client: &mut ClientConn, session_id: String) {
    client
        .send(&Request::SessionEnd { session_id })
        .await
        .unwrap();
    let _ = client.recv::<Response>().await;
}

fn absolute_cpp_source_names(dir: &Path) -> Vec<String> {
    (0..NUM_FILES)
        .map(|i| {
            dir.join(format!("unit_{i:03}.cpp"))
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

struct CppSiblingRemapResult {
    scenario: &'static str,
    bare_warm: Duration,
    sccache_warm: Option<Vec<Duration>>,
    zccache_warm: Vec<Duration>,
}

async fn measure_cpp_sibling_remap_mode(
    scenario: &'static str,
    compiler: &str,
    with_file_tags: bool,
    use_absolute_sources: bool,
) -> CppSiblingRemapResult {
    eprintln!("  Mode: {scenario}");
    eprintln!();

    let parent = zccache_test_support::temp_cache_dir().unwrap();
    let workspace_a = parent.path().join("workspace-a");
    let workspace_b = parent.path().join("workspace-b");
    std::fs::create_dir_all(&workspace_a).unwrap();
    std::fs::create_dir_all(&workspace_b).unwrap();
    make_git_workspace(&workspace_a);
    make_git_workspace(&workspace_b);
    if with_file_tags {
        generate_project_with_file_tags(&workspace_a);
        generate_project_with_file_tags(&workspace_b);
    } else {
        generate_project(&workspace_a);
        generate_project(&workspace_b);
    }

    let sources_a = if use_absolute_sources {
        absolute_cpp_source_names(&workspace_a)
    } else {
        source_names()
    };
    let sources_b = if use_absolute_sources {
        absolute_cpp_source_names(&workspace_b)
    } else {
        source_names()
    };

    eprintln!("  [1/3] Bare clang (workspace B, warm)");
    warmup_compiler(compiler, &workspace_b);
    let _ = baseline_single(compiler, &workspace_b, &sources_b);
    let mut bl_warm = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        bl_warm.push(baseline_single(compiler, &workspace_b, &sources_b));
    }
    print_trials_per("warm:", &bl_warm, Some(NUM_FILES));
    eprintln!();

    let sccache_warm = if let Some(sccache_bin) = find_sccache() {
        let sc_cache_dir = zccache_test_support::temp_cache_dir().unwrap();
        let sc_cache_str = sc_cache_dir.path().to_string_lossy().into_owned();
        std::env::set_var("SCCACHE_DIR", &sc_cache_str);
        eprintln!("  [2/3] sccache (prime: workspace A, warm: workspace B)");
        let mut warm = Vec::with_capacity(WARM_TRIALS);
        for trial in 0..WARM_TRIALS {
            if with_file_tags || trial == 0 {
                let _ = std::process::Command::new(&sccache_bin)
                    .arg("--stop-server")
                    .env("SCCACHE_DIR", &sc_cache_str)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                if with_file_tags {
                    clear_dir_contents(sc_cache_dir.path());
                }
                let _ = std::process::Command::new(&sccache_bin)
                    .arg("--start-server")
                    .env("SCCACHE_DIR", &sc_cache_str)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                warmup_compiler(compiler, &workspace_a);
                let _ = sccache_compile_single(&sccache_bin, compiler, &workspace_a, &sources_a);
            }
            warm.push(sccache_compile_single(
                &sccache_bin,
                compiler,
                &workspace_b,
                &sources_b,
            ));
        }
        print_trials_per("warm:", &warm, Some(NUM_FILES));
        let _ = std::process::Command::new(&sccache_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        std::env::remove_var("SCCACHE_DIR");
        eprintln!();
        Some(warm)
    } else {
        eprintln!("  [2/3] sccache: not found, skipping\n");
        None
    };

    eprintln!("  [3/3] zccache (prime: workspace A, warm: workspace B, remap=auto)");
    let zccache_cache_dir = zccache_test_support::temp_cache_dir().unwrap();
    let _zccache_cache_guard = EnvVarGuard::set_path(
        zccache_core::config::CACHE_DIR_ENV,
        zccache_cache_dir.path(),
    );
    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    let workspace_a_str = workspace_a.to_string_lossy().into_owned();
    let workspace_b_str = workspace_b.to_string_lossy().into_owned();
    let session_a = start_zccache_session(&mut client, &workspace_a_str).await;

    warmup_compiler(compiler, &workspace_a);
    let _ = zccache_compile_cpp_single_with_env(
        &mut client,
        &session_a,
        compiler,
        &workspace_a_str,
        &sources_a,
        path_remap_auto_env(),
    )
    .await;
    end_zccache_session(&mut client, session_a).await;

    let session_b = start_zccache_session(&mut client, &workspace_b_str).await;
    let mut zc_warm = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_warm.push(
            zccache_compile_cpp_single_with_env(
                &mut client,
                &session_b,
                compiler,
                &workspace_b_str,
                &sources_b,
                path_remap_auto_env(),
            )
            .await,
        );
    }
    print_trials_per("warm:", &zc_warm, Some(NUM_FILES));

    end_zccache_session(&mut client, session_b).await;
    shutdown.notify_one();
    server_handle.await.unwrap();
    eprintln!();

    CppSiblingRemapResult {
        scenario,
        bare_warm: median(&bl_warm),
        sccache_warm,
        zccache_warm: zc_warm,
    }
}

/// C++ sibling-workspace remap benchmark. Warm-only. Compares zccache (with
/// ZCCACHE_PATH_REMAP=auto, primed from sibling workspace A) against bare clang
/// and sccache (also primed from workspace A, then measured in workspace B).
#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- perf_cpp_sibling_remap_warm --nocapture --ignored
async fn perf_cpp_sibling_remap_warm() {
    let compiler_path = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no C++ compiler found");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  C++ SIBLING-WORKSPACE REMAP BENCHMARK (warm-only)");
    eprintln!("  {NUM_FILES} .cpp files | {WARM_TRIALS} warm trials | ZCCACHE_PATH_REMAP=auto");
    eprintln!("  Compiler: {compiler}");
    eprintln!("================================================================");
    eprintln!();

    let no_file = measure_cpp_sibling_remap_mode(
        "Sibling-workspace no __FILE__, Warm",
        &compiler,
        false,
        false,
    )
    .await;
    let with_file = measure_cpp_sibling_remap_mode(
        "Sibling-workspace with __FILE__, Warm",
        &compiler,
        true,
        true,
    )
    .await;
    let results = [no_file, with_file];

    let dash = "\u{2014}";
    eprintln!();
    eprintln!(
        "## C++ Sibling-Workspace Remap Benchmark: {NUM_FILES} .cpp files, {WARM_TRIALS} warm trials"
    );
    eprintln!();
    eprintln!("| Scenario | Bare clang | sccache | zccache | vs sccache | vs bare clang |");
    eprintln!("|:---------|----------:|--------:|--------:|-----------:|--------------:|");
    for result in &results {
        let zc_warm_med = median(&result.zccache_warm);
        let sccache_warm_str = result.sccache_warm.as_ref().map(|t| fmt_dur(median(t)));
        let vs_sccache = result
            .sccache_warm
            .as_ref()
            .map(|t| fmt_ratio(median(t), zc_warm_med, true));
        let vs_bare = fmt_ratio(result.bare_warm, zc_warm_med, true);
        eprintln!(
            "| {} | {} | {} | **{}** | {} | {} |",
            result.scenario,
            fmt_dur(result.bare_warm),
            sccache_warm_str.as_deref().unwrap_or(dash),
            fmt_dur(zc_warm_med),
            vs_sccache.as_deref().unwrap_or(dash),
            vs_bare,
        );
    }
    eprintln!();
    eprintln!(
        "> Sibling-workspace = two adjacent git roots; sccache and zccache are primed from workspace A, then warm trials are measured in workspace B. The `with __FILE__` row compiles absolute source paths so each sibling root is embedded in preprocessed output."
    );
    eprintln!();
}

/// Rust sibling-workspace remap benchmark. Warm-only. Compares zccache (with
/// ZCCACHE_PATH_REMAP=auto, primed from sibling workspace A) against bare rustc
/// and sccache (each warm in workspace B).
#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- perf_rustc_sibling_remap_warm --nocapture --ignored
async fn perf_rustc_sibling_remap_warm() {
    let rustc_path = match zccache_test_support::find_rustc() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: rustc not found");
            return;
        }
    };
    let rc = rustc_path.to_string_lossy().to_string();
    let srcs = rust_source_names();

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  RUST SIBLING-WORKSPACE REMAP BENCHMARK (warm-only)");
    eprintln!(
        "  {RUSTC_NUM_FILES} .rs files | {RUSTC_WARM_TRIALS} warm trials | ZCCACHE_PATH_REMAP=auto"
    );
    eprintln!("  Compiler: {rc}");
    eprintln!("================================================================");
    eprintln!();

    let parent = zccache_test_support::temp_cache_dir().unwrap();
    let workspace_a = parent.path().join("workspace-a");
    let workspace_b = parent.path().join("workspace-b");
    std::fs::create_dir_all(&workspace_a).unwrap();
    std::fs::create_dir_all(&workspace_b).unwrap();
    make_git_workspace(&workspace_a);
    make_git_workspace(&workspace_b);
    generate_rust_project(&workspace_a);
    generate_rust_project(&workspace_b);

    // ── Bare rustc warm in workspace B ─────────────────────────────────
    eprintln!("  [1/3] Bare rustc (workspace B, warm)");
    warmup_rustc(&rc, &workspace_b);
    let _ = run_rustc_batch(&rc, &workspace_b, &srcs, rustc_args_for); // discard cold
    let mut bl_warm = Vec::with_capacity(RUSTC_WARM_TRIALS);
    for _ in 0..RUSTC_WARM_TRIALS {
        bl_warm.push(run_rustc_batch(&rc, &workspace_b, &srcs, rustc_args_for));
    }
    print_trials_per("warm:", &bl_warm, Some(RUSTC_NUM_FILES));
    eprintln!();

    // ── sccache warm in workspace B ────────────────────────────────────
    let sccache_warm = if let Some(scc_bin) = find_sccache() {
        let scd = zccache_test_support::temp_cache_dir().unwrap();
        let scd_s = scd.path().to_string_lossy().into_owned();
        std::env::set_var("SCCACHE_DIR", &scd_s);
        eprintln!("  [2/3] sccache (workspace B, warm)");
        let _ = std::process::Command::new(&scc_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::process::Command::new(&scc_bin)
            .arg("--start-server")
            .env("SCCACHE_DIR", &scd_s)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        warmup_rustc(&rc, &workspace_b);
        let _ = run_sccache_rustc_batch(&scc_bin, &rc, &workspace_b, &srcs, rustc_args_for);
        let mut warm = Vec::with_capacity(RUSTC_WARM_TRIALS);
        for _ in 0..RUSTC_WARM_TRIALS {
            warm.push(run_sccache_rustc_batch(
                &scc_bin,
                &rc,
                &workspace_b,
                &srcs,
                rustc_args_for,
            ));
        }
        print_trials_per("warm:", &warm, Some(RUSTC_NUM_FILES));
        let _ = std::process::Command::new(&scc_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        std::env::remove_var("SCCACHE_DIR");
        eprintln!();
        Some(warm)
    } else {
        eprintln!("  [2/3] sccache: not found, skipping\n");
        None
    };

    // ── zccache primed from workspace A, warm in workspace B ───────────
    eprintln!("  [3/3] zccache (prime: workspace A, warm: workspace B, remap=auto)");
    let (ep, sh, sd) = start_daemon().await;
    let mut cl = zccache_ipc::connect(&ep).await.unwrap();
    let workspace_a_str = workspace_a.to_string_lossy().into_owned();
    let workspace_b_str = workspace_b.to_string_lossy().into_owned();

    let session_a = start_zccache_session(&mut cl, &workspace_a_str).await;
    warmup_rustc(&rc, &workspace_a);
    let _ = run_zccache_rustc_batch_with_env(
        &mut cl,
        &session_a,
        &rc,
        &workspace_a_str,
        &srcs,
        rustc_args_for,
        path_remap_auto_env(),
    )
    .await;
    end_zccache_session(&mut cl, session_a).await;

    let session_b = start_zccache_session(&mut cl, &workspace_b_str).await;
    // First compile in B should hit sibling cache entries from A.
    let _ = run_zccache_rustc_batch_with_env(
        &mut cl,
        &session_b,
        &rc,
        &workspace_b_str,
        &srcs,
        rustc_args_for,
        path_remap_auto_env(),
    )
    .await;
    let mut zc_warm = Vec::with_capacity(RUSTC_WARM_TRIALS);
    for _ in 0..RUSTC_WARM_TRIALS {
        zc_warm.push(
            run_zccache_rustc_batch_with_env(
                &mut cl,
                &session_b,
                &rc,
                &workspace_b_str,
                &srcs,
                rustc_args_for,
                path_remap_auto_env(),
            )
            .await,
        );
    }
    print_trials_per("warm:", &zc_warm, Some(RUSTC_NUM_FILES));

    end_zccache_session(&mut cl, session_b).await;
    sd.notify_one();
    sh.await.unwrap();

    // ── Report ─────────────────────────────────────────────────────────
    let dash = "\u{2014}";
    let bl_med = median(&bl_warm);
    let zc_med = median(&zc_warm);
    let sccache_warm_str = sccache_warm.as_ref().map(|t| fmt_dur(median(t)));
    let vs_sccache = sccache_warm
        .as_ref()
        .map(|t| fmt_ratio(median(t), zc_med, true));
    let vs_bare = fmt_ratio(bl_med, zc_med, true);

    eprintln!();
    eprintln!(
        "## Rust Sibling-Workspace Remap Benchmark: {RUSTC_NUM_FILES} .rs files, {RUSTC_WARM_TRIALS} warm trials"
    );
    eprintln!();
    eprintln!("| Scenario | Bare rustc | sccache | zccache | vs sccache | vs bare rustc |");
    eprintln!("|:---------|----------:|--------:|--------:|-----------:|--------------:|");
    eprintln!(
        "| Sibling-workspace, Warm | {} | {} | **{}** | {} | {} |",
        fmt_dur(bl_med),
        sccache_warm_str.as_deref().unwrap_or(dash),
        fmt_dur(zc_med),
        vs_sccache.as_deref().unwrap_or(dash),
        vs_bare,
    );
    eprintln!();
    eprintln!(
        "> Sibling-workspace = two adjacent git roots; zccache primed from workspace A, warm trials measured in workspace B with `ZCCACHE_PATH_REMAP=auto`. Bare/sccache run their normal same-workspace warm trials in workspace B."
    );
    eprintln!();
}

async fn run_zccache_rustc_batch_with_env(
    client: &mut ClientConn,
    sid: &str,
    rc: &str,
    cwd: &str,
    srcs: &[String],
    args_fn: fn(&str, &str, &str) -> Vec<String>,
    env: Vec<(String, String)>,
) -> Duration {
    clean_rlibs(Path::new(cwd));
    let deps = Path::new(cwd).join("deps");
    let deps_s = deps.to_string_lossy().to_string();
    let start = Instant::now();
    for (i, src) in srcs.iter().enumerate() {
        let cn = format!("unit_{i:03}");
        let args = args_fn(&cn, src, &deps_s);
        client
            .send(&Request::Compile {
                session_id: sid.to_string(),
                args,
                cwd: cwd.into(),
                compiler: rc.to_string().into(),
                env: Some(env.clone()),
            })
            .await
            .unwrap();
        match client.recv().await.unwrap() {
            Some(Response::CompileResult { exit_code, .. }) => {
                assert_eq!(exit_code, 0, "zccache rustc failed for {src}")
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }
    }
    start.elapsed()
}

// ═════════════════════════════════════════════════════════════════════════════
// Emscripten (emcc/em++) benchmarks.
//
// emcc/em++ are detected by zccache as Clang-family, so the C++ compile flow
// applies as-is. These benchmarks exercise the Emscripten suite end-to-end
// (single-file + multi-file warm-cache compile) and verify path-remap auto
// works across sibling git worktrees.
// ═════════════════════════════════════════════════════════════════════════════

/// Emscripten warm-cache benchmark: bare em++ vs sccache vs zccache.
/// Mirrors `perf_warm_cache_zccache_vs_sccache` (C++) but uses em++.
#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- perf_emcc_warm_cache_zccache_vs_sccache --nocapture --ignored
async fn perf_emcc_warm_cache_zccache_vs_sccache() {
    let compiler_path = match find_empp() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: em++ not found (install emsdk and source emsdk_env)");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();
    let sources = source_names();

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  EMSCRIPTEN COMPILATION BENCHMARK");
    eprintln!("  {NUM_FILES} .cpp files | {WARM_TRIALS} warm trials");
    eprintln!("  Compiler: {compiler}");
    eprintln!("================================================================");
    eprintln!();

    // ── Bare em++ ────────────────────────────────────────────────────
    let bl_dir = zccache_test_support::temp_cache_dir().unwrap();
    generate_project(bl_dir.path());

    eprintln!("  [1/3] Bare em++ (baseline)");
    nuke_and_regenerate(bl_dir.path());
    warmup_compiler(&compiler, bl_dir.path());
    let bl_cold_single = baseline_single(&compiler, bl_dir.path(), &sources);
    eprintln!("        single cold:  {}", fmt_dur(bl_cold_single));
    let bl_warm_single = baseline_single(&compiler, bl_dir.path(), &sources);
    eprintln!("        single warm:  {}", fmt_dur(bl_warm_single));

    nuke_and_regenerate(bl_dir.path());
    warmup_compiler(&compiler, bl_dir.path());
    let bl_cold_multi = baseline_multi(&compiler, bl_dir.path(), &sources);
    eprintln!("        multi cold:   {}", fmt_dur(bl_cold_multi));
    let bl_warm_multi = baseline_multi(&compiler, bl_dir.path(), &sources);
    eprintln!("        multi warm:   {}", fmt_dur(bl_warm_multi));
    eprintln!();
    drop(bl_dir);

    // ── sccache em++ ──────────────────────────────────────────────────
    let sccache_cold_single;
    let sccache_warm_single;
    let sccache_cold_multi;
    let sccache_warm_multi;
    if let Some(sccache_bin) = find_sccache() {
        let sc_dir = zccache_test_support::temp_cache_dir().unwrap();
        generate_project(sc_dir.path());

        let sc_cache_dir = zccache_test_support::temp_cache_dir().unwrap();
        let sc_cache_str = sc_cache_dir.path().to_string_lossy().into_owned();
        std::env::set_var("SCCACHE_DIR", &sc_cache_str);

        eprintln!("  [2/3] sccache em++ ({})", sccache_bin.display());

        let stop_purge_start = |sccache: &Path, cache_dir: &str| {
            let _ = std::process::Command::new(sccache)
                .arg("--stop-server")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            if Path::new(cache_dir).exists() {
                let _ = std::fs::remove_dir_all(cache_dir);
                let _ = std::fs::create_dir_all(cache_dir);
            }
            let _ = std::process::Command::new(sccache)
                .arg("--start-server")
                .env("SCCACHE_DIR", cache_dir)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        };

        stop_purge_start(&sccache_bin, &sc_cache_str);
        nuke_and_regenerate(sc_dir.path());
        warmup_compiler(&compiler, sc_dir.path());
        let cold = sccache_compile_single(&sccache_bin, &compiler, sc_dir.path(), &sources);
        eprintln!("        single cold:  {}", fmt_dur(cold));
        sccache_cold_single = Some(cold);
        let mut warm = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            warm.push(sccache_compile_single(
                &sccache_bin,
                &compiler,
                sc_dir.path(),
                &sources,
            ));
        }
        print_trials("single warm:", &warm);
        sccache_warm_single = Some(warm);

        stop_purge_start(&sccache_bin, &sc_cache_str);
        nuke_and_regenerate(sc_dir.path());
        warmup_compiler(&compiler, sc_dir.path());
        let cold = sccache_compile_multi(&sccache_bin, &compiler, sc_dir.path(), &sources);
        eprintln!("        multi cold:   {}", fmt_dur(cold));
        sccache_cold_multi = Some(cold);
        let mut warm = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            warm.push(sccache_compile_multi(
                &sccache_bin,
                &compiler,
                sc_dir.path(),
                &sources,
            ));
        }
        print_trials("multi warm:", &warm);
        sccache_warm_multi = Some(warm);

        let _ = std::process::Command::new(&sccache_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        std::env::remove_var("SCCACHE_DIR");
        eprintln!();
    } else {
        eprintln!("  [2/3] sccache: not found, skipping\n");
        sccache_cold_single = None;
        sccache_warm_single = None;
        sccache_cold_multi = None;
        sccache_warm_multi = None;
    }

    // ── zccache em++ ──────────────────────────────────────────────────
    let zc_dir = zccache_test_support::temp_cache_dir().unwrap();
    generate_project(zc_dir.path());
    let zc_cwd = zc_dir.path().to_string_lossy().into_owned();

    eprintln!("  [3/3] zccache em++");
    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let session_id = start_zccache_session(&mut client, &zc_cwd).await;

    nuke_and_regenerate(zc_dir.path());
    warmup_compiler(&compiler, zc_dir.path());
    let zc_cold_single =
        zccache_compile_single(&mut client, &session_id, &compiler, &zc_cwd, &sources).await;
    eprintln!("        single cold:  {}", fmt_dur(zc_cold_single));
    let mut zc_warm_single = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_warm_single.push(
            zccache_compile_single(&mut client, &session_id, &compiler, &zc_cwd, &sources).await,
        );
    }
    print_trials("single warm:", &zc_warm_single);

    nuke_and_regenerate(zc_dir.path());
    warmup_compiler(&compiler, zc_dir.path());
    let zc_cold_multi =
        zccache_compile_multi(&mut client, &session_id, &compiler, &zc_cwd, &sources).await;
    eprintln!("        multi cold:   {}", fmt_dur(zc_cold_multi));
    let mut zc_warm_multi = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_warm_multi.push(
            zccache_compile_multi(&mut client, &session_id, &compiler, &zc_cwd, &sources).await,
        );
    }
    print_trials("multi warm:", &zc_warm_multi);

    end_zccache_session(&mut client, session_id).await;
    shutdown.notify_one();
    server_handle.await.unwrap();

    // ── Report ────────────────────────────────────────────────────────
    let dash = "\u{2014}";
    let zc_single_med = median(&zc_warm_single);
    let zc_multi_med = median(&zc_warm_multi);
    let sc_warm_single_str = sccache_warm_single.as_ref().map(|t| fmt_dur(median(t)));
    let sc_warm_multi_str = sccache_warm_multi.as_ref().map(|t| fmt_dur(median(t)));
    let sc_cold_single_str = sccache_cold_single.map(fmt_dur);
    let sc_cold_multi_str = sccache_cold_multi.map(fmt_dur);
    let vs_sccache_cold_single = sccache_cold_single.map(|d| fmt_ratio(d, zc_cold_single, false));
    let vs_sccache_cold_multi = sccache_cold_multi.map(|d| fmt_ratio(d, zc_cold_multi, false));
    let vs_sccache_warm_single = sccache_warm_single
        .as_ref()
        .map(|t| fmt_ratio(median(t), zc_single_med, true));
    let vs_sccache_warm_multi = sccache_warm_multi
        .as_ref()
        .map(|t| fmt_ratio(median(t), zc_multi_med, true));

    eprintln!();
    eprintln!("## Emscripten Benchmark: {NUM_FILES} .cpp files, {WARM_TRIALS} warm trials");
    eprintln!();
    eprintln!("| Scenario | Bare em++ | sccache | zccache | vs sccache | vs bare em++ |");
    eprintln!("|:---------|---------:|--------:|--------:|-----------:|-------------:|");
    eprintln!(
        "| Single-file, Cold | {} | {} | {} | {} | {} |",
        fmt_dur(bl_cold_single),
        sc_cold_single_str.as_deref().unwrap_or(dash),
        fmt_dur(zc_cold_single),
        vs_sccache_cold_single.as_deref().unwrap_or(dash),
        fmt_ratio(bl_cold_single, zc_cold_single, false),
    );
    eprintln!(
        "| Single-file, Warm | {} | {} | **{}** | {} | {} |",
        fmt_dur(bl_warm_single),
        sc_warm_single_str.as_deref().unwrap_or(dash),
        fmt_dur(zc_single_med),
        vs_sccache_warm_single.as_deref().unwrap_or(dash),
        fmt_ratio(bl_warm_single, zc_single_med, true),
    );
    eprintln!(
        "| Multi-file, Cold | {} | {} | {} | {} | {} |",
        fmt_dur(bl_cold_multi),
        sc_cold_multi_str.as_deref().unwrap_or(dash),
        fmt_dur(zc_cold_multi),
        vs_sccache_cold_multi.as_deref().unwrap_or(dash),
        fmt_ratio(bl_cold_multi, zc_cold_multi, false),
    );
    eprintln!(
        "| Multi-file, Warm | {} | {} | **{}** | {} | {} |",
        fmt_dur(bl_warm_multi),
        sc_warm_multi_str.as_deref().unwrap_or(dash),
        fmt_dur(zc_multi_med),
        vs_sccache_warm_multi.as_deref().unwrap_or(dash),
        fmt_ratio(bl_warm_multi, zc_multi_med, true),
    );
    eprintln!();
    eprintln!("> **Cold** = first compile (empty cache). **Warm** = median of {WARM_TRIALS} subsequent runs.");
    eprintln!();
}

/// Emscripten sibling-workspace remap benchmark. Warm-only. Verifies
/// `ZCCACHE_PATH_REMAP=auto` injects `-ffile-prefix-map` for em++ (Clang-family)
/// so equivalent compiles share cache across sibling git roots.
#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- perf_emcc_sibling_remap_warm --nocapture --ignored
async fn perf_emcc_sibling_remap_warm() {
    let compiler_path = match find_empp() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: em++ not found (install emsdk and source emsdk_env)");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();
    let sources = source_names();

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  EMSCRIPTEN SIBLING-WORKSPACE REMAP BENCHMARK (warm-only)");
    eprintln!("  {NUM_FILES} .cpp files | {WARM_TRIALS} warm trials | ZCCACHE_PATH_REMAP=auto");
    eprintln!("  Compiler: {compiler}");
    eprintln!("================================================================");
    eprintln!();

    let parent = zccache_test_support::temp_cache_dir().unwrap();
    let workspace_a = parent.path().join("workspace-a");
    let workspace_b = parent.path().join("workspace-b");
    std::fs::create_dir_all(&workspace_a).unwrap();
    std::fs::create_dir_all(&workspace_b).unwrap();
    make_git_workspace(&workspace_a);
    make_git_workspace(&workspace_b);
    generate_project(&workspace_a);
    generate_project(&workspace_b);

    // ── Bare em++ warm in workspace B ─────────────────────────────────
    eprintln!("  [1/3] Bare em++ (workspace B, warm)");
    warmup_compiler(&compiler, &workspace_b);
    let _ = baseline_single(&compiler, &workspace_b, &sources);
    let mut bl_warm = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        bl_warm.push(baseline_single(&compiler, &workspace_b, &sources));
    }
    print_trials_per("warm:", &bl_warm, Some(NUM_FILES));
    eprintln!();

    // ── sccache em++ warm in workspace B ──────────────────────────────
    let sccache_warm = if let Some(sccache_bin) = find_sccache() {
        let sc_cache_dir = zccache_test_support::temp_cache_dir().unwrap();
        let sc_cache_str = sc_cache_dir.path().to_string_lossy().into_owned();
        std::env::set_var("SCCACHE_DIR", &sc_cache_str);
        eprintln!("  [2/3] sccache em++ (workspace B, warm)");
        let _ = std::process::Command::new(&sccache_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::process::Command::new(&sccache_bin)
            .arg("--start-server")
            .env("SCCACHE_DIR", &sc_cache_str)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        warmup_compiler(&compiler, &workspace_b);
        let _ = sccache_compile_single(&sccache_bin, &compiler, &workspace_b, &sources);
        let mut warm = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            warm.push(sccache_compile_single(
                &sccache_bin,
                &compiler,
                &workspace_b,
                &sources,
            ));
        }
        print_trials_per("warm:", &warm, Some(NUM_FILES));
        let _ = std::process::Command::new(&sccache_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        std::env::remove_var("SCCACHE_DIR");
        eprintln!();
        Some(warm)
    } else {
        eprintln!("  [2/3] sccache: not found, skipping\n");
        None
    };

    // ── zccache primed from workspace A, warm in workspace B ──────────
    eprintln!("  [3/3] zccache em++ (prime: workspace A, warm: workspace B, remap=auto)");
    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    let workspace_a_str = workspace_a.to_string_lossy().into_owned();
    let workspace_b_str = workspace_b.to_string_lossy().into_owned();
    let session_a = start_zccache_session(&mut client, &workspace_a_str).await;
    warmup_compiler(&compiler, &workspace_a);
    let _ = zccache_compile_cpp_single_with_env(
        &mut client,
        &session_a,
        &compiler,
        &workspace_a_str,
        &sources,
        path_remap_auto_env(),
    )
    .await;
    end_zccache_session(&mut client, session_a).await;

    let session_b = start_zccache_session(&mut client, &workspace_b_str).await;
    let _ = zccache_compile_cpp_single_with_env(
        &mut client,
        &session_b,
        &compiler,
        &workspace_b_str,
        &sources,
        path_remap_auto_env(),
    )
    .await;
    let mut zc_warm = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_warm.push(
            zccache_compile_cpp_single_with_env(
                &mut client,
                &session_b,
                &compiler,
                &workspace_b_str,
                &sources,
                path_remap_auto_env(),
            )
            .await,
        );
    }
    print_trials_per("warm:", &zc_warm, Some(NUM_FILES));

    end_zccache_session(&mut client, session_b).await;
    shutdown.notify_one();
    server_handle.await.unwrap();

    // ── Report ────────────────────────────────────────────────────────
    let dash = "\u{2014}";
    let bl_warm_med = median(&bl_warm);
    let zc_warm_med = median(&zc_warm);
    let sccache_warm_str = sccache_warm.as_ref().map(|t| fmt_dur(median(t)));
    let vs_sccache = sccache_warm
        .as_ref()
        .map(|t| fmt_ratio(median(t), zc_warm_med, true));
    let vs_bare = fmt_ratio(bl_warm_med, zc_warm_med, true);

    eprintln!();
    eprintln!(
        "## Emscripten Sibling-Workspace Remap Benchmark: {NUM_FILES} .cpp files, {WARM_TRIALS} warm trials"
    );
    eprintln!();
    eprintln!("| Scenario | Bare em++ | sccache | zccache | vs sccache | vs bare em++ |");
    eprintln!("|:---------|---------:|--------:|--------:|-----------:|-------------:|");
    eprintln!(
        "| Sibling-workspace, Warm | {} | {} | **{}** | {} | {} |",
        fmt_dur(bl_warm_med),
        sccache_warm_str.as_deref().unwrap_or(dash),
        fmt_dur(zc_warm_med),
        vs_sccache.as_deref().unwrap_or(dash),
        vs_bare,
    );
    eprintln!();
    eprintln!(
        "> Sibling-workspace = two adjacent git roots; zccache primed from workspace A, warm trials measured in workspace B with `ZCCACHE_PATH_REMAP=auto`. Bare/sccache run their normal same-workspace warm trials in workspace B."
    );
    eprintln!();
}

#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- perf_c_archive_link --nocapture --ignored
async fn perf_c_archive_link() {
    let archiver = match find_archiver() {
        Some(path) => path,
        None => {
            eprintln!("SKIP: neither ar nor llvm-ar found on PATH");
            return;
        }
    };
    let output = "libzccache_link_bench.a".to_string();
    let outputs = vec![output.clone()];

    let bare_dir = zccache_test_support::temp_cache_dir().unwrap();
    let sccache_dir = zccache_test_support::temp_cache_dir().unwrap();
    let zccache_dir = zccache_test_support::temp_cache_dir().unwrap();
    let objects = prepare_fake_archive_inputs(bare_dir.path());
    prepare_fake_archive_inputs(sccache_dir.path());
    prepare_fake_archive_inputs(zccache_dir.path());
    let args = archive_link_args(&output, &objects);

    if let Err(error) = try_run_tool(&archiver, &args, bare_dir.path(), "probe ar rcsD") {
        eprintln!(
            "SKIP: archiver does not support deterministic archive benchmark\n{}",
            error
        );
        return;
    }
    clean_link_outputs(bare_dir.path(), &outputs);

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  C STATIC-LIBRARY LINK BENCHMARK");
    eprintln!("  {NUM_FILES} .o inputs | {WARM_TRIALS} warm trials");
    eprintln!("  Archiver: {}", archiver.display());
    eprintln!("================================================================");
    eprintln!();

    let result = measure_ephemeral_link_scenario(
        "Static archive",
        &archiver,
        &args,
        &outputs,
        bare_dir.path(),
        sccache_dir.path(),
        zccache_dir.path(),
    )
    .await;
    print_link_benchmark_table(
        &format!(
            "## C Static-Library Link Benchmark: {NUM_FILES} .o inputs, {WARM_TRIALS} warm trials"
        ),
        "Bare ar",
        &[result],
    );
}

#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- perf_cpp_driver_link --nocapture --ignored
async fn perf_cpp_driver_link() {
    let compiler_path = match zccache_test_support::find_clang() {
        Some(path) => path,
        None => {
            eprintln!("SKIP: no C++ compiler found");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();
    let output = bench_exe_name("cpp_link_app");
    let outputs = vec![output.clone()];

    let bare_dir = zccache_test_support::temp_cache_dir().unwrap();
    let sccache_dir = zccache_test_support::temp_cache_dir().unwrap();
    let zccache_dir = zccache_test_support::temp_cache_dir().unwrap();
    let objects = match prepare_cpp_link_inputs(&compiler, bare_dir.path()) {
        Ok(objects) => objects,
        Err(error) => {
            eprintln!("SKIP: failed to prepare C++ link inputs\n{error}");
            return;
        }
    };
    if let Err(error) = prepare_cpp_link_inputs(&compiler, sccache_dir.path()) {
        eprintln!("SKIP: failed to prepare C++ sccache link inputs\n{error}");
        return;
    }
    if let Err(error) = prepare_cpp_link_inputs(&compiler, zccache_dir.path()) {
        eprintln!("SKIP: failed to prepare C++ zccache link inputs\n{error}");
        return;
    }
    let args = driver_link_args(&output, &objects);
    if let Err(error) = try_run_tool(
        Path::new(&compiler),
        &args,
        bare_dir.path(),
        "probe C++ link",
    ) {
        eprintln!("SKIP: C++ compiler-driver link is not available\n{error}");
        return;
    }
    clean_link_outputs(bare_dir.path(), &outputs);

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  C++ DRIVER-LINK BENCHMARK");
    eprintln!("  {NUM_FILES} .cpp objects + main.o | {WARM_TRIALS} warm trials");
    eprintln!("  Compiler: {compiler}");
    eprintln!("================================================================");
    eprintln!();

    let result = measure_ephemeral_link_scenario(
        "Driver link",
        Path::new(&compiler),
        &args,
        &outputs,
        bare_dir.path(),
        sccache_dir.path(),
        zccache_dir.path(),
    )
    .await;
    print_link_benchmark_table(
        &format!(
            "## C++ Driver-Link Benchmark: {NUM_FILES} .cpp objects, {WARM_TRIALS} warm trials"
        ),
        "Bare clang++",
        &[result],
    );
}

#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- perf_emcc_link --nocapture --ignored
async fn perf_emcc_link() {
    let compiler_path = match find_empp() {
        Some(path) => path,
        None => {
            eprintln!("SKIP: em++ not found (install emsdk and source emsdk_env)");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();

    let bare_dir = zccache_test_support::temp_cache_dir().unwrap();
    let sccache_dir = zccache_test_support::temp_cache_dir().unwrap();
    let zccache_dir = zccache_test_support::temp_cache_dir().unwrap();
    let objects = match prepare_cpp_link_inputs(&compiler, bare_dir.path()) {
        Ok(objects) => objects,
        Err(error) => {
            eprintln!("SKIP: failed to prepare Emscripten link inputs\n{error}");
            return;
        }
    };
    if let Err(error) = prepare_cpp_link_inputs(&compiler, sccache_dir.path()) {
        eprintln!("SKIP: failed to prepare Emscripten sccache link inputs\n{error}");
        return;
    }
    if let Err(error) = prepare_cpp_link_inputs(&compiler, zccache_dir.path()) {
        eprintln!("SKIP: failed to prepare Emscripten zccache link inputs\n{error}");
        return;
    }

    let html_output = "em_link_app.html".to_string();
    let wasm_output = "em_link_app.wasm".to_string();
    let html_outputs = vec![html_output.clone()];
    let wasm_outputs = vec![wasm_output.clone()];
    let html_args = driver_link_args(&html_output, &objects);
    let wasm_args = driver_link_args(&wasm_output, &objects);
    if let Err(error) = try_run_tool(
        Path::new(&compiler),
        &html_args,
        bare_dir.path(),
        "probe em++ html link",
    ) {
        eprintln!("SKIP: Emscripten HTML link is not available\n{error}");
        return;
    }
    clean_link_outputs(bare_dir.path(), &html_outputs);
    if let Err(error) = try_run_tool(
        Path::new(&compiler),
        &wasm_args,
        bare_dir.path(),
        "probe em++ wasm link",
    ) {
        eprintln!("SKIP: Emscripten Wasm link is not available\n{error}");
        return;
    }
    clean_link_outputs(bare_dir.path(), &wasm_outputs);

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  EMSCRIPTEN LINK BENCHMARK");
    eprintln!("  {NUM_FILES} .cpp objects + main.o | {WARM_TRIALS} warm trials");
    eprintln!("  Compiler: {compiler}");
    eprintln!("================================================================");
    eprintln!();

    let html = measure_ephemeral_link_scenario(
        "HTML link",
        Path::new(&compiler),
        &html_args,
        &html_outputs,
        bare_dir.path(),
        sccache_dir.path(),
        zccache_dir.path(),
    )
    .await;
    let wasm = measure_ephemeral_link_scenario(
        "Wasm link",
        Path::new(&compiler),
        &wasm_args,
        &wasm_outputs,
        bare_dir.path(),
        sccache_dir.path(),
        zccache_dir.path(),
    )
    .await;
    print_link_benchmark_table(
        &format!(
            "## Emscripten Link Benchmark: {NUM_FILES} .cpp objects, {WARM_TRIALS} warm trials"
        ),
        "Bare em++",
        &[html, wasm],
    );
}

#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- perf_rust_workspace_link --nocapture --ignored
async fn perf_rust_workspace_link() {
    let rustc_path = match zccache_test_support::find_rustc() {
        Some(path) => path,
        None => {
            eprintln!("SKIP: rustc not found");
            return;
        }
    };
    let rustc = rustc_path.to_string_lossy().to_string();
    let output = rust_final_output_name();
    let args = rust_final_link_args(&output);

    let bare_dir = zccache_test_support::temp_cache_dir().unwrap();
    let sccache_dir = zccache_test_support::temp_cache_dir().unwrap();
    let zccache_dir = zccache_test_support::temp_cache_dir().unwrap();
    if let Err(error) = prepare_rust_link_inputs(&rustc, bare_dir.path()) {
        eprintln!("SKIP: failed to prepare Rust link inputs\n{error}");
        return;
    }
    if let Err(error) = prepare_rust_link_inputs(&rustc, sccache_dir.path()) {
        eprintln!("SKIP: failed to prepare Rust sccache link inputs\n{error}");
        return;
    }
    if let Err(error) = prepare_rust_link_inputs(&rustc, zccache_dir.path()) {
        eprintln!("SKIP: failed to prepare Rust zccache link inputs\n{error}");
        return;
    }
    if let Err(error) = try_run_tool(
        Path::new(&rustc),
        &args,
        bare_dir.path(),
        "probe Rust staticlib link",
    ) {
        eprintln!("SKIP: Rust staticlib link is not available\n{error}");
        return;
    }
    clean_rust_final_output(bare_dir.path(), &output);

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  RUST WORKSPACE LINK BENCHMARK");
    eprintln!("  {RUSTC_NUM_FILES} .rlib inputs | {RUSTC_WARM_TRIALS} warm trials");
    eprintln!("  Compiler: {rustc}");
    eprintln!("================================================================");
    eprintln!();

    eprintln!("  [1/3] Bare rustc");
    let _ = run_rust_final_link_timed(
        Path::new(&rustc),
        &args,
        bare_dir.path(),
        &output,
        "bare Rust link warmup",
    );
    let bare_cold = run_rust_final_link_timed(
        Path::new(&rustc),
        &args,
        bare_dir.path(),
        &output,
        "bare Rust cold link",
    );
    eprintln!("        cold: {}", fmt_dur(bare_cold));
    let mut bare_warm = Vec::with_capacity(RUSTC_WARM_TRIALS);
    for _ in 0..RUSTC_WARM_TRIALS {
        bare_warm.push(run_rust_final_link_timed(
            Path::new(&rustc),
            &args,
            bare_dir.path(),
            &output,
            "bare Rust warm link",
        ));
    }
    print_trials("warm:", &bare_warm);
    eprintln!();

    let (sccache_cold, sccache_warm) = if let Some(sccache_bin) = find_sccache() {
        let sc_cache_dir = zccache_test_support::temp_cache_dir().unwrap();
        let _cache_dir = start_fresh_sccache(&sccache_bin, sc_cache_dir.path());
        eprintln!("  [2/3] sccache ({})", sccache_bin.display());
        let cold = match try_run_sccache_rust_final_link_timed(
            &sccache_bin,
            Path::new(&rustc),
            &args,
            sccache_dir.path(),
            &output,
            "sccache Rust cold link",
        ) {
            Ok(duration) => duration,
            Err(error) => {
                eprintln!(
                    "        sccache Rust link passthrough failed; using direct rustc as no-cache baseline\n        {}",
                    error.lines().next().unwrap_or("unknown failure")
                );
                run_rust_final_link_timed(
                    Path::new(&rustc),
                    &args,
                    sccache_dir.path(),
                    &output,
                    "direct Rust no-cache cold link",
                )
            }
        };
        eprintln!("        cold: {}", fmt_dur(cold));
        let mut passthrough_supported = true;
        let mut warm = Vec::with_capacity(RUSTC_WARM_TRIALS);
        for _ in 0..RUSTC_WARM_TRIALS {
            let duration = if passthrough_supported {
                match try_run_sccache_rust_final_link_timed(
                    &sccache_bin,
                    Path::new(&rustc),
                    &args,
                    sccache_dir.path(),
                    &output,
                    "sccache Rust warm link",
                ) {
                    Ok(duration) => duration,
                    Err(_) => {
                        passthrough_supported = false;
                        run_rust_final_link_timed(
                            Path::new(&rustc),
                            &args,
                            sccache_dir.path(),
                            &output,
                            "direct Rust no-cache warm link",
                        )
                    }
                }
            } else {
                run_rust_final_link_timed(
                    Path::new(&rustc),
                    &args,
                    sccache_dir.path(),
                    &output,
                    "direct Rust no-cache warm link",
                )
            };
            warm.push(duration);
        }
        print_trials("warm:", &warm);
        stop_sccache(&sccache_bin);
        eprintln!();
        (Some(cold), Some(warm))
    } else {
        eprintln!("  [2/3] sccache: not found, skipping");
        eprintln!();
        (None, None)
    };

    eprintln!("  [3/3] zccache");
    let _ = run_rust_final_link_timed(
        Path::new(&rustc),
        &args,
        zccache_dir.path(),
        &output,
        "zccache Rust linker warmup",
    );
    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    clear_zccache(&mut client).await;
    let zccache_cwd = zccache_dir.path().to_string_lossy().into_owned();
    let session_id = start_zccache_session(&mut client, &zccache_cwd).await;
    let zccache_cold = run_zccache_rust_final_link_timed(
        &mut client,
        &session_id,
        Path::new(&rustc),
        &args,
        zccache_dir.path(),
        &output,
        false,
    )
    .await;
    eprintln!("        cold: {}", fmt_dur(zccache_cold));
    let mut zccache_warm = Vec::with_capacity(RUSTC_WARM_TRIALS);
    for _ in 0..RUSTC_WARM_TRIALS {
        zccache_warm.push(
            run_zccache_rust_final_link_timed(
                &mut client,
                &session_id,
                Path::new(&rustc),
                &args,
                zccache_dir.path(),
                &output,
                true,
            )
            .await,
        );
    }
    print_trials("warm:", &zccache_warm);
    end_zccache_session(&mut client, session_id).await;
    shutdown.notify_one();
    server_handle.await.unwrap();

    let result = LinkBenchResult {
        scenario: "Workspace staticlib link",
        bare_cold,
        bare_warm: median(&bare_warm),
        sccache_cold,
        sccache_warm,
        zccache_cold,
        zccache_warm,
    };
    print_link_benchmark_table(
        &format!(
            "## Rust Workspace Link Benchmark: {RUSTC_NUM_FILES} .rlib inputs, {RUSTC_WARM_TRIALS} warm trials"
        ),
        "Bare rustc",
        &[result],
    );
}
