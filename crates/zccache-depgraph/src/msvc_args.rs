//! MSVC (`cl.exe`) argument parser.
//!
//! Extracts include paths, defines, and cache-relevant flags from
//! MSVC-style compiler command-line arguments.

use std::path::Path;

use zccache_core::NormalizedPath;

use super::args::{ParsedArgs, UserDepFlags};
use super::search_paths::IncludeSearchPaths;

/// Parse MSVC-style compile arguments into structured form.
///
/// MSVC uses `/` prefix for flags (e.g., `/I`, `/D`, `/O2`).
/// Also accepts `-` prefix for most flags.
/// `args` should be the arguments after the compiler executable.
/// Relative paths are resolved against `cwd`.
pub fn parse_msvc_args(args: &[String], cwd: &Path) -> ParsedArgs {
    let mut result = ParsedArgs {
        source_file: NormalizedPath::new(""),
        output_file: None,
        include_search: IncludeSearchPaths::default(),
        defines: Vec::new(),
        undefines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        compiler: None,
        dep_flags: UserDepFlags::default(),
        unknown_flags: Vec::new(),
    };

    let mut i = 0;
    let mut source_candidates: Vec<NormalizedPath> = Vec::new();

    while i < args.len() {
        let arg = &args[i];

        // /I<dir> or /I <dir>
        if arg == "/I" {
            if let Some(next) = args.get(i + 1) {
                result.include_search.user.push(resolve_path(next, cwd));
                i += 2;
                continue;
            }
        } else if let Some(dir) = arg.strip_prefix("/I") {
            result.include_search.user.push(resolve_path(dir, cwd));
            i += 1;
            continue;
        }

        // -I<dir> or -I <dir> (MSVC also accepts dash prefix)
        if arg == "-I" {
            if let Some(next) = args.get(i + 1) {
                result.include_search.user.push(resolve_path(next, cwd));
                i += 2;
                continue;
            }
        } else if let Some(dir) = arg.strip_prefix("-I") {
            result.include_search.user.push(resolve_path(dir, cwd));
            i += 1;
            continue;
        }

        // /D<define> or /D <define>
        if arg == "/D" {
            if let Some(next) = args.get(i + 1) {
                result.defines.push(next.clone());
                i += 2;
                continue;
            }
        } else if let Some(def) = arg.strip_prefix("/D") {
            result.defines.push(def.to_string());
            i += 1;
            continue;
        }

        // /U<undef> or /U <undef>
        if arg == "/U" {
            if let Some(next) = args.get(i + 1) {
                result.undefines.push(next.clone());
                i += 2;
                continue;
            }
        } else if let Some(undef) = arg.strip_prefix("/U") {
            result.undefines.push(undef.to_string());
            i += 1;
            continue;
        }

        // /Fo<file> (output object file)
        if let Some(out) = arg.strip_prefix("/Fo") {
            if out.is_empty() {
                if let Some(next) = args.get(i + 1) {
                    result.output_file = Some(resolve_path(next, cwd));
                    i += 2;
                    continue;
                }
            } else {
                result.output_file = Some(resolve_path(out, cwd));
                i += 1;
                continue;
            }
        }

        // /FI<file> (force include)
        if let Some(fi) = arg.strip_prefix("/FI") {
            if fi.is_empty() {
                if let Some(next) = args.get(i + 1) {
                    result.force_includes.push(resolve_path(next, cwd));
                    i += 2;
                    continue;
                }
            } else {
                result.force_includes.push(resolve_path(fi, cwd));
                i += 1;
                continue;
            }
        }

        // /std:<standard> (e.g., /std:c++17, /std:c11)
        if arg.starts_with("/std:") {
            result.flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Optimization flags: /O1, /O2, /Ox, /Od, etc.
        if arg.starts_with("/O") {
            result.flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Warning flags: /W0, /W1, /W2, /W3, /W4, /Wall, /WX
        if arg.starts_with("/W") {
            result.flags.push(arg.clone());
            i += 1;
            continue;
        }

        // PCH flags: /Yu (use PCH), /Yc (create PCH)
        if arg.starts_with("/Yu") || arg.starts_with("/Yc") {
            result.flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Runtime library: /MD, /MDd, /MT, /MTd
        if arg == "/MD" || arg == "/MDd" || arg == "/MT" || arg == "/MTd" {
            result.flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Exception handling: /EHsc, /EHa, etc.
        if arg.starts_with("/EH") {
            result.flags.push(arg.clone());
            i += 1;
            continue;
        }

        // /Zi, /ZI, /Z7 (debug info format)
        if arg.starts_with("/Z") {
            result.flags.push(arg.clone());
            i += 1;
            continue;
        }

        // /c (compile only) â€” skip, recognized but not a cache flag
        if arg == "/c" {
            i += 1;
            continue;
        }

        // /nologo â€” skip
        if arg == "/nologo" {
            i += 1;
            continue;
        }

        // /showIncludes â€” MSVC's dep tracking (like -MD for gcc)
        if arg == "/showIncludes" {
            result.dep_flags.has_md = true;
            i += 1;
            continue;
        }

        // /Fp<file> (PCH file path) â€” takes value
        if arg.starts_with("/Fp") {
            result.flags.push(arg.clone());
            i += 1;
            continue;
        }

        // /Fe<file> (executable output) â€” skip with value
        if arg.starts_with("/Fe") {
            i += 1;
            continue;
        }

        // /Fd<file> (PDB path) â€” skip
        if arg.starts_with("/Fd") {
            i += 1;
            continue;
        }

        // Any other flag starting with / or -
        if arg.starts_with('/') || arg.starts_with('-') {
            result.unknown_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Positional arg â€” source file candidate
        source_candidates.push(resolve_path(arg, cwd));
        i += 1;
    }

    // Pick the source file: typically the one with a C/C++ extension.
    if let Some(src) = source_candidates
        .iter()
        .find(|p| is_source_file(p))
        .cloned()
    {
        result.source_file = src;
    } else if let Some(first) = source_candidates.into_iter().next() {
        result.source_file = first;
    }

    // Sort for deterministic context keys.
    result.defines.sort();
    result.undefines.sort();
    result.flags.sort();
    result.unknown_flags.sort();

    result
}

fn resolve_path(path: &str, cwd: &Path) -> NormalizedPath {
    let p = Path::new(path);
    if p.is_absolute() {
        NormalizedPath::new(p)
    } else {
        NormalizedPath::new(cwd.join(p))
    }
}

fn is_source_file(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    matches!(
        ext.as_str(),
        "c" | "cc" | "cpp" | "cxx" | "c++" | "m" | "mm" | "s" | "sx" | "i" | "ii"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    #[cfg(windows)]
    fn basic_msvc_compile() {
        let parsed = parse_msvc_args(
            &args(&["/c", "foo.cpp", "/Fofoo.obj"]),
            Path::new("C:\\project"),
        );
        assert_eq!(parsed.source_file, Path::new("C:\\project\\foo.cpp"));
        assert_eq!(
            parsed.output_file.as_deref(),
            Some(Path::new("C:\\project\\foo.obj"))
        );
    }

    #[test]
    #[cfg(windows)]
    fn include_dirs() {
        let parsed = parse_msvc_args(
            &args(&["/I", "inc", "/Ilib\\include", "/c", "x.cpp"]),
            Path::new("C:\\p"),
        );
        assert_eq!(parsed.include_search.user.len(), 2);
        assert_eq!(parsed.include_search.user[0], Path::new("C:\\p\\inc"));
        assert_eq!(
            parsed.include_search.user[1],
            Path::new("C:\\p\\lib\\include")
        );
    }

    #[test]
    fn defines_extracted_and_sorted() {
        let parsed = parse_msvc_args(
            &args(&["/DBAR=1", "/DFOO", "/D", "AAA=2", "/c", "x.cpp"]),
            Path::new("C:\\p"),
        );
        assert_eq!(parsed.defines, vec!["AAA=2", "BAR=1", "FOO"]);
    }

    #[test]
    fn undefines_extracted() {
        let parsed = parse_msvc_args(
            &args(&["/UFOO", "/U", "BAR", "/c", "x.cpp"]),
            Path::new("C:\\p"),
        );
        assert_eq!(parsed.undefines, vec!["BAR", "FOO"]);
    }

    #[test]
    fn flags_extracted() {
        let parsed = parse_msvc_args(
            &args(&["/std:c++17", "/O2", "/W4", "/EHsc", "/MD", "/c", "x.cpp"]),
            Path::new("C:\\p"),
        );
        assert!(parsed.flags.contains(&"/std:c++17".to_string()));
        assert!(parsed.flags.contains(&"/O2".to_string()));
        assert!(parsed.flags.contains(&"/W4".to_string()));
        assert!(parsed.flags.contains(&"/EHsc".to_string()));
        assert!(parsed.flags.contains(&"/MD".to_string()));
    }

    #[test]
    #[cfg(windows)]
    fn force_include() {
        let parsed = parse_msvc_args(&args(&["/FIpch.h", "/c", "x.cpp"]), Path::new("C:\\p"));
        assert_eq!(parsed.force_includes, vec![Path::new("C:\\p\\pch.h")]);
    }

    #[test]
    fn pch_flags() {
        let parsed = parse_msvc_args(
            &args(&["/Yupch.h", "/Fppch.pch", "/c", "x.cpp"]),
            Path::new("C:\\p"),
        );
        assert!(parsed.flags.contains(&"/Yupch.h".to_string()));
        assert!(parsed.flags.contains(&"/Fppch.pch".to_string()));
    }

    #[test]
    fn unknown_flags_collected() {
        let parsed = parse_msvc_args(
            &args(&["/c", "foo.cpp", "/experimental:module", "/await"]),
            Path::new("C:\\p"),
        );
        assert!(parsed
            .unknown_flags
            .contains(&"/experimental:module".to_string()));
        assert!(parsed.unknown_flags.contains(&"/await".to_string()));
    }

    #[test]
    fn show_includes_detected() {
        let parsed = parse_msvc_args(
            &args(&["/showIncludes", "/c", "foo.cpp"]),
            Path::new("C:\\p"),
        );
        assert!(parsed.dep_flags.has_md);
    }

    #[test]
    #[cfg(windows)]
    fn dash_prefix_include() {
        let parsed = parse_msvc_args(&args(&["-I", "inc", "/c", "x.cpp"]), Path::new("C:\\p"));
        assert_eq!(parsed.include_search.user, vec![Path::new("C:\\p\\inc")]);
    }
}
