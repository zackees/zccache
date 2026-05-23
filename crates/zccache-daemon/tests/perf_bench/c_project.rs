//! Synthetic C project generation + bare/sccache/zccache C compile helpers.

use std::path::Path;
use std::time::{Duration, Instant};
use zccache_protocol::{Request, Response};

use super::common::{clean_objects, ClientConn, NUM_FILES};

pub fn c_source_names() -> Vec<String> {
    (0..NUM_FILES).map(|i| format!("unit_{i:03}.c")).collect()
}

pub fn generate_c_project(dir: &Path) {
    let incdir = dir.join("include");
    std::fs::create_dir_all(&incdir).unwrap();

    std::fs::write(
        incdir.join("common_c.h"),
        r#"#pragma once
#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>
#include <string.h>
#include <stdlib.h>
#include <stdio.h>
#include <math.h>
#include <time.h>
#include <errno.h>

static inline uint64_t bench_mix(uint64_t x) {
    x ^= x >> 33;
    x *= 0xff51afd7ed558ccdULL;
    x ^= x >> 33;
    x *= 0xc4ceb9fe1a85ec53ULL;
    x ^= x >> 33;
    return x;
}

static inline size_t bench_strlen(const char *s) {
    return s ? strlen(s) : 0;
}

/* Uses only standard C11 <time.h> APIs (no POSIX clock_gettime),
 * so compilation under -std=c11 without feature-test macros works
 * on Ubuntu's glibc. */
static inline double bench_now(void) {
    struct timespec ts = {0, 0};
    if (timespec_get(&ts, TIME_UTC) == 0) {
        return 0.0;
    }
    return (double)ts.tv_sec + (double)ts.tv_nsec * 1e-9;
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

pub fn nuke_and_regenerate_c(dir: &Path) {
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

pub fn warmup_c_compiler(compiler: &str, dir: &Path) {
    let src = dir.join("unit_000.c");
    let obj = dir.join("_warmup.o");
    let output = std::process::Command::new(compiler)
        .args(["-c", "-Iinclude", "-O2", "-std=c11"])
        .arg(&src)
        .arg("-o")
        .arg(&obj)
        .current_dir(dir)
        .output()
        .expect("C warmup compile failed to spawn");
    assert!(
        output.status.success(),
        "C warmup compile failed: status={:?}\ncompiler={compiler}\ndir={dir:?}\nsrc exists={}\ninclude exists={}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        src.exists(),
        dir.join("include").is_dir(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let _ = std::fs::remove_file(&obj);
}

pub fn baseline_c_single(compiler: &str, cwd: &Path, sources: &[String]) -> Duration {
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

pub fn sccache_compile_c_single(
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

pub async fn zccache_compile_c_single(
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
                stdin: Vec::new(),
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
