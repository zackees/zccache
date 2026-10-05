//! End-to-end coverage for issue #1909: the `ProbeBypass` route used to spawn
//! the compiler in the system temp directory instead of the caller's working
//! directory.
//!
//! `wrap.rs` captures the build directory and then chdir's to the temp dir to
//! release the process's kernel CWD handle on Windows. Every route passes that
//! captured `cwd` on explicitly; the `ProbeBypass` route did not, so
//! `run_passthrough` re-derived the child cwd from `std::env::current_dir()` —
//! by then the temp dir. A probe's relative `-I` and `-o` therefore resolved
//! against the temp dir, and the artifact landed where the caller never looks.
//!
//! The same root cause has a mirror-image symptom: `is_probe_shape` stats the
//! source *after* the chdir, so a relative source (`cc -c probe.c -o probe.o`,
//! the shape meson emits) failed to classify as a probe at all, the bypass
//! silently did not fire, and the invocation fell through to the cached-compile
//! route. Both halves are asserted here:
//!
//!  * `probe_bypass_child_runs_in_the_caller_cwd` — the object lands in the
//!    caller's directory, and *not* in `std::env::temp_dir()`.
//!  * `probe_bypass_recognises_a_relative_source` — a relative source with no
//!    include flags still exits 0. A cwd-blind `is_probe_shape` would miss the
//!    stat, fall through to the cached route, and exit 125 with no daemon.
//!
//! Both tests drive the real `zccache` binary as `zccache <cc> ...` with
//! `ZCCACHE_PROBE_BYPASS=1`, which is the opt-in gate for the route (#625), and
//! give the subprocess its own `ZCCACHE_CACHE_DIR` + namespace so no shared
//! daemon state is touched.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const TEST_DAEMON_NAMESPACE: &str = "probe-bypass-cwd";

/// The `zccache` binary under test.
///
/// `CARGO_BIN_EXE_zccache` rather than a `target/<profile>/zccache` path walk:
/// a path that merely fails to exist would let both tests `return` early and
/// report success, which is exactly the shape that hides a regression. Baked
/// at compile time, so a missing binary is a build error instead of a silent
/// skip (same choice as `cli_no_spawn_guard.rs`).
fn zccache_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zccache"))
}

/// The C compiler to wrap, but only when one is actually present.
///
/// The wrapper resolves the tool on `PATH` at spawn time, so a host without a
/// C compiler has nothing for these tests to observe. Probing first lets them
/// self-skip instead of failing with a confusing "cannot spawn cc". The name is
/// passed through as argv[0] of `zccache` rather than via the `cc`
/// subcommand, so a host that ships only `clang` is still covered.
fn clang_tool() -> Option<PathBuf> {
    ["cc", "clang"]
        .into_iter()
        .map(PathBuf::from)
        .find(|candidate| {
            Command::new(candidate)
                .arg("--version")
                .status()
                .is_ok_and(|status| status.success())
        })
}

/// A build tree plus a private cache dir.
///
/// `include/header.h` is reached only through `-Iinclude`, which is a relative
/// path: if the wrapper ever spawns the child in the temp dir again, the include
/// search path silently no-ops (clang tolerates a missing `-I` directory), and
/// only the `-o` assertion catches it. The conditional include keeps the source
/// compiling for the no-`-I` variant of the same probe.
struct Fixture {
    _root: tempfile::TempDir,
    build_dir: PathBuf,
    cache_dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("zccache-probe-bypass-cwd-")
            .tempdir()
            .expect("build tempdir");
        let build_dir = root.path().join("build");
        std::fs::create_dir_all(build_dir.join("include")).expect("create include dir");
        std::fs::write(
            build_dir.join("include").join("header.h"),
            "int probe_value(void);\n",
        )
        .expect("write header.h");
        std::fs::write(
            build_dir.join("probe.c"),
            // `__has_include` so the same probe source compiles with and
            // without `-Iinclude`; the second test relies on that.
            "#if defined(__has_include)\n\
             #  if __has_include(<header.h>)\n\
             #    include <header.h>\n\
             #  endif\n\
             #endif\n\
             int probe_value(void) { return 1; }\n",
        )
        .expect("write probe.c");
        let cache_dir = tempfile::Builder::new()
            .prefix("zccache-probe-bypass-cwd-cache-")
            .tempdir()
            .expect("cache tempdir");
        Self {
            _root: root,
            build_dir,
            cache_dir,
        }
    }

    fn object_path(&self) -> PathBuf {
        self.build_dir.join("probe.o")
    }
}

/// Run `zccache <tool> <args>` in the build dir on the probe-bypass route.
///
/// `<tool>` is the compiler [`clang_tool`] found, passed as the wrapper's argv[0]
/// rather than through the `cc` subcommand: the subcommand hardcodes `cc`, so
/// using it would make the `clang`-only hosts these tests skip for silently.
fn run_probe(zccache: &Path, tool: &Path, fixture: &Fixture, args: &[&str]) -> Output {
    Command::new(zccache)
        .arg(tool)
        .args(args)
        .current_dir(&fixture.build_dir)
        .env("ZCCACHE_CACHE_DIR", fixture.cache_dir.path())
        .env("ZCCACHE_DAEMON_NAMESPACE", TEST_DAEMON_NAMESPACE)
        .env("ZCCACHE_PROBE_BYPASS", "1")
        // Keep the non-probe routes from silently succeeding: a daemon the
        // wrapper spawns for itself would make the cached-compile fallback a
        // green run and hide both regressions. `ZCCACHE_NO_SPAWN=1` turns that
        // fallback into the daemon-unavailable refusal (125, #1170), which is
        // exactly the outcome the routing assertion distinguishes from 0.
        .env("ZCCACHE_NO_SPAWN", "1")
        // An inherited `ZCCACHE_DISABLE` bypasses routing entirely (it is
        // evaluated before the chdir), and an inherited `ZCCACHE_ENDPOINT` is
        // honoured ahead of the cache dir and would point this test at someone
        // else's daemon. Both would silently change which route runs.
        .env_remove("ZCCACHE_DISABLE")
        .env_remove("ZCCACHE_SESSION_ID")
        .env_remove("ZCCACHE_ENDPOINT")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run zccache wrapper")
}

/// The compiler to wrap, or `None` when this host has no C compiler on `PATH`.
fn tool_or_skip() -> Option<PathBuf> {
    let tool = clang_tool();
    if tool.is_none() {
        eprintln!("skipping: no C compiler (`cc` / `clang`) on PATH");
    }
    tool
}

/// #1909: the probe child runs in the caller's directory, so its relative `-o`
/// lands there — and nowhere near `std::env::temp_dir()`, which is where the
/// wrapper parks its own cwd.
#[test]
#[ignore] // Integration: spawns the real binary + compiler. Run via `./test --integration`.
fn probe_bypass_child_runs_in_the_caller_cwd() {
    let Some(tool) = tool_or_skip() else {
        return;
    };
    let fixture = Fixture::new();

    // Clear any unrelated file at the path the bug would write to, so the
    // negative assertion below cannot pass for the wrong reason.
    let stray = std::env::temp_dir().join("probe.o");
    let _ = std::fs::remove_file(&stray);

    let output = run_probe(
        &zccache_bin(),
        &tool,
        &fixture,
        &["-c", "probe.c", "-Iinclude", "-o", "probe.o"],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "a probe with a relative `-o` must succeed; stdout={:?} stderr={stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        fixture.object_path().exists(),
        "the object must land in the caller's cwd ({}), not wherever the \
         wrapper parked its own cwd (#1909); stderr={stderr}",
        fixture.build_dir.display()
    );
    assert!(
        !stray.exists(),
        "the compiler ran in the temp dir: {} exists, so the probe's relative \
         `-o` resolved against std::env::temp_dir() instead of the caller's \
         tree (#1909); stderr={stderr}",
        stray.display()
    );
}

/// #1909's mirror image: `is_probe_shape` stat'd the source *after* the chdir,
/// so the most common meson probe shape — a relative source — never classified
/// as `ProbeBypass` and the bypass silently did not fire. With no daemon
/// reachable the cached-compile fallback refuses with 125 instead of 0.
#[test]
#[ignore] // Integration: spawns the real binary + compiler. Run via `./test --integration`.
fn probe_bypass_recognises_a_relative_source() {
    let Some(tool) = tool_or_skip() else {
        return;
    };
    let fixture = Fixture::new();

    let output = run_probe(
        &zccache_bin(),
        &tool,
        &fixture,
        &["-c", "probe.c", "-o", "probe.o"],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "a relative source must still classify as a probe: the wrapper has \
         already chdir'd to the temp dir by classify time, so a cwd-blind stat \
         misses the file and falls through to the cached route, which exits \
         125 with no daemon (#1909); stderr={stderr}"
    );
    assert!(
        fixture.object_path().exists(),
        "the probe must produce its object in the caller's cwd; stderr={stderr}"
    );
}