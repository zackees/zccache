//! MSVC / clang-cl invocation classifier.
//!
//! This module handles the **cacheability classification** for compilers
//! that speak MSVC argument syntax (`cl.exe`, `clang-cl.exe`).
//!
//! It is intentionally separate from `zccache-depgraph::msvc_args`, which is
//! the much heavier parser that extracts include search paths, defines, and
//! cache-key inputs for an already-classified compilation.
//!
//! The job here is much narrower:
//!   1. Determine whether the invocation is a compile-only step (has `/c`).
//!   2. Locate the input source file(s).
//!   3. Locate the output object path (`/Fo:` etc.).
//!   4. Reject obvious non-cacheable shapes (link-only, `/E`, `/help`, ...).
//!
//! Both `cl.exe` and `clang-cl.exe` accept many flags with either `/` or `-`
//! prefix; we treat both prefixes identically. We also accept the GCC-style
//! `-o`/`-c` aliases that clang-cl honors so that mixed-style invocations
//! (very common in `cc-rs`) classify correctly.

use std::sync::Arc;

use super::{CacheableCompilation, CompilerFamily, ParsedInvocation};
use zccache_core::NormalizedPath;

/// Source file extensions recognised by the MSVC / clang-cl parser.
///
/// Kept aligned with `zccache_depgraph::msvc_args::is_source_file`. Comparison
/// is case-insensitive because Windows users frequently use `.C`, `.CPP` etc.
const MSVC_SOURCE_EXTENSIONS: &[&str] = &[
    "c", "cc", "cpp", "cxx", "c++", "cppm", "ixx", "m", "mm", "i", "ii", "s", "sx",
];

/// Returns `true` when the argument looks like a flag (either prefix style).
fn is_flag(arg: &str) -> bool {
    arg.starts_with('/') || arg.starts_with('-')
}

/// Strip either `/` or `-` prefix and return the remainder.
fn flag_body(arg: &str) -> &str {
    arg.strip_prefix('/')
        .or_else(|| arg.strip_prefix('-'))
        .unwrap_or(arg)
}

/// Returns `Some(rest)` when `arg` matches `/<head><rest>` or `-<head><rest>`.
fn strip_flag<'a>(arg: &'a str, head: &str) -> Option<&'a str> {
    if let Some(rest) = arg.strip_prefix('/') {
        return rest.strip_prefix(head);
    }
    if let Some(rest) = arg.strip_prefix('-') {
        return rest.strip_prefix(head);
    }
    None
}

/// Returns `true` when `arg` is exactly `/X` or `-X`.
fn is_exact_flag(arg: &str, head: &str) -> bool {
    matches!(arg.strip_prefix('/'), Some(rest) if rest == head)
        || matches!(arg.strip_prefix('-'), Some(rest) if rest == head)
}

/// Detect whether a file path looks like a C/C++ source file.
fn is_msvc_source_file(path: &str) -> bool {
    if let Some(ext) = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
    {
        let lower = ext.to_ascii_lowercase();
        MSVC_SOURCE_EXTENSIONS.contains(&lower.as_str())
    } else {
        false
    }
}

/// Heuristic for detecting MSVC-style flags in an argv list.
///
/// Used by the family dispatcher to fall back to the MSVC classifier when
/// `clang-cl` is invoked with `/c`, `/Fo`, `/std:c++17`, etc.
///
/// Anything matching `/<letter>...` that is not a Unix-style filesystem path
/// (`/usr/include`, `/path/to/foo`) counts as an MSVC flag. We are deliberately
/// conservative: a single `/showIncludes` or `/Fo<x>` is enough.
#[must_use]
pub fn looks_like_msvc_args(args: &[String]) -> bool {
    args.iter().any(|arg| {
        if !arg.starts_with('/') {
            return false;
        }
        // Reject Unix paths like /usr/include or /opt/foo: they contain '/'
        // after the first character. MSVC flags are dense (`/Fo:`, `/EHsc`).
        let rest = &arg[1..];
        if rest.is_empty() {
            return false;
        }
        // First character must be alpha (MSVC flags always start with a letter).
        let first = rest.chars().next().unwrap_or(' ');
        if !first.is_ascii_alphabetic() {
            return false;
        }
        // Reject if it looks like an absolute Unix path: `/abs/...` where the
        // first segment is short and the path keeps going.  We treat
        // `/Foobar:abc` (no extra `/`) as a flag, and `/usr/include` as a path.
        // Concretely: if there's a `/` later in the arg, treat as a path.
        if rest.contains('/') {
            return false;
        }
        true
    })
}

/// Default output for an MSVC-style compile with no explicit `/Fo` flag.
///
/// MSVC emits `<stem>.obj` next to the source by default; we follow the same
/// convention as the GCC path and use `<stem>.obj` in the cwd. This keeps the
/// daemon's output reconstruction logic uniform.
fn msvc_default_output(source: &str) -> String {
    let stem = std::path::Path::new(source)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("a");
    format!("{stem}.obj")
}

/// Parse an MSVC / clang-cl compiler invocation for cacheability.
///
/// `compiler` is the executable path. `family` should be `CompilerFamily::Msvc`
/// or, for `clang-cl`, may be `CompilerFamily::Clang` — both are accepted.
///
/// The caller is expected to have already expanded `@response_files`.
#[must_use]
pub fn parse_msvc_invocation(
    compiler: &str,
    args: &[String],
    family: CompilerFamily,
) -> ParsedInvocation {
    let mut has_c_flag = false;
    let mut output_file: Option<String> = None;
    let mut source_files: Vec<(String, usize)> = Vec::new();
    let mut unknown_flags: Vec<String> = Vec::new();
    // Pending source-language override from /Tc<file> / /Tp<file>.
    // Those flags both name and language-tag the file in one go.

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        // Empty arg — skip (rare, but defensive).
        if arg.is_empty() {
            i += 1;
            continue;
        }

        // ── Non-cacheable invocations ───────────────────────────────────
        // Preprocess-only modes.
        if is_exact_flag(arg, "E") || is_exact_flag(arg, "EP") || is_exact_flag(arg, "P") {
            return ParsedInvocation::NonCacheable {
                reason: format!("preprocessing-only flag: {arg}"),
            };
        }

        // Help / version queries: `/?`, `/help`, `--version`, `--help`,
        // `-v`/`/v` (some build systems probe this).
        if arg == "/?"
            || is_exact_flag(arg, "help")
            || is_exact_flag(arg, "HELP")
            || arg == "--version"
            || arg == "--help"
        {
            return ParsedInvocation::NonCacheable {
                reason: format!("help/version query: {arg}"),
            };
        }

        // ── Compile-only flag (/c or -c). MSVC, clang-cl, and `cc-rs` all
        // pass this verbatim. Both prefixes accepted.
        if is_exact_flag(arg, "c") {
            has_c_flag = true;
            i += 1;
            continue;
        }

        // ── Output object path: `/Fo<path>`, `/Fo:<path>`, `/Fo <path>` ──
        // Both `/Fo` and `-Fo` are honored by clang-cl.
        // Also `/Fobuild/foo.obj` (no separator) is valid for MSVC.
        if let Some(rest) = strip_flag(arg, "Fo") {
            // `/Fo` (next arg), `/Fo:<path>` or `/Fo<path>` (concatenated)
            if rest.is_empty() {
                if let Some(next) = args.get(i + 1) {
                    output_file = Some(next.clone());
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
            let path = rest.strip_prefix(':').unwrap_or(rest);
            output_file = Some(path.to_string());
            i += 1;
            continue;
        }

        // ── Executable / linker outputs (non-cacheable if no /c). ────────
        // /Fe<path> => exe output. We still record the argument so we can
        // detect that this is a link invocation rather than a compile.
        if strip_flag(arg, "Fe").is_some() {
            unknown_flags.push(arg.clone());
            i += 1;
            continue;
        }
        // /Fd<path> => PDB output (debug info). Tracked but otherwise ignored.
        if strip_flag(arg, "Fd").is_some() {
            unknown_flags.push(arg.clone());
            i += 1;
            continue;
        }
        // /Fp<path> => PCH output path. Tracked but ignored for classification.
        if strip_flag(arg, "Fp").is_some() {
            unknown_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // ── GCC-style aliases (clang-cl accepts both prefixes). ──────────
        // `-o <path>` (with space) and `-o<path>` (concatenated). MSVC `cl.exe`
        // does NOT accept `-o`, but clang-cl does.
        if arg == "-o" {
            if let Some(next) = args.get(i + 1) {
                output_file = Some(next.clone());
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if let Some(path) = arg.strip_prefix("-o") {
            if !path.is_empty() {
                output_file = Some(path.to_string());
                i += 1;
                continue;
            }
        }

        // ── Language-override / explicit source flags ────────────────────
        // /Tc<file>     — treat <file> as C source.
        // /Tp<file>     — treat <file> as C++ source.
        // /Tc <file>    — same, space separated.
        // /Tp <file>    — same, space separated.
        // /TC           — treat all following as C.
        // /TP           — treat all following as C++.
        if let Some(rest) = strip_flag(arg, "Tc") {
            if rest.is_empty() {
                if let Some(next) = args.get(i + 1) {
                    source_files.push((next.clone(), i + 1));
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
            source_files.push((rest.to_string(), i));
            i += 1;
            continue;
        }
        if let Some(rest) = strip_flag(arg, "Tp") {
            if rest.is_empty() {
                if let Some(next) = args.get(i + 1) {
                    source_files.push((next.clone(), i + 1));
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
            source_files.push((rest.to_string(), i));
            i += 1;
            continue;
        }
        if is_exact_flag(arg, "TC") || is_exact_flag(arg, "TP") {
            unknown_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // ── Flags that take a value in the *next* argv element ───────────
        // For MSVC, `/D NAME`, `/U NAME`, `/I path`, `/FI <file>` all support
        // the space-separated form. We must consume both elements so the
        // value isn't misclassified as a source file.
        if is_exact_flag(arg, "D")
            || is_exact_flag(arg, "U")
            || is_exact_flag(arg, "I")
            || is_exact_flag(arg, "FI")
        {
            unknown_flags.push(arg.clone());
            if let Some(next) = args.get(i + 1) {
                unknown_flags.push(next.clone());
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }

        // ── Generic flag passthrough ─────────────────────────────────────
        if is_flag(arg) {
            // `/showIncludes`, `/nologo`, `/MD`, `/MT`, `/O2`, `/W4`, etc.
            // All are passed to the compiler via `original_args`; we just
            // track them so they aren't misclassified as sources.
            // Note: starts with `/` *might* be a unix path. Treat anything
            // matching a known shape as a flag, otherwise fall through.
            // We are permissive here — anything starting with `/` that has
            // no extra `/` characters is a flag (Unix paths typically have
            // multiple `/`). This matches `looks_like_msvc_args`.
            let body = flag_body(arg);
            let has_inner_slash = body.contains('/');
            let looks_like_msvc_flag = !has_inner_slash
                && body
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_alphabetic() || c == '?')
                    .unwrap_or(false);
            if arg.starts_with('-') || looks_like_msvc_flag {
                unknown_flags.push(arg.clone());
                i += 1;
                continue;
            }
            // It looked like a flag but contains inner `/` — could be a path.
            // Fall through to source-file detection below.
        }

        // ── Positional argument: source file candidate ───────────────────
        if is_msvc_source_file(arg) {
            source_files.push((arg.clone(), i));
        } else {
            // Unknown positional (e.g., import library, .obj file). For
            // strict accounting, surface these via `unknown_flags` so
            // nothing is silently dropped.
            unknown_flags.push(arg.clone());
        }

        i += 1;
    }

    // ── Classification ──────────────────────────────────────────────────
    if !has_c_flag {
        return ParsedInvocation::NonCacheable {
            reason: "no /c flag (likely a link invocation)".to_string(),
        };
    }
    if source_files.is_empty() {
        return ParsedInvocation::NonCacheable {
            reason: "no source file found in MSVC/clang-cl invocation".to_string(),
        };
    }

    // Multi-file invocations: when `cl.exe` or `clang-cl` are given multiple
    // sources with `/c`, each becomes a separate `.obj`. MSVC names them by
    // stem in the cwd just like gcc.
    if source_files.len() > 1 {
        let source_indices: Vec<usize> = source_files.iter().map(|(_, idx)| *idx).collect();
        let shared_args: Arc<[String]> = Arc::from(args.to_vec());
        let compilations = source_files
            .iter()
            .map(|(src, _)| CacheableCompilation {
                compiler: NormalizedPath::new(compiler),
                family,
                source_file: NormalizedPath::new(src),
                output_file: NormalizedPath::new(msvc_default_output(src)),
                original_args: Arc::clone(&shared_args),
                unknown_flags: unknown_flags.clone(),
            })
            .collect();
        return ParsedInvocation::MultiFile {
            compilations,
            original_args: shared_args,
            source_indices,
        };
    }

    #[expect(
        clippy::expect_used,
        reason = "source_files non-empty: structurally guaranteed by the is_empty() guard earlier in this function and the multi-file early return"
    )]
    let (source, _) = source_files
        .into_iter()
        .next()
        .expect("source_files non-empty: checked above as `if !source_files.is_empty()`");
    let output = output_file.unwrap_or_else(|| msvc_default_output(&source));

    ParsedInvocation::Cacheable(CacheableCompilation {
        compiler: NormalizedPath::new(compiler),
        family,
        source_file: NormalizedPath::new(source),
        output_file: NormalizedPath::new(output),
        original_args: Arc::from(args.to_vec()),
        unknown_flags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    // ── looks_like_msvc_args ────────────────────────────────────────────

    #[test]
    fn looks_like_msvc_detects_slash_c() {
        assert!(looks_like_msvc_args(&args(&["/c", "foo.c"])));
    }

    #[test]
    fn looks_like_msvc_detects_fo() {
        assert!(looks_like_msvc_args(&args(&["/Fo:out.obj", "foo.c"])));
    }

    #[test]
    fn looks_like_msvc_rejects_unix_paths() {
        // /usr/include should NOT be treated as an MSVC flag.
        assert!(!looks_like_msvc_args(&args(&[
            "-c",
            "/usr/include/foo.c",
            "-o",
            "foo.o"
        ])));
    }

    #[test]
    fn looks_like_msvc_rejects_pure_gcc() {
        assert!(!looks_like_msvc_args(&args(&[
            "-c", "foo.c", "-o", "foo.o", "-O2", "-Wall"
        ])));
    }

    #[test]
    fn looks_like_msvc_mixed_dash_and_slash() {
        // Some build systems mix conventions: `-D` plus `/c`.
        assert!(looks_like_msvc_args(&args(&["-DFOO=1", "/c", "foo.c"])));
    }

    // ── Cacheable single-file invocations ───────────────────────────────

    #[test]
    fn basic_clang_cl_compile_with_slash_fo_colon() {
        // The canonical clang-cl shape from the issue.
        let result = parse_msvc_invocation(
            "clang-cl",
            &args(&["/c", "/Fo:hello.obj", "hello.c"]),
            CompilerFamily::Msvc,
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, NormalizedPath::new("hello.c"));
                assert_eq!(c.output_file, NormalizedPath::new("hello.obj"));
                assert_eq!(c.family, CompilerFamily::Msvc);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn slash_fo_concatenated_no_separator() {
        // `/Fohello.obj` (no separator) is valid MSVC syntax.
        let result = parse_msvc_invocation(
            "cl",
            &args(&["/c", "/Fohello.obj", "hello.c"]),
            CompilerFamily::Msvc,
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, NormalizedPath::new("hello.obj"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn slash_fo_space_separated() {
        // `/Fo build/hello.obj` (space) - clang-cl accepts both.
        let result = parse_msvc_invocation(
            "clang-cl",
            &args(&["/c", "/Fo", "build/hello.obj", "hello.c"]),
            CompilerFamily::Msvc,
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, NormalizedPath::new("build/hello.obj"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn dash_fo_alias_accepted() {
        // clang-cl accepts -Fo as well as /Fo.
        let result = parse_msvc_invocation(
            "clang-cl",
            &args(&["-c", "-Fo:hello.obj", "hello.c"]),
            CompilerFamily::Msvc,
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, NormalizedPath::new("hello.obj"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn gcc_style_dash_o_accepted_for_clang_cl() {
        // clang-cl also honors `-o`. Confirm we don't drop it.
        let result = parse_msvc_invocation(
            "clang-cl",
            &args(&["-c", "-o", "hello.obj", "hello.c"]),
            CompilerFamily::Msvc,
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, NormalizedPath::new("hello.obj"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn default_output_when_no_fo() {
        let result =
            parse_msvc_invocation("clang-cl", &args(&["/c", "hello.c"]), CompilerFamily::Msvc);
        match result {
            ParsedInvocation::Cacheable(c) => {
                // Default MSVC output is <stem>.obj
                assert_eq!(c.output_file, NormalizedPath::new("hello.obj"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn cpp_with_ehsc_and_std() {
        let result = parse_msvc_invocation(
            "clang-cl",
            &args(&[
                "/c",
                "/EHsc",
                "/std:c++17",
                "/MD",
                "/W4",
                "/Fo:hello.obj",
                "hello.cpp",
            ]),
            CompilerFamily::Msvc,
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, NormalizedPath::new("hello.cpp"));
                assert_eq!(c.output_file, NormalizedPath::new("hello.obj"));
                // /EHsc, /std:c++17, /MD, /W4 must be preserved.
                assert!(c.unknown_flags.contains(&"/EHsc".to_string()));
                assert!(c.unknown_flags.contains(&"/std:c++17".to_string()));
                assert!(c.unknown_flags.contains(&"/MD".to_string()));
                assert!(c.unknown_flags.contains(&"/W4".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn d_macro_with_value_space_separated() {
        // `/D NAME=VALUE` is consumed as a single 2-element flag — the value
        // must NOT be misclassified as a source file.
        let result = parse_msvc_invocation(
            "clang-cl",
            &args(&["/c", "/D", "FOO=1", "/Fo:hello.obj", "hello.c"]),
            CompilerFamily::Msvc,
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, NormalizedPath::new("hello.c"));
                assert!(c.unknown_flags.contains(&"/D".to_string()));
                assert!(c.unknown_flags.contains(&"FOO=1".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn i_path_with_space_and_spaces_in_path() {
        // `/I path with spaces` — path is the next arg.
        let result = parse_msvc_invocation(
            "clang-cl",
            &args(&[
                "/c",
                "/I",
                "C:\\Program Files\\include",
                "/Fo:hello.obj",
                "hello.c",
            ]),
            CompilerFamily::Msvc,
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, NormalizedPath::new("hello.c"));
                assert!(c.unknown_flags.contains(&"/I".to_string()));
                assert!(c
                    .unknown_flags
                    .contains(&"C:\\Program Files\\include".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn mixed_dash_and_slash_flags() {
        // Both prefixes coexist in a single invocation (very common in `cc-rs`).
        let result = parse_msvc_invocation(
            "clang-cl",
            &args(&["/c", "-DFOO=1", "/DBAR=2", "/Fo:hello.obj", "hello.c"]),
            CompilerFamily::Msvc,
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, NormalizedPath::new("hello.c"));
                assert!(c.unknown_flags.contains(&"-DFOO=1".to_string()));
                assert!(c.unknown_flags.contains(&"/DBAR=2".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn unknown_slash_flag_does_not_drop_invocation() {
        // /XYZUnknown should be preserved in unknown_flags and the
        // invocation classified, never silently dropped.
        let result = parse_msvc_invocation(
            "clang-cl",
            &args(&["/c", "/XYZUnknown", "/Fo:hello.obj", "hello.c"]),
            CompilerFamily::Msvc,
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert!(c.unknown_flags.contains(&"/XYZUnknown".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn debug_info_flags() {
        let result = parse_msvc_invocation(
            "clang-cl",
            &args(&["/c", "/Zi", "/Fd:vc.pdb", "/Fo:hello.obj", "hello.c"]),
            CompilerFamily::Msvc,
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, NormalizedPath::new("hello.obj"));
                assert!(c.unknown_flags.contains(&"/Zi".to_string()));
                assert!(c.unknown_flags.contains(&"/Fd:vc.pdb".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn show_includes_kept() {
        let result = parse_msvc_invocation(
            "clang-cl",
            &args(&["/c", "/showIncludes", "/Fo:hello.obj", "hello.c"]),
            CompilerFamily::Msvc,
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert!(c.unknown_flags.contains(&"/showIncludes".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // ── /Tc and /Tp ─────────────────────────────────────────────────────

    #[test]
    fn slash_tc_concatenated_source() {
        // `/Tchello.c` — source name is concatenated.
        let result = parse_msvc_invocation(
            "clang-cl",
            &args(&["/c", "/Tchello.c", "/Fo:hello.obj"]),
            CompilerFamily::Msvc,
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, NormalizedPath::new("hello.c"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn slash_tp_space_separated_source() {
        let result = parse_msvc_invocation(
            "clang-cl",
            &args(&["/c", "/Tp", "hello.cpp", "/Fo:hello.obj"]),
            CompilerFamily::Msvc,
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, NormalizedPath::new("hello.cpp"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // ── Multi-file ──────────────────────────────────────────────────────

    #[test]
    fn multi_file_msvc_split() {
        let result = parse_msvc_invocation(
            "clang-cl",
            &args(&["/c", "a.c", "b.c"]),
            CompilerFamily::Msvc,
        );
        match result {
            ParsedInvocation::MultiFile {
                compilations,
                source_indices,
                ..
            } => {
                assert_eq!(compilations.len(), 2);
                assert_eq!(compilations[0].source_file, NormalizedPath::new("a.c"));
                assert_eq!(compilations[0].output_file, NormalizedPath::new("a.obj"));
                assert_eq!(compilations[1].source_file, NormalizedPath::new("b.c"));
                assert_eq!(compilations[1].output_file, NormalizedPath::new("b.obj"));
                assert_eq!(source_indices, vec![1, 2]);
            }
            other => panic!("expected MultiFile, got: {other:?}"),
        }
    }

    // ── Non-cacheable invocations ───────────────────────────────────────

    #[test]
    fn no_slash_c_is_non_cacheable() {
        // No /c => link invocation.
        let result = parse_msvc_invocation(
            "clang-cl",
            &args(&["hello.c", "/Fe:hello.exe"]),
            CompilerFamily::Msvc,
        );
        assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
    }

    #[test]
    fn slash_e_preprocess_only_is_non_cacheable() {
        let result =
            parse_msvc_invocation("clang-cl", &args(&["/E", "hello.c"]), CompilerFamily::Msvc);
        assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
    }

    #[test]
    fn slash_p_preprocess_to_file_is_non_cacheable() {
        let result =
            parse_msvc_invocation("clang-cl", &args(&["/P", "hello.c"]), CompilerFamily::Msvc);
        assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
    }

    #[test]
    fn slash_help_is_non_cacheable() {
        let result = parse_msvc_invocation("clang-cl", &args(&["/?"]), CompilerFamily::Msvc);
        assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
    }

    #[test]
    fn dash_dash_version_is_non_cacheable() {
        // `clang-cl --version` should be classified, not silently dropped.
        let result = parse_msvc_invocation("clang-cl", &args(&["--version"]), CompilerFamily::Msvc);
        assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
    }

    #[test]
    fn no_source_file_is_non_cacheable() {
        let result = parse_msvc_invocation(
            "clang-cl",
            &args(&["/c", "/Fo:foo.obj"]),
            CompilerFamily::Msvc,
        );
        assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
    }

    // ── Original args preserved verbatim ────────────────────────────────

    #[test]
    fn original_args_preserved_for_compiler_fallback() {
        let input = args(&[
            "/c",
            "/EHsc",
            "/std:c++17",
            "/Fo:hello.obj",
            "/DDEBUG=1",
            "hello.cpp",
        ]);
        let result = parse_msvc_invocation("clang-cl", &input, CompilerFamily::Msvc);
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(*c.original_args, *input);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }
}
