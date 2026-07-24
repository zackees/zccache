//! Compiler flags that produce side artifacts outside the primary output.
//!
//! A cache entry currently stores the primary object and declared depfile, so
//! these flags must bypass the compile cache until their extra outputs are
//! captured.  Keep this narrow: PCH and module interface invocations whose
//! only product is their primary output remain cacheable.

/// Return the argv element that requests an unmodeled compiler side output.
///
/// `msvc_syntax` is true for both `cl.exe` and clang invocations using
/// clang-cl-style slash flags.
#[must_use]
pub fn unmodeled_side_output_flag<'a>(args: &'a [String], msvc_syntax: bool) -> Option<&'a str> {
    args.iter().map(String::as_str).find(|arg| {
        let lower = arg.to_ascii_lowercase();
        if msvc_syntax {
            let body = arg
                .strip_prefix('/')
                .or_else(|| arg.strip_prefix('-'))
                .unwrap_or(arg);
            let lower_body = body.to_ascii_lowercase();
            body.starts_with("Fd")
                || body.starts_with("Fp")
                || body.starts_with("Fr")
                || body.starts_with("FR")
                || body.starts_with("Fi")
                || body.starts_with("Fa")
                || matches!(lower_body.as_str(), "fa" | "fac" | "fas" | "facs")
                || lower_body.starts_with("yc")
                || lower_body.starts_with("doc")
                || lower_body.starts_with("module:")
                || lower_body.starts_with("headerunit")
                || lower_body.starts_with("sourcedependencies")
                || matches!(lower.as_str(), "/zi" | "-zi")
                || lower.starts_with("/ifc")
                || lower.starts_with("-ifc")
                || matches!(lower.as_str(), "/interface" | "-interface")
        } else {
            lower.starts_with("--serialize-diagnostics")
                || lower.starts_with("-dependency-file")
                || lower.starts_with("-mj")
                || (lower.starts_with("-fmodule") && lower != "-fmodules-ts")
                || lower == "-save-temps"
                || lower.starts_with("-save-temps=")
                || lower.starts_with("-gsplit-dwarf")
                || lower.starts_with("-fdump-")
                || matches!(
                    lower.as_str(),
                    "--coverage" | "-coverage" | "-fprofile-arcs" | "-ftest-coverage"
                )
                || lower == "-ftime-trace"
                || lower.starts_with("-ftime-trace=")
                || lower == "-fstack-usage"
                || lower.starts_with("-fcallgraph-info")
                || lower == "-fsave-optimization-record"
                || lower.starts_with("-foptimization-record-file")
                || lower.starts_with("-fopt-info")
                || lower.starts_with("-fdiagnostics-file=")
                || lower.starts_with("-fdiagnostics-format=sarif-file")
                || (lower.starts_with("-wa,") && lower.contains('='))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::unmodeled_side_output_flag;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn recognizes_single_tu_gnu_side_outputs() {
        for flag in [
            "-gsplit-dwarf",
            "--coverage",
            "-ftime-trace",
            "-fstack-usage",
            "-save-temps",
            "-fmodules",
        ] {
            assert_eq!(
                unmodeled_side_output_flag(&args(&["-c", "unit.c", flag]), false),
                Some(flag),
                "{flag} must bypass object-only caching"
            );
        }
    }

    #[test]
    fn recognizes_msvc_pdb_and_listing_side_outputs_case_insensitively() {
        for flag in ["/Zi", "/ZI", "/Fd:unit.pdb", "/FAcs", "/sourceDependencies"] {
            assert_eq!(
                unmodeled_side_output_flag(&args(&["/c", "unit.c", flag]), true),
                Some(flag),
                "{flag} must bypass object-only caching"
            );
        }
    }

    #[test]
    fn primary_output_only_pch_and_module_forms_are_not_side_outputs() {
        for flag in ["c++-header", "c++-module", "module.cppm"] {
            assert!(
                unmodeled_side_output_flag(&args(&["-c", flag]), false).is_none(),
                "{flag} is not itself an unmodeled side output"
            );
        }
    }
}
