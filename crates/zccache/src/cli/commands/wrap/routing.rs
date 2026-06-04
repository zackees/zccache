//! Pure wrapper routing decisions.

/// Maximum source file size (in bytes) for the probe-bypass heuristic.
/// Real production translation units are typically far larger than this
/// even before preprocessor expansion; meson configure probes are
/// typically 50–500 bytes. 4 KiB is a generous ceiling that catches
/// every observed meson probe shape and excludes essentially every
/// real .c / .cpp unit.
pub(super) const PROBE_SOURCE_MAX_BYTES: u64 = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WrapperRoute {
    Formatter,
    LinkOrArchive,
    Compile,
    /// Issue #625: tiny single-TU compile whose cache overhead exceeds
    /// the compile cost. The wrapper exec's the compiler directly with
    /// **zero IPC** and does not consult the cache.
    ///
    /// Gated on `ZCCACHE_PROBE_BYPASS=1` (opt-in) so the default
    /// behaviour is unchanged for existing users while the heuristic
    /// matures.
    ProbeBypass,
}

pub(super) fn classify_invocation(tool: &str, tool_args: &[String]) -> WrapperRoute {
    if crate::compiler::detect_family(tool).is_formatter() {
        return WrapperRoute::Formatter;
    }

    if crate::compiler::parse_archiver::is_archiver(tool)
        || crate::compiler::parse_linker::is_link_invocation(tool, tool_args)
    {
        return WrapperRoute::LinkOrArchive;
    }

    if probe_bypass_enabled() && is_probe_shape(tool_args) {
        return WrapperRoute::ProbeBypass;
    }

    WrapperRoute::Compile
}

/// Read `ZCCACHE_PROBE_BYPASS` at classify time. Cheap (one syscall on
/// the first call per process, env lookup after) and lets the user toggle
/// without restarting the daemon. Default: disabled — opt-in only until
/// the heuristic has shipped to enough downstream consumers.
fn probe_bypass_enabled() -> bool {
    std::env::var("ZCCACHE_PROBE_BYPASS").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Returns `true` when `tool_args` describes a probe-shaped compile that
/// is cheaper to run directly than to round-trip through the cache.
///
/// **Heuristic (all must hold)**:
///
/// 1. `-c` is present (compile-only — not a driver link).
/// 2. `-o <output>` is present (single output target).
/// 3. Exactly one positional argument is a recognised C / C++ / ObjC
///    source extension and the file exists on disk.
/// 4. No response file (`@rsp`) — production builds use rsps to bypass
///    Windows command-length limits; probes don't.
/// 5. No precompiled-header consumer (`-include-pch` / `-Xclang
///    -include-pch`) — PCH bumps key cost back into the cache's favour.
/// 6. The source file is ≤ [`PROBE_SOURCE_MAX_BYTES`] on disk.
///
/// **Why these and not others**: I deliberately do NOT key on `-I` /
/// `-isystem` flag count. Meson sometimes emits include paths even for
/// probes (e.g. `compiler.check_header`), and a real production TU
/// sometimes has zero -I flags (single-file dev sketches). Source size
/// is the cleanest size-of-work proxy that survives both shapes.
///
/// This function does perform one filesystem stat (size check). The
/// alternative — walking every arg twice and inspecting source content —
/// is more expensive, and the stat is amortised over the IPC roundtrip
/// it lets us skip (~5–10 ms saved vs ~10–50 µs spent).
fn is_probe_shape(tool_args: &[String]) -> bool {
    let mut has_compile_only = false;
    let mut has_output = false;
    let mut sources: Vec<&str> = Vec::with_capacity(1);

    let mut i = 0;
    while let Some(arg) = tool_args.get(i) {
        // Response file → not a probe.
        if arg.starts_with('@') && arg.len() > 1 {
            return false;
        }
        // PCH consumer → cache is net-positive here, don't bypass.
        if arg == "-include-pch" {
            return false;
        }
        if arg == "-Xclang" {
            if let Some(next) = tool_args.get(i + 1) {
                if next == "-include-pch" {
                    return false;
                }
            }
        }
        // Compile-only marker.
        if arg == "-c" {
            has_compile_only = true;
            i += 1;
            continue;
        }
        // Output target. Accept both `-o foo` and `-ofoo` shapes; meson
        // emits the spaced form, MSVC emits `/Fo`.
        if arg == "-o" {
            has_output = true;
            i += 2; // skip output path arg
            continue;
        }
        if let Some(_path) = arg.strip_prefix("-o") {
            has_output = true;
            i += 1;
            continue;
        }
        // Positional source candidate. We deliberately do NOT exclude
        // args starting with '/' here — that would reject every absolute
        // path on Unix (`/tmp/meson-XXX/probe.c`). MSVC-style flags
        // (`/Fo`, `/c`, etc.) don't have C/C++/ObjC source extensions,
        // so the extension check downstream is the real filter.
        if !arg.starts_with('-') && is_source_extension(arg) {
            sources.push(arg.as_str());
            i += 1;
            continue;
        }
        i += 1;
    }

    if !has_compile_only || !has_output || sources.len() != 1 {
        return false;
    }

    // Stat the source file. Cheap on local disk; absent or too-large
    // sources fall back to the cache path.
    let source_path = std::path::Path::new(sources[0]);
    let size = match std::fs::metadata(source_path) {
        Ok(metadata) if metadata.is_file() => metadata.len(),
        _ => return false,
    };
    size <= PROBE_SOURCE_MAX_BYTES
}

fn is_source_extension(path: &str) -> bool {
    // Match the case-insensitive extension list that the standard
    // `parse_invocation` cacheability check covers, minus assembly and
    // CUDA which aren't probe-shaped in practice.
    let lower_ext_matches = |ext: &str| {
        std::path::Path::new(path)
            .extension()
            .and_then(|os| os.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case(ext))
    };
    lower_ext_matches("c")
        || lower_ext_matches("cc")
        || lower_ext_matches("cpp")
        || lower_ext_matches("cxx")
        || lower_ext_matches("c++")
        || lower_ext_matches("m")
        || lower_ext_matches("mm")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    /// Process-global env-var test mutex — `set_var` is process-wide and
    /// any test that reads or writes `ZCCACHE_PROBE_BYPASS` must hold
    /// this lock to avoid racing with sibling tests.
    static BYPASS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_bypass_enabled<R>(f: impl FnOnce() -> R) -> R {
        let _guard = BYPASS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("ZCCACHE_PROBE_BYPASS").ok();
        std::env::set_var("ZCCACHE_PROBE_BYPASS", "1");
        let result = f();
        match prev {
            Some(v) => std::env::set_var("ZCCACHE_PROBE_BYPASS", v),
            None => std::env::remove_var("ZCCACHE_PROBE_BYPASS"),
        }
        result
    }

    fn with_bypass_unset<R>(f: impl FnOnce() -> R) -> R {
        let _guard = BYPASS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("ZCCACHE_PROBE_BYPASS").ok();
        std::env::remove_var("ZCCACHE_PROBE_BYPASS");
        let result = f();
        if let Some(v) = prev {
            std::env::set_var("ZCCACHE_PROBE_BYPASS", v);
        }
        result
    }

    #[test]
    fn routes_rustfmt_to_formatter() {
        assert_eq!(
            classify_invocation("rustfmt", &args(&["src/lib.rs"])),
            WrapperRoute::Formatter
        );
    }

    #[test]
    fn routes_archiver_to_link_or_archive() {
        assert_eq!(
            classify_invocation("ar", &args(&["rcs", "libfoo.a", "foo.o"])),
            WrapperRoute::LinkOrArchive
        );
    }

    #[test]
    fn routes_shared_linker_invocation_to_link_or_archive() {
        assert_eq!(
            classify_invocation("gcc", &args(&["-shared", "foo.o", "-o", "libfoo.so"])),
            WrapperRoute::LinkOrArchive
        );
    }

    #[test]
    fn routes_regular_compiler_invocation_to_compile() {
        assert_eq!(
            classify_invocation("rustc", &args(&["--crate-name", "demo", "src/lib.rs"])),
            WrapperRoute::Compile
        );
    }

    // ── ProbeBypass heuristic ──────────────────────────────────────

    #[test]
    fn probe_bypass_disabled_by_default() {
        with_bypass_unset(|| {
            // Even with a perfect probe shape, no env var = no bypass.
            let dir = tempfile::tempdir().unwrap();
            let probe = dir.path().join("p.c");
            std::fs::write(&probe, b"int main(void) { return 0; }").unwrap();
            let probe_str = probe.to_string_lossy().into_owned();
            assert_eq!(
                classify_invocation("clang", &args(&["-c", probe_str.as_str(), "-o", "p.o"]),),
                WrapperRoute::Compile,
                "default (no env var) must NOT bypass"
            );
        });
    }

    #[test]
    fn probe_bypass_matches_tiny_compile_only_unit() {
        with_bypass_enabled(|| {
            let dir = tempfile::tempdir().unwrap();
            let probe = dir.path().join("probe.c");
            std::fs::write(&probe, b"int main(void) { return 0; }").unwrap();
            let probe_str = probe.to_string_lossy().into_owned();
            assert_eq!(
                classify_invocation("clang", &args(&["-c", probe_str.as_str(), "-o", "probe.o"]),),
                WrapperRoute::ProbeBypass,
            );
        });
    }

    #[test]
    fn probe_bypass_rejects_large_source() {
        with_bypass_enabled(|| {
            let dir = tempfile::tempdir().unwrap();
            let probe = dir.path().join("big.c");
            // 8 KiB > 4 KiB ceiling → not a probe.
            std::fs::write(&probe, vec![b'x'; (PROBE_SOURCE_MAX_BYTES + 1) as usize]).unwrap();
            let probe_str = probe.to_string_lossy().into_owned();
            assert_eq!(
                classify_invocation("clang", &args(&["-c", probe_str.as_str(), "-o", "big.o"]),),
                WrapperRoute::Compile,
            );
        });
    }

    #[test]
    fn probe_bypass_rejects_response_file() {
        with_bypass_enabled(|| {
            let dir = tempfile::tempdir().unwrap();
            let probe = dir.path().join("p.c");
            std::fs::write(&probe, b"int main(void) { return 0; }").unwrap();
            let rsp = dir.path().join("flags.rsp");
            std::fs::write(&rsp, b"-O2").unwrap();
            let probe_str = probe.to_string_lossy().into_owned();
            let rsp_arg = format!("@{}", rsp.to_string_lossy());
            assert_eq!(
                classify_invocation(
                    "clang",
                    &args(&["-c", &rsp_arg, probe_str.as_str(), "-o", "p.o"]),
                ),
                WrapperRoute::Compile,
            );
        });
    }

    #[test]
    fn probe_bypass_rejects_pch_consumer() {
        with_bypass_enabled(|| {
            let dir = tempfile::tempdir().unwrap();
            let probe = dir.path().join("p.c");
            std::fs::write(&probe, b"int main(void) { return 0; }").unwrap();
            let probe_str = probe.to_string_lossy().into_owned();
            assert_eq!(
                classify_invocation(
                    "clang",
                    &args(&[
                        "-c",
                        probe_str.as_str(),
                        "-include-pch",
                        "shared.pch",
                        "-o",
                        "p.o",
                    ]),
                ),
                WrapperRoute::Compile,
            );
            assert_eq!(
                classify_invocation(
                    "clang",
                    &args(&[
                        "-c",
                        probe_str.as_str(),
                        "-Xclang",
                        "-include-pch",
                        "-Xclang",
                        "shared.pch",
                        "-o",
                        "p.o",
                    ]),
                ),
                WrapperRoute::Compile,
            );
        });
    }

    #[test]
    fn probe_bypass_rejects_multiple_sources() {
        with_bypass_enabled(|| {
            let dir = tempfile::tempdir().unwrap();
            let a = dir.path().join("a.c");
            let b = dir.path().join("b.c");
            std::fs::write(&a, b"int x;").unwrap();
            std::fs::write(&b, b"int y;").unwrap();
            assert_eq!(
                classify_invocation(
                    "clang",
                    &args(&[
                        "-c",
                        &a.to_string_lossy(),
                        &b.to_string_lossy(),
                        "-o",
                        "out.o",
                    ]),
                ),
                WrapperRoute::Compile,
            );
        });
    }

    #[test]
    fn probe_bypass_rejects_missing_compile_only_flag() {
        with_bypass_enabled(|| {
            let dir = tempfile::tempdir().unwrap();
            let probe = dir.path().join("p.c");
            std::fs::write(&probe, b"int main(void) { return 0; }").unwrap();
            let probe_str = probe.to_string_lossy().into_owned();
            // No `-c` → this is a link invocation, not a probe.
            assert_eq!(
                classify_invocation("clang", &args(&[probe_str.as_str(), "-o", "p.exe"]),),
                WrapperRoute::LinkOrArchive,
            );
        });
    }

    #[test]
    fn probe_bypass_matches_unix_absolute_source_path() {
        // Regression guard: meson temp-dir probes on Linux/macOS look
        // like `/tmp/.tmpXYZ/probe.c` — absolute paths starting with
        // `/`. An earlier draft of this code excluded args starting
        // with `/` (intending to skip MSVC `/Fo`-style flags) and
        // wrongly rejected every Unix probe. The extension check is
        // the only filter we need.
        with_bypass_enabled(|| {
            let dir = tempfile::tempdir().unwrap();
            let probe = dir.path().join("probe.c");
            std::fs::write(&probe, b"int main(void) { return 0; }").unwrap();
            let abs_probe = std::fs::canonicalize(&probe).unwrap();
            let abs_str = abs_probe.to_string_lossy().into_owned();
            assert_eq!(
                classify_invocation("clang", &args(&["-c", abs_str.as_str(), "-o", "probe.o"]),),
                WrapperRoute::ProbeBypass,
                "absolute source paths (Unix /tmp/... or Windows C:\\...) must be \
                 recognised as positional source args, not skipped as MSVC-style flags"
            );
        });
    }

    #[test]
    fn probe_bypass_rejects_missing_source_on_disk() {
        with_bypass_enabled(|| {
            // The arg names a `.c` file but it doesn't exist — fall back
            // to the cache path rather than guessing.
            assert_eq!(
                classify_invocation(
                    "clang",
                    &args(&["-c", "/no/such/path/probe.c", "-o", "p.o"]),
                ),
                WrapperRoute::Compile,
            );
        });
    }
}
