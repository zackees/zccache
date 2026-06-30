//! Synthetic Rust project generation + rustc / sccache-rustc / zccache-rustc
//! batch runners (with and without an explicit env vec).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::path::Path;
use std::time::{Duration, Instant};
use zccache::protocol::{Request, Response};

use super::common::{ClientConn, RUSTC_NUM_FILES};

pub fn generate_rust_project(dir: &Path) {
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

pub fn rust_source_names() -> Vec<String> {
    (0..RUSTC_NUM_FILES)
        .map(|i| format!("unit_{i:03}.rs"))
        .collect()
}

pub fn clean_rlibs(dir: &Path) {
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

pub fn warmup_rustc(rc: &str, dir: &Path) {
    let src = dir.join("unit_000.rs");
    let deps = dir.join("deps");
    let output = std::process::Command::new(rc)
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
        .output()
        .expect("rustc warmup failed to spawn");
    assert!(
        output.status.success(),
        "rustc warmup failed: status={:?}\nrustc={rc}\ndir={dir:?}\nsrc exists={}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        src.exists(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    clean_rlibs(dir);
}

/// Common rustc args that match what `cargo build` passes.
/// Uses --out-dir (required by sccache), --emit=dep-info,metadata,link,
/// and -C metadata/-C extra-filename for output naming.
pub fn rustc_args_for(cn: &str, src: &str, deps_dir: &str) -> Vec<String> {
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
pub fn rustc_check_args_for(cn: &str, src: &str, deps_dir: &str) -> Vec<String> {
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
pub fn run_rustc_batch(
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

pub fn run_sccache_rustc_batch(
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

pub async fn run_zccache_rustc_batch(
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
                stdin: Vec::new(),
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

pub async fn run_zccache_rustc_batch_with_env(
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
                stdin: Vec::new(),
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
