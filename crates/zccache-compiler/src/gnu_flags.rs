//! Shared GNU/Clang command-line flag classification.
//!
//! The compiler-invocation classifier and depgraph key parser must agree on
//! which exact flags consume the following argv element.  Keeping this table
//! in the compiler crate lets the depgraph depend on the classifier's source
//! of truth without creating a reverse dependency.

/// GNU/Clang flags whose value is always the next argv element.
///
/// This intentionally contains only exact flag spellings.  Flags that allow a
/// joined value (such as `-Ipath`) need parser-specific handling before this
/// classification is consulted.
pub const GNU_FLAGS_WITH_VALUE: &[&str] = &[
    "-o",
    "-D",
    "-U",
    "-I",
    "-isystem",
    "-iquote",
    "-idirafter",
    "-include",
    "-include-pch",
    "-imacros",
    "-isysroot",
    "-target",
    "--target",
    "-MF",
    "-MQ",
    "-MT",
    "-std",
    "-x",
    "-arch",
    "-march",
    "-mtune",
    "-mcpu",
    "-Xclang",
    "-mllvm",
    "--serialize-diagnostics",
];

/// Returns whether an exact GNU/Clang flag consumes the following argv value.
#[must_use]
pub fn gnu_flag_takes_value(flag: &str) -> bool {
    GNU_FLAGS_WITH_VALUE.contains(&flag)
}

#[cfg(test)]
mod tests {
    use super::{gnu_flag_takes_value, GNU_FLAGS_WITH_VALUE};

    #[test]
    fn value_flags_include_cross_compilation_and_preprocessor_inputs() {
        for flag in [
            "-target",
            "--target",
            "-arch",
            "-march",
            "-mtune",
            "-mcpu",
            "-isysroot",
            "-imacros",
        ] {
            assert!(gnu_flag_takes_value(flag), "{flag} must consume its value");
        }
    }

    #[test]
    fn value_flag_classifier_matches_the_exported_table() {
        for flag in GNU_FLAGS_WITH_VALUE {
            assert!(gnu_flag_takes_value(flag));
        }
        assert!(!gnu_flag_takes_value("-g"));
        assert!(!gnu_flag_takes_value("--target=x86_64-unknown-linux-gnu"));
    }
}
