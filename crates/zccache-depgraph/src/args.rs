//! Compiler argument parser.
//!
//! Extracts include paths, defines, and cache-relevant flags from
//! compiler command-line arguments.

use std::path::Path;

use super::search_paths::IncludeSearchPaths;
use zccache_compiler::gnu_flag_takes_value;
use zccache_core::NormalizedPath;

/// Dependency-generation flags already present in the user's compiler args.
#[derive(Debug, Clone, Default)]
pub struct UserDepFlags {
    /// User has -MD or -MMD (emit depfile as side-effect of compilation).
    pub has_md: bool,
    /// User selected -MMD, so their depfile intentionally omits system headers.
    pub has_mmd: bool,
    /// User has -MF `<path>` (explicit depfile output path).
    pub mf_path: Option<NormalizedPath>,
    /// User selected `-MF -`, so dependency output is part of compiler stdout.
    pub depfile_to_stdout: bool,
}

/// Result of parsing compiler arguments.
#[derive(Debug, Clone)]
pub struct ParsedArgs {
    /// The source file being compiled.
    pub source_file: NormalizedPath,
    /// The output file (`-o`).
    pub output_file: Option<NormalizedPath>,
    /// Structured include search paths.
    pub include_search: IncludeSearchPaths,
    /// Defines (`-DFOO`, `-DFOO=1`). Sorted for deterministic hashing.
    pub defines: Vec<String>,
    /// Undefines (`-UFOO`). Sorted.
    pub undefines: Vec<String>,
    /// Cache-relevant flags (`-std=`, `-f*`, `-m*`, `-O*`, `-W*`, `-x`).
    /// Sorted for deterministic hashing.
    pub flags: Vec<String>,
    /// Force-included files (`-include <file>`).
    pub force_includes: Vec<NormalizedPath>,
    /// The compiler executable (first arg or from context).
    pub compiler: Option<NormalizedPath>,
    /// Dependency generation flags detected in the user's args.
    pub dep_flags: UserDepFlags,
    /// Flags not recognized by the parser. Sorted for deterministic hashing.
    /// These are hashed into the context key to ensure unknown flags affect
    /// cache invalidation.
    pub unknown_flags: Vec<String>,
}

/// Parse GNU/Clang-style compile arguments into structured form.
///
/// `args` should be the arguments after the compiler executable.
/// Relative paths are resolved against `cwd`.
pub fn parse_gnu_args(args: &[String], cwd: &Path) -> ParsedArgs {
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

        // -I<dir> or -I <dir>
        if arg == "-I" {
            if let Some(next) = args.get(i + 1) {
                result.include_search.user.push(resolve_path(next, cwd));
                i += 2;
                continue;
            }
        } else if !matches!(arg.as_str(), "-isystem" | "-isysroot") {
            if let Some(dir) = arg.strip_prefix("-I") {
                result.include_search.user.push(resolve_path(dir, cwd));
                i += 1;
                continue;
            }
        }

        // -isystem <dir>
        if arg == "-isystem" {
            if let Some(next) = args.get(i + 1) {
                result.include_search.system.push(resolve_path(next, cwd));
                i += 2;
                continue;
            }
        }

        // -iquote <dir>
        if arg == "-iquote" {
            if let Some(next) = args.get(i + 1) {
                result.include_search.iquote.push(resolve_path(next, cwd));
                i += 2;
                continue;
            }
        }

        // -idirafter <dir>
        if arg == "-idirafter" {
            if let Some(next) = args.get(i + 1) {
                result.include_search.after.push(resolve_path(next, cwd));
                i += 2;
                continue;
            }
        }

        // -D<define> or -D <define>
        if arg == "-D" {
            if let Some(next) = args.get(i + 1) {
                result.defines.push(next.clone());
                i += 2;
                continue;
            }
        } else if let Some(def) = arg.strip_prefix("-D") {
            result.defines.push(def.to_string());
            i += 1;
            continue;
        }

        // -U<undef> or -U <undef>
        if arg == "-U" {
            if let Some(next) = args.get(i + 1) {
                result.undefines.push(next.clone());
                i += 2;
                continue;
            }
        } else if let Some(undef) = arg.strip_prefix("-U") {
            result.undefines.push(undef.to_string());
            i += 1;
            continue;
        }

        // -o <file> or -o<file>
        if arg == "-o" {
            if let Some(next) = args.get(i + 1) {
                result.output_file = Some(resolve_path(next, cwd));
                i += 2;
                continue;
            }
        } else if let Some(out) = arg.strip_prefix("-o") {
            result.output_file = Some(resolve_path(out, cwd));
            i += 1;
            continue;
        }

        // -include-pch <file> (precompiled header â€” must come BEFORE -include)
        if arg == "-include-pch" {
            if let Some(next) = args.get(i + 1) {
                result.force_includes.push(resolve_path(next, cwd));
                i += 2;
                continue;
            }
        }

        // -include <file> (force include)
        if arg == "-include" {
            if let Some(next) = args.get(i + 1) {
                result.force_includes.push(resolve_path(next, cwd));
                i += 2;
                continue;
            }
        }

        // -imacros <file> is a force-included macro input whose contents
        // must participate in depgraph invalidation just like -include.
        if arg == "-imacros" {
            if let Some(next) = args.get(i + 1) {
                result.force_includes.push(resolve_path(next, cwd));
                i += 2;
                continue;
            }
        }

        // GCC's -Wp, form forwards comma-separated options directly to the
        // preprocessor. Retain the raw argument in the key, but also surface
        // include and macro inputs to dependency discovery so a header that
        // is reachable only through this spelling cannot become invisible.
        if let Some(options) = arg.strip_prefix("-Wp,") {
            let mut parts = options.split(',');
            while let Some(option) = parts.next() {
                match option {
                    "-I" => {
                        if let Some(path) = parts.next() {
                            result.include_search.user.push(resolve_path(path, cwd));
                        }
                    }
                    "-isystem" => {
                        if let Some(path) = parts.next() {
                            result.include_search.system.push(resolve_path(path, cwd));
                        }
                    }
                    "-imacros" | "-include" => {
                        if let Some(path) = parts.next() {
                            result.force_includes.push(resolve_path(path, cwd));
                        }
                    }
                    _ => {}
                }
            }
            result.flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Cache-relevant flags.
        //
        // PGO profile data is a non-argv compiler input. Keep the raw flag in
        // the context key and feed its referenced file(s) through the same
        // content-hashed path as `-include`: changing a `.profdata` / `.gcda`
        // file in place must produce a new artifact key.
        if let Some(path) = arg
            .strip_prefix("-fprofile-use=")
            .or_else(|| arg.strip_prefix("-fprofile-dir="))
        {
            result.flags.push(arg.clone());
            result.force_includes.extend(profile_input_files(path, cwd));
            i += 1;
            continue;
        }
        if arg.starts_with("-std=") || arg.starts_with("--std=") {
            result.flags.push(arg.clone());
            i += 1;
            continue;
        }
        if arg == "-std" {
            if let Some(next) = args.get(i + 1) {
                result.flags.push(format!("-std={next}"));
                i += 2;
                continue;
            }
        }

        // -x <language>
        if arg == "-x" {
            if let Some(next) = args.get(i + 1) {
                result.flags.push(format!("-x {next}"));
                i += 2;
                continue;
            }
        } else if arg.starts_with("-x") {
            result.flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Exact GNU flags with a separate value must be recognized before
        // the broad `-m*` / `-f*` branches below. In particular,
        // `-march native` is host-dependent just like `-march=native`, so
        // preserve the pair as one canonical cache-key flag.
        if matches!(arg.as_str(), "-march" | "-mtune" | "-mcpu") {
            if let Some(next) = args.get(i + 1) {
                result.flags.push(format!("{arg}={next}"));
                i += 2;
                continue;
            }
        }

        // Optimization, target, feature flags.
        if arg.starts_with("-O")
            || arg.starts_with("-f")
            || arg.starts_with("-m")
            || arg.starts_with("-W")
            || arg.starts_with("-g")
            || arg.starts_with("--target=")
            || arg == "-pthread"
            || arg == "-pipe"
        {
            result.flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Dependency-generation flags: track but don't include in cache key.
        if arg == "-MD" || arg == "-MMD" {
            result.dep_flags.has_md = true;
            result.dep_flags.has_mmd = arg == "-MMD";
            i += 1;
            continue;
        }

        // -MF takes a following argument â€” capture path.
        if arg == "-MF" {
            if let Some(next) = args.get(i + 1) {
                result.dep_flags.depfile_to_stdout = next == "-";
                result.dep_flags.mf_path = (next != "-").then(|| resolve_path(next, cwd));
            }
            i += 2;
            continue;
        } else if let Some(path) = arg.strip_prefix("-MF").filter(|path| !path.is_empty()) {
            result.dep_flags.depfile_to_stdout = path == "-";
            result.dep_flags.mf_path = (path != "-").then(|| resolve_path(path, cwd));
            i += 1;
            continue;
        }

        // -MQ, -MT take a following argument.
        if arg == "-MQ" || arg == "-MT" {
            i += 2;
            continue;
        }

        // The invocation classifier owns this table. Preserve both tokens in
        // the context key for every remaining value-taking flag so a
        // classifier update cannot make distinct argv shapes collide.
        if gnu_flag_takes_value(arg) {
            if let Some(next) = args.get(i + 1) {
                result.flags.push(format!("{arg}={next}"));
                i += 2;
                continue;
            }
        }

        if arg == "-c"
            || arg == "-S"
            || arg == "-E"
            || arg == "-v"
            || arg == "-w"
            || arg.starts_with("-M")
            || arg == "-MP"
        {
            i += 1;
            continue;
        }

        // Anything not starting with - is a source file candidate.
        if !arg.starts_with('-') {
            source_candidates.push(resolve_path(arg, cwd));
        } else {
            // Unknown flags fail safe: preserve a following non-source token
            // too, so an unclassified value-taking compiler flag cannot
            // silently collide with a different value.
            if let Some(next) = args.get(i + 1) {
                if !next.starts_with('-') && !is_source_file(&resolve_path(next, cwd)) {
                    result.unknown_flags.push(format!("{arg}={next}"));
                    i += 2;
                    continue;
                }
            }
            result.unknown_flags.push(arg.clone());
        }

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

    // Sort defines and flags for deterministic context keys.
    result.defines.sort();
    result.undefines.sort();
    result.flags.sort();
    result.unknown_flags.sort();

    result
}

/// Backward-compatible alias for [`parse_gnu_args`].
pub fn parse_compile_args(args: &[String], cwd: &Path) -> ParsedArgs {
    parse_gnu_args(args, cwd)
}

/// Split a shell-style command string into arguments.
///
/// Handles single and double quoting. Does not handle backslash
/// escaping outside of quotes (sufficient for compile_commands.json).
pub fn split_command(command: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut chars = command.chars().peekable();

    while let Some(ch) = chars.next() {
        if in_single_quote {
            if ch == '\'' {
                in_single_quote = false;
            } else {
                current.push(ch);
            }
        } else if in_double_quote {
            if ch == '"' {
                in_double_quote = false;
            } else if ch == '\\' {
                if let Some(&next) = chars.peek() {
                    if next == '"' || next == '\\' {
                        current.push(next);
                        chars.next();
                    } else {
                        current.push(ch);
                    }
                }
            } else {
                current.push(ch);
            }
        } else if ch == '\'' {
            in_single_quote = true;
        } else if ch == '"' {
            in_double_quote = true;
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }

    if !current.is_empty() {
        args.push(current);
    }

    args
}

fn resolve_path(path: &str, cwd: &Path) -> NormalizedPath {
    let p = Path::new(path);
    if p.is_absolute() {
        NormalizedPath::new(p)
    } else {
        NormalizedPath::new(cwd.join(p))
    }
}

/// Return the regular profile-data files rooted at `path`.
///
/// `-fprofile-use=<file>` commonly names a Clang `.profdata` file, while
/// GCC's `-fprofile-use=<dir>` / `-fprofile-dir=<dir>` forms name a directory
/// of `.gcda` files. The depgraph's established force-include input path
/// content-hashes every returned file on both update and hit checks, so PGO
/// data regenerated at the same pathname cannot serve a stale object.
///
/// A nonexistent or non-file root is retained as a single input. That makes
/// the hash lookup fail and safely keeps the context cold rather than treating
/// an unavailable profile input as irrelevant.
fn profile_input_files(path: &str, cwd: &Path) -> Vec<NormalizedPath> {
    let root = resolve_path(path, cwd);
    let root_path = root.as_path();
    if root_path.is_file() {
        return vec![root];
    }
    if !root_path.is_dir() {
        return vec![root];
    }

    fn visit(dir: &Path, files: &mut Vec<NormalizedPath>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                files.push(NormalizedPath::new(path));
            } else if path.is_dir() {
                visit(&path, files);
            }
        }
    }

    let mut files = Vec::new();
    visit(root_path, &mut files);
    files.sort();
    if files.is_empty() {
        // An empty profile directory is not an input file yet. Retaining the
        // directory itself makes the compilation cold instead of incorrectly
        // treating `-fprofile-use` as cacheable without profile data.
        vec![root]
    } else {
        files
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
    use crate::context::compute_artifact_key;
    use crate::CompileContext;
    use tempfile::tempdir;
    use zccache_compiler::GNU_FLAGS_WITH_VALUE;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn basic_compile_command() {
        let parsed = parse_gnu_args(
            &args(&["-c", "foo.c", "-o", "foo.o"]),
            Path::new("/project"),
        );
        assert_eq!(parsed.source_file, Path::new("/project/foo.c"));
        assert_eq!(
            parsed.output_file.as_deref(),
            Some(Path::new("/project/foo.o"))
        );
    }

    #[test]
    fn include_dirs_preserved_in_order() {
        let parsed = parse_gnu_args(
            &args(&["-I", "first", "-Isecond", "-I", "third", "-c", "x.c"]),
            Path::new("/p"),
        );
        assert_eq!(parsed.include_search.user.len(), 3);
        assert_eq!(parsed.include_search.user[0], Path::new("/p/first"));
        assert_eq!(parsed.include_search.user[1], Path::new("/p/second"));
        assert_eq!(parsed.include_search.user[2], Path::new("/p/third"));
    }

    #[test]
    fn isystem_and_iquote_and_idirafter() {
        let parsed = parse_gnu_args(
            &args(&[
                "-iquote",
                "q",
                "-isystem",
                "s",
                "-idirafter",
                "a",
                "-c",
                "x.c",
            ]),
            Path::new("/p"),
        );
        assert_eq!(parsed.include_search.iquote, vec![Path::new("/p/q")]);
        assert_eq!(parsed.include_search.system, vec![Path::new("/p/s")]);
        assert_eq!(parsed.include_search.after, vec![Path::new("/p/a")]);
    }

    #[test]
    fn defines_extracted_and_sorted() {
        let parsed = parse_gnu_args(
            &args(&["-DBAR=1", "-DFOO", "-D", "AAA=2", "-c", "x.c"]),
            Path::new("/p"),
        );
        assert_eq!(parsed.defines, vec!["AAA=2", "BAR=1", "FOO"]);
    }

    #[test]
    fn undefines_extracted() {
        let parsed = parse_gnu_args(&args(&["-UFOO", "-U", "BAR", "-c", "x.c"]), Path::new("/p"));
        assert_eq!(parsed.undefines, vec!["BAR", "FOO"]);
    }

    #[test]
    fn flags_extracted_and_sorted() {
        let parsed = parse_gnu_args(
            &args(&["-std=c++17", "-O2", "-fPIC", "-Wall", "-c", "x.cpp"]),
            Path::new("/p"),
        );
        assert!(parsed.flags.contains(&"-std=c++17".to_string()));
        assert!(parsed.flags.contains(&"-O2".to_string()));
        assert!(parsed.flags.contains(&"-fPIC".to_string()));
        assert!(parsed.flags.contains(&"-Wall".to_string()));
        // Sorted.
        let sorted: Vec<_> = parsed.flags.clone();
        let mut expected = sorted.clone();
        expected.sort();
        assert_eq!(sorted, expected);
    }

    #[test]
    fn force_include() {
        let parsed = parse_gnu_args(&args(&["-include", "pch.h", "-c", "x.c"]), Path::new("/p"));
        assert_eq!(parsed.force_includes, vec![Path::new("/p/pch.h")]);
    }

    #[test]
    fn profile_use_file_is_content_hashed_input() {
        let temp = tempdir().expect("temp profile directory");
        let source = temp.path().join("unit.c");
        let profile = temp.path().join("unit.profdata");
        std::fs::write(&source, "int unit(void) { return 1; }").expect("write source");
        std::fs::write(&profile, "profile one").expect("write profile");

        let parsed = parse_gnu_args(
            &args(&[
                &format!("-fprofile-use={}", profile.display()),
                "-c",
                &source.to_string_lossy(),
            ]),
            temp.path(),
        );
        assert_eq!(parsed.force_includes, vec![profile.as_path()]);
        assert!(parsed
            .flags
            .contains(&format!("-fprofile-use={}", profile.display())));

        let context = CompileContext::from_parsed_args(
            parsed,
            zccache_hash::hash_bytes(b"profile-test-compiler"),
        );
        let context_key = context.context_key();
        let source_hash = zccache_hash::hash_file(&source).expect("hash source");
        let first_profile_hash = zccache_hash::hash_file(&profile).expect("hash first profile");
        let first = compute_artifact_key(
            &context_key,
            &mut [
                (context.source_file.clone(), source_hash),
                (context.force_includes[0].clone(), first_profile_hash),
            ],
            None,
        );

        std::fs::write(&profile, "profile two").expect("rewrite profile");
        let second_profile_hash = zccache_hash::hash_file(&profile).expect("hash second profile");
        let second = compute_artifact_key(
            &context_key,
            &mut [
                (context.source_file.clone(), source_hash),
                (context.force_includes[0].clone(), second_profile_hash),
            ],
            None,
        );
        assert_ne!(
            first, second,
            "profile content must change the artifact key"
        );
    }

    #[test]
    fn profile_dir_recursively_discovers_gcc_profile_inputs() {
        let temp = tempdir().expect("temp profile directory");
        let profile_dir = temp.path().join("profiles");
        let nested = profile_dir.join("nested");
        std::fs::create_dir_all(&nested).expect("create nested profile directory");
        let first = profile_dir.join("first.gcda");
        let second = nested.join("second.gcda");
        std::fs::write(&first, "one").expect("write first profile");
        std::fs::write(&second, "two").expect("write second profile");

        let parsed = parse_gnu_args(
            &args(&[
                &format!("-fprofile-dir={}", profile_dir.display()),
                "-c",
                "unit.c",
            ]),
            temp.path(),
        );
        assert_eq!(
            parsed.force_includes,
            vec![first.as_path(), second.as_path()]
        );
    }

    #[test]
    fn include_pch_parsed() {
        let parsed = parse_gnu_args(
            &args(&["-c", "foo.cpp", "-include-pch", "pch.h.pch"]),
            Path::new("/p"),
        );
        assert_eq!(parsed.force_includes, vec![Path::new("/p/pch.h.pch")]);
    }

    #[test]
    fn include_pch_and_include_both_parsed() {
        let parsed = parse_gnu_args(
            &args(&[
                "-include-pch",
                "pch.h.pch",
                "-include",
                "extra.h",
                "-c",
                "foo.cpp",
            ]),
            Path::new("/p"),
        );
        assert_eq!(
            parsed.force_includes,
            vec![Path::new("/p/pch.h.pch"), Path::new("/p/extra.h")]
        );
    }

    #[test]
    fn cross_compile_and_debug_flags_preserve_their_values() {
        let parsed = parse_gnu_args(
            &args(&[
                "-target",
                "armv7-none-eabi",
                "-arch",
                "arm64",
                "-isysroot",
                "/sdk/a",
                "--target",
                "x86_64-unknown-linux-gnu",
                "-g3",
                "-c",
                "x.c",
            ]),
            Path::new("/p"),
        );
        for flag in [
            "-target=armv7-none-eabi",
            "-arch=arm64",
            "-isysroot=/sdk/a",
            "--target=x86_64-unknown-linux-gnu",
            "-g3",
        ] {
            assert!(parsed.flags.contains(&flag.to_string()), "missing {flag}");
        }
    }

    #[test]
    fn imacros_is_a_force_include() {
        let parsed = parse_gnu_args(
            &args(&["-imacros", "macros.h", "-c", "x.c"]),
            Path::new("/p"),
        );
        assert_eq!(parsed.force_includes, vec![Path::new("/p/macros.h")]);
    }

    #[test]
    fn wp_forwarded_include_and_macro_inputs_are_discovered() {
        let parsed = parse_gnu_args(
            &args(&["-Wp,-I,generated,-imacros,config.h", "-c", "x.c"]),
            Path::new("/p"),
        );
        assert_eq!(parsed.include_search.user, vec![Path::new("/p/generated")]);
        assert_eq!(parsed.force_includes, vec![Path::new("/p/config.h")]);
        assert!(parsed
            .flags
            .contains(&"-Wp,-I,generated,-imacros,config.h".to_string()));
    }

    /// Mechanical #1173 drift guard: the invocation classifier owns the
    /// value-taking table, and the key parser must consume every listed
    /// value rather than reprocessing it as an unrelated positional token.
    #[test]
    fn classifier_value_flag_table_is_consumed_by_key_parser() {
        for flag in GNU_FLAGS_WITH_VALUE {
            let parsed = parse_gnu_args(
                &[
                    (*flag).into(),
                    "classifier-value".into(),
                    "-c".into(),
                    "x.c".into(),
                ],
                Path::new("/p"),
            );
            assert_eq!(parsed.source_file, Path::new("/p/x.c"), "{flag}");
            assert!(parsed.unknown_flags.is_empty(), "{flag}");
        }
    }

    #[test]
    fn absolute_paths_not_prefixed() {
        let parsed = parse_gnu_args(
            &args(&["-I", "/usr/include", "-c", "/src/foo.c"]),
            Path::new("/p"),
        );
        assert_eq!(parsed.include_search.user, vec![Path::new("/usr/include")]);
        assert_eq!(parsed.source_file, Path::new("/src/foo.c"));
    }

    #[test]
    fn relative_paths_resolved_against_cwd() {
        let parsed = parse_gnu_args(
            &args(&["-I", "../inc", "-c", "src/main.c"]),
            Path::new("/project/build"),
        );
        assert_eq!(
            parsed.include_search.user,
            vec![Path::new("/project/build/../inc")]
        );
        assert_eq!(parsed.source_file, Path::new("/project/build/src/main.c"));
    }

    #[test]
    fn mf_and_mt_args_skipped() {
        let parsed = parse_gnu_args(
            &args(&["-MMD", "-MF", "deps.d", "-MT", "foo.o", "-c", "x.c"]),
            Path::new("/p"),
        );
        assert_eq!(parsed.source_file, Path::new("/p/x.c"));
        assert!(parsed.flags.is_empty());
    }

    #[test]
    fn source_file_by_extension() {
        let parsed = parse_gnu_args(&args(&["-c", "main.cpp", "-o", "main.o"]), Path::new("/p"));
        assert_eq!(parsed.source_file, Path::new("/p/main.cpp"));
    }

    #[test]
    fn language_flag() {
        let parsed = parse_gnu_args(&args(&["-x", "c++", "-c", "foo.c"]), Path::new("/p"));
        assert!(parsed.flags.contains(&"-x c++".to_string()));
    }

    // â”€â”€ unknown_flags tests â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn unknown_flags_collected() {
        let parsed = parse_gnu_args(
            &args(&[
                "-c",
                "foo.c",
                "--deploy-dependencies",
                "--custom-flag=value",
            ]),
            Path::new("/p"),
        );
        assert!(parsed
            .unknown_flags
            .contains(&"--deploy-dependencies".to_string()));
        assert!(parsed
            .unknown_flags
            .contains(&"--custom-flag=value".to_string()));
    }

    #[test]
    fn unknown_flag_preserves_a_non_source_value() {
        let parsed = parse_gnu_args(
            &args(&["--vendor-mode", "strict", "-c", "foo.c"]),
            Path::new("/p"),
        );
        assert_eq!(parsed.unknown_flags, vec!["--vendor-mode=strict"]);
    }

    #[test]
    fn unknown_flags_sorted() {
        let parsed = parse_gnu_args(&args(&["-c", "foo.c", "--zzz", "--aaa"]), Path::new("/p"));
        assert_eq!(
            parsed.unknown_flags,
            vec!["--aaa".to_string(), "--zzz".to_string()]
        );
    }

    #[test]
    fn known_flags_not_in_unknown() {
        let parsed = parse_gnu_args(
            &args(&["-c", "foo.c", "-O2", "-Wall", "-std=c++17"]),
            Path::new("/p"),
        );
        assert!(parsed.unknown_flags.is_empty());
    }

    // â”€â”€ split_command tests â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn split_simple_command() {
        let result = split_command("cc -c foo.c -o foo.o");
        assert_eq!(result, vec!["cc", "-c", "foo.c", "-o", "foo.o"]);
    }

    #[test]
    fn split_with_double_quotes() {
        let result = split_command(r#"cc -DFOO="bar baz" -c x.c"#);
        assert_eq!(result, vec!["cc", "-DFOO=bar baz", "-c", "x.c"]);
    }

    #[test]
    fn split_with_single_quotes() {
        let result = split_command("cc '-DFOO=bar baz' -c x.c");
        assert_eq!(result, vec!["cc", "-DFOO=bar baz", "-c", "x.c"]);
    }

    #[test]
    fn split_with_escaped_quote() {
        let result = split_command(r#"cc -DFOO="he said \"hi\"" -c x.c"#);
        assert_eq!(result, vec!["cc", r#"-DFOO=he said "hi""#, "-c", "x.c"]);
    }

    #[test]
    fn split_empty() {
        let result = split_command("");
        assert!(result.is_empty());
    }

    #[test]
    fn dep_flags_none_by_default() {
        let parsed = parse_gnu_args(&args(&["-c", "foo.c", "-O2"]), Path::new("/p"));
        assert!(!parsed.dep_flags.has_md);
        assert!(parsed.dep_flags.mf_path.is_none());
    }

    #[test]
    fn dep_flags_md_detected() {
        let parsed = parse_gnu_args(&args(&["-MMD", "-c", "foo.c"]), Path::new("/p"));
        assert!(parsed.dep_flags.has_md);
        assert!(parsed.dep_flags.has_mmd);
    }

    #[test]
    fn dep_flags_mf_detected() {
        let parsed = parse_gnu_args(&args(&["-MF", "deps.d", "-c", "foo.c"]), Path::new("/p"));
        assert_eq!(
            parsed.dep_flags.mf_path.as_deref(),
            Some(Path::new("/p/deps.d"))
        );
    }

    #[test]
    fn dep_flags_joined_mf_and_stdout_detected() {
        let joined = parse_gnu_args(&args(&["-MFdeps.d", "-c", "foo.c"]), Path::new("/p"));
        assert_eq!(
            joined.dep_flags.mf_path.as_deref(),
            Some(Path::new("/p/deps.d"))
        );
        assert!(!joined.dep_flags.depfile_to_stdout);

        let stdout = parse_gnu_args(&args(&["-MF-", "-c", "foo.c"]), Path::new("/p"));
        assert!(stdout.dep_flags.mf_path.is_none());
        assert!(stdout.dep_flags.depfile_to_stdout);
    }

    #[test]
    fn dep_flags_combined() {
        let parsed = parse_gnu_args(
            &args(&["-MMD", "-MF", "custom.d", "-c", "foo.c"]),
            Path::new("/p"),
        );
        assert!(parsed.dep_flags.has_md);
        assert_eq!(
            parsed.dep_flags.mf_path.as_deref(),
            Some(Path::new("/p/custom.d"))
        );
    }
}
