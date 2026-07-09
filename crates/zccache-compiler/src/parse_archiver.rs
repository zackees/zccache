//! Archiver/static-linker detection and argument parsing for zccache.
//!
//! Handles parsing command-line arguments for `ar`, `llvm-ar`, and MSVC `lib.exe`
//! to determine cacheability and extract cache-relevant information.

use zccache_core::NormalizedPath;

/// Supported archiver tool families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiverFamily {
    /// GNU ar (ar, x86_64-linux-gnu-ar, etc.)
    Ar,
    /// LLVM ar (llvm-ar, llvm-ar-15, etc.)
    LlvmAr,
    /// MSVC lib.exe
    MsvcLib,
}

/// The result of parsing an archiver invocation.
#[derive(Debug, Clone)]
pub enum ParsedArchiveInvocation {
    /// A cacheable archive creation.
    Cacheable(CacheableArchive),
    /// A non-cacheable invocation.
    NonCacheable {
        /// Reason why this invocation is not cacheable.
        reason: String,
    },
}

/// A cacheable archive creation invocation.
#[derive(Debug, Clone)]
pub struct CacheableArchive {
    /// The archiver executable path.
    pub tool: NormalizedPath,
    /// The detected archiver family.
    pub family: ArchiverFamily,
    /// Input object files (order preserved â€” matters for ar).
    pub input_files: Vec<NormalizedPath>,
    /// The output archive file path.
    pub output_file: NormalizedPath,
    /// Flags relevant to cache keying (e.g., "rcs", "rcsD").
    pub cache_relevant_flags: Vec<String>,
    /// The full original argument list (for fallback execution).
    pub original_args: Vec<String>,
    /// Whether non-deterministic output is detected (missing D flag / /BREPRO).
    pub non_deterministic: bool,
}

/// Check if a tool name is a known archiver.
#[must_use]
pub fn is_archiver(tool: &str) -> bool {
    detect_family(tool).is_some()
}

/// Detect the archiver family from the tool path/name.
fn detect_family(tool: &str) -> Option<ArchiverFamily> {
    let name = std::path::Path::new(tool)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(tool);

    // MSVC lib.exe (case-insensitive on Windows)
    if name.eq_ignore_ascii_case("lib") {
        return Some(ArchiverFamily::MsvcLib);
    }

    // llvm-ar, llvm-ar-15, etc. â€” check before plain "ar" to avoid false match
    if name.starts_with("llvm-ar") || name.starts_with("llvm_ar") {
        return Some(ArchiverFamily::LlvmAr);
    }

    // GNU ar: ar, x86_64-linux-gnu-ar, aarch64-linux-gnu-ar, etc.
    // Must end with "ar" (not just contain it â€” "lzma-archiver" is not ar)
    if name == "ar" || name.ends_with("-ar") {
        return Some(ArchiverFamily::Ar);
    }

    None
}

/// Parse an archiver invocation's arguments to determine cacheability.
///
/// Returns a `ParsedArchiveInvocation` indicating whether the invocation is
/// cacheable, and if so, extracts the relevant information.
#[must_use]
pub fn parse_archive_invocation(tool: &str, args: &[String]) -> ParsedArchiveInvocation {
    let family = match detect_family(tool) {
        Some(f) => f,
        None => {
            return ParsedArchiveInvocation::NonCacheable {
                reason: format!("not a recognized archiver: {tool}"),
            };
        }
    };

    match family {
        ArchiverFamily::MsvcLib => parse_msvc_lib(tool, args),
        ArchiverFamily::Ar | ArchiverFamily::LlvmAr => parse_gnu_ar(tool, family, args),
    }
}

/// Parse GNU ar / llvm-ar arguments.
///
/// GNU ar syntax:
///   ar [--plugin name] [-X32_64] [-]operation [relpos] [count] archive [member...]
///
/// We only cache archive creation: operations containing 'r' (replace/insert).
/// Operations containing 'x' (extract), 't' (list), 'd' (delete), 'p' (print)
/// are not cacheable (they read from an archive, not create one).
fn parse_gnu_ar(tool: &str, family: ArchiverFamily, args: &[String]) -> ParsedArchiveInvocation {
    if args.is_empty() {
        return ParsedArchiveInvocation::NonCacheable {
            reason: "no arguments".to_string(),
        };
    }

    // Find the operation string. It's the first arg that doesn't start with '--'.
    // (GNU ar allows `-rcs` or `rcs` â€” the dash prefix is optional.)
    let mut op_idx = 0;
    let mut long_flags = Vec::new();

    // Skip leading long options (--plugin, --target, etc.)
    while op_idx < args.len() && args[op_idx].starts_with("--") {
        long_flags.push(args[op_idx].clone());
        op_idx += 1;
        // Some long options take a value
        if op_idx < args.len()
            && !args[op_idx].starts_with('-')
            && matches!(
                long_flags.last().map(|s| s.as_str()),
                Some("--plugin" | "--target")
            )
        {
            long_flags.push(args[op_idx].clone());
            op_idx += 1;
        }
    }

    if op_idx >= args.len() {
        return ParsedArchiveInvocation::NonCacheable {
            reason: "no operation specified".to_string(),
        };
    }

    let op_str = args[op_idx].strip_prefix('-').unwrap_or(&args[op_idx]);

    // Check for non-cacheable operations
    if op_str.contains('x') {
        return ParsedArchiveInvocation::NonCacheable {
            reason: "extract operation (x) not cacheable".to_string(),
        };
    }
    if op_str.contains('t') {
        return ParsedArchiveInvocation::NonCacheable {
            reason: "list operation (t) not cacheable".to_string(),
        };
    }
    if op_str.contains('d') {
        return ParsedArchiveInvocation::NonCacheable {
            reason: "delete operation (d) not cacheable".to_string(),
        };
    }
    if op_str.contains('p') {
        return ParsedArchiveInvocation::NonCacheable {
            reason: "print operation (p) not cacheable".to_string(),
        };
    }

    // We only cache 'r' (replace/insert) or 'q' (quick append, for fresh archives)
    if !op_str.contains('r') && !op_str.contains('q') {
        return ParsedArchiveInvocation::NonCacheable {
            reason: format!("unsupported operation: {op_str}"),
        };
    }

    // Non-determinism check: 'D' flag enables deterministic mode (zero UIDs, timestamps)
    let non_deterministic = !op_str.contains('D');

    // After the operation, next arg is the archive name, then member files.
    // But some modifiers consume extra positional args:
    //   'a', 'b', 'i' â†’ next arg is relpos (a member name for positioning)
    let has_relpos = op_str.contains('a') || op_str.contains('b') || op_str.contains('i');
    // 'N' â†’ next arg is count
    let has_count = op_str.contains('N');

    let mut pos = op_idx + 1;

    // Skip relpos if present
    if has_relpos {
        pos += 1;
    }
    // Skip count if present
    if has_count {
        pos += 1;
    }

    if pos >= args.len() {
        return ParsedArchiveInvocation::NonCacheable {
            reason: "no archive file specified".to_string(),
        };
    }

    let output_file = NormalizedPath::new(&args[pos]);
    pos += 1;

    // Remaining args are input member files
    let input_files: Vec<NormalizedPath> = args[pos..].iter().map(NormalizedPath::from).collect();

    if input_files.is_empty() {
        return ParsedArchiveInvocation::NonCacheable {
            reason: "no input files specified".to_string(),
        };
    }

    let mut cache_relevant_flags = vec![op_str.to_string()];
    cache_relevant_flags.extend(long_flags);

    ParsedArchiveInvocation::Cacheable(CacheableArchive {
        tool: NormalizedPath::new(tool),
        family,
        input_files,
        output_file,
        cache_relevant_flags,
        original_args: args.to_vec(),
        non_deterministic,
    })
}

/// Parse MSVC lib.exe arguments.
///
/// lib.exe syntax:
///   lib [options] [/OUT:filename] [objfiles...] [libraries...]
///
/// Options start with `/` or `-`. Input files are positional.
fn parse_msvc_lib(tool: &str, args: &[String]) -> ParsedArchiveInvocation {
    if args.is_empty() {
        return ParsedArchiveInvocation::NonCacheable {
            reason: "no arguments".to_string(),
        };
    }

    let mut output_file: Option<NormalizedPath> = None;
    let mut input_files: Vec<NormalizedPath> = Vec::new();
    let mut cache_relevant_flags: Vec<String> = Vec::new();
    let mut is_extract = false;
    let mut has_brepro = false;
    let mut has_list = false;

    for arg in args {
        let upper = arg.to_uppercase();

        // /EXTRACT:member â€” extraction mode
        if upper.starts_with("/EXTRACT:") || upper.starts_with("-EXTRACT:") {
            is_extract = true;
            break;
        }

        // /LIST â€” list mode
        if upper == "/LIST" || upper == "-LIST" {
            has_list = true;
        }

        // /OUT:filename
        if upper.starts_with("/OUT:") || upper.starts_with("-OUT:") {
            output_file = Some(NormalizedPath::new(&arg[5..]));
            continue;
        }

        // /BREPRO â€” binary reproducibility
        if upper == "/BREPRO" || upper == "-BREPRO" {
            has_brepro = true;
            cache_relevant_flags.push(arg.clone());
            continue;
        }

        // Other flags
        if arg.starts_with('/') || arg.starts_with('-') {
            cache_relevant_flags.push(arg.clone());
            continue;
        }

        // Positional â€” input file
        input_files.push(NormalizedPath::new(arg));
    }

    if is_extract {
        return ParsedArchiveInvocation::NonCacheable {
            reason: "extract operation (/EXTRACT) not cacheable".to_string(),
        };
    }

    if has_list {
        return ParsedArchiveInvocation::NonCacheable {
            reason: "list operation (/LIST) not cacheable".to_string(),
        };
    }

    if input_files.is_empty() {
        return ParsedArchiveInvocation::NonCacheable {
            reason: "no input files specified".to_string(),
        };
    }

    // If no /OUT:, lib.exe defaults to first input file with .lib extension
    let output_file = output_file.unwrap_or_else(|| {
        let first = &input_files[0];
        NormalizedPath::new(first.with_extension("lib"))
    });

    ParsedArchiveInvocation::Cacheable(CacheableArchive {
        tool: NormalizedPath::new(tool),
        family: ArchiverFamily::MsvcLib,
        input_files,
        output_file,
        cache_relevant_flags,
        original_args: args.to_vec(),
        non_deterministic: !has_brepro,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    // â”€â”€â”€ Detection â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn detect_gnu_ar() {
        assert_eq!(detect_family("ar"), Some(ArchiverFamily::Ar));
        assert_eq!(detect_family("/usr/bin/ar"), Some(ArchiverFamily::Ar));
        assert_eq!(
            detect_family("x86_64-linux-gnu-ar"),
            Some(ArchiverFamily::Ar)
        );
        assert_eq!(
            detect_family("aarch64-linux-gnu-ar"),
            Some(ArchiverFamily::Ar)
        );
    }

    #[test]
    fn detect_llvm_ar() {
        assert_eq!(detect_family("llvm-ar"), Some(ArchiverFamily::LlvmAr));
        assert_eq!(detect_family("llvm-ar-15"), Some(ArchiverFamily::LlvmAr));
        assert_eq!(
            detect_family("/usr/bin/llvm-ar"),
            Some(ArchiverFamily::LlvmAr)
        );
    }

    #[test]
    fn detect_msvc_lib() {
        assert_eq!(detect_family("lib"), Some(ArchiverFamily::MsvcLib));
        assert_eq!(detect_family("lib.exe"), Some(ArchiverFamily::MsvcLib));
        assert_eq!(detect_family("LIB"), Some(ArchiverFamily::MsvcLib));
        assert_eq!(detect_family("LIB.EXE"), Some(ArchiverFamily::MsvcLib));
    }

    #[test]
    fn detect_unknown_tool() {
        assert_eq!(detect_family("gcc"), None);
        assert_eq!(detect_family("clang"), None);
        assert_eq!(detect_family("ld"), None);
        assert_eq!(detect_family("lzma"), None);
    }

    #[test]
    fn is_archiver_works() {
        assert!(is_archiver("ar"));
        assert!(is_archiver("llvm-ar"));
        assert!(is_archiver("lib.exe"));
        assert!(!is_archiver("gcc"));
        assert!(!is_archiver("ld"));
    }

    // â”€â”€â”€ GNU ar parsing â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn basic_ar_rcs() {
        let result = parse_archive_invocation("ar", &args(&["rcs", "libfoo.a", "a.o", "b.o"]));
        match result {
            ParsedArchiveInvocation::Cacheable(c) => {
                assert_eq!(c.family, ArchiverFamily::Ar);
                assert_eq!(c.output_file, NormalizedPath::new("libfoo.a"));
                assert_eq!(c.input_files.len(), 2);
                assert_eq!(c.input_files[0], NormalizedPath::new("a.o"));
                assert_eq!(c.input_files[1], NormalizedPath::new("b.o"));
                assert!(c.non_deterministic); // no D flag
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn ar_with_dash_prefix() {
        let result = parse_archive_invocation("ar", &args(&["-rcs", "libfoo.a", "a.o"]));
        match result {
            ParsedArchiveInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, NormalizedPath::new("libfoo.a"));
                assert_eq!(c.input_files, vec![NormalizedPath::new("a.o")]);
                assert_eq!(c.cache_relevant_flags, vec!["rcs"]);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn ar_deterministic_flag() {
        let result = parse_archive_invocation("ar", &args(&["rcsD", "libfoo.a", "a.o"]));
        match result {
            ParsedArchiveInvocation::Cacheable(c) => {
                assert!(!c.non_deterministic); // D flag present
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn ar_extract_non_cacheable() {
        let result = parse_archive_invocation("ar", &args(&["x", "libfoo.a"]));
        assert!(matches!(
            result,
            ParsedArchiveInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn ar_list_non_cacheable() {
        let result = parse_archive_invocation("ar", &args(&["t", "libfoo.a"]));
        assert!(matches!(
            result,
            ParsedArchiveInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn ar_delete_non_cacheable() {
        let result = parse_archive_invocation("ar", &args(&["d", "libfoo.a", "old.o"]));
        assert!(matches!(
            result,
            ParsedArchiveInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn ar_print_non_cacheable() {
        let result = parse_archive_invocation("ar", &args(&["p", "libfoo.a"]));
        assert!(matches!(
            result,
            ParsedArchiveInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn ar_no_args() {
        let result = parse_archive_invocation("ar", &args(&[]));
        assert!(matches!(
            result,
            ParsedArchiveInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn ar_no_inputs() {
        let result = parse_archive_invocation("ar", &args(&["rcs", "libfoo.a"]));
        assert!(matches!(
            result,
            ParsedArchiveInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn ar_quick_append() {
        let result =
            parse_archive_invocation("ar", &args(&["qcs", "libfoo.a", "a.o", "b.o", "c.o"]));
        match result {
            ParsedArchiveInvocation::Cacheable(c) => {
                assert_eq!(c.input_files.len(), 3);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn ar_preserves_input_order() {
        let result =
            parse_archive_invocation("ar", &args(&["rcs", "libfoo.a", "z.o", "a.o", "m.o"]));
        match result {
            ParsedArchiveInvocation::Cacheable(c) => {
                assert_eq!(c.input_files[0], NormalizedPath::new("z.o"));
                assert_eq!(c.input_files[1], NormalizedPath::new("a.o"));
                assert_eq!(c.input_files[2], NormalizedPath::new("m.o"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn ar_with_relpos_modifier() {
        // ar rcsb existing.o libfoo.a new.o
        // 'b' modifier: insert before existing.o (relpos arg consumed)
        let result =
            parse_archive_invocation("ar", &args(&["rcsb", "existing.o", "libfoo.a", "new.o"]));
        match result {
            ParsedArchiveInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, NormalizedPath::new("libfoo.a"));
                assert_eq!(c.input_files, vec![NormalizedPath::new("new.o")]);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn ar_with_long_options() {
        let result = parse_archive_invocation(
            "ar",
            &args(&["--plugin", "liblto_plugin.so", "rcs", "libfoo.a", "a.o"]),
        );
        match result {
            ParsedArchiveInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, NormalizedPath::new("libfoo.a"));
                assert_eq!(c.input_files, vec![NormalizedPath::new("a.o")]);
                assert!(c.cache_relevant_flags.contains(&"--plugin".to_string()));
                assert!(c
                    .cache_relevant_flags
                    .contains(&"liblto_plugin.so".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn llvm_ar_basic() {
        let result = parse_archive_invocation("llvm-ar", &args(&["rcs", "libfoo.a", "a.o", "b.o"]));
        match result {
            ParsedArchiveInvocation::Cacheable(c) => {
                assert_eq!(c.family, ArchiverFamily::LlvmAr);
                assert_eq!(c.output_file, NormalizedPath::new("libfoo.a"));
                assert_eq!(c.input_files.len(), 2);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn cross_compile_ar() {
        let result =
            parse_archive_invocation("x86_64-linux-gnu-ar", &args(&["rcs", "libfoo.a", "a.o"]));
        match result {
            ParsedArchiveInvocation::Cacheable(c) => {
                assert_eq!(c.family, ArchiverFamily::Ar);
                assert_eq!(c.tool, NormalizedPath::new("x86_64-linux-gnu-ar"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // â”€â”€â”€ MSVC lib.exe parsing â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn basic_msvc_lib() {
        let result =
            parse_archive_invocation("lib.exe", &args(&["/OUT:foo.lib", "a.obj", "b.obj"]));
        match result {
            ParsedArchiveInvocation::Cacheable(c) => {
                assert_eq!(c.family, ArchiverFamily::MsvcLib);
                assert_eq!(c.output_file, NormalizedPath::new("foo.lib"));
                assert_eq!(c.input_files.len(), 2);
                assert_eq!(c.input_files[0], NormalizedPath::new("a.obj"));
                assert_eq!(c.input_files[1], NormalizedPath::new("b.obj"));
                assert!(c.non_deterministic); // no /BREPRO
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_lib_with_brepro() {
        let result =
            parse_archive_invocation("lib.exe", &args(&["/BREPRO", "/OUT:foo.lib", "a.obj"]));
        match result {
            ParsedArchiveInvocation::Cacheable(c) => {
                assert!(!c.non_deterministic); // /BREPRO present
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_lib_extract_non_cacheable() {
        let result =
            parse_archive_invocation("lib.exe", &args(&["/EXTRACT:member.obj", "foo.lib"]));
        assert!(matches!(
            result,
            ParsedArchiveInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn msvc_lib_list_non_cacheable() {
        let result = parse_archive_invocation("lib.exe", &args(&["/LIST", "foo.lib"]));
        assert!(matches!(
            result,
            ParsedArchiveInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn msvc_lib_default_output_name() {
        // Without /OUT:, output defaults to first input with .lib extension
        let result = parse_archive_invocation("lib.exe", &args(&["a.obj", "b.obj"]));
        match result {
            ParsedArchiveInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, NormalizedPath::new("a.lib"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_lib_no_inputs() {
        let result = parse_archive_invocation("lib.exe", &args(&["/OUT:foo.lib"]));
        assert!(matches!(
            result,
            ParsedArchiveInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn msvc_lib_with_flags() {
        let result = parse_archive_invocation(
            "lib.exe",
            &args(&["/NOLOGO", "/MACHINE:X64", "/OUT:foo.lib", "a.obj"]),
        );
        match result {
            ParsedArchiveInvocation::Cacheable(c) => {
                assert!(c.cache_relevant_flags.contains(&"/NOLOGO".to_string()));
                assert!(c.cache_relevant_flags.contains(&"/MACHINE:X64".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_lib_preserves_input_order() {
        let result = parse_archive_invocation(
            "lib.exe",
            &args(&["/OUT:foo.lib", "z.obj", "a.obj", "m.obj"]),
        );
        match result {
            ParsedArchiveInvocation::Cacheable(c) => {
                assert_eq!(c.input_files[0], NormalizedPath::new("z.obj"));
                assert_eq!(c.input_files[1], NormalizedPath::new("a.obj"));
                assert_eq!(c.input_files[2], NormalizedPath::new("m.obj"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_lib_dash_syntax() {
        // lib.exe also accepts - prefix for flags
        let result =
            parse_archive_invocation("lib.exe", &args(&["-OUT:foo.lib", "-BREPRO", "a.obj"]));
        match result {
            ParsedArchiveInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, NormalizedPath::new("foo.lib"));
                assert!(!c.non_deterministic);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // â”€â”€â”€ Unknown tool â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn unknown_tool_non_cacheable() {
        let result = parse_archive_invocation("gcc", &args(&["-c", "foo.c"]));
        assert!(matches!(
            result,
            ParsedArchiveInvocation::NonCacheable { .. }
        ));
    }
}
