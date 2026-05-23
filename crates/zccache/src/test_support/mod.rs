//! Test utilities for zccache.
//!
//! Provides helpers for integration tests, including temp directories,
//! daemon lifecycle management, and test fixtures.

use std::path::Path;
use std::time::{Duration, SystemTime};
use zccache::core::NormalizedPath;

// ─── Tool discovery ─────────────────────────────────────────────────────────

/// Ensure the clang-tool-chain bin directory is on PATH, then find a tool by name.
///
/// The clang-tool-chain installs compilers under `~/.clang-tool-chain/clang/{platform}/{arch}/bin/`.
/// This function prepends that directory to PATH (once per process) so that
/// `find_on_path("clang++")`, `find_on_path("ar")`, etc. work generically
/// without hardcoding platform-specific paths in every test file.
pub fn ensure_clang_tool_chain_on_path() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        if let Some(bin_dir) = clang_tool_chain_bin_dir() {
            if bin_dir.is_dir() {
                // Prepend to PATH
                let path_var = std::env::var_os("PATH").unwrap_or_default();
                let mut paths = vec![bin_dir];
                paths.extend(std::env::split_paths(&path_var).map(Into::into));
                let new_path = std::env::join_paths(paths).unwrap();
                std::env::set_var("PATH", &new_path);
            }
        }
    });
}

/// Returns the clang-tool-chain bin directory for the current platform, if it exists.
fn clang_tool_chain_bin_dir() -> Option<NormalizedPath> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;

    let (platform, arch) = if cfg!(target_os = "windows") {
        (
            "win",
            if cfg!(target_arch = "aarch64") {
                "aarch64"
            } else {
                "x86_64"
            },
        )
    } else if cfg!(target_os = "macos") {
        (
            "darwin",
            if cfg!(target_arch = "aarch64") {
                "aarch64"
            } else {
                "x86_64"
            },
        )
    } else {
        (
            "linux",
            if cfg!(target_arch = "aarch64") {
                "aarch64"
            } else {
                "x86_64"
            },
        )
    };

    let dir = NormalizedPath::new(home)
        .join(".clang-tool-chain")
        .join("clang")
        .join(platform)
        .join(arch)
        .join("bin");

    if dir.is_dir() {
        Some(dir)
    } else {
        None
    }
}

/// Find a tool binary on PATH. Returns None if not found.
///
/// Call [`ensure_clang_tool_chain_on_path`] first to make clang-tool-chain
/// binaries discoverable.
pub fn find_on_path(name: &str) -> Option<NormalizedPath> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.into());
        }
        // On Windows, also try with .exe suffix
        #[cfg(windows)]
        if std::path::Path::new(name).extension().is_none() {
            let with_exe = dir.join(format!("{name}.exe"));
            if with_exe.is_file() {
                return Some(with_exe.into());
            }
        }
    }
    None
}

/// Find clang++ via clang-tool-chain + PATH. Convenience wrapper.
///
/// Ensures clang-tool-chain is on PATH, then searches for `clang++`.
pub fn find_clang() -> Option<NormalizedPath> {
    ensure_clang_tool_chain_on_path();
    find_on_path("clang++")
}

/// Find `rustc` on PATH.
pub fn find_rustc() -> Option<NormalizedPath> {
    find_on_path("rustc")
}

// ─── Integration test timeout ───────────────────────────────────────────────

/// Default timeout for integration tests that start a daemon / use IPC.
/// Prevents tests from hanging forever if the daemon doesn't respond.
pub const INTEGRATION_TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Run an async future with a timeout, panicking with a clear message if exceeded.
///
/// Wrap integration test bodies with this to ensure they fail fast instead of
/// hanging indefinitely on a stuck daemon or broken IPC channel.
pub async fn test_timeout<F: std::future::Future<Output = ()>>(f: F) {
    tokio::time::timeout(INTEGRATION_TEST_TIMEOUT, f)
        .await
        .expect("integration test timed out after 30s — daemon may be unresponsive");
}

// ─── Temp directories ───────────────────────────────────────────────────────

/// Create a temporary directory for test artifacts.
///
/// The directory and its contents are deleted when the returned
/// `TempDir` is dropped.
///
/// # Errors
///
/// Returns an error if the temp directory cannot be created.
pub fn temp_cache_dir() -> std::io::Result<tempfile::TempDir> {
    use std::sync::Once;

    const TEMP_PREFIX: &str = "zccache-test-";
    const STALE_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);

    static CLEANUP_ONCE: Once = Once::new();
    CLEANUP_ONCE.call_once(|| cleanup_stale_temp_dirs(TEMP_PREFIX, STALE_AFTER));

    tempfile::Builder::new().prefix(TEMP_PREFIX).tempdir()
}

fn cleanup_stale_temp_dirs(prefix: &str, stale_after: Duration) {
    let root = std::env::temp_dir();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    let now = SystemTime::now();

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with(prefix) {
            continue;
        }

        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }

        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age < stale_after {
            continue;
        }

        let _ = std::fs::remove_dir_all(path);
    }
}

/// Initialize tracing for tests (only installs once).
pub fn init_test_tracing() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter("zccache=trace")
            .with_test_writer()
            .try_init()
            .ok();
    });
}

// ─── Test C++ project generator ─────────────────────────────────────────────

/// Configuration for generating a synthetic C++ project.
///
/// The generated project has a realistic structure: shared headers in
/// `include/`, source files in `src/`, and object files in `obj/`.
/// Each source file includes all shared headers, creating a dependency
/// fan-out that exercises the include scanner and dep graph.
pub struct TestProject {
    /// Number of `.cpp` source files to generate.
    pub source_count: usize,
    /// Number of shared headers included by every source file.
    pub header_count: usize,
    /// Number of "private" headers — each included by only one source file.
    /// Creates deeper include trees without shared-header fan-out.
    pub private_header_count: usize,
    /// Approximate lines of generated code per source file (controls body size).
    /// Higher values produce heavier compilation but more realistic files.
    pub body_weight: BodyWeight,
}

/// Controls how much code each source file contains.
#[derive(Clone, Copy)]
pub enum BodyWeight {
    /// ~20 lines: a function, a struct, a loop. Fast to compile.
    Light,
    /// ~60 lines: templates, multiple functions, hash chains. Medium compile.
    Medium,
    /// ~150 lines: deep template instantiation, multiple structs, heavy math.
    Heavy,
}

impl Default for TestProject {
    fn default() -> Self {
        Self {
            source_count: 50,
            header_count: 5,
            private_header_count: 0,
            body_weight: BodyWeight::Medium,
        }
    }
}

impl TestProject {
    /// Small project for quick integration tests (~30 files, light bodies).
    #[must_use]
    pub fn integration() -> Self {
        Self {
            source_count: 30,
            header_count: 4,
            private_header_count: 2,
            body_weight: BodyWeight::Light,
        }
    }

    /// Medium project for benchmarks (~100 files, medium bodies).
    #[must_use]
    pub fn benchmark() -> Self {
        Self {
            source_count: 100,
            header_count: 8,
            private_header_count: 4,
            body_weight: BodyWeight::Medium,
        }
    }

    /// Large stress-test project (~250 files, heavy bodies).
    #[must_use]
    pub fn stress() -> Self {
        Self {
            source_count: 250,
            header_count: 12,
            private_header_count: 8,
            body_weight: BodyWeight::Heavy,
        }
    }

    /// Generate the project on disk under `root`.
    ///
    /// Creates `include/`, `src/`, and `obj/` directories.
    /// Returns the list of (source_path, object_path) compilation units.
    pub fn generate(&self, root: &Path) -> Vec<(NormalizedPath, NormalizedPath)> {
        let incdir = root.join("include");
        let srcdir = root.join("src");
        let objdir = root.join("obj");
        std::fs::create_dir_all(&incdir).unwrap();
        std::fs::create_dir_all(&srcdir).unwrap();
        std::fs::create_dir_all(&objdir).unwrap();

        self.write_shared_headers(&incdir);
        self.write_private_headers(&incdir);
        self.write_sources(&srcdir, &incdir);

        (0..self.source_count)
            .map(|i| {
                (
                    srcdir.join(format!("unit_{i:04}.cpp")).into(),
                    objdir.join(format!("unit_{i:04}.o")).into(),
                )
            })
            .collect()
    }

    /// Delete all `.o` files in the project (simulates `ninja -t clean`).
    pub fn clean_objects(root: &Path) {
        let objdir = root.join("obj");
        if let Ok(entries) = std::fs::read_dir(&objdir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("o") {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
    }

    /// Compiler flags needed to build this project.
    #[must_use]
    pub fn compiler_flags() -> Vec<&'static str> {
        vec!["-c", "-Iinclude", "-std=c++17"]
    }

    /// Generate a meson-compatible project on disk under `root`.
    ///
    /// Creates:
    /// - `include/` — shared and private headers
    /// - `src/` — source files
    /// - `meson.build` — project definition that builds a static library
    ///
    /// Returns a `MesonProject` with methods to run meson setup and ninja.
    pub fn generate_meson(&self, root: &Path) -> MesonProject {
        let incdir = root.join("include");
        let srcdir = root.join("src");
        std::fs::create_dir_all(&incdir).unwrap();
        std::fs::create_dir_all(&srcdir).unwrap();

        self.write_shared_headers(&incdir);
        self.write_private_headers(&incdir);
        self.write_sources(&srcdir, &incdir);

        // Generate meson.build
        let source_list: String = (0..self.source_count)
            .map(|i| format!("  'src/unit_{i:04}.cpp',\n"))
            .collect();

        let meson_build = format!(
            r#"project('zccache-test', 'cpp',
  version: '1.0.0',
  default_options: ['cpp_std=c++17', 'optimization=0', 'debug=false'],
)

sources = files(
{source_list})

inc = include_directories('include')

static_library('testlib', sources, include_directories: inc)
"#
        );
        std::fs::write(root.join("meson.build"), meson_build).unwrap();

        MesonProject {
            source_dir: root.to_path_buf().into(),
            source_count: self.source_count,
        }
    }

    fn write_shared_headers(&self, incdir: &Path) {
        for h in 0..self.header_count {
            let content = format!(
                r#"#pragma once
#include <cstdint>

namespace shared_{h} {{

inline uint64_t hash_{h}(uint64_t x) {{
    x ^= x >> {shift};
    x *= 0x{magic:016x}ULL;
    x ^= x >> {shift2};
    return x;
}}

template<typename T>
inline T transform_{h}(T val, T offset) {{
    return static_cast<T>(val ^ (offset + {h}));
}}

inline int compute_{h}(int n) {{
    int acc = 0;
    for (int i = 0; i < n; i++) {{
        acc += static_cast<int>(hash_{h}(static_cast<uint64_t>(i)));
    }}
    return acc;
}}

}} // namespace shared_{h}
"#,
                shift = 13 + h,
                shift2 = 17 + h % 7,
                magic = 0xBF58476D1CE4E5B9u64
                    .wrapping_add((h as u64).wrapping_mul(0x1234567890ABCDEFu64)),
            );
            std::fs::write(incdir.join(format!("shared_{h}.h")), content).unwrap();
        }
    }

    fn write_private_headers(&self, incdir: &Path) {
        for h in 0..self.private_header_count {
            let content = format!(
                r#"#pragma once

namespace priv_{h} {{

struct Helper_{h} {{
    int data[4];

    int sum() const {{
        int s = 0;
        for (int i = 0; i < 4; i++) s += data[i];
        return s;
    }}
}};

inline int private_func_{h}(int x) {{
    return x * {factor} + {offset};
}}

}} // namespace priv_{h}
"#,
                factor = 3 + h * 7,
                offset = 42 + h * 13,
            );
            std::fs::write(incdir.join(format!("private_{h}.h")), content).unwrap();
        }
    }

    fn write_sources(&self, srcdir: &Path, _incdir: &Path) {
        for i in 0..self.source_count {
            let content = self.generate_source(i);
            std::fs::write(srcdir.join(format!("unit_{i:04}.cpp")), content).unwrap();
        }
    }

    fn generate_source(&self, index: usize) -> String {
        let mut src = String::with_capacity(4096);

        // Include all shared headers
        for h in 0..self.header_count {
            src.push_str(&format!("#include \"shared_{h}.h\"\n"));
        }

        // Include one private header if available (round-robin)
        if self.private_header_count > 0 {
            let priv_h = index % self.private_header_count;
            src.push_str(&format!("#include \"private_{priv_h}.h\"\n"));
        }

        src.push_str("#include <cmath>\n\n");
        src.push_str(&format!("namespace unit_{index:04} {{\n\n"));

        match self.body_weight {
            BodyWeight::Light => self.write_light_body(&mut src, index),
            BodyWeight::Medium => self.write_medium_body(&mut src, index),
            BodyWeight::Heavy => self.write_heavy_body(&mut src, index),
        }

        src.push_str(&format!("\n}} // namespace unit_{index:04}\n"));
        src
    }

    fn write_light_body(&self, src: &mut String, index: usize) {
        src.push_str(&format!(
            r#"double compute(int n) {{
    double v = std::sin(n * 0.{index:04}1);
    v += shared_0::hash_0(static_cast<uint64_t>(n)) * 1e-18;
    return v;
}}
"#
        ));
    }

    fn write_medium_body(&self, src: &mut String, index: usize) {
        src.push_str(&format!(
            r#"struct Data {{
    int values[16];
    int count;

    int sum() const {{
        int s = 0;
        for (int j = 0; j < count; j++) s += values[j];
        return s;
    }}
}};

double compute(int n) {{
    Data d;
    d.count = n > 16 ? 16 : (n < 0 ? 0 : n);
    for (int j = 0; j < d.count; j++) {{
        d.values[j] = static_cast<int>(shared_0::hash_0(j + {index}ULL));
    }}
    double v = std::sin(d.sum() * 0.{index:04}1);
"#
        ));
        // Reference a few shared headers
        let refs = self.header_count.min(4);
        for h in 0..refs {
            src.push_str(&format!(
                "    v += shared_{h}::hash_{h}(static_cast<uint64_t>(n)) * 1e-18;\n"
            ));
        }
        src.push_str("    return v;\n}\n");
    }

    fn write_heavy_body(&self, src: &mut String, index: usize) {
        // Multiple structs
        src.push_str(&format!(
            r#"struct Config {{
    int iterations;
    double scale;
    uint64_t seed;
}};

struct Accumulator {{
    double values[32];
    int count;

    void add(double v) {{
        if (count < 32) values[count++] = v;
    }}

    double total() const {{
        double s = 0;
        for (int i = 0; i < count; i++) s += values[i];
        return s;
    }}

    double mean() const {{
        return count > 0 ? total() / count : 0.0;
    }}
}};

template<typename T>
T heavy_transform(T x, int depth) {{
    for (int i = 0; i < depth; i++) {{
        x = shared_0::transform_0(x, static_cast<T>(i));
    }}
    return x;
}}

double compute(int n) {{
    Config cfg;
    cfg.iterations = n > 100 ? 100 : (n < 1 ? 1 : n);
    cfg.scale = 0.{index:04}1;
    cfg.seed = {seed}ULL;

    Accumulator acc;
    acc.count = 0;
    for (int i = 0; i < cfg.iterations; i++) {{
        uint64_t h = shared_0::hash_0(cfg.seed + static_cast<uint64_t>(i));
"#,
            seed = 0xDEADBEEFu64.wrapping_add((index as u64).wrapping_mul(0x1111111111111111u64)),
        ));
        // Reference all shared headers
        for h in 1..self.header_count {
            src.push_str(&format!("        h ^= shared_{h}::hash_{h}(h);\n"));
        }
        src.push_str(&format!(
            r#"        double v = std::sin(static_cast<double>(h) * cfg.scale);
        v += heavy_transform(static_cast<int>(h & 0xFF), 3);
        acc.add(v);
    }}
    return acc.mean();
}}

int entry_{index}() {{
    return static_cast<int>(compute(50) * 1000);
}}
"#
        ));
    }
}

// ─── Meson project helper ───────────────────────────────────────────────────

/// A generated meson project with methods to run meson setup and ninja builds.
pub struct MesonProject {
    /// Root directory containing `meson.build` and source files.
    pub source_dir: NormalizedPath,
    /// Number of source files in the project.
    pub source_count: usize,
}

/// Result of a meson+ninja build.
pub struct MesonBuildResult {
    /// Wall-clock time for `meson setup` in milliseconds.
    pub setup_ms: u128,
    /// Wall-clock time for `ninja` in milliseconds.
    pub build_ms: u128,
    /// Total time (setup + build) in milliseconds.
    pub total_ms: u128,
}

/// Compute a PATH value that includes the ninja binary's directory.
fn path_with_ninja(ninja_bin: &Path) -> String {
    let ninja_dir = ninja_bin.parent().unwrap_or(Path::new("."));
    let path_var = std::env::var("PATH").unwrap_or_default();
    format!(
        "{}{}{}",
        ninja_dir.to_string_lossy(),
        if cfg!(windows) { ";" } else { ":" },
        path_var,
    )
}

/// Apply extra environment variables + PATH to a command.
fn apply_env(cmd: &mut std::process::Command, path: &str, extra_env: &[(&str, &str)]) {
    cmd.env("PATH", path);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
}

impl MesonProject {
    /// Write a meson native file that wraps the compiler with a cache tool.
    ///
    /// If `wrapper` is `None`, the compiler is invoked directly (bare mode).
    /// If `wrapper` is `Some(path)`, the compiler is wrapped: `[wrapper, compiler]`.
    ///
    /// The `ar` path is optional — if `None`, the native file omits it and meson
    /// will auto-detect the archiver.
    pub fn write_native_file(
        path: &Path,
        cpp_compiler: &Path,
        ar: Option<&Path>,
        wrapper: Option<&Path>,
    ) {
        // Meson native files use forward slashes even on Windows.
        let cpp = cpp_compiler.to_string_lossy().replace('\\', "/");

        let cpp_entry = match wrapper {
            Some(w) => {
                let w = w.to_string_lossy().replace('\\', "/");
                format!("['{w}', '{cpp}']")
            }
            None => format!("['{cpp}']"),
        };

        // Use the same compiler for C (most C++ compilers handle C too).
        let c_entry = cpp_entry.clone();

        let ar_line = match ar {
            Some(ar_path) => {
                let ar = ar_path.to_string_lossy().replace('\\', "/");
                format!("ar = ['{ar}']\n")
            }
            None => String::new(),
        };

        let system = if cfg!(windows) {
            "windows"
        } else if cfg!(target_os = "macos") {
            "darwin"
        } else {
            "linux"
        };

        let cpu_family = if cfg!(target_arch = "aarch64") {
            "aarch64"
        } else {
            "x86_64"
        };

        let content = format!(
            r#"[binaries]
c = {c_entry}
cpp = {cpp_entry}
{ar_line}
[host_machine]
system = '{system}'
cpu_family = '{cpu_family}'
cpu = '{cpu_family}'
endian = 'little'
"#
        );
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(path, content).unwrap();
    }

    /// Run `meson setup` followed by `ninja`, returning timing results.
    ///
    /// - `build_dir`: where meson places the build tree (created fresh).
    /// - `native_file`: path to the native file (from `write_native_file`).
    /// - `meson_bin`: path to the `meson` executable.
    /// - `ninja_bin`: path to the `ninja` executable.
    /// - `extra_env`: additional environment variables (e.g., `ZCCACHE_ENDPOINT`).
    pub fn build(
        &self,
        build_dir: &Path,
        native_file: &Path,
        meson_bin: &Path,
        ninja_bin: &Path,
        extra_env: &[(&str, &str)],
    ) -> MesonBuildResult {
        // Clean build dir
        if build_dir.exists() {
            std::fs::remove_dir_all(build_dir).unwrap();
        }

        let path = path_with_ninja(ninja_bin);

        // meson setup
        let t0 = std::time::Instant::now();
        let mut cmd = std::process::Command::new(meson_bin);
        cmd.args([
            "setup",
            "--native-file",
            &native_file.to_string_lossy(),
            &build_dir.to_string_lossy(),
        ]);
        cmd.current_dir(&self.source_dir);
        apply_env(&mut cmd, &path, extra_env);
        let output = cmd.output().expect("failed to run meson setup");
        let setup_ms = t0.elapsed().as_millis();

        assert!(
            output.status.success(),
            "meson setup failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );

        // ninja build
        let build_ms = Self::run_ninja(ninja_bin, build_dir, &path, extra_env);

        MesonBuildResult {
            setup_ms,
            build_ms,
            total_ms: t0.elapsed().as_millis(),
        }
    }

    /// Run `ninja -t clean` in the build directory (removes all build outputs).
    pub fn ninja_clean(ninja_bin: &Path, build_dir: &Path) {
        let output = std::process::Command::new(ninja_bin)
            .args(["-C", &build_dir.to_string_lossy(), "-t", "clean"])
            .output()
            .expect("failed to run ninja -t clean");
        assert!(
            output.status.success(),
            "ninja clean failed: {}",
            String::from_utf8_lossy(&output.stderr),
        );
    }

    /// Run a ninja rebuild (after clean), returning the build time in milliseconds.
    pub fn ninja_rebuild(ninja_bin: &Path, build_dir: &Path, extra_env: &[(&str, &str)]) -> u128 {
        let path = path_with_ninja(ninja_bin);
        Self::run_ninja(ninja_bin, build_dir, &path, extra_env)
    }

    /// Internal: run ninja with the given PATH and env, return elapsed ms.
    fn run_ninja(
        ninja_bin: &Path,
        build_dir: &Path,
        path: &str,
        extra_env: &[(&str, &str)],
    ) -> u128 {
        let t = std::time::Instant::now();
        let mut cmd = std::process::Command::new(ninja_bin);
        cmd.args(["-C", &build_dir.to_string_lossy()]);
        apply_env(&mut cmd, path, extra_env);
        let output = cmd.output().expect("failed to run ninja");
        let ms = t.elapsed().as_millis();

        assert!(
            output.status.success(),
            "ninja failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        ms
    }
}
